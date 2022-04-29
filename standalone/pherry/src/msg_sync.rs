use anyhow::Result;
use log::{error, info};
use phaxt::subxt::extrinsic::Signer;
use std::time::Duration;

use crate::{
    chain_client::{mq_next_sequence, update_signer_nonce},
    types::{ParachainApi, PrClient, SrSigner},
};
pub use tokio::sync::mpsc::{channel, Receiver, Sender};

pub enum Error {
    BadSignature, // Might due to runtime updated.
    OtherRpcError,
}

pub fn create_report_channel() -> (Sender<Error>, Receiver<Error>) {
    channel(1024)
}

pub async fn maybe_sync_mq_egress(
    api: &ParachainApi,
    pr: &PrClient,
    signer: &mut SrSigner,
    tip: u128,
    longevity: u64,
    max_sync_msgs_per_round: u64,
    err_report: Sender<Error>,
) -> Result<()> {
    // Send the query
    let messages = pr.get_egress_messages(()).await?.decode_messages()?;

    // No pending message. We are done.
    if messages.is_empty() {
        return Ok(());
    }

    update_signer_nonce(api, signer).await?;

    let mut sync_msgs_count = 0;

    'sync_outer: for (sender, messages) in messages {
        if messages.is_empty() {
            continue;
        }
        let min_seq = mq_next_sequence(api, &sender).await?;

        info!("Next seq for {} is {}", sender, min_seq);

        for message in messages {
            if message.sequence < min_seq {
                info!("{} has been submitted. Skipping...", message.sequence);
                continue;
            }
            let msg_info = format!(
                "sender={} seq={} dest={} nonce={:?}",
                sender,
                message.sequence,
                String::from_utf8_lossy(&message.message.destination.path()[..]),
                signer.nonce()
            );
            info!("Submitting message: {}", msg_info);

            let params = crate::mk_params(api, longevity, tip).await?;
            let extrinsic = api
                .tx()
                .phala_mq()
                .sync_offchain_message(message)
                .create_signed(signer, params)
                .await;
            signer.increment_nonce();
            match extrinsic {
                Ok(extrinsic) => {
                    let api = ParachainApi::from(api.client.clone());
                    let err_report = err_report.clone();
                    tokio::spawn(async move {
                        const TIMEOUT: u64 = 120;
                        let fut = api.client.rpc().submit_extrinsic(extrinsic);
                        let result = tokio::time::timeout(Duration::from_secs(TIMEOUT), fut).await;
                        match result {
                            Err(_) => {
                                error!("Submit message timed out: {}", msg_info);
                                let _ = err_report.send(Error::OtherRpcError).await;
                            }
                            Ok(Err(err)) => {
                                error!("Error submitting message {}: {:?}", msg_info, err);
                                use phaxt::subxt::{rpc::RpcError, BasicError as SubxtError};
                                let report = match err {
                                    SubxtError::Rpc(RpcError::Request(err)) => {
                                        if err.contains("bad signature") {
                                            Error::BadSignature
                                        } else {
                                            Error::OtherRpcError
                                        }
                                    }
                                    _ => Error::OtherRpcError,
                                };
                                let _ = err_report.send(report).await;
                            }
                            Ok(Ok(hash)) => {
                                info!("Message submited: {} xt-hash={:?}", msg_info, hash);
                            }
                        }
                    });
                }
                Err(err) => {
                    panic!("Failed to sign the call: {:?}", err);
                }
            }
            sync_msgs_count += 1;
            if sync_msgs_count >= max_sync_msgs_per_round {
                info!("Synced {} messages, take a break", sync_msgs_count);
                break 'sync_outer;
            }
        }
    }
    Ok(())
}
