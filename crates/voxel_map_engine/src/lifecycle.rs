use bevy::ecs::message::MessageWriter;
use bevy::log::{error, info_span};
use bevy::prelude::*;
use bevy::tasks::futures::check_ready;
use bevy::tasks::{AsyncComputeTaskPool, Task};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
#[allow(unused_imports)]
use tracy_client::plot;

use crate::chunk::VoxelChunk;
use crate::config::{MapDimensions, VoxelGenerator, VoxelMapConfig, WorldObjectSpawn};
use crate::generation::{
    GEN_BATCH_SIZE, PendingChunks, PendingEntitySpawns, build_surface_height_map,
    spawn_features_task, spawn_mesh_task, spawn_terrain_batch,
};
use crate::instance::VoxelMapInstance;
use crate::meshing::mesh_chunk_greedy;
use crate::persistence::ChunkFileEnvelope;
use crate::persistence::fs_chunk::FsChunkStore;
use crate::persistence::fs_chunk_entities::FsChunkEntitiesStore;
use crate::propagator::TicketLevelPropagator;
use crate::ticket::{ChunkTicket, TicketType, chunk_to_column, column_to_chunks};
use crate::types::{ChunkStatus, FillType};
use persistence::{PendingStoreOps, SaveOpId, StoreBackend};

/// Per-frame time budget for chunk pipeline work on a single map.
/// Reset at the start of each frame by `update_chunks`.
/// All downstream systems check `has_time()` before doing work.
#[derive(Component)]
pub struct ChunkWorkBudget {
    start: bevy::platform::time::Instant,
    budget: std::time::Duration,
}

/// Default budget: ~50% of a 16ms frame at 60fps.
const CHUNK_WORK_BUDGET_MS: u64 = 8;

/// Safety caps -- even within budget, don't exceed these per frame.
const MAX_GEN_SPAWNS_PER_FRAME: usize = 256;
const MAX_GEN_POLLS_PER_FRAME: usize = 256;
const MAX_REMESH_SPAWNS_PER_FRAME: usize = 256;
const MAX_REMESH_POLLS_PER_FRAME: usize = 256;

/// Maximum number of generation tasks allowed in-flight at once.
const MAX_PENDING_GEN_TASKS: usize = 512;
/// Maximum number of remesh tasks allowed in-flight at once.
const MAX_PENDING_REMESH_TASKS: usize = 512;

impl ChunkWorkBudget {
    fn reset(&mut self) {
        self.start = bevy::platform::time::Instant::now();
    }

    /// Returns true if there is time remaining in the budget.
    pub fn has_time(&self) -> bool {
        self.start.elapsed() < self.budget
    }
}

impl Default for ChunkWorkBudget {
    fn default() -> Self {
        Self {
            start: bevy::platform::time::Instant::now(),
            budget: std::time::Duration::from_millis(CHUNK_WORK_BUDGET_MS),
        }
    }
}

/// Why a throttled loop stopped processing. Emitted as a Tracy plot for tuning.
#[repr(u8)]
enum StopReason {
    /// All available work was processed.
    Completed = 0,
    /// Time budget exhausted.
    TimeBudget = 1,
    /// Per-frame hard cap reached.
    HardCap = 2,
    /// Total in-flight task cap reached.
    InFlightCap = 3,
}

/// A chunk position with priority metadata for the work queue.
struct ChunkWork {
    position: IVec3,
    effective_level: u32,
    distance_to_source: u32,
}

impl Eq for ChunkWork {}
impl PartialEq for ChunkWork {
    fn eq(&self, other: &Self) -> bool {
        self.effective_level == other.effective_level
            && self.distance_to_source == other.distance_to_source
    }
}

/// Min-heap: lower level first, then closer distance first.
/// `BinaryHeap` is a max-heap, so we reverse the ordering.
impl Ord for ChunkWork {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .effective_level
            .cmp(&self.effective_level)
            .then(other.distance_to_source.cmp(&self.distance_to_source))
    }
}
impl PartialOrd for ChunkWork {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Default PBR material applied to voxel chunk meshes.
#[derive(Resource)]
pub struct DefaultVoxelMaterial(pub Handle<StandardMaterial>);

/// Startup system that creates the default voxel material.
pub fn init_default_material(
    mut commands: Commands,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let handle = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        perceptual_roughness: 0.9,
        ..default()
    });
    commands.insert_resource(DefaultVoxelMaterial(handle));
}

/// A pending async remesh task for a chunk mutated in-place.
struct RemeshTask {
    chunk_pos: IVec3,
    task: Task<Option<Mesh>>,
}

/// Component tracking pending remesh tasks for a map instance.
#[derive(Component, Default)]
pub struct PendingRemeshes {
    tasks: Vec<RemeshTask>,
}

/// Queued chunk saves awaiting dispatch to the persistence store.
#[derive(Component, Default)]
pub struct PendingSaves {
    /// Chunks waiting to be saved. FIFO order.
    pub queue: VecDeque<PendingSave>,
}

/// A single chunk save request waiting in the queue.
pub struct PendingSave {
    pub position: IVec3,
    pub envelope: ChunkFileEnvelope,
    pub save_id: Option<SaveOpId>,
}

/// Successful terrain chunk save emitted by the generic voxel persistence drainer.
#[derive(Message, Clone, Debug)]
pub struct ChunkSaveCompleted {
    pub map_entity: Entity,
    pub position: IVec3,
    pub save_id: Option<SaveOpId>,
}

/// Failed terrain chunk save emitted by the generic voxel persistence drainer.
#[derive(Message, Clone, Debug)]
pub struct ChunkSaveFailed {
    pub map_entity: Entity,
    pub position: IVec3,
    pub save_id: Option<SaveOpId>,
    pub error: String,
}

/// Maximum save tasks drained from queue per frame.
const MAX_SAVE_SPAWNS_PER_FRAME: usize = 16;

/// Tracks which chunks have in-flight work to prevent overlapping gen/remesh.
#[derive(Component, Default)]
pub struct ChunkWorkTracker {
    pub generating: HashSet<IVec3>,
    pub remeshing: HashSet<IVec3>,
}

/// Persistent priority queue for chunk generation work.
///
/// Entries are added incrementally when columns load or change level.
/// Stale entries (already generated, pending, or unloaded) are validated
/// and skipped on pop (lazy deletion).
#[derive(Component, Default)]
pub struct GenQueue {
    heap: BinaryHeap<ChunkWork>,
}

/// Cached state for a single ticket, used to detect changes.
pub struct CachedTicket {
    column: IVec2,
    map_entity: Entity,
    ticket_type: TicketType,
    radius: u32,
}

/// Convert a world-space position to a 2D column position (drop Y).
pub fn world_to_column_pos(translation: Vec3, chunk_size: u32) -> IVec2 {
    let chunk = world_to_chunk_pos(translation, chunk_size);
    IVec2::new(chunk.x, chunk.z)
}

/// Reset chunk work budgets when `update_chunks` is not running.
///
/// `update_chunks` (gated on `ChunkGenerationEnabled`) resets the budget after
/// propagation. Without generation enabled (i.e. clients), nothing resets it,
/// starving the remesh pipeline.
pub fn reset_chunk_budgets(mut budgets: Query<&mut ChunkWorkBudget>) {
    for mut budget in &mut budgets {
        budget.reset();
    }
}

/// Auto-insert `PendingChunks`, `PendingRemeshes`, and `TicketLevelPropagator`
/// on map entities that lack them.
///
/// Gated on `With<VoxelGenerator>` -- maps without a generator don't start loading chunks.
pub fn ensure_pending_chunks(
    mut commands: Commands,
    chunks_query: Query<
        Entity,
        (
            With<VoxelMapInstance>,
            With<VoxelGenerator>,
            Without<PendingChunks>,
        ),
    >,
    remesh_query: Query<
        Entity,
        (
            With<VoxelMapInstance>,
            With<VoxelGenerator>,
            Without<PendingRemeshes>,
        ),
    >,
    propagator_query: Query<
        Entity,
        (
            With<VoxelMapInstance>,
            With<VoxelGenerator>,
            Without<TicketLevelPropagator>,
        ),
    >,
    budget_query: Query<
        Entity,
        (
            With<VoxelMapInstance>,
            With<VoxelGenerator>,
            Without<ChunkWorkBudget>,
        ),
    >,
    gen_queue_query: Query<
        Entity,
        (
            With<VoxelMapInstance>,
            With<VoxelGenerator>,
            Without<GenQueue>,
        ),
    >,
    pending_saves_query: Query<
        Entity,
        (
            With<VoxelMapInstance>,
            With<VoxelGenerator>,
            Without<PendingSaves>,
        ),
    >,
    tracker_query: Query<
        Entity,
        (
            With<VoxelMapInstance>,
            With<VoxelGenerator>,
            Without<ChunkWorkTracker>,
        ),
    >,
    entity_spawns_query: Query<
        Entity,
        (
            With<VoxelMapInstance>,
            With<VoxelGenerator>,
            Without<PendingEntitySpawns>,
        ),
    >,
) {
    for entity in &chunks_query {
        trace!("ensure_pending_chunks: adding PendingChunks to {entity:?}");
        commands.entity(entity).insert(PendingChunks::default());
    }
    for entity in &remesh_query {
        trace!("ensure_pending_chunks: adding PendingRemeshes to {entity:?}");
        commands.entity(entity).insert(PendingRemeshes::default());
    }
    for entity in &propagator_query {
        trace!("ensure_pending_chunks: adding TicketLevelPropagator to {entity:?}");
        commands
            .entity(entity)
            .insert(TicketLevelPropagator::default());
    }
    for entity in &budget_query {
        trace!("ensure_pending_chunks: adding ChunkWorkBudget to {entity:?}");
        commands.entity(entity).insert(ChunkWorkBudget::default());
    }
    for entity in &gen_queue_query {
        trace!("ensure_pending_chunks: adding GenQueue to {entity:?}");
        commands.entity(entity).insert(GenQueue::default());
    }
    for entity in &pending_saves_query {
        trace!("ensure_pending_chunks: adding PendingSaves to {entity:?}");
        commands.entity(entity).insert(PendingSaves::default());
    }
    for entity in &tracker_query {
        trace!("ensure_pending_chunks: adding ChunkWorkTracker to {entity:?}");
        commands.entity(entity).insert(ChunkWorkTracker::default());
    }
    for entity in &entity_spawns_query {
        trace!("ensure_pending_chunks: adding PendingEntitySpawns to {entity:?}");
        commands
            .entity(entity)
            .insert(PendingEntitySpawns::default());
    }
}

/// Propagate levels and spawn/remove chunks based on the diff.
pub(crate) fn update_chunks(
    mut map_query: Query<(
        Entity,
        &mut VoxelMapInstance,
        &VoxelMapConfig,
        &MapDimensions,
        &VoxelGenerator,
        &mut PendingChunks,
        &mut TicketLevelPropagator,
        &GlobalTransform,
        &mut ChunkWorkBudget,
        &mut GenQueue,
        &mut PendingSaves,
        &mut ChunkWorkTracker,
    )>,
    store_query: Query<(
        Option<&StoreBackend<IVec3, Vec<WorldObjectSpawn>, FsChunkEntitiesStore>>,
        Option<&StoreBackend<IVec3, ChunkFileEnvelope, FsChunkStore>>,
    )>,
    mut tick: Local<u32>,
) {
    *tick += 1;

    for (
        map_entity,
        mut instance,
        config,
        dimensions,
        generator,
        mut pending,
        mut propagator,
        _,
        mut budget,
        mut gen_queue,
        mut pending_saves,
        mut tracker,
    ) in &mut map_query
    {
        let (y_min, y_max) = dimensions.column_y_range;

        let diff = {
            let _span = info_span!("propagate_ticket_levels").entered();
            propagator.propagate()
        };

        // Reset budget AFTER propagation so amortized BFS counts against it.
        budget.reset();

        for &col in &diff.unloaded {
            remove_column_chunks(&mut instance, &mut pending_saves, col, y_min, y_max);
        }
        for &(col, level) in &diff.loaded {
            if is_column_within_bounds(col, dimensions.bounds) {
                instance.chunk_levels.insert(col, level);
            }
        }
        for &(col, level) in &diff.changed {
            if is_column_within_bounds(col, dimensions.bounds) {
                instance.chunk_levels.insert(col, level);
            }
        }

        if config.generates_chunks {
            enqueue_new_chunks(
                &instance,
                &propagator,
                &mut gen_queue,
                &diff,
                dimensions.bounds,
                y_min,
                y_max,
            );
            let (entity_store, chunk_store) = store_query
                .get(map_entity)
                .map(|(es, cs)| (es.map(|sb| &sb.0), cs.map(|sb| &sb.0)))
                .unwrap_or((None, None));
            drain_gen_queue(
                &mut instance,
                &mut pending,
                &mut gen_queue,
                &mut tracker,
                dimensions,
                generator,
                &budget,
                entity_store,
                chunk_store,
            );
        }

        plot!(
            "chunk_work_budget_remaining_us",
            budget
                .budget
                .saturating_sub(budget.start.elapsed())
                .as_micros() as f64
        );
    }
}

/// Detect stale and changed tickets, updating propagator sources accordingly.
///
/// Runs unconditionally (not gated on `ChunkGenerationEnabled`) so that
/// client-side propagators have source positions for distance-based remesh
/// priority even when chunk generation is disabled.
pub fn collect_tickets(
    mut map_query: Query<(
        Entity,
        &mut VoxelMapInstance,
        &VoxelMapConfig,
        &MapDimensions,
        &VoxelGenerator,
        &mut PendingChunks,
        &mut TicketLevelPropagator,
        &GlobalTransform,
        &mut ChunkWorkBudget,
        &mut GenQueue,
        &mut PendingSaves,
        &mut ChunkWorkTracker,
    )>,
    ticket_query: Query<(Entity, &ChunkTicket, &GlobalTransform)>,
    mut ticket_cache: Local<HashMap<Entity, CachedTicket>>,
) {
    let _span = info_span!("collect_tickets").entered();

    let active: HashSet<Entity> = ticket_query.iter().map(|(e, _, _)| e).collect();
    let stale: Vec<Entity> = ticket_cache
        .keys()
        .filter(|e| !active.contains(e))
        .copied()
        .collect();
    for entity in stale {
        if let Some(cached) = ticket_cache.remove(&entity)
            && let Ok((_, _, _, _, _, _, mut prop, _, _, _, _, _)) =
                map_query.get_mut(cached.map_entity)
        {
            prop.remove_source(entity);
        }
    }

    for (ticket_entity, ticket, transform) in ticket_query.iter() {
        // Compute column from immutable access; borrow drops at end of block.
        let column = {
            let Ok((_, instance, _, _, _, _, _, map_transform, _, _, _, _)) =
                map_query.get(ticket.map_entity)
            else {
                trace!(
                    "collect_tickets: ticket {ticket_entity:?} references non-existent map {:?}, expected during deferred command application",
                    ticket.map_entity
                );
                continue;
            };
            let map_inv = map_transform.affine().inverse();
            let local_pos = map_inv.transform_point3(transform.translation());
            world_to_column_pos(local_pos, instance.chunk_size)
        };

        let needs_update = match ticket_cache.get(&ticket_entity) {
            Some(cached) => {
                cached.column != column
                    || cached.map_entity != ticket.map_entity
                    || cached.ticket_type != ticket.ticket_type
                    || cached.radius != ticket.radius
            }
            None => true,
        };

        if needs_update {
            // If map changed, remove source from old map's propagator first
            if let Some(cached) = ticket_cache.get(&ticket_entity)
                && cached.map_entity != ticket.map_entity
                && let Ok((_, _, _, _, _, _, mut old_prop, _, _, _, _, _)) =
                    map_query.get_mut(cached.map_entity)
            {
                old_prop.remove_source(ticket_entity);
            }
            if let Ok((_, _, _, _, _, _, mut prop, _, _, _, _, _)) =
                map_query.get_mut(ticket.map_entity)
            {
                prop.set_source(
                    ticket_entity,
                    column,
                    ticket.ticket_type.base_level(),
                    ticket.radius,
                );
            }
            ticket_cache.insert(
                ticket_entity,
                CachedTicket {
                    column,
                    map_entity: ticket.map_entity,
                    ticket_type: ticket.ticket_type,
                    radius: ticket.radius,
                },
            );
        }
    }
}

/// Remove all chunk data for a column being unloaded.
/// Dirty chunks are enqueued into `pending_saves` for async persistence.
fn remove_column_chunks(
    instance: &mut VoxelMapInstance,
    pending_saves: &mut PendingSaves,
    col: IVec2,
    y_min: i32,
    y_max: i32,
) {
    let _span = info_span!("remove_column_chunks").entered();
    let chunk_size = instance.chunk_size;
    for chunk_pos in column_to_chunks(col, (y_min, y_max)) {
        if instance.dirty_chunks.remove(&chunk_pos)
            && let Some(chunk_data) = instance.get_chunk_data(chunk_pos)
        {
            pending_saves.queue.push_back(PendingSave {
                position: chunk_pos,
                envelope: ChunkFileEnvelope {
                    version: crate::persistence::CHUNK_SAVE_VERSION,
                    chunk_size,
                    data: chunk_data.clone(),
                },
                save_id: None,
            });
        }
        instance.remove_chunk_data(chunk_pos);
    }
    instance.chunk_levels.remove(&col);
}

/// Drain pending save queue into the map entity's chunk store ops.
pub fn drain_pending_saves(
    mut map_query: Query<(
        Entity,
        &mut PendingSaves,
        &StoreBackend<IVec3, ChunkFileEnvelope, FsChunkStore>,
        &mut PendingStoreOps<IVec3, ChunkFileEnvelope>,
    )>,
    mut completed: MessageWriter<ChunkSaveCompleted>,
    mut failed: MessageWriter<ChunkSaveFailed>,
) {
    for (map_entity, mut pending, chunk_store, mut chunk_ops) in &mut map_query {
        chunk_ops.poll();
        for completion in chunk_ops.completed_saves.drain(..) {
            completed.write(ChunkSaveCompleted {
                map_entity,
                position: completion.key,
                save_id: completion.id,
            });
        }
        for failure in chunk_ops.save_errors.drain(..) {
            error!(
                "Failed to save chunk data at {:?}: {}",
                failure.key, failure.error
            );
            failed.write(ChunkSaveFailed {
                map_entity,
                position: failure.key,
                save_id: failure.id,
                error: failure.error.to_string(),
            });
        }

        let mut spawned = 0;
        while !pending.queue.is_empty() && spawned < MAX_SAVE_SPAWNS_PER_FRAME {
            let save = pending.queue.pop_front().unwrap();
            match save.save_id {
                Some(save_id) => chunk_ops.spawn_save_with_id(
                    &chunk_store.0,
                    save_id,
                    save.position,
                    save.envelope,
                ),
                None => chunk_ops.spawn_save(&chunk_store.0, save.position, save.envelope),
            }
            spawned += 1;
        }

        plot!("save_queue_depth", pending.queue.len() as f64);
        plot!("saves_spawned_this_frame", spawned as f64);

        let stop_reason = if spawned >= MAX_SAVE_SPAWNS_PER_FRAME {
            StopReason::HardCap
        } else {
            StopReason::Completed
        };
        plot!("save_spawn_stop_reason", stop_reason as u8 as f64);
    }
}

/// Push newly loaded/changed columns into the persistent generation queue.
///
/// Checks each chunk's current status against the level-gated max status,
/// enqueuing only chunks that need further advancement.
fn enqueue_new_chunks(
    instance: &VoxelMapInstance,
    propagator: &TicketLevelPropagator,
    gen_queue: &mut GenQueue,
    diff: &crate::propagator::LevelDiff,
    bounds: Option<IVec3>,
    y_min: i32,
    y_max: i32,
) {
    let _span = info_span!("enqueue_new_chunks").entered();
    let mut enqueued = 0;
    for &(col, level) in diff.loaded.iter().chain(diff.changed.iter()) {
        let distance = propagator.min_distance_to_source(col);
        let max_status = ChunkStatus::max_for_level(level);
        for chunk_pos in column_to_chunks(col, (y_min, y_max)) {
            if !is_within_bounds(chunk_pos, bounds) {
                continue;
            }
            let current = instance
                .get_chunk_data(chunk_pos)
                .map(|c| c.status)
                .unwrap_or(ChunkStatus::Empty);
            if current >= max_status {
                continue;
            }
            gen_queue.heap.push(ChunkWork {
                position: chunk_pos,
                effective_level: level,
                distance_to_source: distance,
            });
            enqueued += 1;
        }
    }
    plot!("gen_enqueued_this_frame", enqueued as f64);
}

/// Drain the persistent generation queue, spawning tasks for valid entries.
///
/// Stale entries (chunk already generated, already pending, column unloaded,
/// or out of bounds) are skipped via lazy deletion.
#[allow(clippy::too_many_arguments)]
fn drain_gen_queue(
    instance: &mut VoxelMapInstance,
    pending: &mut PendingChunks,
    gen_queue: &mut GenQueue,
    tracker: &mut ChunkWorkTracker,
    dimensions: &MapDimensions,
    generator: &VoxelGenerator,
    budget: &ChunkWorkBudget,
    entity_store: Option<&FsChunkEntitiesStore>,
    chunk_store: Option<&FsChunkStore>,
) {
    let _span = info_span!("drain_gen_queue").entered();

    plot!("gen_queue_depth", gen_queue.heap.len() as f64);

    if pending.tasks.len() >= MAX_PENDING_GEN_TASKS {
        plot!(
            "gen_spawn_stop_reason",
            StopReason::InFlightCap as u8 as f64
        );
        plot!("gen_spawned_this_frame", 0.0);
        return;
    }

    let mut spawned = 0;
    let mut stale = 0;
    let mut terrain_batch = Vec::with_capacity(GEN_BATCH_SIZE);

    while let Some(work) = gen_queue.heap.pop() {
        if pending.tasks.len() >= MAX_PENDING_GEN_TASKS {
            break;
        }
        if !budget.has_time() || spawned >= MAX_GEN_SPAWNS_PER_FRAME {
            break;
        }

        let col = chunk_to_column(work.position);
        if !instance.chunk_levels.contains_key(&col) {
            stale += 1;
            continue;
        }
        if tracker.generating.contains(&work.position) || tracker.remeshing.contains(&work.position)
        {
            stale += 1;
            continue;
        }
        if !is_within_bounds(work.position, dimensions.bounds) {
            stale += 1;
            continue;
        }

        let current_status = instance
            .get_chunk_data(work.position)
            .map(|c| c.status)
            .unwrap_or(ChunkStatus::Empty);
        let max_status = ChunkStatus::max_for_level(work.effective_level);
        let Some(next_stage) = current_status.next() else {
            stale += 1;
            continue;
        };
        if next_stage > max_status {
            stale += 1;
            continue;
        }

        // NOTE: bare continues above are intentional — the `stale` counter
        // (plotted via Tracy) provides aggregate telemetry. Per-entry trace! would
        // fire thousands of times per frame in the common case (lazy deletion).

        match next_stage {
            ChunkStatus::Terrain => {
                tracker.generating.insert(work.position);
                terrain_batch.push(work.position);
                spawned += 1;
                if terrain_batch.len() >= GEN_BATCH_SIZE {
                    spawn_terrain_batch(
                        pending,
                        std::mem::take(&mut terrain_batch),
                        generator,
                        instance.shape.clone(),
                        chunk_store.cloned(),
                        entity_store.cloned(),
                    );
                    terrain_batch = Vec::with_capacity(GEN_BATCH_SIZE);
                }
            }
            ChunkStatus::Features => {
                let chunk_data = instance
                    .get_chunk_data(work.position)
                    .expect("chunk must exist at Terrain status");
                // Uniform chunks (all solid or all air) have no surface — skip async
                // placement and promote directly.
                if !matches!(chunk_data.fill_type, FillType::Mixed) {
                    let data = instance
                        .get_chunk_data_mut(work.position)
                        .expect("chunk must exist for Features promotion");
                    data.status = ChunkStatus::Features;
                    gen_queue.heap.push(ChunkWork {
                        position: work.position,
                        effective_level: work.effective_level,
                        distance_to_source: work.distance_to_source,
                    });
                    continue;
                }
                let height_map = build_surface_height_map(
                    work.position,
                    &chunk_data.voxels,
                    instance.chunk_size,
                    instance.padded_size,
                    &instance.shape,
                );
                tracker.generating.insert(work.position);
                spawn_features_task(
                    pending,
                    work.position,
                    height_map,
                    generator,
                    entity_store.cloned(),
                );
                spawned += 1;
            }
            ChunkStatus::Mesh => {
                let voxels = instance
                    .get_chunk_data(work.position)
                    .expect("chunk must exist at Features status")
                    .voxels
                    .to_voxels();
                tracker.generating.insert(work.position);
                spawn_mesh_task(pending, work.position, voxels, instance.shape.clone());
                spawned += 1;
            }
            ChunkStatus::Full => {
                // Full is a synchronous promotion — no async work needed.
                if let Some(chunk_data) = instance.get_chunk_data_mut(work.position) {
                    chunk_data.status = ChunkStatus::Full;
                }
            }
            ChunkStatus::Empty => unreachable!("next() never returns Empty"),
        }
    }

    // Flush remaining terrain batch
    if !terrain_batch.is_empty() {
        spawn_terrain_batch(
            pending,
            terrain_batch,
            generator,
            instance.shape.clone(),
            chunk_store.cloned(),
            entity_store.cloned(),
        );
    }

    let stop_reason = if pending.tasks.len() >= MAX_PENDING_GEN_TASKS {
        StopReason::InFlightCap
    } else if spawned >= MAX_GEN_SPAWNS_PER_FRAME {
        StopReason::HardCap
    } else if !budget.has_time() {
        StopReason::TimeBudget
    } else {
        StopReason::Completed
    };
    plot!("gen_spawn_stop_reason", stop_reason as u8 as f64);
    plot!("gen_spawned_this_frame", spawned as f64);
    plot!("gen_stale_skipped", stale as f64);
}

/// Poll pending chunk generation tasks and process completed results.
pub fn poll_chunk_tasks(
    mut commands: Commands,
    mut map_query: Query<(
        Entity,
        &mut VoxelMapInstance,
        &mut PendingChunks,
        &mut ChunkWorkTracker,
        &mut GenQueue,
        &TicketLevelPropagator,
        &mut PendingEntitySpawns,
    )>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    default_material: Option<Res<DefaultVoxelMaterial>>,
) {
    let Some(default_material) = default_material else {
        warn!("DefaultVoxelMaterial resource not found; chunk meshes will not spawn");
        return;
    };

    for (
        map_entity,
        mut instance,
        mut pending,
        mut tracker,
        mut gen_queue,
        propagator,
        mut pending_entity_spawns,
    ) in &mut map_query
    {
        let finished_count = pending.tasks.iter().filter(|t| t.is_finished()).count();
        plot!("gen_tasks_finished", finished_count as f64);

        let mut i = 0;
        let mut polled = 0;
        // NOTE: Polling does NOT check budget. Collecting completed results is
        // essential progress — starving it causes a deadlock where in-flight tasks
        // block new spawns but are never reaped.
        while i < pending.tasks.len() && polled < MAX_GEN_POLLS_PER_FRAME {
            if let Some(results) = check_ready(&mut pending.tasks[i]) {
                let _ = pending.tasks.swap_remove(i);
                for result in results {
                    debug_assert!(
                        tracker.generating.contains(&result.position),
                        "poll_chunk_tasks: completed chunk at {:?} was not in tracker.generating",
                        result.position
                    );
                    tracker.generating.remove(&result.position);
                    handle_completed_chunk(
                        &mut commands,
                        &mut instance,
                        &mut meshes,
                        &mut materials,
                        &default_material,
                        map_entity,
                        result,
                        &mut gen_queue,
                        propagator,
                        &mut pending_entity_spawns,
                    );
                    polled += 1;
                }
            } else {
                i += 1;
            }
        }

        let stop_reason = if polled >= MAX_GEN_POLLS_PER_FRAME {
            StopReason::HardCap
        } else {
            StopReason::Completed
        };
        plot!("gen_poll_stop_reason", stop_reason as u8 as f64);
        plot!("gen_tasks_in_flight", pending.tasks.len() as f64);
        plot!("gen_tasks_polled_this_frame", polled as f64);
    }
}

fn color_from_chunk_pos(pos: IVec3) -> Color {
    let hash = (pos.x.wrapping_mul(73856093))
        ^ (pos.y.wrapping_mul(19349663))
        ^ (pos.z.wrapping_mul(83492791));
    let hue = ((hash as u32) % 360) as f32;
    Color::hsl(hue, 0.5, 0.5)
}

fn handle_completed_chunk(
    commands: &mut Commands,
    instance: &mut VoxelMapInstance,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    default_material: &DefaultVoxelMaterial,
    map_entity: Entity,
    result: crate::generation::ChunkGenResult,
    gen_queue: &mut GenQueue,
    propagator: &TicketLevelPropagator,
    pending_entity_spawns: &mut PendingEntitySpawns,
) {
    // Insert chunk data (Terrain/Mesh stages) or update status in-place (Features stage)
    let completed_status = if let Some(chunk_data) = result.chunk_data {
        let status = chunk_data.status;
        if !result.from_disk {
            instance.dirty_chunks.insert(result.position);
        }
        instance.insert_chunk_data(result.position, chunk_data);
        status
    } else {
        // Features stage: update status in-place, no re-palettization.
        // The chunk may have been unloaded while the async Features task was
        // in-flight (e.g. a ticket moved columns). Discard the stale result.
        let Some(data) = instance.get_chunk_data_mut(result.position) else {
            trace!(
                "discarding stale Features result for {:?}: column unloaded while task was in-flight",
                result.position
            );
            return;
        };
        data.status = ChunkStatus::Features;
        if !result.from_disk {
            instance.dirty_chunks.insert(result.position);
        }
        ChunkStatus::Features
    };

    // Queue entity spawns from Features stage
    if !result.entity_spawns.is_empty() {
        pending_entity_spawns
            .0
            .push((result.position, result.entity_spawns));
    }

    // Spawn mesh entity only when a mesh is present (Mesh stage or disk-loaded Full chunks)
    if let Some(mesh) = result.mesh {
        let mesh_handle = meshes.add(mesh);
        let offset = chunk_world_offset(result.position, instance.chunk_size);

        let material = if instance.debug_colors {
            materials.add(StandardMaterial {
                base_color: color_from_chunk_pos(result.position),
                perceptual_roughness: 0.9,
                ..default()
            })
        } else {
            default_material.0.clone()
        };

        let chunk_entity = commands
            .spawn((
                VoxelChunk {
                    position: result.position,
                    lod_level: 0,
                },
                Mesh3d(mesh_handle),
                MeshMaterial3d(material),
                Transform::from_translation(offset),
            ))
            .id();

        commands.entity(map_entity).add_child(chunk_entity);
    }

    // Re-enqueue if chunk can advance further
    let col = chunk_to_column(result.position);
    if let Some(&level) = instance.chunk_levels.get(&col) {
        let max_status = ChunkStatus::max_for_level(level);
        if completed_status < max_status {
            gen_queue.heap.push(ChunkWork {
                position: result.position,
                effective_level: level,
                distance_to_source: propagator.min_distance_to_source(col),
            });
        }
    }
}

/// Whether a 2D column is within the map's optional bounds.
fn is_column_within_bounds(col: IVec2, bounds: Option<IVec3>) -> bool {
    match bounds {
        Some(b) => col.x.abs() < b.x && col.y.abs() < b.z,
        None => true,
    }
}

/// Whether a 3D chunk position is within the map's optional bounds.
fn is_within_bounds(pos: IVec3, bounds: Option<IVec3>) -> bool {
    match bounds {
        Some(b) => pos.x.abs() < b.x && pos.y.abs() < b.y && pos.z.abs() < b.z,
        None => true,
    }
}

pub fn world_to_chunk_pos(translation: Vec3, chunk_size: u32) -> IVec3 {
    (translation / chunk_size as f32).floor().as_ivec3()
}

fn chunk_world_offset(chunk_pos: IVec3, chunk_size: u32) -> Vec3 {
    chunk_pos.as_vec3() * chunk_size as f32 - Vec3::ONE
}

/// Despawn chunk entities whose column is no longer in the parent map's `chunk_levels`.
pub fn despawn_out_of_range_chunks(
    mut commands: Commands,
    chunk_query: Query<(Entity, &VoxelChunk, &ChildOf)>,
    map_query: Query<&VoxelMapInstance>,
) {
    for (entity, chunk, child_of) in &chunk_query {
        debug_assert!(
            map_query.get(child_of.0).is_ok(),
            "VoxelChunk {:?} at {:?} is child of {:?} which has no VoxelMapInstance",
            entity,
            chunk.position,
            child_of.0
        );
        let Ok(instance) = map_query.get(child_of.0) else {
            warn!(
                "VoxelChunk entity {:?} has ChildOf pointing to non-map entity {:?}",
                entity, child_of.0
            );
            continue;
        };

        if !instance
            .chunk_levels
            .contains_key(&chunk_to_column(chunk.position))
        {
            trace!(
                "despawn_out_of_range_chunks: despawning chunk {:?} at {:?} (parent map {:?})",
                entity, chunk.position, child_of.0
            );
            commands.entity(entity).despawn();
        }
    }
}

/// Drains `chunks_needing_remesh` and spawns async mesh tasks from existing octree data,
/// prioritized by distance to nearest ticket source.
pub fn spawn_remesh_tasks(
    mut map_query: Query<(
        &mut VoxelMapInstance,
        &mut PendingRemeshes,
        &ChunkWorkBudget,
        &TicketLevelPropagator,
        &mut ChunkWorkTracker,
    )>,
) {
    let pool = AsyncComputeTaskPool::get();
    for (mut instance, mut pending, budget, propagator, mut tracker) in &mut map_query {
        if pending.tasks.len() >= MAX_PENDING_REMESH_TASKS {
            plot!(
                "remesh_spawn_stop_reason",
                StopReason::InFlightCap as u8 as f64
            );
            plot!("remesh_spawned_this_frame", 0.0);
            plot!("remesh_tasks_in_flight", pending.tasks.len() as f64);
            return;
        }

        let mut heap = BinaryHeap::new();
        for &pos in instance.chunks_needing_remesh.iter() {
            let col = chunk_to_column(pos);
            heap.push(ChunkWork {
                position: pos,
                effective_level: 0,
                distance_to_source: propagator.min_distance_to_source(col),
            });
        }

        let mut spawned = 0;
        while let Some(work) = heap.pop() {
            if pending.tasks.len() >= MAX_PENDING_REMESH_TASKS {
                break;
            }
            if !budget.has_time() || spawned >= MAX_REMESH_SPAWNS_PER_FRAME {
                break;
            }
            if tracker.generating.contains(&work.position)
                || tracker.remeshing.contains(&work.position)
            {
                // Leave in chunks_needing_remesh for next frame
                continue;
            }
            let Some(chunk_data) = instance.get_chunk_data(work.position) else {
                trace!(
                    "spawn_remesh_tasks: chunk {} no longer in octree, skipping",
                    work.position
                );
                instance.chunks_needing_remesh.remove(&work.position);
                continue;
            };
            if chunk_data.fill_type == crate::types::FillType::Empty {
                trace!(
                    "spawn_remesh_tasks: chunk {} is empty, skipping remesh",
                    work.position
                );
                instance.chunks_needing_remesh.remove(&work.position);
                continue;
            }
            let voxels = {
                let _span = info_span!("expand_palette").entered();
                chunk_data.voxels.to_voxels()
            };
            let shape = instance.shape.clone();
            let task = pool.spawn(async move { mesh_chunk_greedy(&voxels, &shape) });
            pending.tasks.push(RemeshTask {
                chunk_pos: work.position,
                task,
            });
            tracker.remeshing.insert(work.position);
            instance.chunks_needing_remesh.remove(&work.position);
            spawned += 1;
        }

        let stop_reason = if pending.tasks.len() >= MAX_PENDING_REMESH_TASKS {
            StopReason::InFlightCap
        } else if spawned >= MAX_REMESH_SPAWNS_PER_FRAME {
            StopReason::HardCap
        } else if !budget.has_time() {
            StopReason::TimeBudget
        } else {
            StopReason::Completed
        };
        plot!("remesh_spawn_stop_reason", stop_reason as u8 as f64);
        plot!("remesh_spawned_this_frame", spawned as f64);
        plot!("remesh_tasks_in_flight", pending.tasks.len() as f64);
    }
}

/// Polls completed remesh tasks and swaps meshes on existing chunk entities.
pub fn poll_remesh_tasks(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    default_material: Res<DefaultVoxelMaterial>,
    mut map_query: Query<(
        Entity,
        &VoxelMapInstance,
        &mut PendingRemeshes,
        &mut ChunkWorkTracker,
    )>,
    chunk_query: Query<(Entity, &VoxelChunk, &ChildOf)>,
) {
    for (map_entity, instance, mut pending, mut tracker) in &mut map_query {
        let mut i = 0;
        let mut polled = 0;
        // NOTE: Polling does NOT check budget — same reasoning as poll_chunk_tasks.
        while i < pending.tasks.len() && polled < MAX_REMESH_POLLS_PER_FRAME {
            let Some(mesh_opt) = check_ready(&mut pending.tasks[i].task) else {
                i += 1;
                continue;
            };
            let remesh = pending.tasks.swap_remove(i);
            tracker.remeshing.remove(&remesh.chunk_pos);
            polled += 1;

            if !instance
                .chunk_levels
                .contains_key(&chunk_to_column(remesh.chunk_pos))
            {
                continue;
            }

            let existing = chunk_query
                .iter()
                .find(|(_, vc, parent)| vc.position == remesh.chunk_pos && parent.0 == map_entity);

            match (mesh_opt, existing) {
                (Some(mesh), Some((entity, _, _))) => {
                    let handle = meshes.add(mesh);
                    commands.entity(entity).insert(Mesh3d(handle));
                }
                (Some(mesh), None) => {
                    let handle = meshes.add(mesh);
                    let offset = chunk_world_offset(remesh.chunk_pos, instance.chunk_size);
                    let material = if instance.debug_colors {
                        materials.add(StandardMaterial {
                            base_color: color_from_chunk_pos(remesh.chunk_pos),
                            perceptual_roughness: 0.9,
                            ..default()
                        })
                    } else {
                        default_material.0.clone()
                    };
                    let chunk_entity = commands
                        .spawn((
                            VoxelChunk {
                                position: remesh.chunk_pos,
                                lod_level: 0,
                            },
                            Mesh3d(handle),
                            MeshMaterial3d(material),
                            Transform::from_translation(offset),
                        ))
                        .id();
                    commands.entity(map_entity).add_child(chunk_entity);
                }
                (None, Some((entity, _, _))) => {
                    commands.entity(entity).despawn();
                }
                (None, None) => {}
            }
        }

        let stop_reason = if polled >= MAX_REMESH_POLLS_PER_FRAME {
            StopReason::HardCap
        } else {
            StopReason::Completed
        };
        plot!("remesh_poll_stop_reason", stop_reason as u8 as f64);
        plot!("remesh_polled_this_frame", polled as f64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_to_chunk_pos_positive() {
        let pos = world_to_chunk_pos(Vec3::new(20.0, 0.0, 5.0), 16);
        assert_eq!(pos, IVec3::new(1, 0, 0));
    }

    #[test]
    fn world_to_chunk_pos_negative() {
        let pos = world_to_chunk_pos(Vec3::new(-1.0, -17.0, 0.0), 16);
        assert_eq!(pos, IVec3::new(-1, -2, 0));
    }

    #[test]
    fn chunk_world_offset_calculation() {
        let offset = chunk_world_offset(IVec3::new(1, 2, 3), 16);
        assert_eq!(offset, Vec3::new(15.0, 31.0, 47.0));
    }

    #[test]
    fn world_to_column_pos_drops_y() {
        let col = world_to_column_pos(Vec3::new(20.0, 99.0, 5.0), 16);
        assert_eq!(col, IVec2::new(1, 0));
    }
}
