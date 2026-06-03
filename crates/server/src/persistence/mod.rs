pub mod fs_map_entities;
pub mod fs_map_meta;

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bevy::prelude::*;
use nostr_map_persistence::{
    ManifestHash, ManifestPayloadDescriptor, MapPersistenceRejection, MapRevision, PayloadClass,
    PayloadKey, PayloadSlotState, RawChunkEntitiesPayload, RawChunkPayload, RawMapEntitiesPayload,
    RawMapMetaPayload, RawValidatedMapDelta, RawValidatedMapSave,
};
use persistence::{PersistenceError, SaveOpId, Store, StoreBackend};
use protocol::map::SavedEntity;
use protocol::MapInstanceId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use voxel_map_engine::config::WorldObjectSpawn;
use voxel_map_engine::persistence::fs_chunk::FsChunkStore;
use voxel_map_engine::persistence::fs_chunk_entities::FsChunkEntitiesStore;
use voxel_map_engine::persistence::{chunk_file_path, entity_file_path, ChunkFileEnvelope};

pub(crate) const META_VERSION: u32 = 1;

/// Metadata for a single map instance, saved to `map.meta.bin`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct MapMeta {
    pub version: u32,
    pub seed: u64,
    pub generation_version: u32,
    pub spawn_points: Vec<Vec3>,
}

/// Server-decoded complete map save accepted by persistence preflight.
#[derive(Clone, Debug)]
pub struct ServerValidatedMapSave {
    pub meta: MapMeta,
    pub chunks: Vec<(IVec3, ChunkFileEnvelope)>,
    pub chunk_entities: Vec<(IVec3, Vec<WorldObjectSpawn>)>,
    pub map_entities: Option<Vec<SavedEntity>>,
    pub revision: MapRevision,
}

/// Server-decoded map delta accepted by persistence preflight.
#[derive(Clone, Debug)]
pub struct ServerValidatedMapDelta {
    pub revision: MapRevision,
    pub meta: PayloadSlotState<MapMeta>,
    pub chunks: Vec<(IVec3, PayloadSlotState<ChunkFileEnvelope>)>,
    pub chunk_entities: Vec<(IVec3, PayloadSlotState<Vec<WorldObjectSpawn>>)>,
    pub map_entities: PayloadSlotState<Vec<SavedEntity>>,
}

impl TryFrom<RawValidatedMapSave> for ServerValidatedMapSave {
    type Error = MapPersistenceRejection;

    fn try_from(raw: RawValidatedMapSave) -> Result<Self, Self::Error> {
        let meta = decode_map_meta_payload(raw.meta)?;
        let chunks = raw
            .chunks
            .into_iter()
            .map(|(key, payload)| Ok((chunk_key_to_ivec3(key)?, decode_chunk_envelope(payload)?)))
            .collect::<Result<Vec<_>, MapPersistenceRejection>>()?;
        let chunk_entities = raw
            .chunk_entities
            .into_iter()
            .map(|(key, payload)| {
                Ok((
                    chunk_key_to_ivec3(key)?,
                    decode_chunk_entities_payload(payload)?,
                ))
            })
            .collect::<Result<Vec<_>, MapPersistenceRejection>>()?;
        let map_entities = raw
            .map_entities
            .map(decode_map_entities_payload)
            .transpose()?;
        Ok(Self {
            meta,
            chunks,
            chunk_entities,
            map_entities,
            revision: raw.revision,
        })
    }
}

impl TryFrom<RawValidatedMapDelta> for ServerValidatedMapDelta {
    type Error = MapPersistenceRejection;

    fn try_from(raw: RawValidatedMapDelta) -> Result<Self, Self::Error> {
        Ok(Self {
            revision: raw.revision,
            meta: try_map_slot(raw.meta, decode_map_meta_payload)?,
            chunks: try_map_keyed_slots(raw.chunks, decode_chunk_envelope)?,
            chunk_entities: try_map_keyed_slots(raw.chunk_entities, decode_chunk_entities_payload)?,
            map_entities: try_map_slot(raw.map_entities, decode_map_entities_payload)?,
        })
    }
}

fn try_map_slot<T, U>(
    slot: PayloadSlotState<T>,
    decode: impl FnOnce(T) -> Result<U, MapPersistenceRejection>,
) -> Result<PayloadSlotState<U>, MapPersistenceRejection> {
    match slot {
        PayloadSlotState::Present(value) => Ok(PayloadSlotState::Present(decode(value)?)),
        PayloadSlotState::Empty => Ok(PayloadSlotState::Empty),
        PayloadSlotState::Absent => Ok(PayloadSlotState::Absent),
        PayloadSlotState::Tombstoned => Ok(PayloadSlotState::Tombstoned),
    }
}

fn try_map_keyed_slots<T, U>(
    slots: Vec<(PayloadKey, PayloadSlotState<T>)>,
    decode_value: impl Fn(T) -> Result<U, MapPersistenceRejection>,
) -> Result<Vec<(IVec3, PayloadSlotState<U>)>, MapPersistenceRejection> {
    slots
        .into_iter()
        .map(|(key, slot)| Ok((chunk_key_to_ivec3(key)?, try_map_slot(slot, &decode_value)?)))
        .collect()
}

fn chunk_key_to_ivec3(key: PayloadKey) -> Result<IVec3, MapPersistenceRejection> {
    match key {
        PayloadKey::Chunk { x, y, z } => Ok(IVec3::new(x, y, z)),
        PayloadKey::Singleton => Err(MapPersistenceRejection::Invalid(
            "expected chunk payload key, got singleton".to_string(),
        )),
    }
}

fn decode_map_meta_payload(payload: RawMapMetaPayload) -> Result<MapMeta, MapPersistenceRejection> {
    bincode::deserialize(&payload.bytes)
        .map_err(|error| MapPersistenceRejection::Invalid(format!("decode map meta: {error}")))
}

fn decode_chunk_envelope(
    payload: RawChunkPayload,
) -> Result<ChunkFileEnvelope, MapPersistenceRejection> {
    let envelope = zstd_bincode_decode::<ChunkFileEnvelope>(&payload.bytes, "chunk payload")?;
    if envelope.version != voxel_map_engine::persistence::CHUNK_SAVE_VERSION {
        return Err(MapPersistenceRejection::Invalid(format!(
            "chunk payload version mismatch: expected {}, got {}",
            voxel_map_engine::persistence::CHUNK_SAVE_VERSION,
            envelope.version
        )));
    }
    Ok(envelope)
}

fn decode_chunk_entities_payload(
    payload: RawChunkEntitiesPayload,
) -> Result<Vec<WorldObjectSpawn>, MapPersistenceRejection> {
    if payload.bytes.is_empty() {
        return Ok(Vec::new());
    }
    #[derive(Deserialize)]
    struct ChunkEntityPayloadEnvelope {
        version: u32,
        spawns: Vec<WorldObjectSpawn>,
    }
    const CHUNK_ENTITY_SAVE_VERSION: u32 = 3;
    let envelope = zstd_bincode_decode::<ChunkEntityPayloadEnvelope>(
        &payload.bytes,
        "chunk entities payload",
    )?;
    if envelope.version != CHUNK_ENTITY_SAVE_VERSION {
        return Err(MapPersistenceRejection::Invalid(format!(
            "chunk entities payload version mismatch: expected {CHUNK_ENTITY_SAVE_VERSION}, got {}",
            envelope.version
        )));
    }
    Ok(envelope.spawns)
}

fn decode_map_entities_payload(
    payload: RawMapEntitiesPayload,
) -> Result<Vec<SavedEntity>, MapPersistenceRejection> {
    if payload.bytes.is_empty() {
        return Ok(Vec::new());
    }
    let envelope: EntityFileEnvelope = bincode::deserialize(&payload.bytes).map_err(|error| {
        MapPersistenceRejection::Invalid(format!("decode map entities: {error}"))
    })?;
    if envelope.version != ENTITY_SAVE_VERSION {
        return Err(MapPersistenceRejection::Invalid(format!(
            "map entities payload version mismatch: expected {ENTITY_SAVE_VERSION}, got {}",
            envelope.version
        )));
    }
    Ok(envelope.entities)
}

fn zstd_bincode_decode<T: for<'de> Deserialize<'de>>(
    bytes: &[u8],
    label: &str,
) -> Result<T, MapPersistenceRejection> {
    let mut decoder = zstd::Decoder::new(bytes)
        .map_err(|error| MapPersistenceRejection::Invalid(format!("zstd {label}: {error}")))?;
    let mut decoded = Vec::new();
    decoder
        .read_to_end(&mut decoded)
        .map_err(|error| MapPersistenceRejection::Invalid(format!("read {label}: {error}")))?;
    bincode::deserialize(&decoded)
        .map_err(|error| MapPersistenceRejection::Invalid(format!("decode {label}: {error}")))
}

/// Resource holding the base save directory path.
#[derive(Resource)]
pub struct WorldSavePath(pub PathBuf);

impl Default for WorldSavePath {
    fn default() -> Self {
        Self(PathBuf::from("worlds"))
    }
}

/// Resolve the save directory for a `MapInstanceId` within the base save path.
pub fn map_save_dir(base: &Path, map_id: &MapInstanceId) -> PathBuf {
    match map_id {
        MapInstanceId::Overworld => base.join("overworld"),
        MapInstanceId::Homebase { owner } => base.join(format!(
            "homebase_{}",
            nostr_client::npub_from_nostr_public_key(*owner)
        )),
    }
}

/// File containing the active materialized revision directory name.
pub const ACTIVE_REVISION_FILE: &str = "active_revision";
/// Directory containing promoted materialized revisions.
pub const REVISIONS_DIR: &str = "revisions";
/// Directory containing incomplete materialization work.
pub const STAGING_DIR: &str = "staging";

/// Runtime toggle for remote map persistence lookup.
#[derive(Resource, Clone, Debug)]
pub struct RemoteMapPersistenceConfig {
    pub enabled: bool,
    pub fallback_timeout: Duration,
}

impl Default for RemoteMapPersistenceConfig {
    fn default() -> Self {
        Self {
            enabled: env_flag("SERVER_MAP_REMOTE_READ") || env_flag("SERVER_MAP_REMOTE_PUBLISH"),
            fallback_timeout: Duration::from_secs(5),
        }
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.as_str(),
                "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
            )
        })
        .unwrap_or(false)
}

/// Test/local-harness remote restore source using production-shaped validated saves.
#[derive(Resource, Clone, Debug, Default)]
pub struct FakeRemoteMapRestores(pub HashMap<MapInstanceId, ServerValidatedMapSave>);

/// Optional real Nostr/Blossom read context used by server preflight.
#[derive(Resource, Clone)]
pub struct RemoteMapReadContext {
    pub event_client: nostr_client::events::NostrEventClient,
    pub query_policy: nostr_map_persistence::NostrMapQueryPolicy,
    pub persistence_policy: nostr_map_persistence::MapPersistencePolicy,
}

/// Local filesystem head that may be newer than the accepted remote manifest.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalMapHead {
    pub local_revision_number: u64,
    pub active_content_hash: [u8; 32],
    pub accepted_remote_manifest_hash: Option<ManifestHash>,
}

/// Filesystem store for the accepted remote/materialized map head.
#[derive(Clone, Debug)]
pub struct FsAcceptedMapHeadStore {
    pub map_dir: Arc<PathBuf>,
}

impl Store<(), MapRevision> for FsAcceptedMapHeadStore {
    fn load(&self, _key: &()) -> Result<Option<MapRevision>, PersistenceError> {
        let path = self.path();
        if !path.exists() {
            trace!(?path, "accepted map head file is absent");
            return Ok(None);
        }
        let bytes = fs::read(&path)
            .map_err(|e| PersistenceError::Deserialize(format!("read accepted head: {e}")))?;
        let revision = bincode::deserialize(&bytes).map_err(|e| {
            PersistenceError::Deserialize(format!("deserialize accepted head: {e}"))
        })?;
        Ok(Some(revision))
    }

    fn save(&self, _key: &(), revision: &MapRevision) -> Result<(), PersistenceError> {
        let path = self.path();
        fs::create_dir_all(path.parent().expect("accepted head path has parent"))
            .map_err(|e| PersistenceError::Serialize(format!("mkdir accepted head parent: {e}")))?;
        let bytes = bincode::serialize(revision)
            .map_err(|e| PersistenceError::Serialize(format!("serialize accepted head: {e}")))?;
        let tmp_path = path.with_extension("bin.tmp");
        fs::write(&tmp_path, bytes)
            .map_err(|e| PersistenceError::Serialize(format!("write accepted head tmp: {e}")))?;
        fs::rename(&tmp_path, &path)
            .map_err(|e| PersistenceError::Serialize(format!("rename accepted head: {e}")))?;
        Ok(())
    }
}

impl FsAcceptedMapHeadStore {
    pub fn path(&self) -> PathBuf {
        self.map_dir.join("accepted_head.bin")
    }
}

/// Filesystem store for local unpublished map head state.
#[derive(Clone, Debug)]
pub struct FsLocalMapHeadStore {
    pub map_dir: Arc<PathBuf>,
}

impl Store<(), LocalMapHead> for FsLocalMapHeadStore {
    fn load(&self, _key: &()) -> Result<Option<LocalMapHead>, PersistenceError> {
        let path = self.path();
        if !path.exists() {
            trace!(?path, "local map head file is absent");
            return Ok(None);
        }
        let bytes = fs::read(&path)
            .map_err(|e| PersistenceError::Deserialize(format!("read local head: {e}")))?;
        let head = bincode::deserialize(&bytes)
            .map_err(|e| PersistenceError::Deserialize(format!("deserialize local head: {e}")))?;
        Ok(Some(head))
    }

    fn save(&self, _key: &(), head: &LocalMapHead) -> Result<(), PersistenceError> {
        let path = self.path();
        fs::create_dir_all(path.parent().expect("local head path has parent"))
            .map_err(|e| PersistenceError::Serialize(format!("mkdir local head parent: {e}")))?;
        let bytes = bincode::serialize(head)
            .map_err(|e| PersistenceError::Serialize(format!("serialize local head: {e}")))?;
        let tmp_path = path.with_extension("bin.tmp");
        fs::write(&tmp_path, bytes)
            .map_err(|e| PersistenceError::Serialize(format!("write local head tmp: {e}")))?;
        fs::rename(&tmp_path, &path)
            .map_err(|e| PersistenceError::Serialize(format!("rename local head: {e}")))?;
        Ok(())
    }
}

impl FsLocalMapHeadStore {
    pub fn path(&self) -> PathBuf {
        self.map_dir.join("local_head.bin")
    }
}

/// Durable set of chunk keys edited since the accepted-head revision, plus meta/entity change
/// flags. This is the publish candidate set; it survives restart so prior-session edits still
/// publish. Cleared (set-difference) once a revision carrying those keys is durably accepted.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MapChangeSet {
    pub chunk_candidates: std::collections::HashSet<IVec3>,
    /// Chunk positions whose per-chunk world objects changed since the accepted head.
    pub chunk_entity_candidates: std::collections::HashSet<IVec3>,
    pub meta_changed: bool,
    pub map_entities_changed: bool,
}

/// Filesystem store for the durable map change-set. Rooted at the map's top-level dir alongside
/// `accepted_head.bin`/`local_head.bin` so it spans revisions.
#[derive(Clone, Debug)]
pub struct FsMapChangeSetStore {
    pub map_dir: Arc<PathBuf>,
}

impl Store<(), MapChangeSet> for FsMapChangeSetStore {
    fn load(&self, _key: &()) -> Result<Option<MapChangeSet>, PersistenceError> {
        let path = self.path();
        if !path.exists() {
            trace!(?path, "map change-set file is absent");
            return Ok(None);
        }
        let bytes = fs::read(&path)
            .map_err(|e| PersistenceError::Deserialize(format!("read change set: {e}")))?;
        let change_set = bincode::deserialize(&bytes)
            .map_err(|e| PersistenceError::Deserialize(format!("deserialize change set: {e}")))?;
        Ok(Some(change_set))
    }

    fn save(&self, _key: &(), value: &MapChangeSet) -> Result<(), PersistenceError> {
        let path = self.path();
        fs::create_dir_all(path.parent().expect("change set path has parent"))
            .map_err(|e| PersistenceError::Serialize(format!("mkdir change set parent: {e}")))?;
        let bytes = bincode::serialize(value)
            .map_err(|e| PersistenceError::Serialize(format!("serialize change set: {e}")))?;
        let tmp_path = path.with_extension("bin.tmp");
        fs::write(&tmp_path, bytes)
            .map_err(|e| PersistenceError::Serialize(format!("write change set tmp: {e}")))?;
        fs::rename(&tmp_path, &path)
            .map_err(|e| PersistenceError::Serialize(format!("rename change set: {e}")))?;
        Ok(())
    }
}

impl FsMapChangeSetStore {
    pub fn path(&self) -> PathBuf {
        self.map_dir.join("change_set.bin")
    }
}

/// Remote manifest publish lifecycle state persisted per map.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum RemotePublishStatus {
    Pending,
    InFlight,
    Published,
    Failed,
}

/// One deterministic manifest publish attempt in the per-map remote journal.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemotePublishJournalEntry {
    pub map_id: MapInstanceId,
    pub local_revision: MapRevision,
    pub previous_remote_manifest_hash: Option<ManifestHash>,
    pub new_manifest_hash: ManifestHash,
    pub payloads: Vec<ManifestPayloadDescriptor>,
    pub advances_local_head: LocalMapHead,
    pub signed_event_json: Option<String>,
    pub status: RemotePublishStatus,
    pub retry_count: u32,
}

/// Persisted per-map remote publish journal.
#[derive(Component, Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RemotePublishJournal {
    pub entries: Vec<RemotePublishJournalEntry>,
}

/// Filesystem store for a map's remote publish journal.
#[derive(Clone, Debug)]
pub struct FsRemotePublishJournalStore {
    pub save_root: PathBuf,
}

impl Store<MapInstanceId, RemotePublishJournal> for FsRemotePublishJournalStore {
    fn load(
        &self,
        map_id: &MapInstanceId,
    ) -> Result<Option<RemotePublishJournal>, PersistenceError> {
        let path = map_save_dir(&self.save_root, map_id).join("remote_publish_journal.bin");
        if !path.exists() {
            trace!(?map_id, "remote publish journal file is absent");
            return Ok(None);
        }
        let bytes = fs::read(&path).map_err(|error| {
            PersistenceError::Deserialize(format!(
                "read remote publish journal {}: {error}",
                path.display()
            ))
        })?;
        let journal = bincode::deserialize(&bytes).map_err(|error| {
            PersistenceError::Deserialize(format!(
                "deserialize remote publish journal {}: {error}",
                path.display()
            ))
        })?;
        Ok(Some(journal))
    }

    fn save(
        &self,
        map_id: &MapInstanceId,
        journal: &RemotePublishJournal,
    ) -> Result<(), PersistenceError> {
        let map_dir = map_save_dir(&self.save_root, map_id);
        fs::create_dir_all(&map_dir).map_err(|error| {
            PersistenceError::Serialize(format!(
                "mkdir remote publish journal dir {}: {error}",
                map_dir.display()
            ))
        })?;
        atomic_save_bincode(&map_dir.join("remote_publish_journal.bin"), journal)
    }
}

/// Server-owned local map publish draft awaiting remote journal preparation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerMapPublishDraft {
    pub local_revision_number: u64,
    pub meta: PayloadSlotState<MapMeta>,
    pub chunks: Vec<(IVec3, PayloadSlotState<ChunkFileEnvelope>)>,
    pub chunk_entities: Vec<(IVec3, PayloadSlotState<Vec<WorldObjectSpawn>>)>,
    pub map_entities: PayloadSlotState<Vec<SavedEntity>>,
}

/// Durable unpublished draft keyed by the filesystem save completion id.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalUnpublishedPublishDraft {
    pub map_id: MapInstanceId,
    pub draft: ServerMapPublishDraft,
    pub save_id: SaveOpId,
}

/// Filesystem store for unpublished publish drafts.
#[derive(Clone, Debug)]
pub struct FsLocalUnpublishedPublishDraftStore {
    pub map_dir: Arc<PathBuf>,
}

impl Store<SaveOpId, LocalUnpublishedPublishDraft> for FsLocalUnpublishedPublishDraftStore {
    fn load(
        &self,
        save_id: &SaveOpId,
    ) -> Result<Option<LocalUnpublishedPublishDraft>, PersistenceError> {
        let path = self.draft_path(*save_id);
        load_optional_bincode(&path)
    }

    fn save(
        &self,
        save_id: &SaveOpId,
        draft: &LocalUnpublishedPublishDraft,
    ) -> Result<(), PersistenceError> {
        atomic_save_bincode(&self.draft_path(*save_id), draft)
    }
}

impl FsLocalUnpublishedPublishDraftStore {
    pub fn draft_path(&self, save_id: SaveOpId) -> PathBuf {
        self.map_dir
            .join("unpublished")
            .join(format!("{}.bin", save_id.0))
    }

    pub fn load_all(&self) -> Result<Vec<LocalUnpublishedPublishDraft>, PersistenceError> {
        let dir = self.map_dir.join("unpublished");
        if !dir.exists() {
            trace!(?dir, "unpublished publish draft directory is absent");
            return Ok(Vec::new());
        }
        let mut files = fs::read_dir(&dir)
            .map_err(|error| {
                PersistenceError::Deserialize(format!(
                    "read unpublished dir {}: {error}",
                    dir.display()
                ))
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                PersistenceError::Deserialize(format!(
                    "iterate unpublished dir {}: {error}",
                    dir.display()
                ))
            })?;
        files.sort_by_key(|entry| entry.file_name());
        files
            .into_iter()
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("bin"))
            .map(|entry| {
                load_optional_bincode(&entry.path())?.ok_or_else(|| {
                    PersistenceError::Deserialize(format!(
                        "unpublished draft disappeared while loading {}",
                        entry.path().display()
                    ))
                })
            })
            .collect()
    }
}

fn load_optional_bincode<T: for<'de> Deserialize<'de>>(
    path: &Path,
) -> Result<Option<T>, PersistenceError> {
    if !path.exists() {
        trace!(?path, "optional bincode file is absent");
        return Ok(None);
    }
    let bytes = fs::read(path).map_err(|error| {
        PersistenceError::Deserialize(format!("read bincode file {}: {error}", path.display()))
    })?;
    bincode::deserialize(&bytes).map(Some).map_err(|error| {
        PersistenceError::Deserialize(format!(
            "deserialize bincode file {}: {error}",
            path.display()
        ))
    })
}

fn atomic_save_bincode<T: Serialize>(path: &Path, value: &T) -> Result<(), PersistenceError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            PersistenceError::Serialize(format!(
                "mkdir bincode parent {}: {error}",
                parent.display()
            ))
        })?;
    }
    let bytes = bincode::serialize(value)
        .map_err(|error| PersistenceError::Serialize(format!("serialize bincode: {error}")))?;
    let tmp_path = path.with_extension("bin.tmp");
    fs::write(&tmp_path, bytes).map_err(|error| {
        PersistenceError::Serialize(format!("write bincode tmp {}: {error}", tmp_path.display()))
    })?;
    fs::rename(&tmp_path, path).map_err(|error| {
        PersistenceError::Serialize(format!(
            "rename bincode tmp {} -> {}: {error}",
            tmp_path.display(),
            path.display()
        ))
    })
}

/// Formats a manifest hash as lowercase hexadecimal.
pub fn manifest_hash_hex(hash: ManifestHash) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Builds the deterministic directory name for a materialized revision.
pub fn revision_dir_name(revision: &MapRevision) -> String {
    format!(
        "rev-{:016}-{}",
        revision.revision,
        manifest_hash_hex(revision.manifest_hash)
    )
}

/// Returns the path to the active revision pointer file for a map directory.
pub fn active_pointer_path(map_save_dir: &Path) -> PathBuf {
    map_save_dir.join(ACTIVE_REVISION_FILE)
}

/// Resolves the active materialized revision directory, if one has been promoted.
pub fn resolve_active_map_dir(map_save_dir: &Path) -> Result<Option<PathBuf>, PersistenceError> {
    let pointer = active_pointer_path(map_save_dir);
    if !pointer.exists() {
        return Ok(None);
    }
    let name = fs::read_to_string(&pointer)
        .map_err(|e| PersistenceError::Deserialize(format!("read active revision pointer: {e}")))?;
    let active = map_save_dir.join(REVISIONS_DIR).join(name.trim());
    if !active.is_dir() {
        return Err(PersistenceError::Deserialize(format!(
            "active revision pointer references missing directory {}",
            active.display()
        )));
    }
    Ok(Some(active))
}

/// Returns the directory that filesystem stores should read from for normal loading.
pub fn store_map_dir_for_loading(map_save_dir: &Path) -> Result<PathBuf, PersistenceError> {
    resolve_active_map_dir(map_save_dir)
        .map(|active| active.unwrap_or_else(|| map_save_dir.to_path_buf()))
}

/// Computes a deterministic legacy revision hash from the current filesystem save files.
pub fn bootstrap_filesystem_revision(
    save_dir: &Path,
    map_id: &MapInstanceId,
) -> Result<MapRevision, PersistenceError> {
    fn collect_single(
        files: &mut Vec<String>,
        save_dir: &Path,
        relative: &str,
    ) -> Result<(), PersistenceError> {
        let path = save_dir.join(relative);
        if !path.exists() {
            trace!(?path, "legacy persistence file is absent during bootstrap");
            return Ok(());
        }
        if !path.is_file() {
            return Err(PersistenceError::Deserialize(format!(
                "legacy persistence path is not a file: {}",
                path.display()
            )));
        }
        files.push(relative.to_string());
        Ok(())
    }

    fn collect_dir(
        files: &mut Vec<String>,
        save_dir: &Path,
        dir: &str,
        suffix: &str,
    ) -> Result<(), PersistenceError> {
        let path = save_dir.join(dir);
        if !path.exists() {
            trace!(
                ?path,
                "legacy persistence directory is absent during bootstrap"
            );
            return Ok(());
        }
        if !path.is_dir() {
            return Err(PersistenceError::Deserialize(format!(
                "legacy persistence path is not a directory: {}",
                path.display()
            )));
        }
        for entry in fs::read_dir(&path)
            .map_err(|e| PersistenceError::Deserialize(format!("read legacy {dir} dir: {e}")))?
        {
            let entry = entry.map_err(|e| {
                PersistenceError::Deserialize(format!("read legacy {dir} entry: {e}"))
            })?;
            let file_type = entry.file_type().map_err(|e| {
                PersistenceError::Deserialize(format!("stat legacy {dir} entry: {e}"))
            })?;
            if !file_type.is_file() {
                trace!(?entry, "skipping non-file legacy persistence entry");
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(suffix) {
                files.push(format!("{dir}/{name}"));
            }
        }
        Ok(())
    }

    fn hash_len_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }

    let mut files = Vec::new();
    collect_single(&mut files, save_dir, "map.meta.bin")?;
    collect_single(&mut files, save_dir, "entities.bin")?;
    collect_dir(&mut files, save_dir, "terrain", ".bin")?;
    collect_dir(&mut files, save_dir, "entities", ".entities.bin")?;
    files.sort();

    let mut hasher = Sha256::new();
    hasher.update(b"bevy-lightyear-template:legacy-map-revision:v1");
    let map_id_bytes = bincode::serialize(map_id).map_err(|e| {
        PersistenceError::Serialize(format!("serialize map id for legacy hash: {e}"))
    })?;
    hash_len_prefixed(&mut hasher, &map_id_bytes);
    for relative in files {
        hash_len_prefixed(&mut hasher, relative.as_bytes());
        let bytes = fs::read(save_dir.join(&relative)).map_err(|e| {
            PersistenceError::Deserialize(format!("read legacy persistence file {relative}: {e}"))
        })?;
        hash_len_prefixed(&mut hasher, &bytes);
    }

    Ok(MapRevision {
        revision: 0,
        previous_hash: None,
        manifest_hash: hasher.finalize().into(),
    })
}

/// Removes incomplete materialization staging data and temporary active-pointer files.
pub fn cleanup_materialization_staging(save_dir: &Path) -> Result<(), PersistenceError> {
    let staging_dir = save_dir.join(STAGING_DIR);
    if staging_dir.exists() {
        if !staging_dir.is_dir() {
            return Err(PersistenceError::Deserialize(format!(
                "staging path is not a directory: {}",
                staging_dir.display()
            )));
        }
        for entry in fs::read_dir(&staging_dir)
            .map_err(|e| PersistenceError::Deserialize(format!("read staging dir: {e}")))?
        {
            let entry = entry
                .map_err(|e| PersistenceError::Deserialize(format!("read staging entry: {e}")))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|e| PersistenceError::Deserialize(format!("stat staging entry: {e}")))?;
            if file_type.is_dir() {
                fs::remove_dir_all(&path).map_err(|e| {
                    PersistenceError::Serialize(format!(
                        "remove staging dir {}: {e}",
                        path.display()
                    ))
                })?;
            } else {
                fs::remove_file(&path).map_err(|e| {
                    PersistenceError::Serialize(format!(
                        "remove staging file {}: {e}",
                        path.display()
                    ))
                })?;
            }
        }
    } else {
        trace!(
            ?staging_dir,
            "no materialization staging directory to clean up"
        );
    }

    let pointer_tmp = active_pointer_path(save_dir).with_extension("tmp");
    if pointer_tmp.exists() {
        fs::remove_file(&pointer_tmp)
            .map_err(|e| PersistenceError::Serialize(format!("remove active pointer tmp: {e}")))?;
    } else {
        trace!(?pointer_tmp, "no active pointer temp file to clean up");
    }
    Ok(())
}

/// Writes a validated remote save into staging, validates it, and promotes it atomically.
pub fn materialize_validated_map_save(
    save_dir: &Path,
    save: &ServerValidatedMapSave,
) -> Result<(), PersistenceError> {
    cleanup_materialization_staging(save_dir)?;
    let staging_dir = create_revision_staging_dir(save_dir, &save.revision)?;
    write_full_revision_to_staging(&staging_dir, save)?;
    validate_staged_revision(&staging_dir, save)?;
    atomically_promote_staged_revision(save_dir, &staging_dir, &save.revision)?;
    // Heads live at the map's top-level dir (not inside the revision snapshot). Write them after
    // promoting content, accepted-head last as the commit marker; a crash between promote and the
    // head write only makes the idempotent restore re-run.
    let head_dir = Arc::new(save_dir.to_path_buf());
    FsLocalMapHeadStore {
        map_dir: head_dir.clone(),
    }
    .save(&(), &local_head_from_remote_save(save))?;
    FsAcceptedMapHeadStore { map_dir: head_dir }.save(&(), &save.revision)?;
    Ok(())
}

/// Promotes a staged revision directory and atomically updates the active revision pointer.
pub fn atomically_promote_staged_revision(
    map_save_dir: &Path,
    staging_dir: &Path,
    revision: &MapRevision,
) -> Result<PathBuf, PersistenceError> {
    let final_dir = map_save_dir
        .join(REVISIONS_DIR)
        .join(revision_dir_name(revision));
    fs::create_dir_all(final_dir.parent().expect("revision dir has parent"))
        .map_err(|e| PersistenceError::Serialize(format!("mkdir revisions: {e}")))?;
    if final_dir.exists() {
        if existing_revision_dir_is_complete(&final_dir) {
            // The content-addressed revision is already materialized; drop the duplicate staging.
            if staging_dir.exists() {
                fs::remove_dir_all(staging_dir).map_err(|e| {
                    PersistenceError::Serialize(format!("remove duplicate staging dir: {e}"))
                })?;
            }
        } else {
            // An incomplete dir at this path (a pre-migration remnant or external tampering)
            // cannot have come from an atomic promotion. Replace it with the freshly validated
            // staging, which is content-identical by hash.
            warn!(
                ?final_dir,
                "existing revision directory is incomplete; re-promoting from validated staging"
            );
            fs::remove_dir_all(&final_dir).map_err(|e| {
                PersistenceError::Serialize(format!("remove incomplete revision dir: {e}"))
            })?;
            fs::rename(staging_dir, &final_dir).map_err(|e| {
                PersistenceError::Serialize(format!("re-promote staged revision: {e}"))
            })?;
        }
    } else {
        fs::rename(staging_dir, &final_dir)
            .map_err(|e| PersistenceError::Serialize(format!("promote staged revision: {e}")))?;
    }

    let pointer_tmp = active_pointer_path(map_save_dir).with_extension("tmp");
    fs::write(&pointer_tmp, revision_dir_name(revision))
        .map_err(|e| PersistenceError::Serialize(format!("write active pointer tmp: {e}")))?;
    fs::rename(&pointer_tmp, active_pointer_path(map_save_dir))
        .map_err(|e| PersistenceError::Serialize(format!("promote active pointer: {e}")))?;
    Ok(final_dir)
}

/// Validates that staged filesystem files can be read and match the accepted save.
pub fn validate_staged_revision(
    staging_dir: &Path,
    expected: &ServerValidatedMapSave,
) -> Result<(), PersistenceError> {
    let meta = fs_map_meta::FsMapMetaStore {
        map_dir: Arc::new(staging_dir.to_path_buf()),
    }
    .load(&())?
    .ok_or_else(|| {
        PersistenceError::Deserialize("materialized revision missing map metadata".into())
    })?;
    if meta.seed != expected.meta.seed
        || meta.generation_version != expected.meta.generation_version
    {
        return Err(PersistenceError::Deserialize(
            "staged meta does not match validated save".into(),
        ));
    }

    let chunk_store = FsChunkStore {
        map_dir: Arc::new(staging_dir.to_path_buf()),
    };
    for (chunk_pos, expected_chunk) in &expected.chunks {
        let actual = chunk_store.load(chunk_pos)?.ok_or_else(|| {
            PersistenceError::Deserialize(format!(
                "materialized revision missing terrain chunk {chunk_pos}"
            ))
        })?;
        if actual.version != expected_chunk.version {
            return Err(PersistenceError::VersionMismatch {
                expected: expected_chunk.version,
                actual: actual.version,
            });
        }
    }
    Ok(())
}

/// Reinstalls map filesystem store backends to point at the active revision directory.
pub fn install_active_revision_store_backends(
    commands: &mut Commands,
    map_entity: Entity,
    map_save_dir: &Path,
) -> Result<(), PersistenceError> {
    let active_dir = store_map_dir_for_loading(map_save_dir)?;
    let map_dir = Arc::new(active_dir);
    commands.entity(map_entity).insert((
        StoreBackend::new(fs_map_meta::FsMapMetaStore {
            map_dir: map_dir.clone(),
        }),
        StoreBackend::new(fs_map_entities::FsMapEntitiesStore {
            map_dir: map_dir.clone(),
        }),
        StoreBackend::new(FsChunkStore {
            map_dir: map_dir.clone(),
        }),
        StoreBackend::new(FsChunkEntitiesStore { map_dir }),
    ));
    Ok(())
}

/// Base state used when replaying a remote delta chain.
pub enum SaveBase {
    Empty,
    Snapshot(ServerValidatedMapSave),
}

/// Replays validated deltas over a base snapshot to produce a complete map save.
pub fn assemble_validated_map_save(
    base: SaveBase,
    chain: Vec<ServerValidatedMapDelta>,
) -> Result<ServerValidatedMapSave, MapPersistenceRejection> {
    fn upsert_slot<K: PartialEq, V>(slots: &mut Vec<(K, V)>, key: K, value: V) {
        if let Some(index) = slots.iter().position(|(existing, _)| existing == &key) {
            slots[index].1 = value;
        } else {
            slots.push((key, value));
        }
    }

    fn remove_slot<K: PartialEq, V>(slots: &mut Vec<(K, V)>, key: &K) {
        slots.retain(|(existing, _)| existing != key);
    }

    fn apply_required_slot<T>(
        current: &mut Option<T>,
        slot: PayloadSlotState<T>,
        class: PayloadClass,
    ) -> Result<(), MapPersistenceRejection> {
        match slot {
            PayloadSlotState::Present(value) => *current = Some(value),
            PayloadSlotState::Absent => {
                trace!(?class, "delta slot absent; preserving previous value")
            }
            PayloadSlotState::Tombstoned => *current = None,
            PayloadSlotState::Empty => {
                return Err(MapPersistenceRejection::Invalid(format!(
                    "empty slot is invalid for required payload class {class:?}"
                )));
            }
        }
        Ok(())
    }

    fn apply_entity_slot<T: Default>(
        current: &mut Option<T>,
        slot: PayloadSlotState<T>,
        class: PayloadClass,
    ) {
        match slot {
            PayloadSlotState::Present(value) => *current = Some(value),
            PayloadSlotState::Empty => *current = Some(T::default()),
            PayloadSlotState::Absent => trace!(
                ?class,
                "entity delta slot absent; preserving previous value"
            ),
            PayloadSlotState::Tombstoned => *current = None,
        }
    }

    fn apply_keyed_required_slot<K: PartialEq + std::fmt::Debug, T>(
        slots: &mut Vec<(K, T)>,
        key: K,
        slot: PayloadSlotState<T>,
        class: PayloadClass,
    ) -> Result<(), MapPersistenceRejection> {
        match slot {
            PayloadSlotState::Present(value) => upsert_slot(slots, key, value),
            PayloadSlotState::Absent => trace!(
                ?class,
                ?key,
                "keyed delta slot absent; preserving previous value"
            ),
            PayloadSlotState::Tombstoned => remove_slot(slots, &key),
            PayloadSlotState::Empty => {
                return Err(MapPersistenceRejection::Invalid(format!(
                    "empty slot is invalid for required payload class {class:?} at {key:?}"
                )));
            }
        }
        Ok(())
    }

    fn apply_keyed_entity_slot<K: PartialEq + std::fmt::Debug, T: Default>(
        slots: &mut Vec<(K, T)>,
        key: K,
        slot: PayloadSlotState<T>,
        class: PayloadClass,
    ) {
        match slot {
            PayloadSlotState::Present(value) => upsert_slot(slots, key, value),
            PayloadSlotState::Empty => upsert_slot(slots, key, T::default()),
            PayloadSlotState::Absent => trace!(
                ?class,
                ?key,
                "keyed entity delta slot absent; preserving previous value"
            ),
            PayloadSlotState::Tombstoned => remove_slot(slots, &key),
        }
    }

    let (
        mut meta,
        mut chunks,
        mut chunk_entities,
        mut map_entities,
        mut revision,
        mut expected_previous_hash,
    ) = match base {
        SaveBase::Empty => (None, Vec::new(), Vec::new(), None, None, None),
        SaveBase::Snapshot(snapshot) => (
            Some(snapshot.meta),
            snapshot.chunks,
            snapshot.chunk_entities,
            snapshot.map_entities,
            Some(snapshot.revision.clone()),
            Some(snapshot.revision.manifest_hash),
        ),
    };

    for delta in chain {
        if delta.revision.previous_hash != expected_previous_hash {
            return Err(MapPersistenceRejection::Divergent(format!(
                "delta revision {} previous hash {:?} does not match expected {:?}",
                delta.revision.revision, delta.revision.previous_hash, expected_previous_hash
            )));
        }
        apply_required_slot(&mut meta, delta.meta, PayloadClass::MapMeta)?;
        for (chunk_pos, slot) in delta.chunks {
            apply_keyed_required_slot(&mut chunks, chunk_pos, slot, PayloadClass::TerrainChunk)?;
        }
        for (chunk_pos, slot) in delta.chunk_entities {
            apply_keyed_entity_slot(
                &mut chunk_entities,
                chunk_pos,
                slot,
                PayloadClass::ChunkEntities,
            );
        }
        apply_entity_slot(
            &mut map_entities,
            delta.map_entities,
            PayloadClass::MapEntities,
        );
        expected_previous_hash = Some(delta.revision.manifest_hash);
        revision = Some(delta.revision);
    }

    let revision = revision.ok_or_else(|| {
        MapPersistenceRejection::Incomplete(
            "cannot assemble map save without a base or delta revision".into(),
        )
    })?;
    let meta = meta.ok_or_else(|| {
        MapPersistenceRejection::Incomplete("assembled map save is missing metadata".into())
    })?;
    Ok(ServerValidatedMapSave {
        meta,
        chunks,
        chunk_entities,
        map_entities,
        revision,
    })
}

fn create_revision_staging_dir(
    save_dir: &Path,
    revision: &MapRevision,
) -> Result<PathBuf, PersistenceError> {
    let staging_root = save_dir.join(STAGING_DIR);
    fs::create_dir_all(&staging_root)
        .map_err(|e| PersistenceError::Serialize(format!("mkdir staging root: {e}")))?;
    let staging_dir = staging_root.join(format!(
        "{}.staging-{}",
        revision_dir_name(revision),
        std::process::id()
    ));
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir).map_err(|e| {
            PersistenceError::Serialize(format!("remove previous staging dir: {e}"))
        })?;
    }
    fs::create_dir_all(&staging_dir)
        .map_err(|e| PersistenceError::Serialize(format!("mkdir staging dir: {e}")))?;
    Ok(staging_dir)
}

fn write_full_revision_to_staging(
    staging_dir: &Path,
    save: &ServerValidatedMapSave,
) -> Result<(), PersistenceError> {
    let map_dir = Arc::new(staging_dir.to_path_buf());
    fs_map_meta::FsMapMetaStore {
        map_dir: map_dir.clone(),
    }
    .save(&(), &save.meta)?;
    if let Some(map_entities) = &save.map_entities {
        fs_map_entities::FsMapEntitiesStore {
            map_dir: map_dir.clone(),
        }
        .save(&(), map_entities)?;
    }
    let chunk_store = FsChunkStore {
        map_dir: map_dir.clone(),
    };
    for (chunk_pos, chunk) in &save.chunks {
        chunk_store.save(chunk_pos, chunk)?;
    }
    let chunk_entity_store = FsChunkEntitiesStore { map_dir };
    for (chunk_pos, spawns) in &save.chunk_entities {
        chunk_entity_store.save(chunk_pos, spawns)?;
    }
    Ok(())
}

/// Returns whether an existing revision directory is a complete materialized snapshot.
///
/// Revision directories are content-addressed by their name (`rev-{n}-{manifest_hash}`) and
/// are only ever created by atomically renaming a fully written and validated staging
/// directory, so a complete snapshot always has readable map metadata. (Heads are mutable
/// pointers and live at the map's top-level dir, not inside the immutable snapshot.) A dir
/// missing this marker is a pre-migration remnant or external tampering and is re-promoted.
fn existing_revision_dir_is_complete(final_dir: &Path) -> bool {
    final_dir.join("map.meta.bin").is_file()
}

fn local_head_from_remote_save(save: &ServerValidatedMapSave) -> LocalMapHead {
    LocalMapHead {
        local_revision_number: save.revision.revision,
        active_content_hash: save.revision.manifest_hash,
        accepted_remote_manifest_hash: Some(save.revision.manifest_hash),
    }
}

/// Deletes a materialized terrain chunk from a staging directory for tombstone application.
pub fn delete_materialized_chunk(
    staging_dir: &Path,
    chunk_pos: IVec3,
) -> Result<(), PersistenceError> {
    let path = chunk_file_path(staging_dir, chunk_pos);
    if path.exists() {
        fs::remove_file(&path)
            .map_err(|e| PersistenceError::Serialize(format!("remove chunk tombstone: {e}")))?;
    }
    Ok(())
}

/// Deletes materialized chunk-entity data from a staging directory for tombstone application.
pub fn delete_materialized_chunk_entities(
    staging_dir: &Path,
    chunk_pos: IVec3,
) -> Result<(), PersistenceError> {
    let path = entity_file_path(staging_dir, chunk_pos);
    if path.exists() {
        fs::remove_file(&path).map_err(|e| {
            PersistenceError::Serialize(format!("remove chunk entities tombstone: {e}"))
        })?;
    }
    Ok(())
}

pub(crate) const ENTITY_SAVE_VERSION: u32 = 1;

/// Versioned envelope wrapping entity data for on-disk persistence.
#[derive(Serialize, Deserialize)]
pub(crate) struct EntityFileEnvelope {
    pub version: u32,
    pub entities: Vec<SavedEntity>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use persistence::Store;
    use protocol::map::SavedEntityKind;
    use protocol::NostrPublicKey;
    use std::sync::Arc;

    use fs_map_entities::FsMapEntitiesStore;
    use fs_map_meta::FsMapMetaStore;

    fn test_meta_store(dir: &Path) -> FsMapMetaStore {
        FsMapMetaStore {
            map_dir: Arc::new(dir.to_path_buf()),
        }
    }

    fn test_entity_store(dir: &Path) -> FsMapEntitiesStore {
        FsMapEntitiesStore {
            map_dir: Arc::new(dir.to_path_buf()),
        }
    }

    fn owner(byte: u8) -> NostrPublicKey {
        NostrPublicKey([byte; 32])
    }

    #[test]
    fn save_load_map_meta_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_meta_store(dir.path());
        let meta = MapMeta {
            version: 1,
            seed: 42,
            generation_version: 3,
            spawn_points: vec![Vec3::new(1.0, 2.0, 3.0), Vec3::new(4.0, 5.0, 6.0)],
        };
        store.save(&(), &meta).unwrap();
        let loaded = store.load(&()).unwrap().expect("meta should exist");
        assert_eq!(loaded.seed, 42);
        assert_eq!(loaded.generation_version, 3);
        assert_eq!(loaded.spawn_points.len(), 2);
    }

    #[test]
    fn load_map_meta_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_meta_store(dir.path());
        assert!(store.load(&()).unwrap().is_none());
    }

    #[test]
    fn map_save_dir_overworld() {
        let base = Path::new("worlds");
        assert_eq!(
            map_save_dir(base, &MapInstanceId::Overworld),
            PathBuf::from("worlds/overworld")
        );
    }

    #[test]
    fn map_save_dir_homebase() {
        let base = Path::new("worlds");
        assert_eq!(
            map_save_dir(base, &MapInstanceId::Homebase { owner: owner(0x2a) }),
            PathBuf::from(
                "worlds/homebase_npub19g4z52329g4z52329g4z52329g4z52329g4z52329g4z52329g4qrd5mkx"
            )
        );
    }

    #[test]
    fn save_load_entities_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_entity_store(dir.path());
        let entities = vec![
            SavedEntity {
                kind: SavedEntityKind::RespawnPoint,
                position: Vec3::new(1.0, 2.0, 3.0),
            },
            SavedEntity {
                kind: SavedEntityKind::RespawnPoint,
                position: Vec3::new(4.0, 5.0, 6.0),
            },
        ];
        store.save(&(), &entities).unwrap();
        let loaded = store.load(&()).unwrap().expect("entities should exist");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].kind, SavedEntityKind::RespawnPoint);
        assert_eq!(loaded[0].position, Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(loaded[1].position, Vec3::new(4.0, 5.0, 6.0));
    }

    #[test]
    fn load_entities_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_entity_store(dir.path());
        assert!(store.load(&()).unwrap().is_none());
    }

    #[test]
    fn save_entities_creates_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("deep/nested");
        let store = test_entity_store(&nested);
        store.save(&(), &vec![]).unwrap();
        assert!(nested.join("entities.bin").exists());
    }

    #[test]
    fn save_entities_overwrites_previous() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_entity_store(dir.path());
        let v1 = vec![SavedEntity {
            kind: SavedEntityKind::RespawnPoint,
            position: Vec3::ZERO,
        }];
        store.save(&(), &v1).unwrap();

        let v2 = vec![
            SavedEntity {
                kind: SavedEntityKind::RespawnPoint,
                position: Vec3::ONE,
            },
            SavedEntity {
                kind: SavedEntityKind::RespawnPoint,
                position: Vec3::NEG_ONE,
            },
        ];
        store.save(&(), &v2).unwrap();

        let loaded = store.load(&()).unwrap().expect("entities should exist");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].position, Vec3::ONE);
    }

    #[test]
    fn corrupt_entities_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("entities.bin"), b"garbage data").unwrap();
        let store = test_entity_store(dir.path());
        assert!(store.load(&()).is_err());
    }

    #[test]
    fn entity_kind_serialization_roundtrip() {
        let kind = SavedEntityKind::RespawnPoint;
        let bytes = bincode::serialize(&kind).unwrap();
        let back: SavedEntityKind = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, SavedEntityKind::RespawnPoint);
    }
}
