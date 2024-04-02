use std::{
    ops::{Bound, Range, RangeBounds, RangeFull},
    path::Path,
    sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::prelude::*;
use log::debug;
use presage::libsignal_service::zkgroup::GroupMasterKeyBytes;
use presage::libsignal_service::{
    self,
    content::Content,
    groups_v2::Group,
    models::Contact,
    prelude::{ProfileKey, Uuid},
    Profile,
};
use presage::store::{ContentExt, ContentsStore, StateStore, StickerPack, Store, Thread};
use presage::{manager::RegistrationData, AvatarBytes};
use prost::Message;
use protocol::{AciSledStore, PniSledStore, SledProtocolStore};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sled::IVec;

use crate::protobuf::ContentProto;

mod error;
mod protobuf;
mod protocol;

pub use error::SledStoreError;

const SLED_TREE_CONTACTS: &str = "contacts";
const SLED_TREE_GROUPS: &str = "groups";
const SLED_TREE_GROUP_AVATARS: &str = "group_avatars";
const SLED_TREE_STATE: &str = "state";
const SLED_TREE_THREADS_PREFIX: &str = "threads";
const SLED_TREE_PROFILES: &str = "profiles";
const SLED_TREE_PROFILE_AVATARS: &str = "profile_avatars";
const SLED_TREE_PROFILE_KEYS: &str = "profile_keys";
const SLED_TREE_STICKER_PACKS: &str = "sticker_packs";

const SLED_KEY_NEXT_SIGNED_PRE_KEY_ID: &str = "next_signed_pre_key_id";
const SLED_KEY_NEXT_PQ_PRE_KEY_ID: &str = "next_pq_pre_key_id";
const SLED_KEY_PRE_KEYS_OFFSET_ID: &str = "pre_keys_offset_id";
const SLED_KEY_REGISTRATION: &str = "registration";
const SLED_KEY_SCHEMA_VERSION: &str = "schema_version";
#[cfg(feature = "encryption")]
const SLED_KEY_STORE_CIPHER: &str = "store_cipher";

#[derive(Clone)]
pub struct SledStore {
    db: Arc<RwLock<sled::Db>>,
    #[cfg(feature = "encryption")]
    cipher: Option<Arc<presage_store_cipher::StoreCipher>>,
    /// Whether to trust new identities automatically (for instance, when a somebody's phone has changed)
    trust_new_identities: OnNewIdentity,
}

/// Sometimes Migrations can't proceed without having to drop existing
/// data. This allows you to configure, how these cases should be handled.
#[derive(Default, PartialEq, Eq, Clone, Debug)]
pub enum MigrationConflictStrategy {
    /// Just drop the data, we don't care that we have to register or link again
    Drop,
    /// Raise a `Error::MigrationConflict` error with the path to the
    /// DB in question. The caller then has to take care about what they want
    /// to do and try again after.
    #[default]
    Raise,
    /// _Default_: The _entire_ database is backed up under, before the databases are dropped.
    BackupAndDrop,
}

#[derive(PartialEq, Eq, Clone, Debug, Default, Serialize, Deserialize)]
pub enum SchemaVersion {
    /// prior to any versioning of the schema
    #[default]
    V0 = 0,
    V1 = 1,
    V2 = 2,
    V3 = 3,
    // Introduction of avatars, requires dropping all profiles from the cache
    V4 = 4,
}

impl SchemaVersion {
    fn current() -> SchemaVersion {
        Self::V4
    }

    /// return an iterator on all the necessary migration steps from another version
    fn steps(self) -> impl Iterator<Item = SchemaVersion> {
        Range {
            start: self as u8 + 1,
            end: Self::current() as u8 + 1,
        }
        .map(|i| match i {
            1 => SchemaVersion::V1,
            2 => SchemaVersion::V2,
            3 => SchemaVersion::V3,
            4 => SchemaVersion::V4,
            _ => unreachable!("oops, this not supposed to happen!"),
        })
    }
}

#[derive(Debug, Clone)]
pub enum OnNewIdentity {
    Reject,
    Trust,
}

impl SledStore {
    #[allow(unused_variables)]
    fn new(
        db_path: impl AsRef<Path>,
        passphrase: Option<impl AsRef<str>>,
        trust_new_identities: OnNewIdentity,
    ) -> Result<Self, SledStoreError> {
        let database = sled::open(db_path)?;

        #[cfg(feature = "encryption")]
        let cipher = passphrase
            .map(|p| Self::get_or_create_store_cipher(&database, p.as_ref()))
            .transpose()?;

        #[cfg(not(feature = "encryption"))]
        if passphrase.is_some() {
            panic!("A passphrase was supplied but the encryption feature flag is not enabled")
        }

        Ok(SledStore {
            db: Arc::new(RwLock::new(database)),
            #[cfg(feature = "encryption")]
            cipher: cipher.map(Arc::new),
            trust_new_identities,
        })
    }

    pub fn open(
        db_path: impl AsRef<Path>,
        migration_conflict_strategy: MigrationConflictStrategy,
        trust_new_identities: OnNewIdentity,
    ) -> Result<Self, SledStoreError> {
        Self::open_with_passphrase(
            db_path,
            None::<&str>,
            migration_conflict_strategy,
            trust_new_identities,
        )
    }

    pub fn open_with_passphrase(
        db_path: impl AsRef<Path>,
        passphrase: Option<impl AsRef<str>>,
        migration_conflict_strategy: MigrationConflictStrategy,
        trust_new_identities: OnNewIdentity,
    ) -> Result<Self, SledStoreError> {
        let passphrase = passphrase.as_ref();

        migrate(&db_path, passphrase, migration_conflict_strategy)?;
        Self::new(db_path, passphrase, trust_new_identities)
    }

    #[cfg(feature = "encryption")]
    fn get_or_create_store_cipher(
        database: &sled::Db,
        passphrase: &str,
    ) -> Result<presage_store_cipher::StoreCipher, SledStoreError> {
        let cipher = if let Some(key) = database.get(SLED_KEY_STORE_CIPHER)? {
            presage_store_cipher::StoreCipher::import(passphrase, &key)?
        } else {
            let cipher = presage_store_cipher::StoreCipher::new();
            #[cfg(not(test))]
            let export = cipher.export(passphrase);
            #[cfg(test)]
            let export = cipher.insecure_export_fast_for_testing(passphrase);
            database.insert(SLED_KEY_STORE_CIPHER, export?)?;
            cipher
        };

        Ok(cipher)
    }

    #[cfg(test)]
    fn temporary() -> Result<Self, SledStoreError> {
        let db = sled::Config::new().temporary(true).open()?;
        Ok(Self {
            db: Arc::new(RwLock::new(db)),
            #[cfg(feature = "encryption")]
            // use store cipher with a random key
            cipher: Some(Arc::new(presage_store_cipher::StoreCipher::new())),
            trust_new_identities: OnNewIdentity::Reject
        })
    }

    fn read(&self) -> RwLockReadGuard<sled::Db> {
        self.db.read().expect("poisoned rwlock")
    }

    fn write(&self) -> RwLockWriteGuard<sled::Db> {
        self.db.write().expect("poisoned rwlock")
    }

    fn schema_version(&self) -> SchemaVersion {
        self.get(SLED_TREE_STATE, SLED_KEY_SCHEMA_VERSION)
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    #[cfg(feature = "encryption")]
    fn decrypt_value<T: DeserializeOwned>(&self, value: &[u8]) -> Result<T, SledStoreError> {
        if let Some(cipher) = self.cipher.as_ref() {
            Ok(cipher.decrypt_value(value)?)
        } else {
            Ok(serde_json::from_slice(value)?)
        }
    }

    #[cfg(not(feature = "encryption"))]
    fn decrypt_value<T: DeserializeOwned>(&self, value: &[u8]) -> Result<T, SledStoreError> {
        Ok(serde_json::from_slice(value)?)
    }

    #[cfg(feature = "encryption")]
    fn encrypt_value(&self, value: &impl Serialize) -> Result<Vec<u8>, SledStoreError> {
        if let Some(cipher) = self.cipher.as_ref() {
            Ok(cipher.encrypt_value(value)?)
        } else {
            Ok(serde_json::to_vec(value)?)
        }
    }

    #[cfg(not(feature = "encryption"))]
    fn encrypt_value(&self, value: &impl Serialize) -> Result<Vec<u8>, SledStoreError> {
        Ok(serde_json::to_vec(value)?)
    }

    pub fn get<K, V>(&self, tree: &str, key: K) -> Result<Option<V>, SledStoreError>
    where
        K: AsRef<[u8]>,
        V: DeserializeOwned,
    {
        self.read()
            .open_tree(tree)?
            .get(key)?
            .map(|p| self.decrypt_value(&p))
            .transpose()
            .map_err(SledStoreError::from)
    }

    fn insert<K, V>(&self, tree: &str, key: K, value: V) -> Result<bool, SledStoreError>
    where
        K: AsRef<[u8]>,
        V: Serialize,
    {
        let value = self.encrypt_value(&value)?;
        let db = self.write();
        let replaced = db.open_tree(tree)?.insert(key, value)?;
        db.flush()?;
        Ok(replaced.is_some())
    }

    fn remove<K>(&self, tree: &str, key: K) -> Result<bool, SledStoreError>
    where
        K: AsRef<[u8]>,
    {
        let db = self.write();
        let removed = db.open_tree(tree)?.remove(key)?;
        db.flush()?;
        Ok(removed.is_some())
    }

    /// build a hashed messages thread key
    fn messages_thread_tree_name(&self, t: &Thread) -> String {
        let key = match t {
            Thread::Contact(uuid) => {
                format!("{SLED_TREE_THREADS_PREFIX}:contact:{uuid}")
            }
            Thread::Group(group_id) => format!(
                "{SLED_TREE_THREADS_PREFIX}:group:{}",
                BASE64_STANDARD.encode(group_id)
            ),
        };
        let mut hasher = Sha256::new();
        hasher.update(key.as_bytes());
        format!("{SLED_TREE_THREADS_PREFIX}:{:x}", hasher.finalize())
    }

    fn profile_key_for_uuid(&self, uuid: Uuid, key: ProfileKey) -> String {
        let key = uuid.into_bytes().into_iter().chain(key.get_bytes());

        let mut hasher = Sha256::new();
        hasher.update(key.collect::<Vec<_>>());
        format!("{:x}", hasher.finalize())
    }
}

fn migrate(
    db_path: impl AsRef<Path>,
    passphrase: Option<impl AsRef<str>>,
    migration_conflict_strategy: MigrationConflictStrategy,
) -> Result<(), SledStoreError> {
    let db_path = db_path.as_ref();
    let passphrase = passphrase.as_ref();

    let run_migrations = move || {
        let mut store = SledStore::new(db_path, passphrase, OnNewIdentity::Reject)?;
        let schema_version = store.schema_version();
        for step in schema_version.steps() {
            match &step {
                SchemaVersion::V1 => {
                    debug!("migrating from v0, nothing to do")
                }
                SchemaVersion::V2 => {
                    debug!("migrating from schema v1 to v2: encrypting state if cipher is enabled");

                    // load registration data the old school way
                    let registration = store.read().get(SLED_KEY_REGISTRATION)?;
                    if let Some(data) = registration {
                        let state = serde_json::from_slice(&data).map_err(SledStoreError::from)?;

                        // save it the new school way
                        store.save_registration_data(&state)?;

                        // remove old data
                        let db = store.write();
                        db.remove(SLED_KEY_REGISTRATION)?;
                        db.flush()?;
                    }
                }
                SchemaVersion::V3 => {
                    debug!("migrating from schema v2 to v3: dropping encrypted group cache");
                    let db = store.write();
                    db.drop_tree(SLED_TREE_GROUPS)?;
                    db.flush()?;
                }
                SchemaVersion::V4 => {
                    debug!("migrating from schema v3 to v4: dropping profile cache");
                    let db = store.write();
                    db.drop_tree(SLED_TREE_PROFILES)?;
                    db.flush()?;
                }
                _ => return Err(SledStoreError::MigrationConflict),
            }

            store.insert(SLED_TREE_STATE, SLED_KEY_SCHEMA_VERSION, step)?;
        }

        Ok(())
    };

    if let Err(SledStoreError::MigrationConflict) = run_migrations() {
        match migration_conflict_strategy {
            MigrationConflictStrategy::BackupAndDrop => {
                let mut new_db_path = db_path.to_path_buf();
                new_db_path.set_extension(format!(
                    "{}.backup",
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .expect("time doesn't go backwards")
                        .as_secs()
                ));
                fs_extra::dir::create_all(&new_db_path, false)?;
                fs_extra::dir::copy(db_path, new_db_path, &fs_extra::dir::CopyOptions::new())?;
                fs_extra::dir::remove(db_path)?;
            }
            MigrationConflictStrategy::Drop => {
                fs_extra::dir::remove(db_path)?;
            }
            MigrationConflictStrategy::Raise => return Err(SledStoreError::MigrationConflict),
        }
    }

    Ok(())
}

impl StateStore for SledStore {
    type StateStoreError = SledStoreError;

    fn load_registration_data(&self) -> Result<Option<RegistrationData>, SledStoreError> {
        self.get(SLED_TREE_STATE, SLED_KEY_REGISTRATION)
    }

    fn save_registration_data(&mut self, state: &RegistrationData) -> Result<(), SledStoreError> {
        self.insert(SLED_TREE_STATE, SLED_KEY_REGISTRATION, state)?;
        Ok(())
    }

    fn is_registered(&self) -> bool {
        self.load_registration_data().unwrap_or_default().is_some()
    }

    fn clear_registration(&mut self) -> Result<(), SledStoreError> {
        let db = self.write();
        db.remove(SLED_KEY_REGISTRATION)?;

        db.drop_tree(SLED_TREE_STATE)?;
        db.drop_tree(SLED_TREE_PROFILES)?;
        db.drop_tree(SLED_TREE_PROFILE_KEYS)?;

        // drop all keys
        self.aci_protocol_store().clear()?;
        self.pni_protocol_store().clear()?;

        db.flush()?;

        Ok(())
    }
}

impl ContentsStore for SledStore {
    type ContentsStoreError = SledStoreError;

    type ContactsIter = SledContactsIter;
    type GroupsIter = SledGroupsIter;
    type MessagesIter = SledMessagesIter;
    type StickerPacksIter = SledStickerPacksIter;

    fn clear_contacts(&mut self) -> Result<(), SledStoreError> {
        self.write().drop_tree(SLED_TREE_CONTACTS)?;
        Ok(())
    }

    fn save_contact(&mut self, contact: &Contact) -> Result<(), SledStoreError> {
        self.insert(SLED_TREE_CONTACTS, contact.uuid, contact)?;
        debug!("saved contact");
        Ok(())
    }

    fn contacts(&self) -> Result<Self::ContactsIter, SledStoreError> {
        Ok(SledContactsIter {
            iter: self.read().open_tree(SLED_TREE_CONTACTS)?.iter(),
            #[cfg(feature = "encryption")]
            cipher: self.cipher.clone(),
        })
    }

    fn contact_by_id(&self, id: &Uuid) -> Result<Option<Contact>, SledStoreError> {
        self.get(SLED_TREE_CONTACTS, id)
    }

    /// Groups

    fn clear_groups(&mut self) -> Result<(), SledStoreError> {
        let db = self.write();
        db.drop_tree(SLED_TREE_GROUPS)?;
        db.flush()?;
        Ok(())
    }

    fn groups(&self) -> Result<Self::GroupsIter, SledStoreError> {
        Ok(SledGroupsIter {
            iter: self.read().open_tree(SLED_TREE_GROUPS)?.iter(),
            #[cfg(feature = "encryption")]
            cipher: self.cipher.clone(),
        })
    }

    fn group(
        &self,
        master_key_bytes: GroupMasterKeyBytes,
    ) -> Result<Option<Group>, SledStoreError> {
        self.get(SLED_TREE_GROUPS, master_key_bytes)
    }

    fn save_group(
        &self,
        master_key: GroupMasterKeyBytes,
        group: &Group,
    ) -> Result<(), SledStoreError> {
        self.insert(SLED_TREE_GROUPS, master_key, group)?;
        Ok(())
    }

    fn group_avatar(
        &self,
        master_key_bytes: GroupMasterKeyBytes,
    ) -> Result<Option<AvatarBytes>, SledStoreError> {
        self.get(SLED_TREE_GROUP_AVATARS, master_key_bytes)
    }

    fn save_group_avatar(
        &self,
        master_key: GroupMasterKeyBytes,
        avatar: &AvatarBytes,
    ) -> Result<(), SledStoreError> {
        self.insert(SLED_TREE_GROUP_AVATARS, master_key, avatar)?;
        Ok(())
    }

    /// Messages

    fn clear_messages(&mut self) -> Result<(), SledStoreError> {
        let db = self.write();
        for name in db.tree_names() {
            if name
                .as_ref()
                .starts_with(SLED_TREE_THREADS_PREFIX.as_bytes())
            {
                db.drop_tree(&name)?;
            }
        }
        db.flush()?;
        Ok(())
    }

    fn clear_thread(&mut self, thread: &Thread) -> Result<(), SledStoreError> {
        log::trace!("clearing thread {thread}");

        let db = self.write();
        db.drop_tree(self.messages_thread_tree_name(thread))?;
        db.flush()?;

        Ok(())
    }

    fn save_message(&self, thread: &Thread, message: Content) -> Result<(), SledStoreError> {
        let ts = message.timestamp();
        log::trace!("storing a message with thread: {thread}, timestamp: {ts}",);

        let tree = self.messages_thread_tree_name(thread);
        let key = ts.to_be_bytes();

        let proto: ContentProto = message.into();
        let value = proto.encode_to_vec();

        self.insert(&tree, key, value)?;

        Ok(())
    }

    fn delete_message(&mut self, thread: &Thread, timestamp: u64) -> Result<bool, SledStoreError> {
        let tree = self.messages_thread_tree_name(thread);
        self.remove(&tree, timestamp.to_be_bytes())
    }

    fn message(
        &self,
        thread: &Thread,
        timestamp: u64,
    ) -> Result<Option<libsignal_service::prelude::Content>, SledStoreError> {
        // Big-Endian needed, otherwise wrong ordering in sled.
        let val: Option<Vec<u8>> = self.get(
            &self.messages_thread_tree_name(thread),
            timestamp.to_be_bytes(),
        )?;
        match val {
            Some(ref v) => {
                let proto = ContentProto::decode(v.as_slice())?;
                let content = proto.try_into()?;
                Ok(Some(content))
            }
            None => Ok(None),
        }
    }

    fn messages(
        &self,
        thread: &Thread,
        range: impl RangeBounds<u64>,
    ) -> Result<Self::MessagesIter, SledStoreError> {
        let tree_thread = self
            .read()
            .open_tree(self.messages_thread_tree_name(thread))?;
        debug!("{} messages in this tree", tree_thread.len());

        let iter = match (range.start_bound(), range.end_bound()) {
            (Bound::Included(start), Bound::Unbounded) => tree_thread.range(start.to_be_bytes()..),
            (Bound::Included(start), Bound::Excluded(end)) => {
                tree_thread.range(start.to_be_bytes()..end.to_be_bytes())
            }
            (Bound::Included(start), Bound::Included(end)) => {
                tree_thread.range(start.to_be_bytes()..=end.to_be_bytes())
            }
            (Bound::Unbounded, Bound::Included(end)) => tree_thread.range(..=end.to_be_bytes()),
            (Bound::Unbounded, Bound::Excluded(end)) => tree_thread.range(..end.to_be_bytes()),
            (Bound::Unbounded, Bound::Unbounded) => tree_thread.range::<[u8; 8], RangeFull>(..),
            (Bound::Excluded(_), _) => {
                unreachable!("range that excludes the initial value")
            }
        };

        Ok(SledMessagesIter {
            #[cfg(feature = "encryption")]
            cipher: self.cipher.clone(),
            iter,
        })
    }

    fn upsert_profile_key(&mut self, uuid: &Uuid, key: ProfileKey) -> Result<bool, SledStoreError> {
        self.insert(SLED_TREE_PROFILE_KEYS, uuid.as_bytes(), key)
    }

    fn profile_key(&self, uuid: &Uuid) -> Result<Option<ProfileKey>, SledStoreError> {
        self.get(SLED_TREE_PROFILE_KEYS, uuid.as_bytes())
    }

    fn save_profile(
        &mut self,
        uuid: Uuid,
        key: ProfileKey,
        profile: Profile,
    ) -> Result<(), SledStoreError> {
        let key = self.profile_key_for_uuid(uuid, key);
        self.insert(SLED_TREE_PROFILES, key, profile)?;
        Ok(())
    }

    fn profile(&self, uuid: Uuid, key: ProfileKey) -> Result<Option<Profile>, SledStoreError> {
        let key = self.profile_key_for_uuid(uuid, key);
        self.get(SLED_TREE_PROFILES, key)
    }

    fn save_profile_avatar(
        &mut self,
        uuid: Uuid,
        key: ProfileKey,
        avatar: &AvatarBytes,
    ) -> Result<(), SledStoreError> {
        let key = self.profile_key_for_uuid(uuid, key);
        self.insert(SLED_TREE_PROFILE_AVATARS, key, avatar)?;
        Ok(())
    }

    fn profile_avatar(
        &self,
        uuid: Uuid,
        key: ProfileKey,
    ) -> Result<Option<AvatarBytes>, SledStoreError> {
        let key = self.profile_key_for_uuid(uuid, key);
        self.get(SLED_TREE_PROFILE_AVATARS, key)
    }

    fn add_sticker_pack(&mut self, pack: &StickerPack) -> Result<(), SledStoreError> {
        self.insert(SLED_TREE_STICKER_PACKS, pack.id.clone(), pack)?;
        Ok(())
    }

    fn remove_sticker_pack(&mut self, id: &[u8]) -> Result<bool, SledStoreError> {
        self.remove(SLED_TREE_STICKER_PACKS, id)
    }

    fn sticker_pack(&self, id: &[u8]) -> Result<Option<StickerPack>, SledStoreError> {
        self.get(SLED_TREE_STICKER_PACKS, id)
    }

    fn sticker_packs(&self) -> Result<Self::StickerPacksIter, SledStoreError> {
        Ok(SledStickerPacksIter {
            cipher: self.cipher.clone(),
            iter: self.read().open_tree(SLED_TREE_STICKER_PACKS)?.iter(),
        })
    }
}

impl Store for SledStore {
    type Error = SledStoreError;
    type AciStore = SledProtocolStore<AciSledStore>;
    type PniStore = SledProtocolStore<PniSledStore>;

    fn clear(&mut self) -> Result<(), SledStoreError> {
        self.clear_registration()?;

        let db = self.write();
        db.drop_tree(SLED_TREE_CONTACTS)?;
        db.drop_tree(SLED_TREE_GROUPS)?;
        db.drop_tree(SLED_TREE_PROFILES)?;
        db.drop_tree(SLED_TREE_PROFILE_AVATARS)?;

        for tree in db
            .tree_names()
            .into_iter()
            .filter(|n| n.starts_with(SLED_TREE_THREADS_PREFIX.as_bytes()))
        {
            db.drop_tree(tree)?;
        }

        db.flush()?;

        Ok(())
    }

    fn aci_protocol_store(&self) -> Self::AciStore {
        SledProtocolStore::aci_protocol_store(self.clone())
    }

    fn pni_protocol_store(&self) -> Self::PniStore {
        SledProtocolStore::pni_protocol_store(self.clone())
    }
}

pub struct SledContactsIter {
    #[cfg(feature = "encryption")]
    cipher: Option<Arc<presage_store_cipher::StoreCipher>>,
    iter: sled::Iter,
}

impl SledContactsIter {
    #[cfg(feature = "encryption")]
    fn decrypt_value<T: DeserializeOwned>(&self, value: &[u8]) -> Result<T, SledStoreError> {
        if let Some(cipher) = self.cipher.as_ref() {
            Ok(cipher.decrypt_value(value)?)
        } else {
            Ok(serde_json::from_slice(value)?)
        }
    }

    #[cfg(not(feature = "encryption"))]
    fn decrypt_value<T: DeserializeOwned>(&self, value: &[u8]) -> Result<T, SledStoreError> {
        Ok(serde_json::from_slice(value)?)
    }
}

impl Iterator for SledContactsIter {
    type Item = Result<Contact, SledStoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter
            .next()?
            .map_err(SledStoreError::from)
            .and_then(|(_key, value)| self.decrypt_value(&value))
            .into()
    }
}

pub struct SledGroupsIter {
    #[cfg(feature = "encryption")]
    cipher: Option<Arc<presage_store_cipher::StoreCipher>>,
    iter: sled::Iter,
}

impl SledGroupsIter {
    #[cfg(feature = "encryption")]
    fn decrypt_value<T: DeserializeOwned>(&self, value: &[u8]) -> Result<T, SledStoreError> {
        if let Some(cipher) = self.cipher.as_ref() {
            Ok(cipher.decrypt_value(value)?)
        } else {
            Ok(serde_json::from_slice(value)?)
        }
    }

    #[cfg(not(feature = "encryption"))]
    fn decrypt_value<T: DeserializeOwned>(&self, value: &[u8]) -> Result<T, SledStoreError> {
        Ok(serde_json::from_slice(value)?)
    }
}

impl Iterator for SledGroupsIter {
    type Item = Result<(GroupMasterKeyBytes, Group), SledStoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.iter.next()?.map_err(SledStoreError::from).and_then(
            |(group_master_key_bytes, value)| {
                let group = self.decrypt_value(&value)?;
                Ok((
                    group_master_key_bytes
                        .as_ref()
                        .try_into()
                        .map_err(|_| SledStoreError::GroupDecryption)?,
                    group,
                ))
            },
        ))
    }
}

pub struct SledStickerPacksIter {
    #[cfg(feature = "encryption")]
    cipher: Option<Arc<presage_store_cipher::StoreCipher>>,
    iter: sled::Iter,
}

impl Iterator for SledStickerPacksIter {
    type Item = Result<StickerPack, SledStoreError>;

    #[cfg(feature = "encryption")]
    fn next(&mut self) -> Option<Self::Item> {
        self.iter
            .next()?
            .map_err(SledStoreError::from)
            .and_then(|(_key, value)| {
                if let Some(cipher) = self.cipher.as_ref() {
                    cipher.decrypt_value(&value).map_err(SledStoreError::from)
                } else {
                    serde_json::from_slice(&value).map_err(SledStoreError::from)
                }
            })
            .into()
    }

    #[cfg(not(feature = "encryption"))]
    fn next(&mut self) -> Option<Self::Item> {
        self.iter
            .next()?
            .map_err(SledStoreError::from)
            .and_then(|(_key, value)| serde_json::from_slice(&value).map_err(SledStoreError::from))
            .into()
    }
}

pub struct SledMessagesIter {
    #[cfg(feature = "encryption")]
    cipher: Option<Arc<presage_store_cipher::StoreCipher>>,
    iter: sled::Iter,
}

impl SledMessagesIter {
    #[cfg(feature = "encryption")]
    fn decrypt_value<T: DeserializeOwned>(&self, value: &[u8]) -> Result<T, SledStoreError> {
        if let Some(cipher) = self.cipher.as_ref() {
            Ok(cipher.decrypt_value(value)?)
        } else {
            Ok(serde_json::from_slice(value)?)
        }
    }

    #[cfg(not(feature = "encryption"))]
    fn decrypt_value<T: DeserializeOwned>(&self, value: &[u8]) -> Result<T, SledStoreError> {
        Ok(serde_json::from_slice(value)?)
    }
}

impl SledMessagesIter {
    fn decode(
        &self,
        elem: Result<(IVec, IVec), sled::Error>,
    ) -> Option<Result<Content, SledStoreError>> {
        elem.map_err(SledStoreError::from)
            .and_then(|(_, value)| self.decrypt_value(&value).map_err(SledStoreError::from))
            .and_then(|data: Vec<u8>| ContentProto::decode(&data[..]).map_err(SledStoreError::from))
            .map_or_else(|e| Some(Err(e)), |p| Some(p.try_into()))
    }
}

impl Iterator for SledMessagesIter {
    type Item = Result<Content, SledStoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        let elem = self.iter.next()?;
        self.decode(elem)
    }
}

impl DoubleEndedIterator for SledMessagesIter {
    fn next_back(&mut self) -> Option<Self::Item> {
        let elem = self.iter.next_back()?;
        self.decode(elem)
    }
}

#[cfg(test)]
mod tests {
    use core::fmt;

    use base64::prelude::*;
    use presage::libsignal_service::{
        content::{ContentBody, Metadata},
        prelude::Uuid,
        proto::DataMessage,
        protocol::{
            self, Direction, GenericSignedPreKey, IdentityKeyStore, PreKeyRecord, PreKeyStore,
            SessionRecord, SessionStore, SignedPreKeyRecord, SignedPreKeyStore,
        },
        ServiceAddress,
    };
    use presage::store::ContentsStore;
    use quickcheck::{Arbitrary, Gen};

    use super::SledStore;

    #[derive(Debug, Clone)]
    struct ProtocolAddress(protocol::ProtocolAddress);

    #[derive(Clone)]
    struct KeyPair(protocol::KeyPair);

    #[derive(Debug, Clone)]
    struct Thread(presage::store::Thread);

    #[derive(Debug, Clone)]
    struct Content(presage::libsignal_service::content::Content);

    impl fmt::Debug for KeyPair {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            writeln!(
                f,
                "{}",
                BASE64_STANDARD.encode(self.0.public_key.serialize())
            )
        }
    }

    impl Arbitrary for ProtocolAddress {
        fn arbitrary(g: &mut Gen) -> Self {
            let name: String = Arbitrary::arbitrary(g);
            let device_id: u32 = Arbitrary::arbitrary(g);
            ProtocolAddress(protocol::ProtocolAddress::new(name, device_id.into()))
        }
    }

    impl Arbitrary for KeyPair {
        fn arbitrary(_g: &mut Gen) -> Self {
            // Gen is not rand::CryptoRng here, see https://github.com/BurntSushi/quickcheck/issues/241
            KeyPair(protocol::KeyPair::generate(&mut rand::thread_rng()))
        }
    }

    impl Arbitrary for Content {
        fn arbitrary(g: &mut Gen) -> Self {
            let timestamp: u64 = Arbitrary::arbitrary(g);
            let contacts = [
                Uuid::from_u128(Arbitrary::arbitrary(g)),
                Uuid::from_u128(Arbitrary::arbitrary(g)),
                Uuid::from_u128(Arbitrary::arbitrary(g)),
            ];
            let metadata = Metadata {
                sender: ServiceAddress {
                    uuid: *g.choose(&contacts).unwrap(),
                },
                sender_device: Arbitrary::arbitrary(g),
                server_guid: None,
                timestamp,
                needs_receipt: Arbitrary::arbitrary(g),
                unidentified_sender: Arbitrary::arbitrary(g),
            };
            let content_body = ContentBody::DataMessage(DataMessage {
                body: Arbitrary::arbitrary(g),
                timestamp: Some(timestamp),
                ..Default::default()
            });
            Self(presage::libsignal_service::content::Content::from_body(
                content_body,
                metadata,
            ))
        }
    }

    impl Arbitrary for Thread {
        fn arbitrary(g: &mut Gen) -> Self {
            Self(presage::store::Thread::Contact(Uuid::from_u128(
                Arbitrary::arbitrary(g),
            )))
        }
    }

    #[quickcheck_async::tokio]
    async fn test_save_get_trust_identity(addr: ProtocolAddress, key_pair: KeyPair) -> bool {
        let mut db = SledStore::temporary().unwrap();
        let identity_key = protocol::IdentityKey::new(key_pair.0.public_key);
        db.save_identity(&addr.0, &identity_key).await.unwrap();
        let id = db.get_identity(&addr.0).await.unwrap().unwrap();
        if id != identity_key {
            return false;
        }
        db.is_trusted_identity(&addr.0, &id, Direction::Receiving)
            .await
            .unwrap()
    }

    #[quickcheck_async::tokio]
    async fn test_store_load_session(addr: ProtocolAddress) -> bool {
        let session = SessionRecord::new_fresh();

        let mut db = SledStore::temporary().unwrap();
        db.store_session(&addr.0, &session).await.unwrap();
        if db.load_session(&addr.0).await.unwrap().is_none() {
            return false;
        }
        let loaded_session = db.load_session(&addr.0).await.unwrap().unwrap();
        session.serialize().unwrap() == loaded_session.serialize().unwrap()
    }

    #[quickcheck_async::tokio]
    async fn test_prekey_store(id: u32, key_pair: KeyPair) -> bool {
        let id = id.into();
        let mut db = SledStore::temporary().unwrap();
        let pre_key_record = PreKeyRecord::new(id, &key_pair.0);
        db.save_pre_key(id, &pre_key_record).await.unwrap();
        if db.get_pre_key(id).await.unwrap().serialize().unwrap()
            != pre_key_record.serialize().unwrap()
        {
            return false;
        }

        db.remove_pre_key(id).await.unwrap();
        db.get_pre_key(id).await.is_err()
    }

    #[quickcheck_async::tokio]
    async fn test_signed_prekey_store(
        id: u32,
        timestamp: u64,
        key_pair: KeyPair,
        signature: Vec<u8>,
    ) -> bool {
        let mut db = SledStore::temporary().unwrap();
        let id = id.into();
        let signed_pre_key_record = SignedPreKeyRecord::new(id, timestamp, &key_pair.0, &signature);
        db.save_signed_pre_key(id, &signed_pre_key_record)
            .await
            .unwrap();

        db.get_signed_pre_key(id)
            .await
            .unwrap()
            .serialize()
            .unwrap()
            == signed_pre_key_record.serialize().unwrap()
    }

    fn content_with_timestamp(
        content: &Content,
        ts: u64,
    ) -> presage::libsignal_service::content::Content {
        presage::libsignal_service::content::Content {
            metadata: Metadata {
                timestamp: ts,
                ..content.0.metadata.clone()
            },
            body: content.0.body.clone(),
        }
    }

    #[quickcheck_async::tokio]
    async fn test_store_messages(thread: Thread, content: Content) -> anyhow::Result<()> {
        let db = SledStore::temporary()?;
        let thread = thread.0;
        db.save_message(&thread, content_with_timestamp(&content, 1678295210))?;
        db.save_message(&thread, content_with_timestamp(&content, 1678295220))?;
        db.save_message(&thread, content_with_timestamp(&content, 1678295230))?;
        db.save_message(&thread, content_with_timestamp(&content, 1678295240))?;
        db.save_message(&thread, content_with_timestamp(&content, 1678280000))?;

        assert_eq!(db.messages(&thread, ..).unwrap().count(), 5);
        assert_eq!(db.messages(&thread, 0..).unwrap().count(), 5);
        assert_eq!(db.messages(&thread, 1678280000..).unwrap().count(), 5);

        assert_eq!(db.messages(&thread, 0..1678280000)?.count(), 0);
        assert_eq!(db.messages(&thread, 0..1678295210)?.count(), 1);
        assert_eq!(db.messages(&thread, 1678295210..1678295240)?.count(), 3);
        assert_eq!(db.messages(&thread, 1678295210..=1678295240)?.count(), 4);

        assert_eq!(
            db.messages(&thread, 0..=1678295240)?
                .next()
                .unwrap()?
                .metadata
                .timestamp,
            1678280000
        );
        assert_eq!(
            db.messages(&thread, 0..=1678295240)?
                .next_back()
                .unwrap()?
                .metadata
                .timestamp,
            1678295240
        );

        Ok(())
    }
}
