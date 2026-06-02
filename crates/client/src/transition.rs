use std::sync::Arc;

use avian3d::prelude::{Collider, ColliderDisabled, LinearVelocity, Position, RigidBodyDisabled};
use bevy::prelude::*;
use lightyear::prelude::*;
use protocol::map::*;
use protocol::transition::*;
use protocol::world_object::WorldObjectId;
use protocol::{
    CharacterMarker, MapInstanceId, MapRegistry, PendingTransition, TerrainDefRegistry,
};
use ui::state::MapTransitionState;
use voxel_map_engine::lifecycle::ChunkWorkTracker;
use voxel_map_engine::prelude::*;

use crate::map::VoxelPredictionState;

/// Receives MapTransitionEnd, sets end_received flag.
pub fn receive_transition_end(
    mut receivers: Query<&mut MessageReceiver<MapTransitionEnd>>,
    mut state: ResMut<ClientTransitionState>,
) {
    for mut receiver in &mut receivers {
        for _end in receiver.receive() {
            trace!("Received MapTransitionEnd");
            state.end_received = true;
        }
    }
}

/// Receives MapTransitionEntity, accumulates server entity IDs.
pub fn receive_transition_entities(
    mut receivers: Query<&mut MessageReceiver<MapTransitionEntity>>,
    mut state: ResMut<ClientTransitionState>,
) {
    for mut receiver in &mut receivers {
        for msg in receiver.receive() {
            trace!("Received MapTransitionEntity entity={:?}", msg.entity);
            state.pending_entities.push(msg.entity);
        }
    }
}

/// Handles MapTransitionStart for both initial connect and mid-game.
/// Shows loading screen, despawns old maps, clears prediction state,
/// freezes player (if exists), spawns new VoxelMapInstance, starts state machine.
pub fn on_transition_start_received(
    mut commands: Commands,
    mut receivers: Query<&mut MessageReceiver<MapTransitionStart>>,
    mut registry: ResMut<MapRegistry>,
    terrain_registry: Res<TerrainDefRegistry>,
    player_query: Query<Entity, (With<Predicted>, With<CharacterMarker>, With<Controlled>)>,
    world_objects: Query<(Entity, &MapInstanceId), With<WorldObjectId>>,
    mut transition_state: ResMut<ClientTransitionState>,
    mut prediction_state: ResMut<VoxelPredictionState>,
    mut next_transition: ResMut<NextState<MapTransitionState>>,
) {
    for mut receiver in &mut receivers {
        for transition in receiver.receive() {
            trace!(
                "MapTransitionStart target={:?} chunk_size={}",
                transition.target,
                transition.chunk_size
            );

            // Show loading screen
            next_transition.set(MapTransitionState::Transitioning);

            // Clear prediction state
            prediction_state.pending.clear();

            // Despawn old maps (no-op on initial connect)
            despawn_all_maps_except(&mut commands, &mut registry, &transition.target);
            despawn_foreign_world_objects(&mut commands, &world_objects, &transition.target);

            // Freeze player if one exists (mid-game only)
            if let Ok(player) = player_query.single() {
                commands.entity(player).insert((
                    RigidBodyDisabled,
                    ColliderDisabled,
                    DisableRollback,
                    PendingTransition(transition.target.clone()),
                    Position(transition.spawn_position),
                    LinearVelocity(Vec3::ZERO),
                ));
            }

            // Spawn new VoxelMapInstance if not already in registry
            if !registry.0.contains_key(&transition.target) {
                let map_entity =
                    spawn_map_from_transition(&mut commands, &transition, &terrain_registry);
                registry.insert(transition.target.clone(), map_entity);
            }

            let map_entity = registry.get(&transition.target);

            // Update ChunkTicket on player if exists
            if let Ok(player) = player_query.single() {
                commands
                    .entity(player)
                    .insert(ChunkTicket::map_transition(map_entity));
            }
            // On initial connect, no player exists yet. The server's ChunkTicket
            // on the server-side character entity drives chunk sending via
            // push_chunks_to_clients. Client doesn't need a local ChunkTicket
            // to receive chunks.

            // Start state machine
            transition_state.begin(&transition);
        }
    }
}

/// Spawn a client-side VoxelMapInstance from a MapTransitionStart message.
fn spawn_map_from_transition(
    commands: &mut Commands,
    transition: &MapTransitionStart,
    terrain_registry: &TerrainDefRegistry,
) -> Entity {
    let def_name = terrain_def_name(&transition.target);
    let terrain_def = terrain_registry
        .get(def_name)
        .expect("terrain def must be loaded");
    let dimensions = terrain_def
        .map_dimensions()
        .expect("terrain def must contain MapDimensions");

    let padded = dimensions.padded_size();
    let generator = VoxelGenerator(Arc::new(FlatGenerator {
        chunk_size: dimensions.chunk_size,
        shape: RuntimeShape::<u32, 3>::new([padded, padded, padded]),
    }));

    let spawning_distance = dimensions
        .bounds
        .map(|b| b.max_element().max(1) as u32)
        .unwrap_or(10);
    let config = VoxelMapConfig::new(transition.seed, 0, spawning_distance, false);

    let entity = commands
        .spawn((
            VoxelMapInstance::new(dimensions.tree_height, dimensions.chunk_size),
            config,
            dimensions.clone(),
            generator,
            Transform::default(),
            transition.target.clone(),
        ))
        .id();

    trace!(
        "Spawned client map instance for {:?}: {entity:?}",
        transition.target
    );
    entity
}

fn terrain_def_name(map_id: &MapInstanceId) -> &'static str {
    match map_id {
        MapInstanceId::Overworld => "overworld",
        MapInstanceId::Homebase { .. } => "homebase",
    }
}

/// Despawn all map entities except the transition target.
fn despawn_all_maps_except(
    commands: &mut Commands,
    registry: &mut MapRegistry,
    keep: &MapInstanceId,
) {
    let to_remove: Vec<(MapInstanceId, Entity)> = registry
        .0
        .iter()
        .filter(|(id, _)| *id != keep)
        .map(|(id, &entity)| (id.clone(), entity))
        .collect();
    for (map_id, map_entity) in to_remove {
        trace!("Despawning map {map_id:?} entity {map_entity:?}");
        registry.0.remove(&map_id);
        commands.entity(map_entity).despawn();
    }
}

/// Despawn replicated world objects that don't belong to the transition target map.
fn despawn_foreign_world_objects(
    commands: &mut Commands,
    world_objects: &Query<(Entity, &MapInstanceId), With<WorldObjectId>>,
    keep: &MapInstanceId,
) {
    let mut count = 0;
    for (entity, map_id) in world_objects {
        if map_id != keep {
            commands.entity(entity).despawn();
            count += 1;
        }
    }
    if count > 0 {
        trace!("Despawned {count} world objects from previous map");
    }
}

/// Main state machine driver. Evaluates phase gates, advances phase.
pub fn update_transition_state(
    mut commands: Commands,
    mut state: ResMut<ClientTransitionState>,
    registry: Res<MapRegistry>,
    instance_query: Query<(&VoxelMapInstance, Option<&ChunkWorkTracker>, &MapDimensions)>,
    children_query: Query<&Children>,
    chunk_query: Query<(&VoxelChunk, Has<Collider>), With<Mesh3d>>,
    mut ready_senders: Query<&mut MessageSender<MapTransitionReady>>,
    manager_query: Query<&MessageManager, With<Client>>,
    entity_exists: Query<Entity>,
    mut next_transition: ResMut<NextState<MapTransitionState>>,
    player_query: Query<Entity, (With<Predicted>, With<protocol::CharacterMarker>)>,
) {
    match state.phase {
        TransitionPhase::Idle => (),

        TransitionPhase::Cleanup => {
            let Some(target) = &state.target_map else {
                debug_assert!(false, "Cleanup phase with no target_map");
                return;
            };
            let Some(&map_entity) = registry.0.get(target) else {
                trace!("Cleanup: target map {target:?} not yet in registry");
                return;
            };
            if instance_query.get(map_entity).is_err() {
                trace!("Cleanup: map entity {map_entity:?} missing VoxelMapInstance");
                return;
            }
            trace!("Transition: Cleanup -> Loading");
            state.phase = TransitionPhase::Loading;
        }

        TransitionPhase::Loading => {
            if !check_spatial_readiness(
                &state,
                &registry,
                &instance_query,
                &children_query,
                &chunk_query,
            ) {
                return;
            }
            for mut sender in &mut ready_senders {
                sender.send::<MapChannel>(MapTransitionReady);
            }
            trace!("Transition: Loading -> Ready (sent MapTransitionReady)");
            state.phase = TransitionPhase::Ready;
        }

        TransitionPhase::Ready => {
            if !state.end_received {
                return;
            }
            if !state.pending_entities.is_empty()
                && !check_entities_resolved(&state, &manager_query, &entity_exists)
            {
                return;
            }
            trace!("Transition: Ready -> Complete");
            state.phase = TransitionPhase::Complete;
        }

        TransitionPhase::Complete => {
            // Unfreeze player entities (lightyear may recreate predicted
            // entity during transition, so iterate all matches)
            for entity in &player_query {
                commands.entity(entity).remove::<(
                    avian3d::prelude::RigidBodyDisabled,
                    avian3d::prelude::ColliderDisabled,
                    DisableRollback,
                    protocol::PendingTransition,
                    protocol::TransitionReadySent,
                )>();
            }
            next_transition.set(MapTransitionState::Playing);
            state.reset();
            trace!("Transition: Complete -> Idle");
        }
    }
}

/// Check that all columns within Chebyshev radius of spawn have loaded data,
/// no pending remesh work, and meshed chunks have colliders.
///
/// Queries are split so each gate only blocks on the components it needs:
/// - VoxelMapInstance: always present after spawn
/// - ChunkWorkTracker: added by ensure_pending_chunks (1 frame after spawn)
/// - Children: added when first chunk mesh entity is spawned (requires full pipeline)
fn check_spatial_readiness(
    state: &ClientTransitionState,
    registry: &MapRegistry,
    instance_query: &Query<(&VoxelMapInstance, Option<&ChunkWorkTracker>, &MapDimensions)>,
    children_query: &Query<&Children>,
    chunk_query: &Query<(&VoxelChunk, Has<Collider>), With<Mesh3d>>,
) -> bool {
    let Some(target) = state.target_map.as_ref() else {
        trace!("spatial_readiness: no target_map");
        return false;
    };
    let Some(&map_entity) = registry.0.get(target) else {
        trace!("spatial_readiness: target {target:?} not in registry");
        return false;
    };
    let Ok((instance, tracker, dimensions)) = instance_query.get(map_entity) else {
        trace!("spatial_readiness: {map_entity:?} missing VoxelMapInstance");
        return false;
    };
    let Some(tracker) = tracker else {
        trace!("spatial_readiness: {map_entity:?} missing ChunkWorkTracker, waiting for voxel engine init");
        return false;
    };

    let radius = state.readiness_radius as i32;
    let (y_min, y_max) = state.column_y_range;
    let bounds = dimensions.bounds;

    for dx in -radius..=radius {
        for dz in -radius..=radius {
            let col = IVec2::new(state.spawn_column.x + dx, state.spawn_column.y + dz);

            // Skip columns outside the map's finite bounds -- the server
            // never generates data for them (is_column_within_bounds).
            if let Some(b) = bounds {
                if col.x.abs() >= b.x || col.y.abs() >= b.z {
                    continue;
                }
            }

            if !instance.chunk_levels.contains_key(&col) {
                trace!(
                    "spatial_readiness: column {col} not in chunk_levels (have {} cols)",
                    instance.chunk_levels.len()
                );
                return false;
            }

            for y in y_min..=y_max {
                let pos = IVec3::new(col.x, y, col.y);
                if instance.chunks_needing_remesh.contains(&pos) || tracker.remeshing.contains(&pos)
                {
                    trace!("spatial_readiness: chunk {pos} still pending remesh/in-flight");
                    return false;
                }
            }
        }
    }

    // Mesh entities (Children) must exist before we declare readiness --
    // they are the physical terrain the player stands on.
    let Ok(children) = children_query.get(map_entity) else {
        trace!("spatial_readiness: no mesh children yet, waiting for mesh pipeline");
        return false;
    };

    // Verify VoxelChunk children within radius have Collider.
    for child in children.iter() {
        if let Ok((chunk, has_collider)) = chunk_query.get(child) {
            let chunk_col = IVec2::new(chunk.position.x, chunk.position.z);
            let dx = (chunk_col.x - state.spawn_column.x).abs();
            let dz = (chunk_col.y - state.spawn_column.y).abs();
            if dx <= radius && dz <= radius && !has_collider {
                trace!(
                    "spatial_readiness: chunk at {:?} within radius missing Collider",
                    chunk.position
                );
                return false;
            }
        }
    }

    true
}

/// Check that all pending server entity IDs have been mapped to local entities.
fn check_entities_resolved(
    state: &ClientTransitionState,
    manager_query: &Query<&MessageManager, With<Client>>,
    entity_exists: &Query<Entity>,
) -> bool {
    let Ok(manager) = manager_query.single() else {
        return false;
    };
    state.pending_entities.iter().all(|&remote| {
        manager
            .entity_mapper
            .get_local(remote)
            .is_some_and(|local| entity_exists.get(local).is_ok())
    })
}

/// Per-frame safety net: despawn any Replicated entity whose MapInstanceId
/// doesn't match any registered map. Primary defense is per-handler guards;
/// this catches omissions. Only runs during transitions.
pub fn cleanup_stale_map_entities(
    mut commands: Commands,
    registry: Res<MapRegistry>,
    stale_query: Query<(Entity, &MapInstanceId), With<Replicated>>,
    state: Res<ClientTransitionState>,
) {
    if state.phase == TransitionPhase::Idle {
        return;
    }
    for (entity, mid) in &stale_query {
        if !registry.0.contains_key(mid) {
            trace!("Safety-net: despawning stale entity {entity:?} map {mid:?}");
            commands.entity(entity).despawn();
        }
    }
}

pub struct ClientTransitionPlugin;

impl Plugin for ClientTransitionPlugin {
    fn build(&self, app: &mut App) {
        use ui::state::ClientState;
        let in_game = in_state(ClientState::InGame);
        // Transition handler must flush before chunk sync runs so the
        // newly spawned map entity is in the registry. Without this,
        // ChunkDataSync arriving on the same frame as MapTransitionStart
        // would be silently dropped (registry lookup fails) and the
        // server never re-sends them.
        app.add_systems(
            Update,
            (
                on_transition_start_received.run_if(resource_exists::<TerrainDefRegistry>),
                ApplyDeferred,
                (
                    crate::map::handle_chunk_data_sync,
                    crate::map::handle_unload_column,
                    crate::map::attach_chunk_ticket_to_player,
                    protocol::attach_chunk_colliders,
                ),
                (
                    receive_transition_end,
                    receive_transition_entities,
                    update_transition_state,
                    cleanup_stale_map_entities,
                )
                    .chain(),
            )
                .chain()
                .run_if(in_game),
        );
    }
}
