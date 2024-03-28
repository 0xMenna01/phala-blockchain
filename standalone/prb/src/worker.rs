use crate::inv_db::{get_pool_by_pid, Worker};
use crate::pruntime::{PRuntimeClient, PRuntimeClientWithSemaphore};
use crate::pool_operator::PoolOperatorAccess;
use crate::utils::fetch_storage_bytes;
use crate::wm::{WorkerManagerMessage, WrappedWorkerManagerContext};
use crate::worker::WorkerLifecycleCommand::*;
use crate::use_parachain_api;
use anyhow::{anyhow, Result};
use chrono::prelude::*;
use futures::future::join;
use log::{debug, error, info, warn};

use phactory_api::prpc::{
    GetRuntimeInfoRequest, PhactoryInfo, SignEndpointsRequest,
};
use phala_pallets::pallet_computation::{SessionInfo, WorkerState};
use phala_pallets::registry::WorkerInfoV2;
use phala_types::AttestationProvider;
use phaxt::subxt::ext::sp_runtime;
use pherry::chain_client::{mq_next_sequence, search_suitable_genesis_for_worker};

use pherry::attestation_to_report;
use serde::{Deserialize, Serialize};
use sp_core::sr25519::Public as Sr25519Public;
use sp_core::{ByteArray, Pair};

use std::sync::Arc;
use std::time::Duration;
use subxt::dynamic::{storage, Value};
use tokio::sync::{mpsc, oneshot, Mutex as TokioMutex, RwLock};
use tokio::time::sleep;

pub type WorkerLifecycleCommandTx = mpsc::UnboundedSender<WorkerLifecycleCommand>;
pub type WorkerLifecycleCommandRx = mpsc::UnboundedReceiver<WorkerLifecycleCommand>;

pub enum WorkerLifecycleCommand {
    ShouldRestart,
    ShouldForceRegister,
    ShouldUpdateEndpoint(Vec<String>),
    ShouldTakeCheckpoint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkerLifecycleState {
    Starting,
    Synchronizing,
    Preparing,
    Working,
    GatekeeperWorking,

    HasError(String),
    Restarting,
    Disabled,
}

pub type WrappedWorkerContext = Arc<RwLock<WorkerContext>>;
pub type WorkerLifecycleStateTx = mpsc::UnboundedSender<WorkerLifecycleState>;
pub type WorkerLifecycleStateRx = mpsc::UnboundedReceiver<WorkerLifecycleState>;

macro_rules! use_lm {
    ($ctx:expr) => {{
        let lm = $ctx.current_lifecycle_manager.clone();
        let lm = lm.lock().unwrap();
        let ret = lm.clone().unwrap();
        drop(lm);
        ret
    }};
}

macro_rules! use_lm_with_ctx {
    ($ctx:expr) => {{
        let ctx = $ctx.clone();
        let ret = use_lm!(ctx);
        ret
    }};
}

macro_rules! set_worker_message {
    ($c:expr, $m:expr) => {{
        let cc = $c.clone();
        let mut cc = cc.write().await;
        cc.set_last_message($m);
        let lm = use_lm!(cc.ctx);
        drop(cc);
        tokio::spawn(lm.clone().webhook_send($c.clone()));
    }};
}

#[macro_export]
macro_rules! extract_essential_values {
    ($c:expr) => {{
        let cc = $c.clone();
        let cc = cc.read().await;
        let pr = cc.pr.clone();
        let lm = use_lm_with_ctx!(cc.ctx);
        let worker = cc.worker.clone();
        drop(cc);
        (lm, worker, pr)
    }};
}

macro_rules! return_if_error_or_restarting {
    ($c:expr, $r: expr) => {{
        let cc = $c.clone();
        let cc = cc.read().await;
        match &cc.state {
            WorkerLifecycleState::HasError(_) => {
                return $r;
            }
            WorkerLifecycleState::Restarting => {
                return $r;
            }
            _ => {}
        }
        drop(cc);
    }};
    ($c:expr) => {{
        let cc = $c.clone();
        let cc = cc.read().await;
        match &cc.state {
            WorkerLifecycleState::HasError(_) => {
                return;
            }
            WorkerLifecycleState::Restarting => {
                return;
            }
            _ => {}
        }
        drop(cc);
    }};
}

pub struct WorkerContext {
    pub id: String,
    pub self_ref: Option<WrappedWorkerContext>,
    pub sm_tx: Option<WorkerLifecycleStateTx>,
    pub worker: Worker,
    pub state: WorkerLifecycleState,
    pub tx: WorkerLifecycleCommandTx,
    pub rx: Arc<TokioMutex<WorkerLifecycleCommandRx>>,
    pub ctx: WrappedWorkerManagerContext,
    pub pr: Arc<PRuntimeClient>,
    pub info: Option<PhactoryInfo>,
    pub last_message: String,
    pub session_info: Option<SessionInfo>,
}

impl WorkerContext {
    pub async fn create(w: Worker, ctx: WrappedWorkerManagerContext) -> Result<Self> {
        let pr = crate::pruntime::create_client(w.endpoint.clone());
        let pr = Arc::new(pr);
        let (tx, rx) = mpsc::unbounded_channel::<WorkerLifecycleCommand>();

        let mut ret = Self {
            id: w.id.clone(),
            self_ref: None,
            sm_tx: None,
            worker: w,
            state: WorkerLifecycleState::Starting,
            tx,
            rx: Arc::new(TokioMutex::new(rx)),
            ctx,
            pr,
            info: None,
            last_message: String::new(),
            session_info: None,
        };
        ret.set_last_message("Starting lifecycle...");
        Ok(ret)
    }

    pub fn set_last_message<M: Into<String>>(&mut self, m: M) {
        let time: DateTime<Local> = Local::now();
        let worker = &self.worker;
        let m = m.into();
        self.last_message = format!("[{}] {}", time.format("%m-%d %H:%M:%S %Z"), &m);
        info!(
            "Worker {}({}, {}): {}",
            &worker.name, &worker.id, &worker.endpoint, m
        );
    }

    pub async fn start(c: WrappedWorkerContext) {
        debug!("WorkerContext::start");
        loop {
            let cc = c.clone();
            let mut cc = cc.write().await;
            let ctx = cc.ctx.clone();
            let lm = use_lm_with_ctx!(ctx);
            cc.state = WorkerLifecycleState::Starting;

            let worker = cc.worker.clone();
            drop(ctx);
            drop(cc);
            if let WorkerManagerMessage::ResponseErr(err_str) = lm
                .clone()
                .send_to_main_channel_and_wait_for_response(
                    WorkerManagerMessage::ShouldStartWorkerLifecycle(c.clone()),
                )
                .await
                .unwrap_or_else(|e| panic!("Failed to start worker {}: {e}", worker.name))
            {
                let cc = c.clone();
                let mut cc = cc.write().await;
                cc.state = WorkerLifecycleState::HasError(err_str);
                drop(cc);
            }

            sleep(Duration::from_secs(114514000)).await;

            /*
            let _ = join(
                tokio::spawn(Self::do_start(c.clone())),
                Self::message_loop(c.clone()),
            )
            .await; */
            let cc = c.clone();
            let cc = cc.read().await;
            let state = cc.state.clone();
            drop(cc);
            set_worker_message!(c, format!("Stopped in state: {:?}", state).as_str());
            loop {
                let cc = c.clone();
                let cc = cc.read().await;
                let state = cc.state.clone();
                drop(cc);
                if let WorkerLifecycleState::Restarting = state {
                    break;
                }
                sleep(Duration::from_secs(6)).await;
            }
            sleep(Duration::from_secs(3)).await;
            info!(
                "Worker {}({}, {}) restarted!",
                worker.name, worker.endpoint, worker.id
            );
        }
    }

    async fn set_state(c: WrappedWorkerContext, state: WorkerLifecycleState) {
        let c = c.read().await;
        match &c.state {
            WorkerLifecycleState::Restarting => {}
            _ => {
                let sm_tx = c.sm_tx.as_ref().unwrap().clone();
                sm_tx.send(state).expect("should update sm state");
            }
        }
    }

    async fn handle_on_starting(c: WrappedWorkerContext) -> Result<()> {
        let (lm, worker, pr) = extract_essential_values!(c);

        if !worker.enabled {
            anyhow::bail!("Worker not enabled!");
        }

        let dsm = lm.dsm.clone();

        let mut i = pr.get_info(()).await?;

        if !i.initialized {
            set_worker_message!(c, "Initializing pRuntime...");
            // pRuntime versions lower than 2.2.0 always returns an empty list.
            let supported = &i.supported_attestation_methods;
            let attestation_provider = if supported.is_empty() || supported.contains(&"epid".into())
            {
                Some(AttestationProvider::Ias)
            } else if supported.contains(&"dcap".into()) {
                Some(AttestationProvider::Dcap)
            } else {
                None
            };
            let init_req = dsm
                .get_init_runtime_default_request(attestation_provider)
                .await?;
            let res = pr.init_runtime(init_req).await?;
            set_worker_message!(c, "Initialized pRuntime.");
            debug!(
                "Worker {}({}, {}) init_runtime resp: {:?}",
                &worker.name, &worker.id, &worker.endpoint, res
            )
        }

        if i.public_key.is_none() {
            i = pr.get_info(()).await?;
        }

        if lm.fast_sync_enabled
            && i.can_load_chain_state
            && lm.dsm.is_relaychain_full
            && lm.dsm.is_parachain_full
        {
            let para_api = &lm
                .dsm
                .clone()
                .current_parachain_rpc_client(true)
                .await
                .expect("No online rpc client")
                .client;
            let pubkey = &i.public_key.unwrap();
            let pubkey = hex::decode(pubkey)?;
            set_worker_message!(c, "Trying to load chain state...");
            let lock = lm.fast_sync_semaphore.clone();
            let lock = lock.acquire().await?;
            let search = search_suitable_genesis_for_worker(para_api, &pubkey, None).await;
            drop(lock);
            match search {
                Ok((block_number, state)) => {
                    pr.with_lock(pr.load_chain_state(phactory_api::prpc::ChainState::new(
                        block_number,
                        state,
                    )))
                    .await??;
                    set_worker_message!(c, "Loaded chain state!");
                }
                Err(e) => {
                    set_worker_message!(c, "Failed to get suitable genesis.");
                    error!(
                        "Worker {}({}, {}) search_suitable_genesis_for_worker: {}",
                        &worker.name, &worker.id, &worker.endpoint, e
                    );
                }
            }
        }

        Self::set_state(c.clone(), WorkerLifecycleState::Synchronizing).await;
        Ok(())
    }

    async fn handle_on_preparing(c: WrappedWorkerContext) -> Result<()> {
        set_worker_message!(c, "Reached latest finalized height, start preparing...");
        let (lm, worker, pr) = extract_essential_values!(c);
        let txm = lm.txm.clone();

        let pid = worker.pid.ok_or(anyhow!("Worker belongs to no pool!"))?;
        let pool = get_pool_by_pid(lm.inv_db.clone(), pid)?
            .ok_or(anyhow!(format!("pool record #{pid} not found.")))?;
        let po = txm.db.get_po(pid)?;

        let mut sync_only = false;
        if pool.sync_only {
            set_worker_message!(
                c,
                format!("Sync only mode enabled for pool #{pid}").as_str()
            );
            sync_only = true;
        } else if worker.sync_only {
            set_worker_message!(c, "Sync only mode enabled for the pool.");
            sync_only = true;
        } else if po.is_none() {
            set_worker_message!(
                c,
                format!("Sync only mode enabled for pool #{pid} has no operator set.").as_str()
            );
            sync_only = true;
        }
        if sync_only {
            return Ok(());
        }

        let po = po.unwrap();
        let i = pr.get_info(()).await?;
        let mq_rx = Self::start_mq_sync(c.clone(), pid).await?;
        tokio::pin!(mq_rx);

        if !i.registered {
            Self::register_worker(c.clone(), true).await?;
        }

        if worker.gatekeeper {
            let cc = c.clone();
            let cc = cc.read().await;
            let sm_tx = cc.sm_tx.as_ref().unwrap().clone();
            drop(cc);
            sm_tx.send(WorkerLifecycleState::GatekeeperWorking)?;
            return Ok(());
        }

        let pubkey = i.public_key.ok_or(anyhow!("public key not found!"))?;
        let pubkey = hex::decode(pubkey)?;
        let pubkey = pubkey.as_slice();
        let pubkey = Sr25519Public::from_slice(pubkey).unwrap();

        let api =
            use_parachain_api!(lm.dsm, false).ok_or(anyhow!("no online substrate session"))?;

        let registry_query = storage("PhalaRegistry", "Workers", vec![Value::from_bytes(pubkey)]);
        let registry_info: Option<WorkerInfoV2<subxt::utils::AccountId32>> =
            fetch_storage_bytes(&api, &registry_query).await?;
        if let Some(registry_info) = registry_info {
            let po = if po.proxied.is_some() {
                po.proxied
                    .as_ref()
                    .map(|po| subxt::utils::AccountId32(*po.as_ref()))
            } else {
                Some(subxt::utils::AccountId32(
                    po.pair.public().as_slice().try_into()?,
                ))
            };
            if registry_info.operator.ne(&po) {
                Self::register_worker(c.clone(), true).await?;
            }
        }
        tokio::spawn(Self::update_session_loop(c.clone(), pubkey));

        let worker_binding_query = storage(
            "PhalaComputation",
            "WorkerBindings",
            vec![Value::from_bytes(pubkey)],
        );
        let worker_binding: Option<subxt::utils::AccountId32> =
            fetch_storage_bytes(&api, &worker_binding_query).await?;
        if worker_binding.is_none() {
            set_worker_message!(c, "Enabling worker in stakepool pallet...");
            txm.clone().add_worker(pid, pubkey).await?;
        }

        set_worker_message!(c, "Waiting for session info to update...");
        loop {
            let cc = c.clone();
            let cc = cc.read().await;
            let session = cc.session_info.clone();
            drop(cc);
            if let Some(session) = session {
                match session.state {
                    WorkerState::Ready => {
                        mq_rx.await?;
                        set_worker_message!(c, "Starting computing...");
                        txm.clone()
                            .start_computing(pid, pubkey, worker.stake)
                            .await?;
                        Self::set_state(c.clone(), WorkerLifecycleState::Working).await;
                    }
                    WorkerState::WorkerCoolingDown => {
                        set_worker_message!(c, "Worker is cooling down!");
                    }
                    _ => {
                        Self::set_state(c.clone(), WorkerLifecycleState::Working).await;
                    }
                }
                break;
            }
            sleep(Duration::from_secs(3)).await;
        }

        Ok(())
    }

    async fn update_info_loop(c: WrappedWorkerContext) {
        let (_lm, worker, pr) = extract_essential_values!(c);

        let mut retry_count: u8 = 0;

        loop {
            return_if_error_or_restarting!(c);

            let get_info_req = pr.get_info(()).await;
            match get_info_req {
                Ok(p) => {
                    retry_count = 0;
                    let cc = c.clone();
                    let mut cc = cc.write().await;
                    cc.info = Some(p);
                    drop(cc);
                }
                Err(e) => {
                    warn!("Failed to get_info from {}: {}", &worker.endpoint, &e);
                    if retry_count > 3 {
                        let cc = c.clone();
                        let mut cc = cc.write().await;
                        let m = format!("Failed to get_info from {}: {}", &worker.endpoint, &e);
                        cc.set_last_message(m.as_str());
                        drop(cc);
                        Self::set_state(c.clone(), WorkerLifecycleState::HasError(e.to_string()))
                            .await;
                    }
                    retry_count += 1;
                }
            }

            sleep(Duration::from_secs(5)).await;
        }
    }

    async fn update_session_loop(c: WrappedWorkerContext, pubkey: Sr25519Public) {
        loop {
            match Self::update_session_loop_inner(c.clone(), pubkey).await {
                Ok(_) => return,
                Err(e) => {
                    set_worker_message!(c.clone(), format!("{e}").as_str());
                }
            }
            sleep(Duration::from_secs(6)).await;
        }
    }

    async fn update_session_loop_inner(
        c: WrappedWorkerContext,
        pubkey: Sr25519Public,
    ) -> Result<()> {
        let (lm, _worker, _pr) = extract_essential_values!(c);
        let worker_binding_query = storage(
            "PhalaComputation",
            "WorkerBindings",
            vec![Value::from_bytes(pubkey)],
        );
        let mut worker_binding: Option<subxt::utils::AccountId32> = None;
        let mut session_query = None;

        loop {
            return_if_error_or_restarting!(c, Ok(()));

            let api = use_parachain_api!(lm.dsm, false);
            if api.is_none() {
                set_worker_message!(c, "No online parachain session!");
                sleep(Duration::from_secs(3)).await;
                continue;
            }
            let api = api.unwrap();
            if worker_binding.is_none() {
                worker_binding = fetch_storage_bytes(&api, &worker_binding_query).await?;
            }
            if worker_binding.is_none() {
                sleep(Duration::from_secs(3)).await;
                continue;
            }
            if session_query.is_none() {
                session_query = Some(storage(
                    "PhalaComputation",
                    "Sessions",
                    vec![Value::from_bytes(worker_binding.as_ref().unwrap())],
                ));
            }
            let session: Option<SessionInfo> =
                fetch_storage_bytes(&api, session_query.as_ref().unwrap()).await?;
            if let Some(session) = session {
                if session.state == WorkerState::WorkerUnresponsive {
                    set_worker_message!(c, "Worker unresponsive!")
                }
                let cc = c.clone();
                let mut cc = cc.write().await;
                cc.session_info = Some(session);
                drop(cc);
            }
            sleep(Duration::from_secs(6)).await;
        }
    }

    async fn update_endpoint(c: WrappedWorkerContext, endpoints: Vec<String>) -> Result<()> {
        let (lm, worker, pr) = extract_essential_values!(c);
        let pid = worker.pid.ok_or(anyhow!("missing pid"))?;
        let txm = lm.txm.clone();
        set_worker_message!(c, "Attempt to update endpoints...");
        let signed = pr
            .sign_endpoint_info(SignEndpointsRequest::new(endpoints))
            .await?;
        txm.update_worker_endpoint(pid, signed).await?;
        set_worker_message!(c, "Updated endpoints.");
        Ok(())
    }

    async fn register_worker(c: WrappedWorkerContext, force_ra: bool) -> Result<()> {
        let (lm, worker, pr) = extract_essential_values!(c);
        let txm = lm.txm.clone();
        let pid = worker.pid.ok_or(anyhow!("Worker belongs to no pool!"))?;
        let po = txm
            .db
            .get_po(pid)?
            .ok_or(anyhow!("Pool #{pid} has not operator set!"))?;
        let operator = match po.proxied {
            Some(o) => Some(o),
            None => {
                let public = po.pair.public();
                Some(public.into())
            }
        };
        set_worker_message!(c, "Registering worker...");
        let runtime_info = pr
            .with_lock(pr.get_runtime_info(GetRuntimeInfoRequest::new(force_ra, operator)))
            .await??;
        let pubkey = runtime_info.decode_public_key()?;
        let attestation = runtime_info
            .attestation
            .ok_or(anyhow!("Worker has no attestation!"))?;
        let v2 = attestation.payload.is_none();
        let attestation = attestation_to_report(
            attestation,
            &lm.main_ctx.pccs_url,
            lm.main_ctx.pccs_timeout_secs,
        )
        .await?;
        txm.clone()
            .register_worker(pid, runtime_info.encoded_runtime_info, attestation, v2)
            .await?;

        let api =
            use_parachain_api!(lm.dsm, false).ok_or(anyhow!("no online substrate session"))?;

        if !worker.gatekeeper {
            set_worker_message!(c, "Waiting for benchmark...");
            let registry_query =
                storage("PhalaRegistry", "Workers", vec![Value::from_bytes(pubkey)]);
            loop {
                let registry_info: Option<WorkerInfoV2<subxt::utils::AccountId32>> =
                    fetch_storage_bytes(&api, &registry_query).await?;
                if let Some(registry_info) = registry_info {
                    if let Some(score) = registry_info.initial_score {
                        if score > 0 {
                            set_worker_message!(c, "Got valid benchmark score!");
                            break;
                        }
                    }
                }
                sleep(Duration::from_secs(6)).await
            }
        }

        set_worker_message!(c, "Register done.");
        Ok(())
    }
}

impl WorkerContext {
    async fn start_mq_sync(c: WrappedWorkerContext, pid: u64) -> Result<oneshot::Receiver<()>> {
        set_worker_message!(c, "Now start synchronizing message queue!");
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(Self::mq_sync_loop(c.clone(), pid, tx));
        Ok(rx)
    }
    async fn mq_sync_loop(c: WrappedWorkerContext, pid: u64, first_shot: oneshot::Sender<()>) {
        let mut first_shot = Some(first_shot);
        loop {
            return_if_error_or_restarting!(c);

            debug!("mq_sync_loop new round");
            match Self::mq_sync_loop_round(c.clone(), pid).await {
                Ok(_) => {
                    if let Some(shot) = first_shot {
                        if shot.send(()).is_err() {
                            warn!("mq_sync_loop_round send first_shot returned Err");
                        };
                        first_shot = None;
                    }
                    sleep(Duration::from_secs(18)).await;
                }
                Err(e) => {
                    let msg = format!("Error while synchronizing mq: {e}");
                    warn!("{}", &msg);
                    set_worker_message!(c, msg.as_str());
                    sleep(Duration::from_secs(if msg.contains("BadSequence") {
                        18
                    } else {
                        6
                    }))
                    .await;
                }
            }
        }
    }
    async fn mq_sync_loop_round(c: WrappedWorkerContext, pid: u64) -> Result<()> {
        let (lm, _worker, pr) = extract_essential_values!(c);
        let txm = lm.txm.clone();
        let messages = pr
            .with_lock(pr.get_egress_messages(()))
            .await??
            .decode_messages()?;
        debug!("mq_sync_loop_round: {:?}", &messages);
        if messages.is_empty() {
            return Ok(());
        }
        let api =
            use_parachain_api!(lm.dsm, false).ok_or(anyhow!("Substrate client not ready."))?;
        let mut futures = Vec::new();
        for (sender, messages) in messages {
            if !messages.is_empty() {
                let min_seq = mq_next_sequence(&api, &sender).await?;
                for message in messages {
                    if message.sequence >= min_seq {
                        futures.push(txm.clone().sync_offchain_message(pid, message));
                    }
                }
            }
        }
        let _ = futures::future::try_join_all(futures).await?;
        Ok(())
    }
}