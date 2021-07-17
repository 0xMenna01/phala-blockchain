use crate::light_validation::{storage_proof::StorageProof, LightValidation};
use crate::std::vec::Vec;
use enclave_api::storage_sync::{BlockValidator, Error as SyncError, Result};

pub use storage_ext::{Storage, StorageExt};

impl BlockValidator for LightValidation<chain::Runtime> {
    fn submit_finalized_headers(
        &mut self,
        bridge_id: u64,
        header: chain::Header,
        ancestry_proof: Vec<chain::Header>,
        grandpa_proof: Vec<u8>,
        auhtority_set_change: Option<enclave_api::blocks::AuthoritySetChange>,
    ) -> Result<()> {
        self.submit_finalized_headers(
            bridge_id,
            header,
            ancestry_proof,
            grandpa_proof,
            auhtority_set_change,
        )
        .or(Err(SyncError::HeaderValidateFailed))
    }

    fn validate_storage_proof(
        &self,
        state_root: chain::Hash,
        proof: StorageProof,
        items: &[(&[u8], &[u8])],
    ) -> Result<()> {
        self.validate_storage_proof(state_root, proof, items)
            .or(Err(SyncError::HeaderValidateFailed))
    }
}

mod storage_ext {
    use crate::chain;
    use crate::light_validation::utils::storage_prefix;
    use crate::std::vec::Vec;
    use enclave_api::blocks::ParaId;
    use frame_system::EventRecord;
    use parity_scale_codec::Decode;
    use trie_storage::TrieStorage;

    pub type Storage = TrieStorage<crate::RuntimeHasher>;

    pub trait StorageExt {
        fn get_raw(&self, key: impl AsRef<[u8]>) -> Option<Vec<u8>>;
        fn get_decoded<T: Decode>(&self, key: impl AsRef<[u8]>) -> Option<T> {
            self.get_raw(key)
                .map(|v| Decode::decode(&mut &v[..]).ok())
                .flatten()
        }
        fn para_id(&self) -> Option<ParaId> {
            self.get_decoded(storage_prefix("ParachainInfo", "ParachainId"))
        }
        fn events(&self) -> Option<Vec<EventRecord<chain::Event, chain::Hash>>> {
            self.get_decoded(storage_prefix("System", "Events"))
        }
        fn timestamp_now(&self) -> Option<chain::Moment> {
            self.get_decoded(storage_prefix("Timestamp", "Now"))
        }
    }

    impl StorageExt for Storage {
        fn get_raw(&self, key: impl AsRef<[u8]>) -> Option<Vec<u8>> {
            self.get(key)
        }
    }
}
