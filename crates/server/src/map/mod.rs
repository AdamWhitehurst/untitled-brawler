pub mod homebase_publication;
pub mod preflight;
pub mod preparation;
pub mod remote_publish;
pub mod switching;
pub mod types;

pub use homebase_publication::*;
pub use preflight::*;
pub use preparation::*;
pub use switching::*;
pub use types::*;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Debug;
use std::path::PathBuf;
use std::sync::Arc;

use avian3d::prelude::{Position, Rotation};
use bevy::app::AppExit;
use bevy::prelude::*;
use lightyear::prelude::{
    ControlledBy, MessageReceiver, MessageSender, NetworkVisibility, Room, RoomEvent, RoomTarget,
    ServerMultiMessageSender,
};
use protocol::{
    CharacterMarker, ChunkChannel, ChunkDataSync, MapInstanceId, MapRegistry, SectionBlocksUpdate,
    UnloadColumn, VoxelBrushEditRequest, VoxelChange, VoxelChannel, VoxelConcreteEditRequest,
    VoxelEditAck, VoxelEditBroadcast, VoxelEditReject, VoxelEditRequest, VoxelType,
};
#[allow(unused_imports)]
use tracy_client::plot;
use voxel_map_engine::lifecycle::{self, PendingSaves};
use voxel_map_engine::prelude::{
    brush_footprint, build_generator_from_components, BiomeRules, ChunkTicket, HeightMap,
    MapDimensions, MoistureMap, PlacementRules, RuntimeShape, TerrainBrushMode, VoxelGenerator,
    VoxelMapConfig, VoxelMapInstance, VoxelPlugin, VoxelWorld, WorldVoxel,
};

use crate::persistence::fs_map_entities::FsMapEntitiesStore;
use crate::persistence::fs_map_meta::FsMapMetaStore;
use crate::persistence::{
    map_save_dir, store_map_dir_for_loading, FsAcceptedMapHeadStore, FsLocalMapHeadStore,
    FsLocalUnpublishedPublishDraftStore, FsRemotePublishJournalStore, LocalMapHead, MapMeta,
    RemoteMapPersistenceConfig, RemotePublishJournal, ServerMapPublishDraft, WorldSavePath,
};
use persistence::{PendingStoreOps, SaveOpIdAllocator, StoreBackend};
use protocol::map::{ChunkEntityRef, MapSaveTarget, SavedEntity, SavedEntityKind};
use protocol::terrain::TerrainDef;
use protocol::vox_model::{VoxModelAsset, VoxModelRegistry};
use protocol::world_object::{
    apply_object_components, ActiveTransformation, WorldObjectDef, WorldObjectDefRegistry,
    WorldObjectDeleteAck, WorldObjectDeleteRequest, WorldObjectEditChannel, WorldObjectEditReject,
    WorldObjectEditRejectReason, WorldObjectId, WorldObjectMoveAck, WorldObjectMoveRequest,
    WorldObjectPlacementAck, WorldObjectPlacementChannel, WorldObjectPlacementReject,
    WorldObjectPlacementRejectReason, WorldObjectPlacementRequest, WorldObjectRotateAck,
    WorldObjectRotateRequest,
};
use protocol::{AppState, RespawnPoint, TerrainDefRegistry};
use voxel_map_engine::config::WorldObjectSpawn;
use voxel_map_engine::persistence::fs_chunk::FsChunkStore;
use voxel_map_engine::persistence::fs_chunk_entities::FsChunkEntitiesStore;
use voxel_map_engine::persistence::{ChunkFileEnvelope, CHUNK_SAVE_VERSION};

const MAX_BRUSH_VOXELS: usize = 4096;

fn log_save_errors<K, V>(ops: &mut PendingStoreOps<K, V>, context: &str)
where
    K: Send + Sync + Debug + 'static,
    V: Send + Sync + 'static,
{
    ops.completed_saves
        .retain(|completion| completion.id.is_some());
    for failure in ops.save_errors.drain(..) {
        error!(
            "Failed to save {context} at {:?}: {}",
            failure.key, failure.error
        );
    }
}

/// Plugin managing server-side voxel map functionality.
pub struct ServerMapPlugin;

/// Maps `MapInstanceId` to lightyear room entities. Server-only.
#[derive(Resource, Default)]
pub struct RoomRegistry(pub HashMap<MapInstanceId, Entity>);

impl RoomRegistry {
    pub fn get_or_create(&mut self, id: &MapInstanceId, commands: &mut Commands) -> Entity {
        *self.0.entry(id.clone()).or_insert_with(|| {
            let room = commands.spawn(Room::default()).id();
            trace!("Created room for map {id:?}: {room:?}");
            room
        })
    }
}

pub(crate) const DEFAULT_OVERWORLD_SEED: u64 = 999;
pub(crate) const GENERATION_VERSION: u32 = 0;
const SAVE_DEBOUNCE_SECONDS: f64 = 1.0;
const MAX_DIRTY_SECONDS: f64 = 5.0;

/// Tracks whether any map has unsaved dirty chunks.
#[derive(Resource)]
pub struct WorldDirtyState {
    pub is_dirty: bool,
    pub last_edit_time: f64,
    pub first_dirty_time: Option<f64>,
    /// Maps whose saved entities changed since the last save; drives the entity publish slot.
    pub entities_dirty: HashSet<MapInstanceId>,
}

impl Default for WorldDirtyState {
    fn default() -> Self {
        Self {
            is_dirty: false,
            last_edit_time: 0.0,
            first_dirty_time: None,
            entities_dirty: HashSet::new(),
        }
    }
}

impl WorldDirtyState {
    /// Starts (or extends) the debounced save window for any map content change.
    pub fn mark_dirty(&mut self, now: f64) {
        if !self.is_dirty {
            self.first_dirty_time = Some(now);
        }
        self.is_dirty = true;
        self.last_edit_time = now;
    }

    /// Records that a map's saved entities changed and schedules a save.
    pub fn mark_entities_dirty(&mut self, map_id: &MapInstanceId, now: f64) {
        self.mark_dirty(now);
        self.entities_dirty.insert(map_id.clone());
    }
}

/// A voxel edit pending broadcast, with context for room-scoped sending.
pub struct PendingVoxelEdit {
    pub position: IVec3,
    pub voxel: VoxelType,
    /// Client entity that made the edit (excluded from broadcast).
    pub originator: Entity,
    pub map_id: MapInstanceId,
}

/// Accumulates voxel edits per map and chunk during a tick for batching.
#[derive(Resource, Default)]
pub struct PendingVoxelBroadcasts {
    pub per_chunk: HashMap<(MapInstanceId, IVec3), Vec<PendingVoxelEdit>>,
}

fn configure_remote_map_read_context(
    mut commands: Commands,
    remote_config: Res<RemoteMapPersistenceConfig>,
    remote_publish_config: Res<remote_publish::RemoteMapPublishConfig>,
    relay_pool: Option<Res<nostr_client::RelayPool>>,
    nostr_config: Res<nostr_client::NostrClientConfig>,
) {
    if !remote_config.enabled {
        trace!("remote map restore disabled");
        return;
    }

    let relay_pool = relay_pool.expect("SERVER_MAP_REMOTE_READ=1 requires RelayPool resource");
    let allowed_blossom_hosts = remote_read_allowed_blossom_hosts(&remote_publish_config);
    let mut persistence_policy = nostr_map_persistence::MapPersistencePolicy::default();
    persistence_policy.allowed_blossom_hosts = allowed_blossom_hosts.clone();
    let mut query_policy = nostr_map_persistence::NostrMapQueryPolicy::default();
    query_policy.relays = nostr_config.relays.clone();
    query_policy.timeout = remote_config.fallback_timeout;

    commands.insert_resource(crate::persistence::RemoteMapReadContext {
        event_client: relay_pool.event_client(),
        query_policy,
        persistence_policy,
    });
    info!(
        ?allowed_blossom_hosts,
        "real Nostr/Blossom remote map restore enabled"
    );
}

fn remote_read_allowed_blossom_hosts(
    remote_publish_config: &remote_publish::RemoteMapPublishConfig,
) -> BTreeSet<String> {
    let mut hosts = parse_blossom_allowed_hosts(
        std::env::var("SERVER_BLOSSOM_ALLOWED_HOSTS")
            .ok()
            .as_deref(),
    );
    if let Some(host) = remote_publish_config
        .blossom_public_base_url
        .as_ref()
        .and_then(|url| url.host_str())
    {
        hosts.insert(host.to_ascii_lowercase());
    } else if let Ok(public_base_url) = std::env::var("SERVER_BLOSSOM_PUBLIC_BASE_URL") {
        let url = url::Url::parse(&public_base_url)
            .expect("SERVER_BLOSSOM_PUBLIC_BASE_URL must be a valid URL");
        let host = url
            .host_str()
            .expect("SERVER_BLOSSOM_PUBLIC_BASE_URL must include a host");
        hosts.insert(host.to_ascii_lowercase());
    }
    if hosts.is_empty() {
        panic!(
            "remote map restore requires SERVER_BLOSSOM_PUBLIC_BASE_URL or SERVER_BLOSSOM_ALLOWED_HOSTS"
        );
    }
    hosts
}

fn parse_blossom_allowed_hosts(raw: Option<&str>) -> BTreeSet<String> {
    raw.map(|value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|host| !host.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    })
    .unwrap_or_default()
}

/// Spawn the overworld map entity with store components and begin async meta load.
fn init_overworld_entity(
    mut commands: Commands,
    mut registry: ResMut<MapRegistry>,
    mut queue: ResMut<PendingMapPreflights>,
    save_path: Res<WorldSavePath>,
    server_identity: Res<nostr_client::NostrKeys>,
    remote_publish_config: Res<remote_publish::RemoteMapPublishConfig>,
    relay_pool: Option<Res<nostr_client::RelayPool>>,
) {
    let canonical_map_dir = map_save_dir(&save_path.0, &MapInstanceId::Overworld);
    let map_dir = Arc::new(
        store_map_dir_for_loading(&canonical_map_dir)
            .expect("overworld active revision pointer must be valid before startup"),
    );
    let owner = server_identity.protocol_public_key();
    // Head pointers span revisions, so they live at the map's top-level dir, not inside the
    // active revision snapshot. This keeps the running publish path reading the same heads the
    // restore seeds, regardless of fresh-start / restore / existing-world.
    let head_dir = Arc::new(canonical_map_dir.clone());
    let accepted_head_store = FsAcceptedMapHeadStore {
        map_dir: head_dir.clone(),
    };
    let local_head_store = FsLocalMapHeadStore { map_dir: head_dir };
    let draft_store = FsLocalUnpublishedPublishDraftStore {
        map_dir: map_dir.clone(),
    };
    let journal_store = FsRemotePublishJournalStore {
        save_root: save_path.0.clone(),
    };
    let mut journal = persistence::Store::load(&journal_store, &MapInstanceId::Overworld)
        .expect("overworld remote publish journal should load during startup")
        .unwrap_or_default();
    let original_journal = journal.clone();
    remote_publish::reset_inflight_publish_entries(&mut journal);
    if journal != original_journal {
        persistence::Store::save(&journal_store, &MapInstanceId::Overworld, &journal)
            .expect("overworld remote publish journal should persist after startup recovery");
    }
    let mut pending_deltas = remote_publish::PendingRemotePublishDeltas::default();
    for persisted in draft_store
        .load_all()
        .expect("overworld unpublished publish drafts should load during startup")
    {
        if persisted.map_id != MapInstanceId::Overworld {
            panic!(
                "overworld unpublished publish draft has mismatched map id {:?}",
                persisted.map_id
            );
        }
        if journal.entries.iter().any(|entry| {
            entry.advances_local_head.local_revision_number >= persisted.draft.local_revision_number
        }) {
            trace!(
                local_revision_number = persisted.draft.local_revision_number,
                "unpublished publish draft already has a journal entry"
            );
            continue;
        }
        pending_deltas.queue.push_back(persisted.draft);
    }

    let mut map_commands = commands.spawn((
        MapInstanceId::Overworld,
        protocol::map::Owner(owner),
        MapLoadState::CheckingPersistence,
        Transform::default(),
    ));
    map_commands.insert((
        StoreBackend::new(FsMapMetaStore {
            map_dir: map_dir.clone(),
        }),
        PendingStoreOps::<(), MapMeta>::default(),
        StoreBackend::new(FsMapEntitiesStore {
            map_dir: map_dir.clone(),
        }),
        PendingStoreOps::<(), Vec<SavedEntity>>::default(),
        StoreBackend::new(FsChunkEntitiesStore {
            map_dir: map_dir.clone(),
        }),
        PendingStoreOps::<IVec3, Vec<WorldObjectSpawn>>::default(),
        StoreBackend::new(FsChunkStore {
            map_dir: map_dir.clone(),
        }),
        PendingStoreOps::<IVec3, ChunkFileEnvelope>::default(),
    ));
    map_commands.insert((
        StoreBackend::new(accepted_head_store),
        StoreBackend::new(local_head_store),
        StoreBackend::new(draft_store),
        StoreBackend::new(journal_store),
        journal,
        pending_deltas,
    ));
    if remote_publish_config.enabled {
        let relay_pool = relay_pool
            .as_ref()
            .expect("SERVER_MAP_REMOTE_PUBLISH=1 requires NostrClientPlugin RelayPool resource");
        let upload_url = remote_publish_config
            .blossom_upload_url
            .clone()
            .expect("enabled remote publish config must include Blossom upload URL");
        map_commands.insert((
            persistence::AsyncStoreBackend::new(nostr_map_persistence::BlossomBlobPutStore {
                upload_url,
                auth: nostr_client::BlossomAuth::from_keys(&server_identity),
            }),
            persistence::AsyncStoreBackend::new(remote_publish::ServerManifestPublishStore::new(
                relay_pool.event_client(),
                remote_publish_config.fail_first_manifest_publish,
            )),
            persistence::PendingAsyncStoreOps::<nostr_map_persistence::ManifestHash, String>::default(
            ),
            remote_publish::PendingRemotePublishEntryTasks::default(),
        ));
        info!("server-owned Overworld remote publishing enabled");
    } else {
        trace!("server-owned Overworld remote publishing disabled");
    }
    let map = map_commands.id();

    registry.insert(MapInstanceId::Overworld, map);
    queue.0.push_back(PendingMapPreflight {
        target_map_id: MapInstanceId::Overworld,
        kind: MapPreflightKind::StartupOverworld,
    });
}

/// Poll async meta loads, configure map entities when meta arrives.
fn poll_map_meta(
    mut commands: Commands,
    mut query: Query<(
        Entity,
        &MapInstanceId,
        &mut PendingStoreOps<(), MapMeta>,
        &StoreBackend<(), MapMeta, FsMapMetaStore>,
        &mut MapLoadState,
    )>,
    terrain_registry: Res<TerrainDefRegistry>,
    type_registry: Res<AppTypeRegistry>,
    mut save_completed: MessageWriter<remote_publish::MapPayloadSaveCompleted>,
) {
    for (entity, map_id, mut ops, store, mut state) in &mut query {
        ops.poll();
        log_save_errors(&mut ops, "map metadata");
        for completion in ops.completed_saves.drain(..) {
            if let Some(save_id) = completion.id {
                save_completed.write(remote_publish::MapPayloadSaveCompleted {
                    map_entity: entity,
                    save_id,
                    key: remote_publish::MapPayloadSaveKey::MapMeta,
                });
            } else {
                trace!(
                    ?map_id,
                    "map metadata save completion has no publish save id"
                );
            }
        }

        if *state != MapLoadState::AwaitingMeta {
            continue;
        }

        // First frame: kick off the load
        if ops.completed_loads.is_empty() && !ops.has_pending() && ops.load_errors.is_empty() {
            ops.spawn_load(&store.0, ());
            return;
        }

        if let Some((_, meta_opt)) = ops.completed_loads.pop() {
            let (seed, gen_version) = match meta_opt {
                Some(meta) => {
                    info!(
                        "Loaded map meta: seed={}, gen_version={}",
                        meta.seed, meta.generation_version
                    );
                    (meta.seed, meta.generation_version)
                }
                None => {
                    info!("No saved meta found, using defaults: seed={DEFAULT_OVERWORLD_SEED}");
                    (DEFAULT_OVERWORLD_SEED, GENERATION_VERSION)
                }
            };

            configure_map_from_meta(
                &mut commands,
                entity,
                map_id,
                seed,
                gen_version,
                &store.0.map_dir,
                &terrain_registry,
                &type_registry,
            );

            *state = MapLoadState::AwaitingEntities;
        }

        for (_, e) in ops.load_errors.drain(..) {
            warn!("Failed to load map meta: {e}, using defaults");
            configure_map_from_meta(
                &mut commands,
                entity,
                map_id,
                DEFAULT_OVERWORLD_SEED,
                GENERATION_VERSION,
                &store.0.map_dir,
                &terrain_registry,
                &type_registry,
            );
            *state = MapLoadState::AwaitingEntities;
        }
    }
}

/// Apply map configuration components after meta is resolved.
pub(crate) fn configure_map_from_meta(
    commands: &mut Commands,
    entity: Entity,
    map_id: &MapInstanceId,
    seed: u64,
    generation_version: u32,
    map_dir: &PathBuf,
    terrain_registry: &TerrainDefRegistry,
    type_registry: &AppTypeRegistry,
) {
    let terrain_key = terrain_key_for_map(map_id);
    let terrain_def = terrain_registry
        .get(terrain_key)
        .unwrap_or_else(|| panic!("{terrain_key}.terrain.ron must be loaded by AppState::Ready"));
    let dimensions = terrain_def
        .map_dimensions()
        .unwrap_or_else(|| panic!("{terrain_key}.terrain.ron must contain MapDimensions"));

    let mut config = VoxelMapConfig::new(seed, generation_version, 2, true);
    config.save_dir = Some(map_dir.clone());

    let instance = VoxelMapInstance::new(dimensions.tree_height, dimensions.chunk_size);
    let shape = instance.shape.clone();

    commands
        .entity(entity)
        .insert((instance, config, dimensions.clone()));

    let components = clone_terrain_components_excluding_dimensions(terrain_def);
    apply_object_components(commands, entity, components, type_registry.0.clone());

    let generator = build_generator_from_def(
        terrain_def,
        seed,
        dimensions.chunk_size,
        dimensions.padded_size(),
        shape,
    );
    commands.entity(entity).insert(generator);
}

/// Returns the terrain asset key used to configure a map instance.
pub(crate) fn terrain_key_for_map(map_id: &MapInstanceId) -> &'static str {
    match map_id {
        MapInstanceId::Overworld => "overworld",
        MapInstanceId::Homebase { .. } => "homebase",
    }
}

/// Clone terrain definition components via `reflect_clone`, excluding `MapDimensions`.
///
/// `MapDimensions` is inserted directly on the map entity at spawn time;
/// skipping it here avoids double-insertion via `apply_object_components`.
fn clone_terrain_components_excluding_dimensions(
    def: &TerrainDef,
) -> Vec<Box<dyn bevy::reflect::PartialReflect>> {
    def.components
        .iter()
        .filter(|c| c.try_downcast_ref::<MapDimensions>().is_none())
        .map(|c| {
            c.reflect_clone()
                .expect("terrain component must be cloneable")
                .into_partial_reflect()
        })
        .collect()
}

/// Build a [`VoxelGenerator`] directly from a [`TerrainDef`].
///
/// Reads terrain components from the def rather than an `EntityRef`, so this
/// works during inline spawn (before components have been flushed onto the entity).
fn build_generator_from_def(
    def: &TerrainDef,
    seed: u64,
    chunk_size: u32,
    padded_size: u32,
    shape: RuntimeShape<u32, 3>,
) -> VoxelGenerator {
    let height = def
        .components
        .iter()
        .find_map(|c| c.try_downcast_ref::<HeightMap>().cloned());
    let moisture = def
        .components
        .iter()
        .find_map(|c| c.try_downcast_ref::<MoistureMap>().cloned());
    let biomes = def
        .components
        .iter()
        .find_map(|c| c.try_downcast_ref::<BiomeRules>().cloned());
    let placement = def
        .components
        .iter()
        .find_map(|c| c.try_downcast_ref::<PlacementRules>().cloned());
    build_generator_from_components(
        seed,
        chunk_size,
        padded_size,
        shape,
        height,
        moisture,
        biomes,
        placement,
    )
}

fn save_dirty_chunks_debounced(
    time: Res<Time>,
    mut dirty_state: ResMut<WorldDirtyState>,
    mut map_query: Query<(
        Entity,
        &mut VoxelMapInstance,
        &VoxelMapConfig,
        &MapInstanceId,
        &mut PendingSaves,
        &StoreBackend<(), MapMeta, FsMapMetaStore>,
        &mut PendingStoreOps<(), MapMeta>,
        &StoreBackend<(), Vec<SavedEntity>, FsMapEntitiesStore>,
        &mut PendingStoreOps<(), Vec<SavedEntity>>,
        Option<&StoreBackend<(), LocalMapHead, FsLocalMapHeadStore>>,
        Option<&RemotePublishJournal>,
        Option<&remote_publish::PendingRemotePublishDeltas>,
    )>,
    entity_query: Query<(
        &MapSaveTarget,
        &MapInstanceId,
        &Position,
        Option<&RespawnPoint>,
    )>,
    respawn_query: Query<(&Position, &MapInstanceId), With<RespawnPoint>>,
    mut save_ids: ResMut<SaveOpIdAllocator>,
    mut pending_publish: ResMut<remote_publish::PendingPublishBySaveId>,
    remote_publish_config: Res<remote_publish::RemoteMapPublishConfig>,
) {
    if !dirty_state.is_dirty {
        return;
    }

    let now = time.elapsed_secs_f64();
    let time_since_edit = now - dirty_state.last_edit_time;
    let time_since_first_dirty = dirty_state.first_dirty_time.map(|t| now - t).unwrap_or(0.0);

    let should_save =
        time_since_edit >= SAVE_DEBOUNCE_SECONDS || time_since_first_dirty >= MAX_DIRTY_SECONDS;

    if !should_save {
        return;
    }

    let by_map = collect_entities_by_map(&entity_query);

    for (
        map_entity,
        mut instance,
        config,
        map_id,
        mut pending_saves,
        meta_store,
        mut meta_ops,
        entity_store,
        mut entity_ops,
        local_head_store,
        publish_journal,
        pending_deltas,
    ) in &mut map_query
    {
        if config.save_dir.is_none() {
            trace!("save_dirty_chunks_debounced: no save_dir for {map_id:?}, skipping");
            continue;
        }

        let saved_chunks = enqueue_dirty_chunks(&mut instance, &mut pending_saves, None);
        let content_dirty: HashSet<IVec3> = instance.content_dirty_chunks.drain().collect();
        let entities_dirty = dirty_state.entities_dirty.remove(map_id);

        let spawn_points: Vec<Vec3> = respawn_query
            .iter()
            .filter(|(_, mid)| *mid == map_id)
            .map(|(pos, _)| pos.0)
            .collect();
        let meta = MapMeta {
            version: 1,
            seed: config.seed,
            generation_version: config.generation_version,
            spawn_points,
        };
        if matches!(map_id, MapInstanceId::Overworld)
            && remote_publish_config.enabled
            && (!content_dirty.is_empty() || entities_dirty)
        {
            let save_id = save_ids.allocate();
            let local_head = persistence::Store::load(
                &local_head_store
                    .expect("remote publishable Overworld map must have a local head store")
                    .0,
                &(),
            )
            .expect("local map head should load while allocating a remote publish revision");
            let local_revision_number = remote_publish::next_publish_revision_number(
                local_head.as_ref(),
                publish_journal
                    .expect("remote publishable Overworld map must have a publish journal"),
                pending_deltas
                    .expect("remote publishable Overworld map must have pending publish deltas"),
                &pending_publish,
            );
            let map_entities = by_map.get(map_id).cloned().unwrap_or_default();
            pending_publish.0.insert(
                save_id,
                ServerMapPublishDraft {
                    local_revision_number,
                    meta: nostr_map_persistence::PayloadSlotState::Present(meta.clone()),
                    chunks: saved_chunks
                        .iter()
                        .filter(|(pos, _)| content_dirty.contains(pos))
                        .cloned()
                        .map(|(pos, envelope)| {
                            (
                                pos,
                                nostr_map_persistence::PayloadSlotState::Present(envelope),
                            )
                        })
                        .collect(),
                    chunk_entities: Vec::new(),
                    map_entities: map_entities_publish_slot(entities_dirty, &map_entities),
                },
            );
            trace!(
                ?map_entity,
                ?save_id,
                "queued overworld publish draft after dirty save"
            );
            meta_ops.spawn_save_with_id(&meta_store.0, save_id, (), meta);
        } else {
            meta_ops.spawn_save(&meta_store.0, (), meta);
        }

        if let Some(entities) = by_map.get(map_id) {
            entity_ops.spawn_save(&entity_store.0, (), entities.clone());
        }
    }

    dirty_state.is_dirty = false;
    dirty_state.first_dirty_time = None;
}

/// Selects the publish slot for map-level entities.
///
/// `Absent` when the map's entities did not change this cycle, so chunk-only edits do not
/// re-upload the entity payload; `Present`/`Empty` carries the current entities when they did.
fn map_entities_publish_slot(
    entities_dirty: bool,
    entities: &[SavedEntity],
) -> nostr_map_persistence::PayloadSlotState<Vec<SavedEntity>> {
    if !entities_dirty {
        nostr_map_persistence::PayloadSlotState::Absent
    } else if entities.is_empty() {
        nostr_map_persistence::PayloadSlotState::Empty
    } else {
        nostr_map_persistence::PayloadSlotState::Present(entities.to_vec())
    }
}

/// Drain dirty chunks from an instance into the `PendingSaves` queue.
fn enqueue_dirty_chunks(
    instance: &mut VoxelMapInstance,
    pending_saves: &mut PendingSaves,
    save_id: Option<persistence::SaveOpId>,
) -> Vec<(IVec3, ChunkFileEnvelope)> {
    let chunk_size = instance.chunk_size;
    let dirty: Vec<IVec3> = instance.dirty_chunks.drain().collect();
    let mut saved = Vec::new();
    for chunk_pos in dirty {
        if let Some(chunk_data) = instance.get_chunk_data(chunk_pos) {
            let envelope = ChunkFileEnvelope {
                version: CHUNK_SAVE_VERSION,
                chunk_size,
                data: chunk_data.clone(),
            };
            pending_saves.queue.push_back(lifecycle::PendingSave {
                position: chunk_pos,
                envelope: envelope.clone(),
                save_id,
            });
            saved.push((chunk_pos, envelope));
        }
    }
    saved
}

/// Flush all dirty chunks through the store. Blocks until complete.
/// Used during shutdown where we must guarantee persistence before exit.
fn save_dirty_chunks_flush(
    instance: &mut VoxelMapInstance,
    chunk_store: &StoreBackend<IVec3, ChunkFileEnvelope, FsChunkStore>,
    chunk_ops: &mut PendingStoreOps<IVec3, ChunkFileEnvelope>,
) {
    let chunk_size = instance.chunk_size;
    let dirty: Vec<IVec3> = instance.dirty_chunks.drain().collect();
    for chunk_pos in dirty {
        if let Some(chunk_data) = instance.get_chunk_data(chunk_pos) {
            let envelope = ChunkFileEnvelope {
                version: CHUNK_SAVE_VERSION,
                chunk_size,
                data: chunk_data.clone(),
            };
            chunk_ops.spawn_save(&chunk_store.0, chunk_pos, envelope);
        }
    }
    chunk_ops.flush();
    log_save_errors(chunk_ops, "chunk data");
}

pub fn save_world_on_shutdown(
    mut exit_reader: MessageReader<AppExit>,
    mut map_query: Query<(
        &mut VoxelMapInstance,
        &VoxelMapConfig,
        &MapInstanceId,
        &StoreBackend<(), MapMeta, FsMapMetaStore>,
        &mut PendingStoreOps<(), MapMeta>,
        &StoreBackend<(), Vec<SavedEntity>, FsMapEntitiesStore>,
        &mut PendingStoreOps<(), Vec<SavedEntity>>,
        &StoreBackend<IVec3, ChunkFileEnvelope, FsChunkStore>,
        &mut PendingStoreOps<IVec3, ChunkFileEnvelope>,
    )>,
    dirty_state: Res<WorldDirtyState>,
    entity_query: Query<(
        &MapSaveTarget,
        &MapInstanceId,
        &Position,
        Option<&RespawnPoint>,
    )>,
    respawn_query: Query<(&Position, &MapInstanceId), With<RespawnPoint>>,
) {
    if exit_reader.is_empty() {
        return;
    }
    exit_reader.clear();

    let by_map = collect_entities_by_map(&entity_query);

    // Dirty chunks only need saving when edits occurred
    if dirty_state.is_dirty {
        for (mut instance, config, _, _, _, _, _, chunk_store, mut chunk_ops) in &mut map_query {
            if config.save_dir.is_none() {
                continue;
            }
            save_dirty_chunks_flush(&mut instance, chunk_store, &mut chunk_ops);
        }
    }

    // Meta and entities are always saved — their state is independent of voxel edits
    for (_, config, map_id, meta_store, mut meta_ops, entity_store, mut entity_ops, _, _) in
        &mut map_query
    {
        if config.save_dir.is_none() {
            continue;
        }
        let spawn_points: Vec<Vec3> = respawn_query
            .iter()
            .filter(|(_, mid)| *mid == map_id)
            .map(|(pos, _)| pos.0)
            .collect();
        let meta = MapMeta {
            version: 1,
            seed: config.seed,
            generation_version: config.generation_version,
            spawn_points,
        };
        meta_ops.spawn_save(&meta_store.0, (), meta);
        meta_ops.flush();
        log_save_errors(&mut meta_ops, "map metadata");

        if let Some(entities) = by_map.get(map_id) {
            entity_ops.spawn_save(&entity_store.0, (), entities.clone());
        }
        entity_ops.flush();
        log_save_errors(&mut entity_ops, "map entities");
    }

    info!("World saved on shutdown");
}

/// Collect all persistable entities grouped by map instance.
fn collect_entities_by_map(
    entity_query: &Query<(
        &MapSaveTarget,
        &MapInstanceId,
        &Position,
        Option<&RespawnPoint>,
    )>,
) -> HashMap<MapInstanceId, Vec<SavedEntity>> {
    let mut by_map: HashMap<MapInstanceId, Vec<SavedEntity>> = HashMap::new();

    for (_marker, map_id, position, respawn) in entity_query.iter() {
        let kind = if respawn.is_some() {
            SavedEntityKind::RespawnPoint
        } else {
            debug_assert!(
                false,
                "Entity with MapSaveTarget has no recognized SavedEntityKind"
            );
            continue;
        };

        by_map.entry(map_id.clone()).or_default().push(SavedEntity {
            kind,
            position: position.0,
        });
    }

    by_map
}

/// Spawn map entities (respawn points, etc.) from loaded data.
fn spawn_saved_entities(commands: &mut Commands, map_id: &MapInstanceId, entities: &[SavedEntity]) {
    for saved in entities {
        match saved.kind {
            SavedEntityKind::RespawnPoint => {
                commands.spawn((RespawnPoint, Position(saved.position), map_id.clone()));
            }
        }
    }
}

/// Async load of map entities. Kicks off load on first frame, polls for completion.
fn poll_map_entities(
    mut commands: Commands,
    mut query: Query<(
        Entity,
        &MapInstanceId,
        &mut MapLoadState,
        &StoreBackend<(), Vec<SavedEntity>, FsMapEntitiesStore>,
        &mut PendingStoreOps<(), Vec<SavedEntity>>,
    )>,
    mut save_completed: MessageWriter<remote_publish::MapPayloadSaveCompleted>,
) {
    for (entity, map_id, mut state, store, mut ops) in &mut query {
        ops.poll();
        log_save_errors(&mut ops, "map entities");
        for completion in ops.completed_saves.drain(..) {
            if let Some(save_id) = completion.id {
                save_completed.write(remote_publish::MapPayloadSaveCompleted {
                    map_entity: entity,
                    save_id,
                    key: remote_publish::MapPayloadSaveKey::MapEntities,
                });
            } else {
                trace!(
                    ?map_id,
                    "map entities save completion has no publish save id"
                );
            }
        }

        if *state != MapLoadState::AwaitingEntities {
            continue;
        }

        // First frame: kick off the load
        if ops.completed_loads.is_empty() && !ops.has_pending() && ops.load_errors.is_empty() {
            ops.spawn_load(&store.0, ());
            return;
        }

        if let Some((_, entities_opt)) = ops.completed_loads.pop() {
            if let Some(entities) = &entities_opt {
                info!("Loaded {} entities for {map_id:?}", entities.len());
                spawn_saved_entities(&mut commands, map_id, entities);
            }
            *state = MapLoadState::Ready;
        }

        for (_, e) in ops.load_errors.drain(..) {
            warn!("Failed to load entities for {map_id:?}: {e}, proceeding with none");
            *state = MapLoadState::Ready;
        }
    }
}

fn on_map_instance_id_added(
    trigger: On<Add, MapInstanceId>,
    mut commands: Commands,
    map_ids: Query<&MapInstanceId>,
    mut room_registry: ResMut<RoomRegistry>,
) {
    let entity = trigger.entity;
    let map_id = map_ids
        .get(entity)
        .expect("Entity with MapInstanceId trigger must have MapInstanceId");
    let room = room_registry.get_or_create(map_id, &mut commands);
    commands.entity(entity).try_insert(NetworkVisibility);
    commands.trigger(RoomEvent {
        room,
        target: RoomTarget::AddEntity(entity),
    });
}

/// Poll async chunk entity store operations each frame.
fn poll_chunk_entity_ops(
    mut query: Query<(Entity, &mut PendingStoreOps<IVec3, Vec<WorldObjectSpawn>>)>,
    mut save_completed: MessageWriter<remote_publish::MapPayloadSaveCompleted>,
) {
    for (map_entity, mut ops) in &mut query {
        ops.poll();
        log_save_errors(&mut ops, "chunk entities");
        for completion in ops.completed_saves.drain(..) {
            if let Some(save_id) = completion.id {
                save_completed.write(remote_publish::MapPayloadSaveCompleted {
                    map_entity,
                    save_id,
                    key: remote_publish::MapPayloadSaveKey::ChunkEntities(completion.key),
                });
            } else {
                trace!(
                    ?map_entity,
                    "chunk entity save completion has no publish save id"
                );
            }
        }
    }
}

impl Plugin for ServerMapPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(lightyear::prelude::RoomPlugin)
            .add_plugins(VoxelPlugin)
            .insert_resource(voxel_map_engine::ChunkGenerationEnabled)
            .init_resource::<MapRegistry>()
            .init_resource::<RoomRegistry>()
            .init_resource::<WorldDirtyState>()
            .init_resource::<PendingVoxelBroadcasts>()
            .init_resource::<PendingMapPreflights>()
            .init_resource::<RemoteMapPersistenceConfig>()
            .init_resource::<remote_publish::RemoteMapPublishConfig>()
            .init_resource::<WorldSavePath>()
            .init_resource::<SaveOpIdAllocator>()
            .init_resource::<remote_publish::PendingPublishBySaveId>()
            .init_resource::<remote_publish::RemoteMapPublishWorker>()
            .add_message::<remote_publish::MapPayloadSaveCompleted>()
            .add_message::<remote_publish::MapPayloadSaveFailed>()
            .add_systems(
                OnEnter(AppState::Ready),
                (configure_remote_map_read_context, init_overworld_entity).chain(),
            )
            .add_systems(
                Update,
                (
                    spawn_map_preflight_tasks.run_if(in_state(AppState::Ready)),
                    poll_map_persistence_preflight.run_if(in_state(AppState::Ready)),
                    poll_map_meta.run_if(in_state(AppState::Ready)),
                    poll_map_entities.run_if(in_state(AppState::Ready)),
                    commit_ready_map_preflights.run_if(in_state(AppState::Ready)),
                    (
                        (
                            handle_voxel_edit_requests,
                            handle_voxel_brush_edit_requests,
                            handle_voxel_concrete_edit_requests,
                        ),
                        flush_voxel_broadcasts,
                    )
                        .chain(),
                    handle_world_object_placement_requests.run_if(
                        resource_exists::<WorldObjectDefRegistry>
                            .and(resource_exists::<VoxModelRegistry>),
                    ),
                    handle_world_object_delete_requests,
                    handle_world_object_move_requests,
                    handle_world_object_rotate_requests,
                    push_chunks_to_clients,
                    save_dirty_chunks_debounced,
                    handle_map_switch_requests,
                    homebase_publication::handle_homebase_attestation_requests,
                    crate::transition::complete_map_transition,
                    protocol::attach_chunk_colliders,
                    crate::chunk_entities::spawn_chunk_entities
                        .after(lifecycle::poll_chunk_tasks)
                        .run_if(
                            resource_exists::<WorldObjectDefRegistry>
                                .and(resource_exists::<VoxModelRegistry>),
                        ),
                    crate::chunk_entities::evict_chunk_entities
                        .after(lifecycle::despawn_out_of_range_chunks),
                    poll_chunk_entity_ops,
                    crate::chunk_entities::save_chunk_entities_periodic,
                ),
            )
            .add_systems(
                Update,
                (
                    remote_publish::normalize_chunk_save_completions,
                    remote_publish::handle_completed_map_payload_save_for_publish,
                    remote_publish::prepare_pending_remote_publish_journal_entries
                        .run_if(remote_publish::remote_map_publish_enabled),
                    remote_publish::poll_remote_publish_journal
                        .run_if(remote_publish::remote_map_publish_enabled),
                ),
            )
            .add_systems(
                Last,
                (
                    save_world_on_shutdown,
                    crate::chunk_entities::save_all_chunk_entities_on_exit,
                ),
            )
            .add_observer(on_map_instance_id_added);
    }
}

/// Resolves which map entity a client's character is on.
pub(crate) fn resolve_player_map(
    client_entity: Entity,
    controlled_query: &Query<(&ControlledBy, &MapInstanceId), With<CharacterMarker>>,
    map_registry: &MapRegistry,
) -> Option<(Entity, MapInstanceId)> {
    let (_, player_map_id) = controlled_query
        .iter()
        .find(|(ctrl, _)| ctrl.owner == client_entity)?;
    Some((map_registry.get(player_map_id), player_map_id.clone()))
}

/// Validates the edit and sends a reject if invalid. Returns `true` if edit is valid.
fn is_edit_valid(
    request: &VoxelEditRequest,
    map_entity: Entity,
    map_id: &MapInstanceId,
    client_entity: Entity,
    voxel_world: &VoxelWorld,
    reject_senders: &mut Query<&mut MessageSender<VoxelEditReject>>,
) -> bool {
    if validate_voxel_edit(request, map_entity, voxel_world) {
        return true;
    }
    let current_voxel = voxel_world.get_voxel(map_entity, request.position);
    if let Ok(mut sender) = reject_senders.get_mut(client_entity) {
        sender.send::<VoxelChannel>(VoxelEditReject {
            sequence: request.sequence,
            map_id: map_id.clone(),
            position: request.position,
            correct_voxel: current_voxel.into(),
        });
    }
    false
}

/// Applies the voxel edit and marks the world dirty.
fn apply_voxel_edit(
    request: &VoxelEditRequest,
    map_entity: Entity,
    voxel_world: &mut VoxelWorld,
    dirty_state: &mut WorldDirtyState,
    time: &Time,
) {
    voxel_world.set_voxel(
        map_entity,
        request.position,
        WorldVoxel::from(request.voxel),
    );
    dirty_state.mark_dirty(time.elapsed_secs_f64());
}

/// Sends an edit acknowledgment to the originating client.
fn send_edit_ack(
    client_entity: Entity,
    sequence: u32,
    map_id: MapInstanceId,
    ack_senders: &mut Query<&mut MessageSender<VoxelEditAck>>,
) {
    if let Ok(mut sender) = ack_senders.get_mut(client_entity) {
        sender.send::<VoxelChannel>(VoxelEditAck {
            sequence,
            map_id,
            changes: Vec::new(),
        });
    } else {
        warn!("send_edit_ack: no ack sender for {client_entity:?}");
    }
}

/// Queues a voxel edit for batched broadcast.
fn queue_edit_broadcast(
    edit: PendingVoxelEdit,
    chunk_size: u32,
    pending: &mut PendingVoxelBroadcasts,
) {
    let chunk_pos = voxel_map_engine::prelude::voxel_to_chunk_pos(edit.position, chunk_size);
    let map_id = edit.map_id.clone();
    pending
        .per_chunk
        .entry((map_id, chunk_pos))
        .or_default()
        .push(edit);
}

#[allow(clippy::too_many_arguments)]
pub fn handle_world_object_placement_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<WorldObjectPlacementRequest>)>,
    mut ack_senders: Query<&mut MessageSender<WorldObjectPlacementAck>>,
    mut reject_senders: Query<&mut MessageSender<WorldObjectPlacementReject>>,
    controlled_query: Query<(&ControlledBy, &MapInstanceId), With<CharacterMarker>>,
    map_registry: Res<MapRegistry>,
    map_query: Query<(&VoxelMapInstance, &MapDimensions)>,
    defs: Res<WorldObjectDefRegistry>,
    type_registry: Res<AppTypeRegistry>,
    vox_registry: Res<VoxModelRegistry>,
    vox_assets: Res<Assets<VoxModelAsset>>,
    meshes: Res<Assets<Mesh>>,
    mut dirty_state: ResMut<WorldDirtyState>,
    time: Res<Time>,
    mut commands: Commands,
) {
    for (client_entity, mut receiver) in &mut receivers {
        for request in receiver.receive() {
            let Some((map_entity, map_id)) =
                resolve_player_map(client_entity, &controlled_query, &map_registry)
            else {
                trace!(
                    "handle_world_object_placement_requests: no character for client {client_entity:?}"
                );
                send_placement_reject(
                    client_entity,
                    request.sequence,
                    WorldObjectPlacementRejectReason::NoControlledCharacter,
                    &mut reject_senders,
                );
                continue;
            };

            let (instance, dimensions) = map_query
                .get(map_entity)
                .expect("resolved map entity must have VoxelMapInstance and MapDimensions");

            match validate_world_object_placement(&request, instance, dimensions, &defs) {
                Ok((def, final_position, _)) => {
                    crate::world_object::spawn_placed_world_object(
                        &mut commands,
                        request.object_id.clone(),
                        def,
                        request.base_position,
                        map_entity,
                        map_id.clone(),
                        dimensions.chunk_size,
                        &type_registry,
                        &vox_registry,
                        &vox_assets,
                        &meshes,
                    );
                    send_placement_ack(
                        client_entity,
                        WorldObjectPlacementAck {
                            sequence: request.sequence,
                            object_id: request.object_id,
                            final_position,
                        },
                        &mut ack_senders,
                    );
                    dirty_state.mark_entities_dirty(&map_id, time.elapsed_secs_f64());
                }
                Err(reason) => {
                    trace!(
                        "handle_world_object_placement_requests: rejecting sequence {}: {:?}",
                        request.sequence,
                        reason
                    );
                    send_placement_reject(
                        client_entity,
                        request.sequence,
                        reason,
                        &mut reject_senders,
                    );
                    continue;
                }
            }
        }
    }
}

/// Validates a world-object placement against object definitions and loaded chunk state.
pub fn validate_world_object_placement<'a>(
    request: &WorldObjectPlacementRequest,
    instance: &VoxelMapInstance,
    dimensions: &MapDimensions,
    defs: &'a WorldObjectDefRegistry,
) -> Result<(&'a WorldObjectDef, Vec3, IVec3), WorldObjectPlacementRejectReason> {
    if !request.base_position.is_finite() {
        return Err(WorldObjectPlacementRejectReason::NonFinitePosition);
    }

    let Some(def) = defs.get(&request.object_id) else {
        return Err(WorldObjectPlacementRejectReason::UnknownObject);
    };

    let final_position =
        crate::world_object::final_placed_world_object_position(def, request.base_position);
    if !final_position.is_finite() {
        return Err(WorldObjectPlacementRejectReason::NonFinitePosition);
    }

    let chunk_pos =
        crate::chunk_entities::chunk_pos_for_world_position(final_position, dimensions.chunk_size);

    if !placement_chunk_in_bounds(chunk_pos, dimensions) {
        return Err(WorldObjectPlacementRejectReason::OutOfBounds);
    }

    let column = voxel_map_engine::prelude::chunk_to_column(chunk_pos);
    if !instance.chunk_levels.contains_key(&column) || instance.get_chunk_data(chunk_pos).is_none()
    {
        return Err(WorldObjectPlacementRejectReason::ChunkUnavailable);
    }

    Ok((def, final_position, chunk_pos))
}

/// Validated data needed to delete a world object and persist its chunk.
#[derive(Debug, PartialEq, Eq)]
pub struct ValidatedWorldObjectDelete {
    pub map_entity: Entity,
    pub chunk_pos: IVec3,
}

/// Validates a world-object delete target against player map and loaded chunk state.
pub fn validate_world_object_delete(
    target: Entity,
    player_map_entity: Entity,
    player_map_id: &MapInstanceId,
    target_exists: &Query<Entity>,
    object_query: &Query<(&WorldObjectId, &MapInstanceId, &ChunkEntityRef)>,
    map_query: &Query<&VoxelMapInstance>,
) -> Result<ValidatedWorldObjectDelete, WorldObjectEditRejectReason> {
    let Ok((_id, object_map_id, chunk_ref)) = object_query.get(target) else {
        if target_exists.get(target).is_ok() {
            return Err(WorldObjectEditRejectReason::NotWorldObject);
        }
        return Err(WorldObjectEditRejectReason::MissingTarget);
    };
    if object_map_id != player_map_id || chunk_ref.map_entity != player_map_entity {
        return Err(WorldObjectEditRejectReason::ForeignMap);
    }
    let instance = map_query
        .get(player_map_entity)
        .expect("resolved map entity must have VoxelMapInstance");
    let column = voxel_map_engine::prelude::chunk_to_column(chunk_ref.chunk_pos);
    if !instance.chunk_levels.contains_key(&column)
        || instance.get_chunk_data(chunk_ref.chunk_pos).is_none()
    {
        return Err(WorldObjectEditRejectReason::ChunkUnavailable);
    }
    Ok(ValidatedWorldObjectDelete {
        map_entity: player_map_entity,
        chunk_pos: chunk_ref.chunk_pos,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn handle_world_object_delete_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<WorldObjectDeleteRequest>)>,
    mut ack_senders: Query<&mut MessageSender<WorldObjectDeleteAck>>,
    mut reject_senders: Query<&mut MessageSender<WorldObjectEditReject>>,
    controlled_query: Query<(&ControlledBy, &MapInstanceId), With<CharacterMarker>>,
    map_registry: Res<MapRegistry>,
    map_query: Query<&VoxelMapInstance>,
    target_exists: Query<Entity>,
    object_query: Query<(&WorldObjectId, &MapInstanceId, &ChunkEntityRef)>,
    entity_save_query: Query<(
        Entity,
        &ChunkEntityRef,
        &WorldObjectId,
        &Position,
        Option<&ActiveTransformation>,
        Option<&protocol::Health>,
        Option<&Rotation>,
    )>,
    mut store_query: Query<(
        &StoreBackend<IVec3, Vec<WorldObjectSpawn>, FsChunkEntitiesStore>,
        &mut PendingStoreOps<IVec3, Vec<WorldObjectSpawn>>,
    )>,
    mut dirty_state: ResMut<WorldDirtyState>,
    time: Res<Time>,
    mut commands: Commands,
) {
    for (client_entity, mut receiver) in &mut receivers {
        for request in receiver.receive() {
            let Some((map_entity, map_id)) =
                resolve_player_map(client_entity, &controlled_query, &map_registry)
            else {
                send_world_object_edit_reject(
                    client_entity,
                    request.sequence,
                    WorldObjectEditRejectReason::NoControlledCharacter,
                    &mut reject_senders,
                );
                continue;
            };
            match validate_world_object_delete(
                request.target,
                map_entity,
                &map_id,
                &target_exists,
                &object_query,
                &map_query,
            ) {
                Ok(validated) => {
                    commands.entity(request.target).despawn();
                    crate::chunk_entities::save_chunk_entities_now_or_queue(
                        validated.map_entity,
                        validated.chunk_pos,
                        Some(request.target),
                        None,
                        &entity_save_query,
                        &mut store_query,
                    );
                    send_world_object_delete_ack(
                        client_entity,
                        WorldObjectDeleteAck {
                            sequence: request.sequence,
                            target: request.target,
                        },
                        &mut ack_senders,
                    );
                    dirty_state.mark_entities_dirty(&map_id, time.elapsed_secs_f64());
                }
                Err(reason) => {
                    send_world_object_edit_reject(
                        client_entity,
                        request.sequence,
                        reason,
                        &mut reject_senders,
                    );
                }
            }
        }
    }
}

/// Validated data needed to move a world object and persist its chunk.
#[derive(Debug, PartialEq)]
pub struct ValidatedWorldObjectMove {
    pub map_entity: Entity,
    pub old_chunk_pos: IVec3,
    pub new_chunk_pos: IVec3,
    pub final_position: Vec3,
}

/// Validates a world-object move against player map and loaded chunk state.
pub fn validate_world_object_move(
    request: &WorldObjectMoveRequest,
    player_map_entity: Entity,
    player_map_id: &MapInstanceId,
    object_query: &Query<(&WorldObjectId, &MapInstanceId, &ChunkEntityRef)>,
    map_query: &Query<(&VoxelMapInstance, &MapDimensions)>,
) -> Result<ValidatedWorldObjectMove, WorldObjectEditRejectReason> {
    if !request.final_position.is_finite() {
        return Err(WorldObjectEditRejectReason::NonFinitePosition);
    }
    let Ok((_id, object_map_id, chunk_ref)) = object_query.get(request.target) else {
        return Err(WorldObjectEditRejectReason::MissingTarget);
    };
    if object_map_id != player_map_id || chunk_ref.map_entity != player_map_entity {
        return Err(WorldObjectEditRejectReason::ForeignMap);
    }
    let (instance, dimensions) = map_query
        .get(player_map_entity)
        .expect("resolved map entity must have VoxelMapInstance and MapDimensions");
    let new_chunk_pos = crate::chunk_entities::chunk_pos_for_world_position(
        request.final_position,
        dimensions.chunk_size,
    );
    if !placement_chunk_in_bounds(new_chunk_pos, dimensions) {
        return Err(WorldObjectEditRejectReason::OutOfBounds);
    }
    let column = voxel_map_engine::prelude::chunk_to_column(new_chunk_pos);
    if !instance.chunk_levels.contains_key(&column)
        || instance.get_chunk_data(new_chunk_pos).is_none()
    {
        return Err(if new_chunk_pos == chunk_ref.chunk_pos {
            WorldObjectEditRejectReason::ChunkUnavailable
        } else {
            WorldObjectEditRejectReason::DestinationChunkUnavailable
        });
    }
    Ok(ValidatedWorldObjectMove {
        map_entity: player_map_entity,
        old_chunk_pos: chunk_ref.chunk_pos,
        new_chunk_pos,
        final_position: request.final_position,
    })
}

/// Applies a validated world-object move to the target entity.
pub fn apply_world_object_move(
    entity: Entity,
    validated: &ValidatedWorldObjectMove,
    commands: &mut Commands,
) {
    commands
        .entity(entity)
        .insert(Position(validated.final_position));
    if validated.old_chunk_pos != validated.new_chunk_pos {
        commands.entity(entity).insert(ChunkEntityRef {
            map_entity: validated.map_entity,
            chunk_pos: validated.new_chunk_pos,
        });
    }
}

#[allow(clippy::too_many_arguments)]
pub fn handle_world_object_move_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<WorldObjectMoveRequest>)>,
    mut ack_senders: Query<&mut MessageSender<WorldObjectMoveAck>>,
    mut reject_senders: Query<&mut MessageSender<WorldObjectEditReject>>,
    controlled_query: Query<(&ControlledBy, &MapInstanceId), With<CharacterMarker>>,
    map_registry: Res<MapRegistry>,
    map_query: Query<(&VoxelMapInstance, &MapDimensions)>,
    object_query: Query<(&WorldObjectId, &MapInstanceId, &ChunkEntityRef)>,
    entity_save_query: Query<(
        Entity,
        &ChunkEntityRef,
        &WorldObjectId,
        &Position,
        Option<&ActiveTransformation>,
        Option<&protocol::Health>,
        Option<&Rotation>,
    )>,
    mut store_query: Query<(
        &StoreBackend<IVec3, Vec<WorldObjectSpawn>, FsChunkEntitiesStore>,
        &mut PendingStoreOps<IVec3, Vec<WorldObjectSpawn>>,
    )>,
    mut dirty_state: ResMut<WorldDirtyState>,
    time: Res<Time>,
    mut commands: Commands,
) {
    for (client_entity, mut receiver) in &mut receivers {
        for request in receiver.receive() {
            let Some((map_entity, map_id)) =
                resolve_player_map(client_entity, &controlled_query, &map_registry)
            else {
                send_world_object_edit_reject(
                    client_entity,
                    request.sequence,
                    WorldObjectEditRejectReason::NoControlledCharacter,
                    &mut reject_senders,
                );
                continue;
            };
            match validate_world_object_move(
                &request,
                map_entity,
                &map_id,
                &object_query,
                &map_query,
            ) {
                Ok(validated) => {
                    apply_world_object_move(request.target, &validated, &mut commands);
                    crate::chunk_entities::queue_world_object_move_persistence(
                        validated.map_entity,
                        validated.old_chunk_pos,
                        validated.new_chunk_pos,
                        request.target,
                        validated.final_position,
                        &entity_save_query,
                        &mut store_query,
                    );
                    send_world_object_move_ack(
                        client_entity,
                        WorldObjectMoveAck {
                            sequence: request.sequence,
                            target: request.target,
                            final_position: validated.final_position,
                        },
                        &mut ack_senders,
                    );
                    dirty_state.mark_entities_dirty(&map_id, time.elapsed_secs_f64());
                }
                Err(reason) => {
                    send_world_object_edit_reject(
                        client_entity,
                        request.sequence,
                        reason,
                        &mut reject_senders,
                    );
                }
            }
        }
    }
}

/// Validates a world-object rotation against player map and loaded chunk state.
pub fn validate_world_object_rotation(
    request: &WorldObjectRotateRequest,
    player_map_entity: Entity,
    player_map_id: &MapInstanceId,
    object_query: &Query<(&WorldObjectId, &MapInstanceId, &ChunkEntityRef)>,
    map_query: &Query<&VoxelMapInstance>,
) -> Result<Quat, WorldObjectEditRejectReason> {
    if !request.rotation.is_finite() || request.rotation.length_squared() <= f32::EPSILON {
        return Err(WorldObjectEditRejectReason::InvalidRotation);
    }
    let Ok((_id, object_map_id, chunk_ref)) = object_query.get(request.target) else {
        return Err(WorldObjectEditRejectReason::MissingTarget);
    };
    if object_map_id != player_map_id || chunk_ref.map_entity != player_map_entity {
        return Err(WorldObjectEditRejectReason::ForeignMap);
    }
    let instance = map_query
        .get(player_map_entity)
        .expect("resolved map entity must have VoxelMapInstance");
    let column = voxel_map_engine::prelude::chunk_to_column(chunk_ref.chunk_pos);
    if !instance.chunk_levels.contains_key(&column)
        || instance.get_chunk_data(chunk_ref.chunk_pos).is_none()
    {
        return Err(WorldObjectEditRejectReason::ChunkUnavailable);
    }
    Ok(request.rotation.normalize())
}

#[allow(clippy::too_many_arguments)]
pub fn handle_world_object_rotate_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<WorldObjectRotateRequest>)>,
    mut ack_senders: Query<&mut MessageSender<WorldObjectRotateAck>>,
    mut reject_senders: Query<&mut MessageSender<WorldObjectEditReject>>,
    controlled_query: Query<(&ControlledBy, &MapInstanceId), With<CharacterMarker>>,
    map_registry: Res<MapRegistry>,
    map_query: Query<&VoxelMapInstance>,
    object_query: Query<(&WorldObjectId, &MapInstanceId, &ChunkEntityRef)>,
    entity_save_query: Query<(
        Entity,
        &ChunkEntityRef,
        &WorldObjectId,
        &Position,
        Option<&ActiveTransformation>,
        Option<&protocol::Health>,
        Option<&Rotation>,
    )>,
    mut store_query: Query<(
        &StoreBackend<IVec3, Vec<WorldObjectSpawn>, FsChunkEntitiesStore>,
        &mut PendingStoreOps<IVec3, Vec<WorldObjectSpawn>>,
    )>,
    mut commands: Commands,
) {
    for (client_entity, mut receiver) in &mut receivers {
        for request in receiver.receive() {
            let Some((map_entity, map_id)) =
                resolve_player_map(client_entity, &controlled_query, &map_registry)
            else {
                send_world_object_edit_reject(
                    client_entity,
                    request.sequence,
                    WorldObjectEditRejectReason::NoControlledCharacter,
                    &mut reject_senders,
                );
                continue;
            };
            match validate_world_object_rotation(
                &request,
                map_entity,
                &map_id,
                &object_query,
                &map_query,
            ) {
                Ok(rotation) => {
                    let (_, _, chunk_ref) = object_query
                        .get(request.target)
                        .expect("validated rotate target must remain queryable");
                    commands.entity(request.target).insert(Rotation(rotation));
                    crate::chunk_entities::save_chunk_entities_now_or_queue(
                        map_entity,
                        chunk_ref.chunk_pos,
                        None,
                        Some(crate::chunk_entities::ChunkEntitySaveOverride {
                            entity: request.target,
                            position: None,
                            chunk_pos: None,
                            rotation: Some(rotation),
                        }),
                        &entity_save_query,
                        &mut store_query,
                    );
                    send_world_object_rotate_ack(
                        client_entity,
                        WorldObjectRotateAck {
                            sequence: request.sequence,
                            target: request.target,
                            rotation,
                        },
                        &mut ack_senders,
                    );
                }
                Err(reason) => {
                    send_world_object_edit_reject(
                        client_entity,
                        request.sequence,
                        reason,
                        &mut reject_senders,
                    );
                }
            }
        }
    }
}

/// Returns whether a candidate chunk is within a map's configured placement bounds.
fn placement_chunk_in_bounds(chunk_pos: IVec3, dimensions: &MapDimensions) -> bool {
    (dimensions.column_y_range.0..dimensions.column_y_range.1).contains(&chunk_pos.y)
        && match dimensions.bounds {
            Some(bounds) => {
                chunk_pos.x.abs() < bounds.x
                    && chunk_pos.y.abs() < bounds.y
                    && chunk_pos.z.abs() < bounds.z
            }
            None => true,
        }
}

/// Sends a placement rejection to a client if its reject sender exists.
fn send_placement_reject(
    client_entity: Entity,
    sequence: u32,
    reason: WorldObjectPlacementRejectReason,
    reject_senders: &mut Query<&mut MessageSender<WorldObjectPlacementReject>>,
) {
    let Ok(mut sender) = reject_senders.get_mut(client_entity) else {
        trace!("send_placement_reject: no reject sender for {client_entity:?}");
        return;
    };
    sender.send::<WorldObjectPlacementChannel>(WorldObjectPlacementReject { sequence, reason });
}

/// Sends a placement acknowledgment to a client if its acknowledgment sender exists.
fn send_placement_ack(
    client_entity: Entity,
    ack: WorldObjectPlacementAck,
    ack_senders: &mut Query<&mut MessageSender<WorldObjectPlacementAck>>,
) {
    let Ok(mut sender) = ack_senders.get_mut(client_entity) else {
        trace!("send_placement_ack: no ack sender for {client_entity:?}");
        return;
    };
    sender.send::<WorldObjectPlacementChannel>(ack);
}

/// Sends a world-object edit rejection to a client if its reject sender exists.
fn send_world_object_edit_reject(
    client_entity: Entity,
    sequence: u32,
    reason: WorldObjectEditRejectReason,
    reject_senders: &mut Query<&mut MessageSender<WorldObjectEditReject>>,
) {
    let Ok(mut sender) = reject_senders.get_mut(client_entity) else {
        trace!("send_world_object_edit_reject: no reject sender for {client_entity:?}");
        return;
    };
    sender.send::<WorldObjectEditChannel>(WorldObjectEditReject { sequence, reason });
}

/// Sends a world-object delete acknowledgment to a client if its acknowledgment sender exists.
fn send_world_object_delete_ack(
    client_entity: Entity,
    ack: WorldObjectDeleteAck,
    ack_senders: &mut Query<&mut MessageSender<WorldObjectDeleteAck>>,
) {
    let Ok(mut sender) = ack_senders.get_mut(client_entity) else {
        trace!("send_world_object_delete_ack: no ack sender for {client_entity:?}");
        return;
    };
    sender.send::<WorldObjectEditChannel>(ack);
}

/// Sends a world-object move acknowledgment to a client if its acknowledgment sender exists.
fn send_world_object_move_ack(
    client_entity: Entity,
    ack: WorldObjectMoveAck,
    ack_senders: &mut Query<&mut MessageSender<WorldObjectMoveAck>>,
) {
    let Ok(mut sender) = ack_senders.get_mut(client_entity) else {
        trace!("send_world_object_move_ack: no ack sender for {client_entity:?}");
        return;
    };
    sender.send::<WorldObjectEditChannel>(ack);
}

/// Sends a world-object rotation acknowledgment to a client if its acknowledgment sender exists.
fn send_world_object_rotate_ack(
    client_entity: Entity,
    ack: WorldObjectRotateAck,
    ack_senders: &mut Query<&mut MessageSender<WorldObjectRotateAck>>,
) {
    let Ok(mut sender) = ack_senders.get_mut(client_entity) else {
        trace!("send_world_object_rotate_ack: no ack sender for {client_entity:?}");
        return;
    };
    sender.send::<WorldObjectEditChannel>(ack);
}

pub fn handle_voxel_edit_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<VoxelEditRequest>)>,
    mut ack_senders: Query<&mut MessageSender<VoxelEditAck>>,
    mut reject_senders: Query<&mut MessageSender<VoxelEditReject>>,
    mut pending_broadcasts: ResMut<PendingVoxelBroadcasts>,
    mut dirty_state: ResMut<WorldDirtyState>,
    time: Res<Time>,
    mut voxel_world: VoxelWorld,
    controlled_query: Query<(&ControlledBy, &MapInstanceId), With<CharacterMarker>>,
    map_registry: Res<MapRegistry>,
) {
    for (client_entity, mut receiver) in &mut receivers {
        for request in receiver.receive() {
            let Some((map_entity, player_map_id)) =
                resolve_player_map(client_entity, &controlled_query, &map_registry)
            else {
                trace!("handle_voxel_edit_requests: no character for client {client_entity:?}");
                continue;
            };

            if !is_edit_valid(
                &request,
                map_entity,
                &player_map_id,
                client_entity,
                &voxel_world,
                &mut reject_senders,
            ) {
                continue;
            }

            apply_voxel_edit(
                &request,
                map_entity,
                &mut voxel_world,
                &mut dirty_state,
                &time,
            );
            send_edit_ack(
                client_entity,
                request.sequence,
                player_map_id.clone(),
                &mut ack_senders,
            );
            let chunk_size = voxel_world
                .chunk_size(map_entity)
                .expect("map entity has VoxelMapInstance");
            queue_edit_broadcast(
                PendingVoxelEdit {
                    position: request.position,
                    voxel: request.voxel,
                    originator: client_entity,
                    map_id: player_map_id,
                },
                chunk_size,
                &mut pending_broadcasts,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn handle_voxel_brush_edit_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<VoxelBrushEditRequest>)>,
    mut ack_senders: Query<&mut MessageSender<VoxelEditAck>>,
    mut reject_senders: Query<&mut MessageSender<VoxelEditReject>>,
    mut pending_broadcasts: ResMut<PendingVoxelBroadcasts>,
    mut dirty_state: ResMut<WorldDirtyState>,
    time: Res<Time>,
    mut voxel_world: VoxelWorld,
    controlled_query: Query<(&ControlledBy, &MapInstanceId), With<CharacterMarker>>,
    map_registry: Res<MapRegistry>,
) {
    for (client_entity, mut receiver) in &mut receivers {
        for request in receiver.receive() {
            let Some((map_entity, player_map_id)) =
                resolve_player_map(client_entity, &controlled_query, &map_registry)
            else {
                trace!(
                    "handle_voxel_brush_edit_requests: no character for client {client_entity:?}"
                );
                continue;
            };
            if !request_map_matches_player(&request.map_id, &player_map_id) {
                warn!(
                    "handle_voxel_brush_edit_requests: rejecting map mismatch request={:?} player={:?}",
                    request.map_id, player_map_id
                );
                send_voxel_edit_reject(
                    client_entity,
                    request.sequence,
                    request.map_id.clone(),
                    request.anchor,
                    voxel_world.get_voxel(map_entity, request.anchor).into(),
                    &mut reject_senders,
                );
                continue;
            }

            if !validate_voxel_brush_edit(&request) {
                warn!(
                    "handle_voxel_brush_edit_requests: rejecting invalid brush request sequence {}",
                    request.sequence
                );
                send_voxel_edit_reject(
                    client_entity,
                    request.sequence,
                    player_map_id.clone(),
                    request.anchor,
                    voxel_world.get_voxel(map_entity, request.anchor).into(),
                    &mut reject_senders,
                );
                continue;
            }

            let changes = concrete_brush_changes(&request, map_entity, &voxel_world);
            if changes.is_empty() {
                trace!(
                    "handle_voxel_brush_edit_requests: sequence {} produced no concrete changes",
                    request.sequence
                );
                send_brush_edit_ack(
                    client_entity,
                    request.sequence,
                    player_map_id.clone(),
                    changes,
                    &mut ack_senders,
                );
                continue;
            }

            apply_voxel_changes(
                &changes,
                map_entity,
                &mut voxel_world,
                &mut dirty_state,
                &time,
            );
            send_brush_edit_ack(
                client_entity,
                request.sequence,
                player_map_id.clone(),
                changes.clone(),
                &mut ack_senders,
            );
            let chunk_size = voxel_world
                .chunk_size(map_entity)
                .expect("map entity has VoxelMapInstance");
            for change in changes {
                queue_edit_broadcast(
                    PendingVoxelEdit {
                        position: change.position,
                        voxel: change.voxel,
                        originator: client_entity,
                        map_id: player_map_id.clone(),
                    },
                    chunk_size,
                    &mut pending_broadcasts,
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn handle_voxel_concrete_edit_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<VoxelConcreteEditRequest>)>,
    mut ack_senders: Query<&mut MessageSender<VoxelEditAck>>,
    mut reject_senders: Query<&mut MessageSender<VoxelEditReject>>,
    mut pending_broadcasts: ResMut<PendingVoxelBroadcasts>,
    mut dirty_state: ResMut<WorldDirtyState>,
    time: Res<Time>,
    mut voxel_world: VoxelWorld,
    controlled_query: Query<(&ControlledBy, &MapInstanceId), With<CharacterMarker>>,
    map_registry: Res<MapRegistry>,
) {
    for (client_entity, mut receiver) in &mut receivers {
        for request in receiver.receive() {
            let Some((map_entity, player_map_id)) =
                resolve_player_map(client_entity, &controlled_query, &map_registry)
            else {
                trace!(
                    "handle_voxel_concrete_edit_requests: no character for client {client_entity:?}"
                );
                continue;
            };
            let reject_position = request
                .changes
                .first()
                .map(|change| change.position)
                .unwrap_or(IVec3::ZERO);
            if !request_map_matches_player(&request.map_id, &player_map_id) {
                warn!(
                    "handle_voxel_concrete_edit_requests: rejecting map mismatch request={:?} player={:?}",
                    request.map_id, player_map_id
                );
                send_voxel_edit_reject(
                    client_entity,
                    request.sequence,
                    request.map_id.clone(),
                    reject_position,
                    voxel_world.get_voxel(map_entity, reject_position).into(),
                    &mut reject_senders,
                );
                continue;
            }
            if !validate_voxel_concrete_edit(&request) {
                warn!(
                    "handle_voxel_concrete_edit_requests: rejecting invalid concrete request sequence {}",
                    request.sequence
                );
                send_voxel_edit_reject(
                    client_entity,
                    request.sequence,
                    player_map_id.clone(),
                    reject_position,
                    voxel_world.get_voxel(map_entity, reject_position).into(),
                    &mut reject_senders,
                );
                continue;
            }

            apply_voxel_changes(
                &request.changes,
                map_entity,
                &mut voxel_world,
                &mut dirty_state,
                &time,
            );
            send_brush_edit_ack(
                client_entity,
                request.sequence,
                player_map_id.clone(),
                request.changes.clone(),
                &mut ack_senders,
            );
            let chunk_size = voxel_world
                .chunk_size(map_entity)
                .expect("map entity has VoxelMapInstance");
            for change in request.changes {
                queue_edit_broadcast(
                    PendingVoxelEdit {
                        position: change.position,
                        voxel: change.voxel,
                        originator: client_entity,
                        map_id: player_map_id.clone(),
                    },
                    chunk_size,
                    &mut pending_broadcasts,
                );
            }
        }
    }
}

fn concrete_brush_changes(
    request: &VoxelBrushEditRequest,
    map_entity: Entity,
    voxel_world: &VoxelWorld,
) -> Vec<VoxelChange> {
    brush_footprint(request.anchor, request.shape, request.width, request.height)
        .into_iter()
        .filter_map(|position| {
            let old = voxel_world.get_voxel(map_entity, position);
            let voxel = voxel_for_brush_mode(old, request.mode, request.material)?;
            Some(VoxelChange { position, voxel })
        })
        .collect()
}

fn voxel_for_brush_mode(
    old: WorldVoxel,
    mode: TerrainBrushMode,
    material: u8,
) -> Option<VoxelType> {
    match mode {
        TerrainBrushMode::FillAir if matches!(old, WorldVoxel::Air | WorldVoxel::Unset) => {
            Some(VoxelType::Solid(material))
        }
        TerrainBrushMode::Remove if matches!(old, WorldVoxel::Solid(_)) => Some(VoxelType::Air),
        TerrainBrushMode::PaintExisting if matches!(old, WorldVoxel::Solid(_)) => {
            Some(VoxelType::Solid(material))
        }
        TerrainBrushMode::ReplaceAll => Some(VoxelType::Solid(material)),
        _ => None,
    }
}

fn apply_voxel_changes(
    changes: &[VoxelChange],
    map_entity: Entity,
    voxel_world: &mut VoxelWorld,
    dirty_state: &mut WorldDirtyState,
    time: &Time,
) {
    voxel_world.set_voxels(
        map_entity,
        changes
            .iter()
            .map(|change| (change.position, WorldVoxel::from(change.voxel))),
    );
    dirty_state.mark_dirty(time.elapsed_secs_f64());
}

fn send_brush_edit_ack(
    client_entity: Entity,
    sequence: u32,
    map_id: MapInstanceId,
    changes: Vec<VoxelChange>,
    ack_senders: &mut Query<&mut MessageSender<VoxelEditAck>>,
) {
    if let Ok(mut sender) = ack_senders.get_mut(client_entity) {
        sender.send::<VoxelChannel>(VoxelEditAck {
            sequence,
            map_id,
            changes,
        });
    } else {
        warn!("send_brush_edit_ack: no ack sender for {client_entity:?}");
    }
}

fn send_voxel_edit_reject(
    client_entity: Entity,
    sequence: u32,
    map_id: MapInstanceId,
    position: IVec3,
    correct_voxel: VoxelType,
    reject_senders: &mut Query<&mut MessageSender<VoxelEditReject>>,
) {
    if let Ok(mut sender) = reject_senders.get_mut(client_entity) {
        sender.send::<VoxelChannel>(VoxelEditReject {
            sequence,
            map_id,
            position,
            correct_voxel,
        });
    } else {
        warn!("send_voxel_edit_reject: no reject sender for {client_entity:?}");
    }
}

fn request_map_matches_player(
    request_map_id: &MapInstanceId,
    player_map_id: &MapInstanceId,
) -> bool {
    request_map_id == player_map_id
}

/// Validates a concrete voxel edit request. Returns false if the whole request should be rejected.
fn validate_voxel_concrete_edit(request: &VoxelConcreteEditRequest) -> bool {
    !request.changes.is_empty() && request.changes.len() <= MAX_BRUSH_VOXELS
}

/// Validates a voxel brush edit request. Returns false if the whole brush should be rejected.
fn validate_voxel_brush_edit(request: &VoxelBrushEditRequest) -> bool {
    if request.width == 0 || request.height == 0 {
        return false;
    }
    brush_footprint(request.anchor, request.shape, request.width, request.height).len()
        <= MAX_BRUSH_VOXELS
}

/// Validates a voxel edit request. Returns false if the edit should be rejected.
fn validate_voxel_edit(
    _request: &VoxelEditRequest,
    _map_entity: Entity,
    _voxel_world: &VoxelWorld,
) -> bool {
    // TODO: Add validation rules as needed (bounds, range, anti-cheat)
    true
}

/// Drains accumulated voxel edits and broadcasts them to clients in the same room.
/// Single edits send individual `VoxelEditBroadcast`; 2+ edits in the same chunk
/// send a batched `SectionBlocksUpdate`. The originating client is excluded.
pub fn flush_voxel_broadcasts(
    mut pending: ResMut<PendingVoxelBroadcasts>,
    mut sender: ServerMultiMessageSender,
    room_registry: Res<RoomRegistry>,
    rooms: Query<&Room>,
) {
    if pending.per_chunk.is_empty() {
        return;
    }

    for ((map_id, chunk_pos), edits) in pending.per_chunk.drain() {
        let Some(_first) = edits.first() else {
            continue;
        };
        let Some(&room_entity) = room_registry.0.get(&map_id) else {
            warn!("flush_voxel_broadcasts: no room for map {:?}", map_id);
            continue;
        };
        let Ok(room) = rooms.get(room_entity) else {
            warn!("flush_voxel_broadcasts: room entity {room_entity:?} has no Room component");
            continue;
        };

        let originators: bevy::ecs::entity::EntityHashSet =
            edits.iter().map(|e| e.originator).collect();
        let targets: bevy::ecs::entity::EntityHashSet = room
            .clients
            .iter()
            .filter(|e| !originators.contains(*e))
            .copied()
            .collect();

        if edits.len() == 1 {
            let edit = &edits[0];
            sender
                .send_to_entities::<_, VoxelChannel>(
                    &VoxelEditBroadcast {
                        map_id: map_id.clone(),
                        position: edit.position,
                        voxel: edit.voxel,
                    },
                    &targets,
                )
                .ok();
        } else {
            let changes: Vec<(IVec3, VoxelType)> =
                edits.iter().map(|e| (e.position, e.voxel)).collect();
            sender
                .send_to_entities::<_, VoxelChannel>(
                    &SectionBlocksUpdate {
                        map_id: map_id.clone(),
                        chunk_pos,
                        changes,
                    },
                    &targets,
                )
                .ok();
        }
    }
}

/// Per-player tracking of which chunks have been sent to the client.
#[derive(Component, Default)]
pub struct ClientChunkVisibility {
    /// Individual chunks (IVec3) whose data has been sent.
    sent_chunks: HashSet<IVec3>,
    /// Columns the client believes are loaded (for sending UnloadColumn).
    sent_columns: HashSet<IVec2>,
    /// The map entity these tracking sets are scoped to. Reset when the
    /// player's ticket switches maps (e.g. map transition).
    tracked_map: Option<Entity>,
}

/// Maximum chunk data messages sent to a single client per tick.
const MAX_CHUNK_SENDS_PER_TICK: usize = 16;

/// Server system: for each connected player, compare their ticket's loaded columns
/// against what we've already sent. Push new chunks (throttled, closest first),
/// send unload for removed.
pub fn push_chunks_to_clients(
    mut player_query: Query<(
        &ChunkTicket,
        &ControlledBy,
        &Position,
        &mut ClientChunkVisibility,
    )>,
    map_query: Query<(&VoxelMapInstance, &MapDimensions, &MapInstanceId)>,
    mut senders: Query<&mut MessageSender<ChunkDataSync>>,
    mut multi_sender: ServerMultiMessageSender,
) {
    for (ticket, controlled_by, pos, mut visibility) in &mut player_query {
        if visibility.tracked_map != Some(ticket.map_entity) {
            visibility.sent_chunks.clear();
            visibility.sent_columns.clear();
            visibility.tracked_map = Some(ticket.map_entity);
        }

        let Ok((instance, dimensions, map_id)) = map_query.get(ticket.map_entity) else {
            trace!(
                "push_chunks_to_clients: map entity {:?} not found",
                ticket.map_entity
            );
            continue;
        };

        let player_col =
            voxel_map_engine::lifecycle::world_to_column_pos(pos.0, instance.chunk_size);
        let current_columns = compute_loaded_columns(ticket, instance, player_col);
        let client_entity = controlled_by.owner;

        let sent = send_unsent_chunks(
            &current_columns,
            &mut visibility,
            instance,
            dimensions.column_y_range,
            map_id,
            player_col,
            client_entity,
            &mut senders,
        );
        plot!("chunks_sent_this_tick", sent as f64);

        unload_stale_columns(
            &mut visibility,
            &current_columns,
            dimensions.column_y_range,
            map_id,
            client_entity,
            &mut multi_sender,
        );

        visibility.sent_columns = current_columns;
    }
}

/// Computes which columns are currently in the player's loaded range.
fn compute_loaded_columns(
    ticket: &ChunkTicket,
    instance: &VoxelMapInstance,
    player_col: IVec2,
) -> HashSet<IVec2> {
    let radius = ticket.radius as i32;
    let mut columns = HashSet::new();
    for dx in -radius..=radius {
        for dz in -radius..=radius {
            let col = player_col + IVec2::new(dx, dz);
            let distance = dx.abs().max(dz.abs()) as u32;
            let level = ticket.ticket_type.base_level() + distance;
            if level > voxel_map_engine::prelude::LOAD_LEVEL_THRESHOLD {
                continue;
            }
            if instance.chunk_levels.contains_key(&col) {
                columns.insert(col);
            }
        }
    }
    columns
}

/// Sends up to `MAX_CHUNK_SENDS_PER_TICK` unsent chunks, closest to player first.
/// Returns the number of chunks sent.
fn send_unsent_chunks(
    current_columns: &HashSet<IVec2>,
    visibility: &mut ClientChunkVisibility,
    instance: &VoxelMapInstance,
    column_y_range: (i32, i32),
    map_id: &MapInstanceId,
    player_col: IVec2,
    client_entity: Entity,
    senders: &mut Query<&mut MessageSender<ChunkDataSync>>,
) -> usize {
    let mut candidates: Vec<(IVec3, u32)> = Vec::new();
    for &col in current_columns {
        let dist = (col.x - player_col.x)
            .abs()
            .max((col.y - player_col.y).abs()) as u32;
        for chunk_pos in voxel_map_engine::prelude::column_to_chunks(col, column_y_range) {
            if visibility.sent_chunks.contains(&chunk_pos) {
                continue;
            }
            if instance.get_chunk_data(chunk_pos).is_none() {
                continue;
            }
            candidates.push((chunk_pos, dist));
        }
    }
    candidates.sort_unstable_by_key(|&(_, dist)| dist);

    let mut sent = 0;
    for (chunk_pos, _) in candidates {
        if sent >= MAX_CHUNK_SENDS_PER_TICK {
            break;
        }
        let Some(chunk_data) = instance.get_chunk_data(chunk_pos) else {
            continue;
        };
        if let Ok(mut sender) = senders.get_mut(client_entity) {
            sender.send::<ChunkChannel>(ChunkDataSync {
                map_id: map_id.clone(),
                chunk_pos,
                chunk_size: instance.chunk_size,
                data: chunk_data.voxels.clone(),
            });
            visibility.sent_chunks.insert(chunk_pos);
            sent += 1;
        } else {
            trace!(
                "send_unsent_chunks: no MessageSender<ChunkDataSync> on client {client_entity:?}, skipping"
            );
            break;
        }
    }
    sent
}

/// Sends `UnloadColumn` messages for columns that left the player's loaded range.
fn unload_stale_columns(
    visibility: &mut ClientChunkVisibility,
    current_columns: &HashSet<IVec2>,
    column_y_range: (i32, i32),
    map_id: &MapInstanceId,
    client_entity: Entity,
    multi_sender: &mut ServerMultiMessageSender,
) {
    let unloaded_cols: Vec<IVec2> = visibility
        .sent_columns
        .difference(current_columns)
        .copied()
        .collect();
    if !unloaded_cols.is_empty() {
        let targets: bevy::ecs::entity::EntityHashSet = [client_entity].into_iter().collect();
        for &col in &unloaded_cols {
            multi_sender
                .send_to_entities::<_, ChunkChannel>(
                    &UnloadColumn {
                        map_id: map_id.clone(),
                        column: col,
                    },
                    &targets,
                )
                .ok();
            for chunk_pos in voxel_map_engine::prelude::column_to_chunks(col, column_y_range) {
                visibility.sent_chunks.remove(&chunk_pos);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::map::VoxelType;
    use protocol::NostrPublicKey;

    fn make_edit(position: IVec3, voxel: VoxelType) -> PendingVoxelEdit {
        PendingVoxelEdit {
            position,
            voxel,
            originator: Entity::PLACEHOLDER,
            map_id: MapInstanceId::Overworld,
        }
    }

    #[test]
    fn seed_from_nostr_public_key_uses_first_eight_bytes_little_endian() {
        let owner = NostrPublicKey([
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff,
        ]);

        assert_eq!(seed_from_nostr_public_key(owner), 0x0102_0304_0506_0708);
    }

    #[test]
    fn single_change_takes_individual_broadcast_path() {
        let mut pending = PendingVoxelBroadcasts::default();
        pending
            .per_chunk
            .entry((MapInstanceId::Overworld, IVec3::ZERO))
            .or_default()
            .push(make_edit(IVec3::new(1, 2, 3), VoxelType::Solid(1)));

        for (_, edits) in pending.per_chunk.drain() {
            assert_eq!(
                edits.len(),
                1,
                "single edit should take individual broadcast path"
            );
        }
    }

    #[test]
    fn multiple_changes_in_same_chunk_takes_batched_path() {
        let mut pending = PendingVoxelBroadcasts::default();
        let entry = pending
            .per_chunk
            .entry((MapInstanceId::Overworld, IVec3::ZERO))
            .or_default();
        entry.push(make_edit(IVec3::new(1, 2, 3), VoxelType::Solid(1)));
        entry.push(make_edit(IVec3::new(4, 5, 6), VoxelType::Air));

        for (_, edits) in pending.per_chunk.drain() {
            assert_eq!(edits.len(), 2, "multi-edit should take batched update path");
        }
    }

    #[test]
    fn paint_brush_mode_recolors_only_existing_solids() {
        assert_eq!(
            voxel_for_brush_mode(WorldVoxel::Solid(1), TerrainBrushMode::PaintExisting, 7),
            Some(VoxelType::Solid(7))
        );
        assert_eq!(
            voxel_for_brush_mode(WorldVoxel::Air, TerrainBrushMode::PaintExisting, 7),
            None
        );
    }

    #[test]
    fn paint_brush_replace_all_writes_air_and_solids() {
        assert_eq!(
            voxel_for_brush_mode(WorldVoxel::Air, TerrainBrushMode::ReplaceAll, 9),
            Some(VoxelType::Solid(9))
        );
        assert_eq!(
            voxel_for_brush_mode(WorldVoxel::Solid(2), TerrainBrushMode::ReplaceAll, 9),
            Some(VoxelType::Solid(9))
        );
    }

    fn brush_request(width: u32, height: u32) -> VoxelBrushEditRequest {
        VoxelBrushEditRequest {
            sequence: 1,
            map_id: MapInstanceId::Overworld,
            anchor: IVec3::ZERO,
            shape: voxel_map_engine::prelude::TerrainBrushShape::Rect,
            width,
            height,
            mode: TerrainBrushMode::FillAir,
            material: 0,
        }
    }

    #[test]
    fn brush_request_validation_rejects_oversized_footprints() {
        assert!(!validate_voxel_brush_edit(&brush_request(65, 1)));
    }

    #[test]
    fn brush_request_validation_accepts_limit_sized_footprints() {
        assert!(validate_voxel_brush_edit(&brush_request(64, 1)));
    }

    #[test]
    fn fill_air_only_writes_air_or_unset_positions() {
        assert_eq!(
            voxel_for_brush_mode(WorldVoxel::Air, TerrainBrushMode::FillAir, 3),
            Some(VoxelType::Solid(3))
        );
        assert_eq!(
            voxel_for_brush_mode(WorldVoxel::Unset, TerrainBrushMode::FillAir, 3),
            Some(VoxelType::Solid(3))
        );
        assert_eq!(
            voxel_for_brush_mode(WorldVoxel::Solid(1), TerrainBrushMode::FillAir, 3),
            None
        );
    }

    #[test]
    fn remove_only_writes_solid_positions() {
        assert_eq!(
            voxel_for_brush_mode(WorldVoxel::Solid(1), TerrainBrushMode::Remove, 3),
            Some(VoxelType::Air)
        );
        assert_eq!(
            voxel_for_brush_mode(WorldVoxel::Air, TerrainBrushMode::Remove, 3),
            None
        );
    }

    #[test]
    fn replace_all_writes_every_footprint_position() {
        assert_eq!(
            voxel_for_brush_mode(WorldVoxel::Air, TerrainBrushMode::ReplaceAll, 5),
            Some(VoxelType::Solid(5))
        );
        assert_eq!(
            voxel_for_brush_mode(WorldVoxel::Solid(1), TerrainBrushMode::ReplaceAll, 5),
            Some(VoxelType::Solid(5))
        );
        assert_eq!(
            voxel_for_brush_mode(WorldVoxel::Unset, TerrainBrushMode::ReplaceAll, 5),
            Some(VoxelType::Solid(5))
        );
    }

    #[test]
    fn remote_read_allowed_hosts_parse_comma_list() {
        assert_eq!(
            parse_blossom_allowed_hosts(Some("Blossom.Example, cdn.example ")),
            BTreeSet::from(["blossom.example".to_string(), "cdn.example".to_string()])
        );
    }

    #[test]
    fn entity_change_marks_dirty_and_records_map() {
        let mut dirty = WorldDirtyState::default();
        let map_id = MapInstanceId::Overworld;

        dirty.mark_entities_dirty(&map_id, 1.0);

        assert!(dirty.is_dirty);
        assert_eq!(dirty.first_dirty_time, Some(1.0));
        assert!(dirty.entities_dirty.contains(&map_id));
    }

    #[test]
    fn map_entities_slot_absent_unless_entities_changed() {
        let entities = vec![SavedEntity {
            kind: SavedEntityKind::RespawnPoint,
            position: Vec3::ZERO,
        }];

        // Unchanged this cycle (e.g. a chunk-only edit) -> do not re-upload entities.
        assert!(matches!(
            map_entities_publish_slot(false, &entities),
            nostr_map_persistence::PayloadSlotState::Absent
        ));
        // Changed with entities present -> upload the current list.
        assert!(matches!(
            map_entities_publish_slot(true, &entities),
            nostr_map_persistence::PayloadSlotState::Present(_)
        ));
        // Changed to empty -> explicit empty slot, not absent.
        assert!(matches!(
            map_entities_publish_slot(true, &[]),
            nostr_map_persistence::PayloadSlotState::Empty
        ));
    }

    #[test]
    fn request_map_must_match_player_map() {
        let homebase = MapInstanceId::Homebase {
            owner: NostrPublicKey([9; 32]),
        };

        assert!(request_map_matches_player(
            &MapInstanceId::Overworld,
            &MapInstanceId::Overworld
        ));
        assert!(!request_map_matches_player(
            &homebase,
            &MapInstanceId::Overworld
        ));
    }

    #[test]
    fn concrete_request_validation_rejects_empty_changes() {
        let request = VoxelConcreteEditRequest {
            sequence: 42,
            map_id: MapInstanceId::Overworld,
            changes: Vec::new(),
        };

        assert!(!validate_voxel_concrete_edit(&request));
    }

    #[test]
    fn concrete_request_validation_rejects_oversized_payloads() {
        let request = VoxelConcreteEditRequest {
            sequence: 42,
            map_id: MapInstanceId::Overworld,
            changes: (0..=MAX_BRUSH_VOXELS)
                .map(|index| VoxelChange {
                    position: IVec3::new(index as i32, 0, 0),
                    voxel: VoxelType::Solid(7),
                })
                .collect(),
        };

        assert!(!validate_voxel_concrete_edit(&request));
    }

    #[test]
    fn concrete_request_validation_accepts_changes() {
        let request = VoxelConcreteEditRequest {
            sequence: 42,
            map_id: MapInstanceId::Overworld,
            changes: vec![VoxelChange {
                position: IVec3::new(1, 2, 3),
                voxel: VoxelType::Solid(7),
            }],
        };

        assert!(validate_voxel_concrete_edit(&request));
    }

    #[test]
    fn different_chunks_produce_separate_entries() {
        let mut pending = PendingVoxelBroadcasts::default();
        pending
            .per_chunk
            .entry((MapInstanceId::Overworld, IVec3::ZERO))
            .or_default()
            .push(make_edit(IVec3::new(1, 2, 3), VoxelType::Solid(1)));
        pending
            .per_chunk
            .entry((MapInstanceId::Overworld, IVec3::ONE))
            .or_default()
            .push(make_edit(IVec3::new(17, 18, 19), VoxelType::Solid(2)));

        let chunks: Vec<_> = pending.per_chunk.drain().collect();
        assert_eq!(
            chunks.len(),
            2,
            "different chunks should produce separate entries"
        );
        for (_, edits) in &chunks {
            assert_eq!(edits.len(), 1);
        }
    }

    #[test]
    fn pending_cleared_after_drain() {
        let mut pending = PendingVoxelBroadcasts::default();
        pending
            .per_chunk
            .entry((MapInstanceId::Overworld, IVec3::ZERO))
            .or_default()
            .push(make_edit(IVec3::new(1, 2, 3), VoxelType::Solid(1)));

        for _ in pending.per_chunk.drain() {}
        assert!(
            pending.per_chunk.is_empty(),
            "pending should be empty after drain"
        );
    }
}
