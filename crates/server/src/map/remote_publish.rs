use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::prelude::*;
use bevy::tasks::Task;
use nostr_client::BlobRef;
use nostr_map_persistence::{
    build_signed_map_manifest_event, encode_chunk_entities_payload, encode_chunk_payload,
    encode_map_entities_payload, encode_map_meta_payload, finalize_manifest, upload_publish_slot,
    ManifestHash, ManifestPayloadDescriptor, ManifestPayloadSlot, MapManifestSigner,
    MapPersistenceRejection, MapRevision, NostrManifestPublishStore, PayloadClass, PayloadKey,
};
use persistence::{
    AsyncStore, AsyncStoreBackend, PendingAsyncStoreOps, PersistenceError, SaveOpId, Store,
    StoreBackend,
};
use protocol::{MapInstanceId, NostrPublicKey};
use sha2::{Digest, Sha256};
use voxel_map_engine::prelude::{ChunkSaveCompleted, ChunkSaveFailed};

use crate::persistence::{
    FsAcceptedMapHeadStore, FsLocalMapHeadStore, FsLocalUnpublishedPublishDraftStore,
    FsMapChangeSetStore, FsRemotePublishJournalStore, LocalMapHead, LocalUnpublishedPublishDraft,
    MapChangeSet, MapMeta, RemotePublishJournal, RemotePublishStatus, ServerMapPublishDraft,
};

/// Identifies which persisted map payload completed a filesystem save.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MapPayloadSaveKey {
    MapMeta,
    MapEntities,
    TerrainChunk(IVec3),
    ChunkEntities(IVec3),
}

/// Normalized successful payload save completion consumed by remote publishing.
#[derive(Message, Clone, Debug)]
pub struct MapPayloadSaveCompleted {
    pub map_entity: Entity,
    pub save_id: SaveOpId,
    pub key: MapPayloadSaveKey,
}

/// Normalized failed payload save completion consumed by remote publishing.
#[derive(Message, Clone, Debug)]
pub struct MapPayloadSaveFailed {
    pub map_entity: Entity,
    pub save_id: SaveOpId,
    pub key: MapPayloadSaveKey,
    pub error: String,
}

/// Server-owned unpublished publish drafts waiting for remote journal conversion.
#[derive(Component, Default)]
pub struct PendingRemotePublishDeltas {
    pub queue: VecDeque<ServerMapPublishDraft>,
    pub blocked_revision: Option<u64>,
}

impl PendingRemotePublishDeltas {
    /// Returns whether remote publish preparation is blocked after a failed draft.
    pub fn is_prepare_blocked(&self) -> bool {
        self.blocked_revision.is_some()
    }

    /// Pops the next draft only when no earlier prepare failure blocks the queue.
    pub fn pop_front_for_prepare(&mut self) -> Option<ServerMapPublishDraft> {
        if self.is_prepare_blocked() {
            None
        } else {
            self.queue.pop_front()
        }
    }

    /// Restores a failed draft to the front and blocks later preparation attempts.
    pub fn block_after_prepare_failure(&mut self, draft: ServerMapPublishDraft) -> u64 {
        let blocked_revision = draft.local_revision_number;
        self.blocked_revision = Some(blocked_revision);
        self.queue.push_front(draft);
        blocked_revision
    }
}

/// Computes the next durable local revision number for an Overworld publish draft.
pub fn next_publish_revision_number(
    local_head: Option<&LocalMapHead>,
    journal: &RemotePublishJournal,
    pending_deltas: &PendingRemotePublishDeltas,
    pending_publish: &PendingPublishBySaveId,
) -> u64 {
    let local_head_revision = local_head
        .map(|head| head.local_revision_number)
        .unwrap_or(0);
    let journal_revision = journal
        .entries
        .iter()
        .map(|entry| entry.advances_local_head.local_revision_number)
        .max()
        .unwrap_or(0);
    let queued_revision = pending_deltas
        .queue
        .iter()
        .map(|draft| draft.local_revision_number)
        .chain(
            pending_publish
                .0
                .values()
                .map(|draft| draft.local_revision_number),
        )
        .max()
        .unwrap_or(0);

    local_head_revision
        .max(journal_revision)
        .max(queued_revision)
        + 1
}

/// In-memory correlation between filesystem save ids and publish drafts.
#[derive(Resource, Default)]
pub struct PendingPublishBySaveId(pub HashMap<SaveOpId, ServerMapPublishDraft>);

/// Runtime configuration for server-owned remote Overworld publishing.
#[derive(Resource, Clone, Debug)]
pub struct RemoteMapPublishConfig {
    pub enabled: bool,
    pub blossom_upload_url: Option<String>,
    pub blossom_public_base_url: Option<url::Url>,
    pub fail_first_manifest_publish: bool,
}

impl Default for RemoteMapPublishConfig {
    fn default() -> Self {
        let enabled = env_flag("SERVER_MAP_REMOTE_PUBLISH");
        let fail_first_manifest_publish = env_flag("SERVER_MAP_REMOTE_PUBLISH_FAIL_FIRST");
        if !enabled {
            return Self {
                enabled,
                blossom_upload_url: None,
                blossom_public_base_url: None,
                fail_first_manifest_publish,
            };
        }

        let blossom_upload_url = std::env::var("SERVER_BLOSSOM_UPLOAD_URL")
            .expect("SERVER_MAP_REMOTE_PUBLISH=1 requires SERVER_BLOSSOM_UPLOAD_URL");
        let blossom_public_base_url = std::env::var("SERVER_BLOSSOM_PUBLIC_BASE_URL")
            .expect("SERVER_MAP_REMOTE_PUBLISH=1 requires SERVER_BLOSSOM_PUBLIC_BASE_URL");
        let blossom_public_base_url = url::Url::parse(&blossom_public_base_url)
            .expect("SERVER_BLOSSOM_PUBLIC_BASE_URL must be a valid URL");

        Self {
            enabled,
            blossom_upload_url: Some(blossom_upload_url),
            blossom_public_base_url: Some(blossom_public_base_url),
            fail_first_manifest_publish,
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

/// Returns whether server-owned remote map publishing is enabled.
pub fn remote_map_publish_enabled(config: Res<RemoteMapPublishConfig>) -> bool {
    config.enabled
}

/// Tracks maps with an in-flight remote manifest publish.
#[derive(Resource, Default)]
pub struct RemoteMapPublishWorker {
    pub in_flight_by_map: HashSet<MapInstanceId>,
}

/// Pending async journal-entry preparation tasks for a map.
#[derive(Component, Default)]
pub struct PendingRemotePublishEntryTasks {
    tasks: Vec<Task<RemotePublishPrepareResult>>,
}

/// Manifest publish store with optional one-shot failure injection for manual verification.
#[derive(Clone)]
pub struct ServerManifestPublishStore {
    inner: NostrManifestPublishStore,
    fail_next: Arc<AtomicBool>,
}

impl ServerManifestPublishStore {
    pub fn new(client: nostr_client::events::NostrEventClient, fail_first_publish: bool) -> Self {
        Self {
            inner: NostrManifestPublishStore { client },
            fail_next: Arc::new(AtomicBool::new(fail_first_publish)),
        }
    }
}

impl AsyncStore<ManifestHash, String> for ServerManifestPublishStore {
    fn load<'a>(
        &'a self,
        key: &'a ManifestHash,
    ) -> persistence::BoxedStoreFuture<'a, Result<Option<String>, PersistenceError>> {
        self.inner.load(key)
    }

    fn save<'a>(
        &'a self,
        key: &'a ManifestHash,
        value: &'a String,
    ) -> persistence::BoxedStoreFuture<'a, Result<(), PersistenceError>> {
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Box::pin(async {
                Err(PersistenceError::Serialize(
                    "forced first remote manifest publish failure".to_string(),
                ))
            });
        }
        self.inner.save(key, value)
    }
}

struct RemotePublishPrepareFailure {
    draft: ServerMapPublishDraft,
    error: MapPersistenceRejection,
}

type RemotePublishPrepareResult =
    Result<crate::persistence::RemotePublishJournalEntry, RemotePublishPrepareFailure>;

/// Local adapter implementing map manifest signing for the configured server identity.
struct ServerManifestSigner<'a>(&'a nostr_client::NostrKeys);

impl MapManifestSigner for ServerManifestSigner<'_> {
    fn public_key(&self) -> NostrPublicKey {
        self.0.protocol_public_key()
    }

    fn sign_map_manifest_event(
        &self,
        draft: nostr_client::events::NostrEventDraft,
    ) -> Result<String, nostr_map_persistence::RemotePersistenceError> {
        self.0
            .sign_event(&draft)
            .map_err(nostr_map_persistence::RemotePersistenceError::from)
    }
}

/// Builds the local head that should advance after a durable unpublished draft exists.
pub fn local_head_from_unpublished_draft(persisted: &LocalUnpublishedPublishDraft) -> LocalMapHead {
    let mut hasher = Sha256::new();
    hasher.update(b"untitled-brawler/server-unpublished-draft/v1");
    hasher.update(
        bincode::serialize(&persisted.draft.local_revision_number)
            .expect("publish draft revision must serialize"),
    );
    hasher.update(bincode::serialize(&persisted.map_id).expect("map id must serialize"));
    LocalMapHead {
        local_revision_number: persisted.draft.local_revision_number,
        active_content_hash: hasher.finalize().into(),
        accepted_remote_manifest_hash: None,
    }
}

/// Encodes server `MapMeta` through the shared payload encoder.
///
/// Adapts the bevy-typed `MapMeta` to the bevy-free primitive signature
/// `nostr_map_persistence` exposes so client and server produce identical bytes.
fn encode_server_map_meta(value: MapMeta) -> Result<Vec<u8>, MapPersistenceRejection> {
    let spawn_points = value
        .spawn_points
        .iter()
        .map(|point| [point.x, point.y, point.z])
        .collect();
    encode_map_meta_payload(
        value.version,
        value.seed,
        value.generation_version,
        spawn_points,
    )
}

/// Converts a server Overworld draft into a signed pending remote journal entry.
pub async fn prepare_server_map_publish_entry(
    identity: &nostr_client::NostrKeys,
    draft: ServerMapPublishDraft,
    previous_remote_manifest_hash: Option<ManifestHash>,
    blob_store: &impl AsyncStore<BlobRef, Vec<u8>>,
    public_blossom_base_url: &url::Url,
) -> Result<crate::persistence::RemotePublishJournalEntry, MapPersistenceRejection> {
    let map_id = MapInstanceId::Overworld;
    let signer = ServerManifestSigner(identity);
    let owner = signer.public_key();
    let mut payloads = Vec::new();

    upload_publish_slot(
        &mut payloads,
        blob_store,
        public_blossom_base_url,
        PayloadClass::MapMeta,
        PayloadKey::Singleton,
        1,
        draft.meta.clone(),
        encode_server_map_meta,
    )
    .await?;
    for (chunk_pos, slot) in draft.chunks.clone() {
        upload_publish_slot(
            &mut payloads,
            blob_store,
            public_blossom_base_url,
            PayloadClass::TerrainChunk,
            PayloadKey::Chunk {
                x: chunk_pos.x,
                y: chunk_pos.y,
                z: chunk_pos.z,
            },
            voxel_map_engine::persistence::CHUNK_SAVE_VERSION,
            slot,
            encode_chunk_payload,
        )
        .await?;
    }
    for (chunk_pos, slot) in draft.chunk_entities.clone() {
        upload_publish_slot(
            &mut payloads,
            blob_store,
            public_blossom_base_url,
            PayloadClass::ChunkEntities,
            PayloadKey::Chunk {
                x: chunk_pos.x,
                y: chunk_pos.y,
                z: chunk_pos.z,
            },
            3,
            slot,
            encode_chunk_entities_payload,
        )
        .await?;
    }
    upload_publish_slot(
        &mut payloads,
        blob_store,
        public_blossom_base_url,
        PayloadClass::MapEntities,
        PayloadKey::Singleton,
        1,
        draft.map_entities.clone(),
        encode_map_entities_payload,
    )
    .await?;

    let manifest = finalize_manifest(
        payloads,
        map_id.clone(),
        owner,
        draft.local_revision_number,
        previous_remote_manifest_hash,
        None,
    )?;
    let descriptor_root = manifest.descriptor_root;
    let payloads = manifest.payloads.clone();
    let (new_manifest_hash, signed_event_json) = build_signed_map_manifest_event(&signer, manifest)
        .map_err(|error| MapPersistenceRejection::Invalid(error.to_string()))?;
    let local_revision = MapRevision {
        revision: draft.local_revision_number,
        previous_hash: previous_remote_manifest_hash,
        manifest_hash: new_manifest_hash,
    };

    Ok(crate::persistence::RemotePublishJournalEntry {
        map_id,
        local_revision,
        previous_remote_manifest_hash,
        new_manifest_hash,
        payloads,
        advances_local_head: LocalMapHead {
            local_revision_number: draft.local_revision_number,
            active_content_hash: descriptor_root,
            accepted_remote_manifest_hash: Some(new_manifest_hash),
        },
        signed_event_json: Some(signed_event_json),
        status: RemotePublishStatus::Pending,
        retry_count: 0,
    })
}

/// Removes the keys a successfully-published journal entry carried from the durable change-set, so
/// each candidate publishes once. Candidates re-edited after the draft was built are re-added by
/// the next debounced save, so they are not lost.
fn clear_published_change_set_keys(
    change_set: &mut MapChangeSet,
    payloads: &[ManifestPayloadDescriptor],
) {
    for descriptor in payloads {
        match (&descriptor.class, &descriptor.key) {
            (PayloadClass::TerrainChunk, PayloadKey::Chunk { x, y, z }) => {
                change_set.chunk_candidates.remove(&IVec3::new(*x, *y, *z));
            }
            (PayloadClass::ChunkEntities, PayloadKey::Chunk { x, y, z }) => {
                change_set
                    .chunk_entity_candidates
                    .remove(&IVec3::new(*x, *y, *z));
            }
            (PayloadClass::MapEntities, PayloadKey::Singleton)
                if !matches!(descriptor.slot, ManifestPayloadSlot::Absent) =>
            {
                change_set.map_entities_changed = false;
            }
            (PayloadClass::MapMeta, PayloadKey::Singleton)
                if !matches!(descriptor.slot, ManifestPayloadSlot::Absent) =>
            {
                change_set.meta_changed = false;
            }
            _ => {}
        }
    }
}

/// Applies completed or failed manifest publish operations to durable heads and journal state.
pub fn apply_publish_results(
    map_id: &MapInstanceId,
    journal: &mut RemotePublishJournal,
    worker: &mut RemoteMapPublishWorker,
    publish_ops: &mut PendingAsyncStoreOps<ManifestHash, String>,
    accepted_head_store: &FsAcceptedMapHeadStore,
    local_head_store: &FsLocalMapHeadStore,
    journal_store: &FsRemotePublishJournalStore,
    change_set_store: &FsMapChangeSetStore,
) -> Result<(), PersistenceError> {
    while let Some(manifest_hash) = publish_ops.completed_saves.first().copied() {
        let entry_index = journal
            .entries
            .iter()
            .position(|entry| {
                entry.new_manifest_hash == manifest_hash
                    && entry.status == RemotePublishStatus::InFlight
            })
            .expect("completed manifest publish must match an in-flight journal entry");
        accepted_head_store.save(&(), &journal.entries[entry_index].local_revision)?;
        local_head_store.save(&(), &journal.entries[entry_index].advances_local_head)?;
        journal.entries[entry_index].status = RemotePublishStatus::Published;
        if let Err(error) = journal_store.save(map_id, journal) {
            journal.entries[entry_index].status = RemotePublishStatus::InFlight;
            return Err(error);
        }
        info!(
            ?map_id,
            ?manifest_hash,
            local_revision_number = journal.entries[entry_index]
                .advances_local_head
                .local_revision_number,
            "remote manifest publish succeeded; advanced map heads"
        );
        // Heads are committed above; clear the published candidates so they don't republish. A
        // failure here only causes a harmless redundant republish, so log rather than unwind.
        match change_set_store.load(&()) {
            Ok(loaded) => {
                let mut change_set = loaded.unwrap_or_default();
                clear_published_change_set_keys(
                    &mut change_set,
                    &journal.entries[entry_index].payloads,
                );
                if let Err(error) = change_set_store.save(&(), &change_set) {
                    error!(?map_id, ?error, "failed to clear change-set after publish");
                }
            }
            Err(error) => error!(?map_id, ?error, "failed to load change-set after publish"),
        }
        publish_ops.completed_saves.remove(0);
        worker.in_flight_by_map.remove(map_id);
    }

    while !publish_ops.save_errors.is_empty() {
        let manifest_hash = publish_ops.save_errors[0].0;
        let entry_index = journal
            .entries
            .iter()
            .position(|entry| {
                entry.new_manifest_hash == manifest_hash
                    && entry.status == RemotePublishStatus::InFlight
            })
            .expect("failed manifest publish must match an in-flight journal entry");
        let old_retry_count = journal.entries[entry_index].retry_count;
        journal.entries[entry_index].status = RemotePublishStatus::Failed;
        journal.entries[entry_index].retry_count =
            journal.entries[entry_index].retry_count.saturating_add(1);
        if let Err(save_error) = journal_store.save(map_id, journal) {
            journal.entries[entry_index].status = RemotePublishStatus::InFlight;
            journal.entries[entry_index].retry_count = old_retry_count;
            return Err(save_error);
        }
        let (_, error) = publish_ops.save_errors.remove(0);
        trace!(
            ?map_id,
            ?manifest_hash,
            %error,
            "remote manifest publish failed"
        );
        error!(
            ?map_id,
            ?manifest_hash,
            "remote manifest publish failed: {error}"
        );
        worker.in_flight_by_map.remove(map_id);
    }
    Ok(())
}

/// Returns true when local unpublished data must prevent older remote materialization.
pub fn has_unpublished_local_state(
    local_head: Option<&LocalMapHead>,
    accepted_head: Option<&MapRevision>,
    journal: &RemotePublishJournal,
) -> bool {
    if journal.entries.iter().any(|entry| {
        matches!(
            entry.status,
            RemotePublishStatus::Pending
                | RemotePublishStatus::InFlight
                | RemotePublishStatus::Failed
        )
    }) {
        return true;
    }
    match (local_head, accepted_head) {
        (Some(local), Some(accepted)) => {
            local.local_revision_number > accepted.revision
                || local.accepted_remote_manifest_hash != Some(accepted.manifest_hash)
        }
        (Some(_), None) => true,
        _ => false,
    }
}

/// Reset interrupted in-flight publishes to pending during startup recovery.
pub fn reset_inflight_publish_entries(journal: &mut RemotePublishJournal) {
    for entry in &mut journal.entries {
        if entry.status == RemotePublishStatus::InFlight {
            trace!(?entry.map_id, ?entry.new_manifest_hash, "resetting in-flight publish to pending");
            entry.status = RemotePublishStatus::Pending;
        }
    }
}

/// Returns true when an earlier failed entry blocks later pending publishes.
pub fn remote_publish_blocked_by_failed_entry(journal: &RemotePublishJournal) -> bool {
    journal
        .entries
        .iter()
        .any(|entry| entry.status == RemotePublishStatus::Failed)
}

/// Converts durable local unpublished drafts into signed remote publish journal entries.
pub fn prepare_pending_remote_publish_journal_entries(
    identity: Res<nostr_client::NostrKeys>,
    config: Res<RemoteMapPublishConfig>,
    mut maps: Query<(
        &MapInstanceId,
        &StoreBackend<(), MapRevision, FsAcceptedMapHeadStore>,
        &StoreBackend<MapInstanceId, RemotePublishJournal, FsRemotePublishJournalStore>,
        &AsyncStoreBackend<BlobRef, Vec<u8>, nostr_map_persistence::BlossomBlobPutStore>,
        &mut RemotePublishJournal,
        &mut PendingRemotePublishDeltas,
        &mut PendingRemotePublishEntryTasks,
    )>,
    mut in_flight_logged: Local<HashSet<MapInstanceId>>,
) {
    if !config.enabled {
        trace!("remote publish preparation skipped because remote publishing is disabled");
        return;
    }
    let public_blossom_base_url = config
        .blossom_public_base_url
        .clone()
        .expect("enabled remote publish config must include public Blossom base URL");

    for (
        map_id,
        accepted_head_store,
        journal_store,
        blob_store,
        mut journal,
        mut deltas,
        mut tasks,
    ) in &mut maps
    {
        poll_prepare_tasks(
            map_id,
            &journal_store.0,
            &mut journal,
            &mut deltas,
            &mut tasks,
        );
        if !tasks.tasks.is_empty() {
            if in_flight_logged.insert(map_id.clone()) {
                trace!(
                    ?map_id,
                    "remote publish journal entry preparation already in flight"
                );
            }
            continue;
        }
        in_flight_logged.remove(map_id);
        if remote_publish_blocked_by_failed_entry(&journal) {
            trace!(
                ?map_id,
                "remote publish preparation blocked by earlier failed journal entry"
            );
            continue;
        }
        discard_already_journaled_deltas(map_id, &journal, &mut deltas);
        if let Some(blocked_revision) = deltas.blocked_revision {
            trace!(
                ?map_id,
                blocked_revision,
                "remote publish preparation blocked after earlier failure"
            );
            continue;
        }
        let Some(draft) = deltas.pop_front_for_prepare() else {
            continue;
        };
        let previous_remote_manifest_hash = journal
            .entries
            .last()
            .map(|entry| entry.new_manifest_hash)
            .or_else(|| {
                accepted_head_store
                    .0
                    .load(&())
                    .expect("accepted map head store should load during remote publish preparation")
                    .map(|head| head.manifest_hash)
            });
        let identity = identity.clone();
        let blob_store = blob_store.0.clone();
        let public_blossom_base_url = public_blossom_base_url.clone();
        let draft_for_failure = draft.clone();
        tasks
            .tasks
            .push(bevy::tasks::IoTaskPool::get().spawn(async move {
                prepare_server_map_publish_entry(
                    &identity,
                    draft,
                    previous_remote_manifest_hash,
                    &blob_store,
                    &public_blossom_base_url,
                )
                .await
                .map_err(|error| RemotePublishPrepareFailure {
                    draft: draft_for_failure,
                    error,
                })
            }));
    }
}

fn poll_prepare_tasks(
    map_id: &MapInstanceId,
    journal_store: &FsRemotePublishJournalStore,
    journal: &mut RemotePublishJournal,
    deltas: &mut PendingRemotePublishDeltas,
    tasks: &mut PendingRemotePublishEntryTasks,
) {
    let mut index = 0;
    while index < tasks.tasks.len() {
        let Some(result) = bevy::tasks::futures::check_ready(&mut tasks.tasks[index]) else {
            index += 1;
            continue;
        };
        let _ = tasks.tasks.swap_remove(index);
        match result {
            Ok(entry) => {
                if journal
                    .entries
                    .iter()
                    .any(|existing| existing.new_manifest_hash == entry.new_manifest_hash)
                {
                    trace!(?map_id, ?entry.new_manifest_hash, "remote publish journal entry already exists");
                    continue;
                }
                let manifest_hash = entry.new_manifest_hash;
                journal.entries.push(entry);
                journal_store
                    .save(map_id, journal)
                    .expect("remote publish journal should persist after preparing entry");
                info!(
                    ?map_id,
                    ?manifest_hash,
                    "queued remote publish journal entry"
                );
            }
            Err(failure) => {
                let blocked_revision = deltas.block_after_prepare_failure(failure.draft);
                error!(
                    ?map_id,
                    blocked_revision,
                    error = %failure.error,
                    "failed to prepare remote publish journal entry; blocking later remote publish attempts until restart or manual retry"
                );
            }
        }
    }
}

fn discard_already_journaled_deltas(
    _map_id: &MapInstanceId,
    journal: &RemotePublishJournal,
    deltas: &mut PendingRemotePublishDeltas,
) {
    while let Some(draft) = deltas.queue.front() {
        if !journal.entries.iter().any(|entry| {
            entry.advances_local_head.local_revision_number >= draft.local_revision_number
        }) {
            break;
        }
        let draft = deltas
            .queue
            .pop_front()
            .expect("front draft was just observed");
        if deltas.blocked_revision == Some(draft.local_revision_number) {
            deltas.blocked_revision = None;
        }
    }
}

/// Polls per-map remote publish journals while preserving serial ordering per map.
pub fn poll_remote_publish_journal(
    mut worker: ResMut<RemoteMapPublishWorker>,
    mut journals: Query<(
        &MapInstanceId,
        &StoreBackend<MapInstanceId, RemotePublishJournal, FsRemotePublishJournalStore>,
        &StoreBackend<(), MapRevision, FsAcceptedMapHeadStore>,
        &StoreBackend<(), LocalMapHead, FsLocalMapHeadStore>,
        &StoreBackend<(), MapChangeSet, FsMapChangeSetStore>,
        &AsyncStoreBackend<ManifestHash, String, ServerManifestPublishStore>,
        &mut RemotePublishJournal,
        &mut PendingAsyncStoreOps<ManifestHash, String>,
    )>,
) {
    for (
        map_id,
        journal_store,
        accepted_head_store,
        local_head_store,
        change_set_store,
        publish_store,
        mut journal,
        mut publish_ops,
    ) in &mut journals
    {
        publish_ops.poll();
        if let Err(error) = apply_publish_results(
            map_id,
            &mut journal,
            &mut worker,
            &mut publish_ops,
            &accepted_head_store.0,
            &local_head_store.0,
            &journal_store.0,
            &change_set_store.0,
        ) {
            error!(
                ?map_id,
                ?error,
                "failed to persist remote publish result state"
            );
            continue;
        }

        if worker.in_flight_by_map.contains(map_id) {
            trace!(?map_id, "remote publish already in flight for map");
            continue;
        }
        if journal
            .entries
            .iter()
            .any(|entry| entry.status == RemotePublishStatus::Failed)
        {
            trace!(
                ?map_id,
                "remote publish blocked by earlier failed journal entry"
            );
            continue;
        }
        let Some(entry) = journal
            .entries
            .iter_mut()
            .find(|entry| entry.status == RemotePublishStatus::Pending)
        else {
            continue;
        };
        entry.status = RemotePublishStatus::InFlight;
        worker.in_flight_by_map.insert(map_id.clone());
        let event_json = entry
            .signed_event_json
            .clone()
            .expect("pending publish journal entry must contain signed event JSON");
        trace!(
            ?map_id,
            ?entry.new_manifest_hash,
            local_revision_number = entry.advances_local_head.local_revision_number,
            "starting remote manifest publish"
        );
        publish_ops.spawn_save(&publish_store.0, entry.new_manifest_hash, event_json);
        journal_store
            .0
            .save(map_id, &journal)
            .expect("remote publish journal should persist after marking entry in-flight");
    }
}

/// Converts terrain chunk save completions from voxel_map_engine into map payload events.
pub fn normalize_chunk_save_completions(
    mut completed: MessageReader<ChunkSaveCompleted>,
    mut failed: MessageReader<ChunkSaveFailed>,
    mut completed_writer: MessageWriter<MapPayloadSaveCompleted>,
    mut failed_writer: MessageWriter<MapPayloadSaveFailed>,
) {
    for event in completed.read() {
        let Some(save_id) = event.save_id else {
            continue;
        };
        completed_writer.write(MapPayloadSaveCompleted {
            map_entity: event.map_entity,
            save_id,
            key: MapPayloadSaveKey::TerrainChunk(event.position),
        });
    }
    for event in failed.read() {
        let Some(save_id) = event.save_id else {
            continue;
        };
        failed_writer.write(MapPayloadSaveFailed {
            map_entity: event.map_entity,
            save_id,
            key: MapPayloadSaveKey::TerrainChunk(event.position),
            error: event.error.clone(),
        });
    }
}

/// Persists unpublished drafts after filesystem save completion and advances local head.
pub fn handle_completed_map_payload_save_for_publish(
    mut completed: MessageReader<MapPayloadSaveCompleted>,
    map_ids: Query<&MapInstanceId>,
    draft_stores: Query<
        &StoreBackend<SaveOpId, LocalUnpublishedPublishDraft, FsLocalUnpublishedPublishDraftStore>,
    >,
    local_head_stores: Query<&StoreBackend<(), LocalMapHead, FsLocalMapHeadStore>>,
    mut pending_by_save_id: ResMut<PendingPublishBySaveId>,
    mut deltas: Query<&mut PendingRemotePublishDeltas>,
) {
    for event in completed.read() {
        let map_id = map_ids
            .get(event.map_entity)
            .expect("payload save event map entity must have MapInstanceId");
        if !matches!(map_id, MapInstanceId::Overworld) {
            trace!(?map_id, ?event.key, "remote publish skipped for non-overworld path");
            continue;
        }
        let Some(draft) = pending_by_save_id.0.remove(&event.save_id) else {
            warn!(?event.save_id, ?event.key, "completed save id has no publish draft");
            continue;
        };
        let persisted = LocalUnpublishedPublishDraft {
            map_id: map_id.clone(),
            draft: draft.clone(),
            save_id: event.save_id,
        };
        draft_stores
            .get(event.map_entity)
            .expect("publishable map must have local unpublished draft store")
            .0
            .save(&event.save_id, &persisted)
            .expect("local unpublished publish draft must persist before local_head advances");
        local_head_stores
            .get(event.map_entity)
            .expect("publishable map must have local head store")
            .0
            .save(&(), &local_head_from_unpublished_draft(&persisted))
            .expect("local head must advance after local publish draft persists");
        deltas
            .get_mut(event.map_entity)
            .expect("map with publishable save must have PendingRemotePublishDeltas")
            .queue
            .push_back(draft);
    }
}

/// Loads durable unpublished drafts into each map's pending publish queue.
pub fn recover_unpublished_publish_drafts(
    mut maps: Query<(
        &MapInstanceId,
        &StoreBackend<SaveOpId, LocalUnpublishedPublishDraft, FsLocalUnpublishedPublishDraftStore>,
        &mut PendingRemotePublishDeltas,
    )>,
) -> Result<(), PersistenceError> {
    for (map_id, draft_store, mut deltas) in &mut maps {
        for persisted in draft_store.0.load_all()? {
            if persisted.map_id != *map_id {
                return Err(PersistenceError::Deserialize(
                    "unpublished draft map id does not match owning map".to_string(),
                ));
            }
            deltas.queue.push_back(persisted.draft);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn chunk_descriptor(class: PayloadClass, x: i32, y: i32, z: i32) -> ManifestPayloadDescriptor {
        ManifestPayloadDescriptor {
            class,
            key: PayloadKey::Chunk { x, y, z },
            slot: ManifestPayloadSlot::Tombstoned,
            schema_version: 1,
        }
    }

    fn singleton_descriptor(
        class: PayloadClass,
        slot: ManifestPayloadSlot,
    ) -> ManifestPayloadDescriptor {
        ManifestPayloadDescriptor {
            class,
            key: PayloadKey::Singleton,
            slot,
            schema_version: 1,
        }
    }

    #[test]
    fn clear_published_keys_removes_only_published_candidates() {
        let mut change_set = MapChangeSet::default();
        change_set
            .chunk_candidates
            .extend([IVec3::new(1, 0, 0), IVec3::new(2, 0, 0)]);
        change_set
            .chunk_entity_candidates
            .extend([IVec3::new(3, 0, 0), IVec3::new(4, 0, 0)]);
        change_set.map_entities_changed = true;
        change_set.meta_changed = true;

        let payloads = vec![
            chunk_descriptor(PayloadClass::TerrainChunk, 1, 0, 0),
            chunk_descriptor(PayloadClass::ChunkEntities, 3, 0, 0),
            // Published map-entities slot clears the flag.
            singleton_descriptor(PayloadClass::MapEntities, ManifestPayloadSlot::Empty),
            // Absent meta slot was not published, so its flag is preserved.
            singleton_descriptor(PayloadClass::MapMeta, ManifestPayloadSlot::Absent),
        ];
        clear_published_change_set_keys(&mut change_set, &payloads);

        assert_eq!(
            change_set.chunk_candidates,
            HashSet::from([IVec3::new(2, 0, 0)])
        );
        assert_eq!(
            change_set.chunk_entity_candidates,
            HashSet::from([IVec3::new(4, 0, 0)])
        );
        assert!(!change_set.map_entities_changed);
        assert!(change_set.meta_changed);
    }
}
