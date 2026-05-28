use std::collections::{HashMap, HashSet, VecDeque};

use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::prelude::*;
use nostr_client::BlobRef;
use nostr_map_persistence::{
    build_signed_map_manifest_event, compute_descriptor_root, manifest_payload_descriptor_order,
    ManifestHash, ManifestPayloadDescriptor, ManifestPayloadSlot, MapManifestSigner,
    MapPersistenceRejection, MapRevision, NostrMapManifest, PayloadClass, PayloadKey,
    PayloadSlotState, MAP_MANIFEST_SCHEMA_VERSION,
};
use persistence::{
    AsyncStore, AsyncStoreBackend, PendingAsyncStoreOps, PersistenceError, SaveOpId, Store,
    StoreBackend,
};
use protocol::map::SavedEntity;
use protocol::{MapInstanceId, NostrPublicKey};
use sha2::{Digest, Sha256};
use voxel_map_engine::config::WorldObjectSpawn;
use voxel_map_engine::persistence::ChunkFileEnvelope;
use voxel_map_engine::prelude::{ChunkSaveCompleted, ChunkSaveFailed};

use crate::persistence::{
    FsAcceptedMapHeadStore, FsLocalMapHeadStore, FsLocalUnpublishedPublishDraftStore,
    FsRemotePublishJournalStore, LocalMapHead, LocalUnpublishedPublishDraft, MapMeta,
    RemotePublishJournal, RemotePublishStatus, ServerMapPublishDraft,
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
pub struct PendingRemotePublishDeltas(pub VecDeque<ServerMapPublishDraft>);

/// In-memory correlation between filesystem save ids and publish drafts.
#[derive(Resource, Default)]
pub struct PendingPublishBySaveId(pub HashMap<SaveOpId, ServerMapPublishDraft>);

/// Tracks maps with an in-flight remote manifest publish.
#[derive(Resource, Default)]
pub struct RemoteMapPublishWorker {
    pub in_flight_by_map: HashSet<MapInstanceId>,
}

/// Local adapter implementing map manifest signing for the configured server identity.
struct ServerManifestSigner<'a>(&'a nostr_client::ServerIdentity);

impl MapManifestSigner for ServerManifestSigner<'_> {
    fn public_key(&self) -> NostrPublicKey {
        NostrPublicKey(*self.0.keys.public_key().as_bytes())
    }

    fn sign_map_manifest_event(
        &self,
        draft: nostr_client::events::NostrEventDraft,
    ) -> Result<String, nostr_map_persistence::RemotePersistenceError> {
        draft
            .sign_with_keys(&self.0.keys)
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

/// Encodes map metadata in the same format as the filesystem map metadata store.
pub fn encode_map_meta_payload(value: MapMeta) -> Result<Vec<u8>, MapPersistenceRejection> {
    bincode::serialize(&value)
        .map_err(|error| MapPersistenceRejection::Invalid(format!("encode map meta: {error}")))
}

/// Encodes terrain chunk data in the same format as the filesystem terrain store.
pub fn encode_chunk_payload(value: ChunkFileEnvelope) -> Result<Vec<u8>, MapPersistenceRejection> {
    zstd_bincode_encode(&value, "chunk payload")
}

/// Encodes chunk entity data in the same format as the filesystem chunk entity store.
pub fn encode_chunk_entities_payload(
    value: Vec<WorldObjectSpawn>,
) -> Result<Vec<u8>, MapPersistenceRejection> {
    #[derive(serde::Serialize)]
    struct Envelope {
        version: u32,
        spawns: Vec<WorldObjectSpawn>,
    }
    zstd_bincode_encode(
        &Envelope {
            version: 3,
            spawns: value,
        },
        "chunk entities payload",
    )
}

/// Encodes map-level entities in the same format as the filesystem entity store.
pub fn encode_map_entities_payload(
    value: Vec<SavedEntity>,
) -> Result<Vec<u8>, MapPersistenceRejection> {
    #[derive(serde::Serialize)]
    struct Envelope {
        version: u32,
        entities: Vec<SavedEntity>,
    }
    bincode::serialize(&Envelope {
        version: 1,
        entities: value,
    })
    .map_err(|error| {
        MapPersistenceRejection::Invalid(format!("encode map entities payload: {error}"))
    })
}

fn zstd_bincode_encode<T: serde::Serialize>(
    value: &T,
    label: &str,
) -> Result<Vec<u8>, MapPersistenceRejection> {
    let encoded = bincode::serialize(value)
        .map_err(|error| MapPersistenceRejection::Invalid(format!("encode {label}: {error}")))?;
    zstd::encode_all(encoded.as_slice(), 0)
        .map_err(|error| MapPersistenceRejection::Invalid(format!("compress {label}: {error}")))
}

/// Uploads one payload slot and appends the signed manifest descriptor.
pub async fn upload_publish_slot<T>(
    payloads: &mut Vec<ManifestPayloadDescriptor>,
    blob_store: &impl AsyncStore<BlobRef, Vec<u8>>,
    public_blossom_base_url: &url::Url,
    class: PayloadClass,
    key: PayloadKey,
    schema_version: u32,
    slot: PayloadSlotState<T>,
    encode: impl FnOnce(T) -> Result<Vec<u8>, MapPersistenceRejection>,
) -> Result<(), MapPersistenceRejection> {
    let manifest_slot = match slot {
        PayloadSlotState::Present(value) => {
            let bytes = encode(value)?;
            let sha256: [u8; 32] = Sha256::digest(&bytes).into();
            let mut get_url = public_blossom_base_url.clone();
            get_url.set_path(&hex::encode(sha256));
            let blob = BlobRef {
                sha256,
                size: bytes.len() as u64,
                content_type: "application/octet-stream".to_string(),
                urls: vec![get_url.to_string()],
            };
            blob_store
                .save(&blob, &bytes)
                .await
                .map_err(|error| MapPersistenceRejection::Unavailable(error.to_string()))?;
            ManifestPayloadSlot::Present { blob }
        }
        PayloadSlotState::Empty => ManifestPayloadSlot::Empty,
        PayloadSlotState::Absent => ManifestPayloadSlot::Absent,
        PayloadSlotState::Tombstoned => ManifestPayloadSlot::Tombstoned,
    };
    payloads.push(ManifestPayloadDescriptor {
        class,
        key,
        slot: manifest_slot,
        schema_version,
    });
    Ok(())
}

/// Converts a server Overworld draft into a signed pending remote journal entry.
pub async fn prepare_server_map_publish_entry(
    identity: &nostr_client::ServerIdentity,
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
        encode_map_meta_payload,
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

    payloads.sort_by_key(manifest_payload_descriptor_order);
    let descriptor_root = compute_descriptor_root(&payloads)
        .map_err(|error| MapPersistenceRejection::Invalid(error.to_string()))?;
    let manifest = NostrMapManifest {
        map_id: map_id.clone(),
        owner,
        revision: draft.local_revision_number,
        previous_hash: previous_remote_manifest_hash,
        payloads: payloads.clone(),
        schema_version: MAP_MANIFEST_SCHEMA_VERSION,
        descriptor_root,
    };
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

/// Applies completed or failed manifest publish operations to durable heads and journal state.
pub fn apply_publish_results(
    map_id: &MapInstanceId,
    journal: &mut RemotePublishJournal,
    worker: &mut RemoteMapPublishWorker,
    publish_ops: &mut PendingAsyncStoreOps<ManifestHash, String>,
    accepted_head_store: &FsAcceptedMapHeadStore,
    local_head_store: &FsLocalMapHeadStore,
    journal_store: &FsRemotePublishJournalStore,
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

/// Polls per-map remote publish journals while preserving serial ordering per map.
pub fn poll_remote_publish_journal(
    mut worker: ResMut<RemoteMapPublishWorker>,
    mut journals: Query<(
        &MapInstanceId,
        &StoreBackend<MapInstanceId, RemotePublishJournal, FsRemotePublishJournalStore>,
        &StoreBackend<(), MapRevision, FsAcceptedMapHeadStore>,
        &StoreBackend<(), LocalMapHead, FsLocalMapHeadStore>,
        &AsyncStoreBackend<ManifestHash, String, nostr_map_persistence::NostrManifestPublishStore>,
        &mut RemotePublishJournal,
        &mut PendingAsyncStoreOps<ManifestHash, String>,
    )>,
) {
    for (
        map_id,
        journal_store,
        accepted_head_store,
        local_head_store,
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
            trace!(?map_id, "no pending remote publish journal entry");
            continue;
        };
        entry.status = RemotePublishStatus::InFlight;
        worker.in_flight_by_map.insert(map_id.clone());
        let event_json = entry
            .signed_event_json
            .clone()
            .expect("pending publish journal entry must contain signed event JSON");
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
            trace!(?event.position, "terrain chunk save completion has no publish save id");
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
            trace!(?event.position, "terrain chunk save failure has no publish save id");
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
            .0
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
            deltas.0.push_back(persisted.draft);
        }
    }
    Ok(())
}
