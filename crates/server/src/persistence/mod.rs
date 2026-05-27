pub mod fs_map_entities;
pub mod fs_map_meta;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bevy::prelude::*;
use nostr_map_persistence::{
    ManifestHash, MapPersistenceRejection, MapRevision, PayloadClass, PayloadSlotState,
};
use persistence::{PersistenceError, Store, StoreBackend};
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
#[derive(Serialize, Deserialize, Debug, Clone)]
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
            enabled: false,
            fallback_timeout: Duration::from_secs(5),
        }
    }
}

/// Test/local-harness remote restore source using production-shaped validated saves.
#[derive(Resource, Clone, Debug, Default)]
pub struct FakeRemoteMapRestores(pub HashMap<MapInstanceId, ServerValidatedMapSave>);

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
    FsAcceptedMapHeadStore {
        map_dir: Arc::new(staging_dir.clone()),
    }
    .save(&(), &save.revision)?;
    FsLocalMapHeadStore {
        map_dir: Arc::new(staging_dir.clone()),
    }
    .save(&(), &local_head_from_remote_save(save))?;
    atomically_promote_staged_revision(save_dir, &staging_dir, &save.revision)?;
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
        validate_revision_directory_identity(&final_dir, revision)?;
        if staging_dir.exists() {
            fs::remove_dir_all(staging_dir).map_err(|e| {
                PersistenceError::Serialize(format!("remove duplicate staging dir: {e}"))
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

fn validate_revision_directory_identity(
    final_dir: &Path,
    revision: &MapRevision,
) -> Result<(), PersistenceError> {
    let accepted = FsAcceptedMapHeadStore {
        map_dir: Arc::new(final_dir.to_path_buf()),
    }
    .load(&())?
    .ok_or_else(|| {
        PersistenceError::Deserialize("existing revision missing accepted head".into())
    })?;
    if accepted != *revision {
        return Err(PersistenceError::Deserialize(
            "existing revision directory has different accepted head".into(),
        ));
    }
    Ok(())
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
