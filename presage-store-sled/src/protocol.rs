use std::marker::PhantomData;

use async_trait::async_trait;
use log::{error, trace, warn};
use presage::{
    libsignal_service::{
        pre_keys::{KyberPreKeyStoreExt, PreKeysStore},
        prelude::Uuid,
        protocol::{
            Direction, GenericSignedPreKey, IdentityKey, IdentityKeyPair, IdentityKeyStore,
            KyberPreKeyId, KyberPreKeyRecord, KyberPreKeyStore, PreKeyId, PreKeyRecord,
            PreKeyStore, ProtocolAddress, ProtocolStore, SenderKeyRecord, SenderKeyStore,
            SessionRecord, SessionStore, SignalProtocolError, SignedPreKeyId, SignedPreKeyRecord,
            SignedPreKeyStore,
        },
        push_service::DEFAULT_DEVICE_ID,
        session_store::SessionStoreExt,
        ServiceAddress,
    },
    manager::RegistrationData,
    proto::verified,
    store::{ContentsStore, StateStore},
};
use sled::Batch;

use crate::{
    OnNewIdentity, SledStore, SledStoreError, SLED_KEY_NEXT_PQ_PRE_KEY_ID,
    SLED_KEY_NEXT_SIGNED_PRE_KEY_ID, SLED_KEY_PRE_KEYS_OFFSET_ID,
};

#[derive(Clone)]
pub struct SledProtocolStore<T: SledTrees> {
    pub(crate) store: SledStore,
    _trees: PhantomData<T>,
}

impl SledProtocolStore<AciSledStore> {
    pub(crate) fn aci_protocol_store(store: SledStore) -> Self {
        Self {
            store,
            _trees: Default::default(),
        }
    }
}

impl SledProtocolStore<PniSledStore> {
    pub(crate) fn pni_protocol_store(store: SledStore) -> Self {
        Self {
            store,
            _trees: Default::default(),
        }
    }
}

pub trait SledTrees: Clone {
    fn identities() -> &'static str;
    fn state() -> &'static str;
    fn pre_keys() -> &'static str;
    fn signed_pre_keys() -> &'static str;
    fn kyber_pre_keys() -> &'static str;
    fn kyber_pre_keys_last_resort() -> &'static str;
    fn sender_keys() -> &'static str;
    fn sessions() -> &'static str;

    fn identity_keypair(data: &RegistrationData) -> Result<IdentityKeyPair, SignalProtocolError>;
}

#[derive(Clone)]
pub struct AciSledStore;

impl SledTrees for AciSledStore {
    fn identities() -> &'static str {
        "identities"
    }

    fn state() -> &'static str {
        "state"
    }

    fn pre_keys() -> &'static str {
        "pre_keys"
    }

    fn signed_pre_keys() -> &'static str {
        "sender_keys"
    }

    fn kyber_pre_keys() -> &'static str {
        "signed_pre_keys"
    }

    fn kyber_pre_keys_last_resort() -> &'static str {
        "kyber_pre_keys_last_resort"
    }

    fn sender_keys() -> &'static str {
        "kyber_pre_keys"
    }

    fn sessions() -> &'static str {
        "sessions"
    }

    fn identity_keypair(data: &RegistrationData) -> Result<IdentityKeyPair, SignalProtocolError> {
        Ok(data.aci_identity_keypair())
    }
}

#[derive(Clone)]
pub struct PniSledStore;

impl SledTrees for PniSledStore {
    fn identities() -> &'static str {
        "identities"
    }

    fn state() -> &'static str {
        "pni_state"
    }

    fn pre_keys() -> &'static str {
        "pni_pre_keys"
    }

    fn signed_pre_keys() -> &'static str {
        "pni_sender_keys"
    }

    fn kyber_pre_keys() -> &'static str {
        "pni_signed_pre_keys"
    }

    fn kyber_pre_keys_last_resort() -> &'static str {
        "pni_kyber_pre_keys_last_resort"
    }

    fn sender_keys() -> &'static str {
        "pni_kyber_pre_keys"
    }

    fn sessions() -> &'static str {
        "pni_sessions"
    }

    fn identity_keypair(data: &RegistrationData) -> Result<IdentityKeyPair, SignalProtocolError> {
        data.pni_identity_keypair()
            .ok_or(SignalProtocolError::InvalidState(
                "failed to load identity key pair",
                "no registration data".into(),
            ))
    }
}

impl<T: SledTrees> SledProtocolStore<T> {
    pub(crate) fn clear(&self) -> Result<(), SledStoreError> {
        let db = self.store.db.write().expect("poisoned mutex");
        db.drop_tree(T::pre_keys())?;
        db.drop_tree(T::sender_keys())?;
        db.drop_tree(T::sessions())?;
        db.drop_tree(T::signed_pre_keys())?;
        db.drop_tree(T::kyber_pre_keys())?;
        Ok(())
    }
}

impl<T: SledTrees> ProtocolStore for SledProtocolStore<T> {}

#[async_trait(?Send)]
impl<T: SledTrees> PreKeyStore for SledProtocolStore<T> {
    async fn get_pre_key(&self, prekey_id: PreKeyId) -> Result<PreKeyRecord, SignalProtocolError> {
        let buf: Vec<u8> = self
            .store
            .get(T::pre_keys(), prekey_id.to_string())
            .ok()
            .flatten()
            .ok_or(SignalProtocolError::InvalidPreKeyId)?;

        PreKeyRecord::deserialize(&buf)
    }

    async fn save_pre_key(
        &mut self,
        prekey_id: PreKeyId,
        record: &PreKeyRecord,
    ) -> Result<(), SignalProtocolError> {
        self.store
            .insert(T::pre_keys(), prekey_id.to_string(), record.serialize()?)
            .expect("failed to store pre-key");
        Ok(())
    }

    async fn remove_pre_key(&mut self, prekey_id: PreKeyId) -> Result<(), SignalProtocolError> {
        self.store
            .remove(T::pre_keys(), prekey_id.to_string())
            .expect("failed to remove pre-key");
        Ok(())
    }
}

#[async_trait(?Send)]
impl<T: SledTrees> PreKeysStore for SledProtocolStore<T> {
    async fn next_pre_key_id(&self) -> Result<u32, SignalProtocolError> {
        Ok(self
            .store
            .get(T::state(), SLED_KEY_PRE_KEYS_OFFSET_ID)
            .map_err(|_| SignalProtocolError::InvalidPreKeyId)?
            .unwrap_or(0))
    }

    async fn set_next_pre_key_id(&mut self, id: u32) -> Result<(), SignalProtocolError> {
        self.store
            .insert(T::state(), SLED_KEY_PRE_KEYS_OFFSET_ID, id)
            .map_err(|_| SignalProtocolError::InvalidPreKeyId)?;
        Ok(())
    }

    async fn next_signed_pre_key_id(&self) -> Result<u32, SignalProtocolError> {
        Ok(self
            .store
            .get(T::state(), SLED_KEY_NEXT_SIGNED_PRE_KEY_ID)
            .map_err(|_| SignalProtocolError::InvalidSignedPreKeyId)?
            .unwrap_or(0))
    }

    async fn set_next_signed_pre_key_id(&mut self, id: u32) -> Result<(), SignalProtocolError> {
        self.store
            .insert(T::state(), SLED_KEY_NEXT_SIGNED_PRE_KEY_ID, id)
            .map_err(|_| SignalProtocolError::InvalidSignedPreKeyId)?;
        Ok(())
    }

    async fn next_pq_pre_key_id(&self) -> Result<u32, SignalProtocolError> {
        Ok(self
            .store
            .get(T::state(), SLED_KEY_NEXT_PQ_PRE_KEY_ID)
            .map_err(|_| SignalProtocolError::InvalidKyberPreKeyId)?
            .unwrap_or(0))
    }

    async fn set_next_pq_pre_key_id(&mut self, id: u32) -> Result<(), SignalProtocolError> {
        self.store
            .insert(T::state(), SLED_KEY_NEXT_PQ_PRE_KEY_ID, id)
            .map_err(|_| SignalProtocolError::InvalidKyberPreKeyId)?;
        Ok(())
    }
}

#[async_trait(?Send)]
impl<T: SledTrees> SignedPreKeyStore for SledProtocolStore<T> {
    async fn get_signed_pre_key(
        &self,
        signed_prekey_id: SignedPreKeyId,
    ) -> Result<SignedPreKeyRecord, SignalProtocolError> {
        let buf: Vec<u8> = self
            .store
            .get(T::signed_pre_keys(), signed_prekey_id.to_string())
            .ok()
            .flatten()
            .ok_or(SignalProtocolError::InvalidSignedPreKeyId)?;
        SignedPreKeyRecord::deserialize(&buf)
    }

    async fn save_signed_pre_key(
        &mut self,
        signed_prekey_id: SignedPreKeyId,
        record: &SignedPreKeyRecord,
    ) -> Result<(), SignalProtocolError> {
        self.store
            .insert(
                T::signed_pre_keys(),
                signed_prekey_id.to_string(),
                record.serialize()?,
            )
            .map_err(|e| {
                log::error!("sled error: {}", e);
                SignalProtocolError::InvalidState("save_signed_pre_key", "sled error".into())
            })?;
        Ok(())
    }
}

#[async_trait(?Send)]
impl<T: SledTrees> KyberPreKeyStore for SledProtocolStore<T> {
    async fn get_kyber_pre_key(
        &self,
        kyber_prekey_id: KyberPreKeyId,
    ) -> Result<KyberPreKeyRecord, SignalProtocolError> {
        let buf: Vec<u8> = self
            .store
            .get(T::kyber_pre_keys(), kyber_prekey_id.to_string())
            .ok()
            .flatten()
            .ok_or(SignalProtocolError::InvalidKyberPreKeyId)?;
        KyberPreKeyRecord::deserialize(&buf)
    }

    async fn save_kyber_pre_key(
        &mut self,
        kyber_prekey_id: KyberPreKeyId,
        record: &KyberPreKeyRecord,
    ) -> Result<(), SignalProtocolError> {
        self.store
            .insert(
                T::kyber_pre_keys(),
                kyber_prekey_id.to_string(),
                record.serialize()?,
            )
            .map_err(|e| {
                log::error!("sled error: {}", e);
                SignalProtocolError::InvalidState("save_kyber_pre_key", "sled error".into())
            })?;
        Ok(())
    }

    async fn mark_kyber_pre_key_used(
        &mut self,
        kyber_prekey_id: KyberPreKeyId,
    ) -> Result<(), SignalProtocolError> {
        let removed = self
            .store
            .remove(T::kyber_pre_keys(), kyber_prekey_id.to_string())
            .map_err(|e| {
                log::error!("sled error: {}", e);
                SignalProtocolError::InvalidState("mark_kyber_pre_key_used", "sled error".into())
            })?;
        if removed {
            log::trace!("removed kyber pre-key {kyber_prekey_id}");
        }
        Ok(())
    }
}

#[async_trait(?Send)]
impl<T: SledTrees> KyberPreKeyStoreExt for SledProtocolStore<T> {
    async fn store_last_resort_kyber_pre_key(
        &mut self,
        kyber_prekey_id: KyberPreKeyId,
        record: &KyberPreKeyRecord,
    ) -> Result<(), SignalProtocolError> {
        self.store
            .insert(
                T::kyber_pre_keys_last_resort(),
                kyber_prekey_id.to_string(),
                record.serialize()?,
            )
            .map_err(|e| {
                log::error!("sled error: {}", e);
                SignalProtocolError::InvalidState(
                    "store_last_resort_kyber_pre_key",
                    "sled error".into(),
                )
            })?;
        Ok(())
    }

    async fn load_last_resort_kyber_pre_keys(
        &self,
    ) -> Result<Vec<KyberPreKeyRecord>, SignalProtocolError> {
        self.store
            .db
            .read()
            .expect("poisoned mutex")
            .open_tree(T::kyber_pre_keys_last_resort())
            .map_err(|e| {
                log::error!("sled error: {}", e);
                SignalProtocolError::InvalidState(
                    "load_last_resort_kyber_pre_keys",
                    "sled error".into(),
                )
            })?
            .iter()
            .values()
            .filter_map(Result::ok)
            .map(|data| KyberPreKeyRecord::deserialize(&data))
            .collect()
    }

    async fn remove_kyber_pre_key(
        &mut self,
        kyber_prekey_id: KyberPreKeyId,
    ) -> Result<(), SignalProtocolError> {
        self.store
            .remove(T::kyber_pre_keys_last_resort(), kyber_prekey_id.to_string())?;
        self.store
            .remove(T::kyber_pre_keys_last_resort(), kyber_prekey_id.to_string())?;
        Ok(())
    }

    /// Analogous to markAllOneTimeKyberPreKeysStaleIfNecessary
    async fn mark_all_one_time_kyber_pre_keys_stale_if_necessary(
        &mut self,
        _stale_time: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), SignalProtocolError> {
        unimplemented!("should not be used yet")
    }

    /// Analogue of deleteAllStaleOneTimeKyberPreKeys
    async fn delete_all_stale_one_time_kyber_pre_keys(
        &mut self,
        _threshold: chrono::DateTime<chrono::Utc>,
        _min_count: usize,
    ) -> Result<(), SignalProtocolError> {
        unimplemented!("should not be used yet")
    }
}

#[async_trait(?Send)]
impl<T: SledTrees> SessionStore for SledProtocolStore<T> {
    async fn load_session(
        &self,
        address: &ProtocolAddress,
    ) -> Result<Option<SessionRecord>, SignalProtocolError> {
        let session = self.store.get(T::sessions(), address.to_string())?;
        trace!("loading session {} / exists={}", address, session.is_some());
        session
            .map(|b: Vec<u8>| SessionRecord::deserialize(&b))
            .transpose()
    }

    async fn store_session(
        &mut self,
        address: &ProtocolAddress,
        record: &SessionRecord,
    ) -> Result<(), SignalProtocolError> {
        trace!("storing session {}", address);
        self.store
            .insert(T::sessions(), address.to_string(), record.serialize()?)?;
        Ok(())
    }
}

#[async_trait(?Send)]
impl<T: SledTrees> SessionStoreExt for SledProtocolStore<T> {
    async fn get_sub_device_sessions(
        &self,
        address: &ServiceAddress,
    ) -> Result<Vec<u32>, SignalProtocolError> {
        let session_prefix = format!("{}.", address.uuid);
        trace!("get_sub_device_sessions {}", session_prefix);
        let session_ids: Vec<u32> = self
            .store
            .read()
            .open_tree(T::sessions())
            .map_err(SledStoreError::Db)?
            .scan_prefix(&session_prefix)
            .filter_map(|r| {
                let (key, _) = r.ok()?;
                let key_str = String::from_utf8_lossy(&key);
                let device_id = key_str.strip_prefix(&session_prefix)?;
                device_id.parse().ok()
            })
            .filter(|d| *d != DEFAULT_DEVICE_ID)
            .collect();
        Ok(session_ids)
    }

    async fn delete_session(&self, address: &ProtocolAddress) -> Result<(), SignalProtocolError> {
        trace!("deleting session {}", address);
        self.store
            .write()
            .open_tree(T::sessions())
            .map_err(SledStoreError::Db)?
            .remove(address.to_string())
            .map_err(|_e| SignalProtocolError::SessionNotFound(address.clone()))?;
        Ok(())
    }

    async fn delete_all_sessions(
        &self,
        address: &ServiceAddress,
    ) -> Result<usize, SignalProtocolError> {
        let db = self.store.write();
        let sessions_tree = db.open_tree(T::sessions()).map_err(SledStoreError::Db)?;

        let mut batch = Batch::default();
        sessions_tree
            .scan_prefix(address.uuid.to_string())
            .filter_map(|r| {
                let (key, _) = r.ok()?;
                Some(key)
            })
            .for_each(|k| batch.remove(k));

        db.apply_batch(batch).map_err(SledStoreError::Db)?;

        let len = sessions_tree.len();
        sessions_tree.clear().map_err(|_e| {
            SignalProtocolError::InvalidSessionStructure("failed to delete all sessions")
        })?;
        Ok(len)
    }
}

#[async_trait(?Send)]
impl<T: SledTrees> IdentityKeyStore for SledProtocolStore<T> {
    async fn get_identity_key_pair(&self) -> Result<IdentityKeyPair, SignalProtocolError> {
        trace!("getting identity_key_pair");
        let registration_data =
            self.store
                .load_registration_data()?
                .ok_or(SignalProtocolError::InvalidState(
                    "failed to load identity key pair",
                    "no registration data".into(),
                ))?;

        T::identity_keypair(&registration_data)
    }

    async fn get_local_registration_id(&self) -> Result<u32, SignalProtocolError> {
        let data =
            self.store
                .load_registration_data()?
                .ok_or(SignalProtocolError::InvalidState(
                    "failed to load registration ID",
                    "no registration data".into(),
                ))?;
        Ok(data.registration_id)
    }

    async fn save_identity(
        &mut self,
        address: &ProtocolAddress,
        identity_key: &IdentityKey,
    ) -> Result<bool, SignalProtocolError> {
        trace!("saving identity");
        let existed_before = self
            .store
            .insert(
                T::identities(),
                address.to_string(),
                identity_key.serialize(),
            )
            .map_err(|e| {
                error!("error saving identity for {:?}: {}", address, e);
                e
            })?;

        self.store.save_trusted_identity_message(
            address,
            *identity_key,
            if existed_before {
                verified::State::Unverified
            } else {
                verified::State::Default
            },
        );

        Ok(true)
    }

    async fn is_trusted_identity(
        &self,
        address: &ProtocolAddress,
        right_identity_key: &IdentityKey,
        _direction: Direction,
    ) -> Result<bool, SignalProtocolError> {
        match self
            .store
            .get(T::identities(), address.to_string())?
            .map(|b: Vec<u8>| IdentityKey::decode(&b))
            .transpose()?
        {
            None => {
                // when we encounter a new identity, we trust it by default
                warn!("trusting new identity {:?}", address);
                Ok(true)
            }
            // when we encounter some identity we know, we need to decide whether we trust it or not
            Some(left_identity_key) => {
                if left_identity_key == *right_identity_key {
                    Ok(true)
                } else {
                    match self.store.trust_new_identities {
                        OnNewIdentity::Trust => Ok(true),
                        OnNewIdentity::Reject => Ok(false),
                    }
                }
            }
        }
    }

    async fn get_identity(
        &self,
        address: &ProtocolAddress,
    ) -> Result<Option<IdentityKey>, SignalProtocolError> {
        self.store
            .get(T::identities(), address.to_string())?
            .map(|b: Vec<u8>| IdentityKey::decode(&b))
            .transpose()
    }
}

#[async_trait(?Send)]
impl<T: SledTrees> SenderKeyStore for SledProtocolStore<T> {
    async fn store_sender_key(
        &mut self,
        sender: &ProtocolAddress,
        distribution_id: Uuid,
        record: &SenderKeyRecord,
    ) -> Result<(), SignalProtocolError> {
        let key = format!(
            "{}.{}/{}",
            sender.name(),
            sender.device_id(),
            distribution_id
        );
        self.store
            .insert(T::sender_keys(), key, record.serialize()?)?;
        Ok(())
    }

    async fn load_sender_key(
        &mut self,
        sender: &ProtocolAddress,
        distribution_id: Uuid,
    ) -> Result<Option<SenderKeyRecord>, SignalProtocolError> {
        let key = format!(
            "{}.{}/{}",
            sender.name(),
            sender.device_id(),
            distribution_id
        );
        self.store
            .get(T::sender_keys(), key)?
            .map(|b: Vec<u8>| SenderKeyRecord::deserialize(&b))
            .transpose()
    }
}