use bevy::prelude::*;
use lightyear::prelude::MessageSender;
use nostr_map_persistence::MapPersistenceRejection;
use persistence::{PendingStoreOps, StoreBackend};
use protocol::map::MapTransitionStart;
use protocol::{MapInstanceId, MapRegistry, RespawnPoint, TerrainDefRegistry};
use voxel_map_engine::prelude::{MapDimensions, VoxelMapConfig};

use crate::persistence::fs_map_meta::FsMapMetaStore;
use crate::persistence::{map_save_dir, MapMeta, WorldSavePath};

use super::{
    configure_map_from_meta, ensure_map_exists, seed_from_nostr_public_key, ActiveMapPreflight,
    MapLoadState, MapPersistencePreflightDecision, MapPreflightKind, MapPreflightStage,
    MapPreparation, PendingMapPreflight, PendingMapPreflights, PendingMapSwitchPreflight,
    RoomRegistry, DEFAULT_OVERWORLD_SEED, GENERATION_VERSION,
};

/// Spawns at most one concrete filesystem preflight task from the pending queue.
pub fn spawn_map_preflight_tasks(
    mut commands: Commands,
    mut queue: ResMut<PendingMapPreflights>,
    active: Query<&ActiveMapPreflight>,
) {
    if !active.is_empty() {
        trace!("map preflight already active; waiting before spawning another");
        return;
    }
    let Some(request) = queue.0.pop_front() else {
        trace!("no pending map persistence preflight requests");
        return;
    };
    commands.spawn((
        ActiveMapPreflight {
            request,
            stage: MapPreflightStage::LoadFilesystemMeta,
        },
        PendingStoreOps::<(), MapMeta>::default(),
    ));
}

/// Polls filesystem-only Phase 1 preflight and applies the selected backend state.
pub fn poll_map_persistence_preflight(
    mut commands: Commands,
    mut active: Query<(
        Entity,
        &mut ActiveMapPreflight,
        &mut PendingStoreOps<(), MapMeta>,
    )>,
    meta_stores: Query<&StoreBackend<(), MapMeta, FsMapMetaStore>>,
    mut map_states: Query<&mut MapLoadState>,
    mut registry: ResMut<MapRegistry>,
    save_path: Res<WorldSavePath>,
    terrain_registry: Option<Res<TerrainDefRegistry>>,
    type_registry: Res<AppTypeRegistry>,
) {
    let Some(terrain_registry) = terrain_registry else {
        trace!("terrain registry not loaded; map persistence preflight is waiting");
        return;
    };

    for (entity, mut preflight, mut meta_ops) in &mut active {
        meta_ops.poll();
        if let Some((_, error)) = meta_ops.load_errors.pop() {
            let rejection = MapPersistenceRejection::Filesystem(error.to_string());
            block_preflight_target(
                &registry,
                &mut map_states,
                &preflight.request.target_map_id,
                rejection,
            );
            clear_pending_switch_marker(&mut commands, &preflight.request.kind);
            commands.entity(entity).despawn();
            continue;
        }

        match preflight.stage {
            MapPreflightStage::LoadFilesystemMeta => {
                let Some(map_entity) = ensure_preflight_target_registered(
                    &mut commands,
                    &mut registry,
                    &save_path,
                    &preflight.request.target_map_id,
                ) else {
                    trace!(?preflight.request.target_map_id, "preflight target placeholder was just spawned; waiting for commands to apply");
                    continue;
                };
                let store = meta_stores.get(map_entity).expect(
                    "preflight target must have FsMapMetaStore backend before metadata load",
                );
                meta_ops.spawn_load(&store.0, ());
                preflight.stage = MapPreflightStage::WaitingFilesystemMeta;
            }
            MapPreflightStage::WaitingFilesystemMeta if meta_ops.completed_loads.is_empty() => {
                trace!(?preflight.request.target_map_id, "waiting for filesystem metadata preflight load");
                continue;
            }
            MapPreflightStage::WaitingFilesystemMeta => {
                let (_, loaded_meta) = meta_ops
                    .completed_loads
                    .pop()
                    .expect("checked completed filesystem metadata load exists");
                let decision = loaded_meta
                    .map(MapPersistencePreflightDecision::UseFilesystem)
                    .unwrap_or(MapPersistencePreflightDecision::Missing);
                apply_preflight_result(
                    &mut commands,
                    &registry,
                    &mut map_states,
                    &save_path,
                    &terrain_registry,
                    &type_registry,
                    &preflight.request,
                    decision,
                );
                match preflight.request.kind {
                    MapPreflightKind::StartupOverworld => commands.entity(entity).despawn(),
                    MapPreflightKind::MapSwitch { .. } => {
                        preflight.stage = MapPreflightStage::CommitTransition;
                    }
                }
            }
            MapPreflightStage::CommitTransition => {
                trace!(?preflight.request.target_map_id, "map preflight ready for transition commit");
            }
            MapPreflightStage::DecideFilesystemOnly
            | MapPreflightStage::MaterializeRemote
            | MapPreflightStage::PrepareMap => {
                trace!(?preflight.stage, "preflight stage is reserved for later phases");
                continue;
            }
        }
    }
}

/// Commits map switches after preflight-selected filesystem loading reaches Ready.
#[allow(clippy::too_many_arguments)]
pub fn commit_ready_map_preflights(
    mut commands: Commands,
    mut active: Query<(Entity, &ActiveMapPreflight)>,
    mut registry: ResMut<MapRegistry>,
    map_state_query: Query<&MapLoadState>,
    map_params_query: Query<(&VoxelMapConfig, &MapDimensions)>,
    save_path: Res<WorldSavePath>,
    mut room_registry: ResMut<RoomRegistry>,
    mut senders: Query<&mut MessageSender<MapTransitionStart>>,
    respawn_query: Query<(&avian3d::prelude::Position, &MapInstanceId), With<RespawnPoint>>,
) {
    for (entity, preflight) in &mut active {
        if preflight.stage != MapPreflightStage::CommitTransition {
            trace!(?preflight.stage, "active preflight is not ready to commit transition");
            continue;
        }
        let MapPreflightKind::MapSwitch {
            client_entity,
            player_entity,
            ref current_map_id,
            ..
        } = preflight.request.kind
        else {
            trace!("startup preflight has no transition to commit");
            continue;
        };

        match ensure_map_exists(
            &mut commands,
            &mut registry,
            &map_state_query,
            &map_params_query,
            &save_path,
            &preflight.request.target_map_id,
        ) {
            MapPreparation::Ready {
                entity: map_entity,
                params,
            } => {
                crate::transition::commit_map_transition(
                    &mut commands,
                    player_entity,
                    client_entity,
                    current_map_id,
                    &preflight.request.target_map_id,
                    map_entity,
                    params,
                    &mut room_registry,
                    &mut senders,
                    &respawn_query,
                );
                commands
                    .entity(player_entity)
                    .remove::<PendingMapSwitchPreflight>();
                commands.entity(entity).despawn();
            }
            MapPreparation::Pending => {
                trace!(?preflight.request.target_map_id, "waiting for prepared map to become ready before transition commit");
                continue;
            }
            MapPreparation::Blocked(reason) => {
                warn!(?preflight.request.target_map_id, ?reason, "map switch blocked by persistence preflight");
                commands
                    .entity(player_entity)
                    .remove::<PendingMapSwitchPreflight>();
                commands.entity(entity).despawn();
            }
        }
    }
}

fn ensure_preflight_target_registered(
    commands: &mut Commands,
    registry: &mut MapRegistry,
    save_path: &WorldSavePath,
    target_map_id: &MapInstanceId,
) -> Option<Entity> {
    if let Some(&entity) = registry.0.get(target_map_id) {
        return Some(entity);
    }
    match target_map_id {
        MapInstanceId::Overworld => panic!("overworld must be registered before preflight"),
        MapInstanceId::Homebase { owner } => {
            let entity = super::spawn_homebase_preflight_placeholder_with_stores(
                commands,
                save_path,
                *owner,
                MapLoadState::CheckingPersistence,
            );
            registry.0.insert(target_map_id.clone(), entity);
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_preflight_result(
    commands: &mut Commands,
    registry: &MapRegistry,
    map_states: &mut Query<&mut MapLoadState>,
    save_path: &WorldSavePath,
    terrain_registry: &TerrainDefRegistry,
    type_registry: &AppTypeRegistry,
    request: &PendingMapPreflight,
    decision: MapPersistencePreflightDecision,
) {
    match decision {
        MapPersistencePreflightDecision::UseFilesystem(meta) => {
            configure_preflight_map(
                commands,
                registry,
                map_states,
                save_path,
                terrain_registry,
                type_registry,
                &request.target_map_id,
                meta.seed,
                meta.generation_version,
            );
        }
        MapPersistencePreflightDecision::Missing
        | MapPersistencePreflightDecision::RemoteUnavailable => {
            let seed = default_seed_for_map(&request.target_map_id);
            configure_preflight_map(
                commands,
                registry,
                map_states,
                save_path,
                terrain_registry,
                type_registry,
                &request.target_map_id,
                seed,
                GENERATION_VERSION,
            );
        }
        MapPersistencePreflightDecision::UseRemote(_save) => {
            block_preflight_target(
                registry,
                map_states,
                &request.target_map_id,
                MapPersistenceRejection::Invalid(
                    "remote materialization is not available in Phase 1".to_string(),
                ),
            );
        }
        MapPersistencePreflightDecision::Blocked(rejection) => {
            block_preflight_target(registry, map_states, &request.target_map_id, rejection);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn configure_preflight_map(
    commands: &mut Commands,
    registry: &MapRegistry,
    map_states: &mut Query<&mut MapLoadState>,
    save_path: &WorldSavePath,
    terrain_registry: &TerrainDefRegistry,
    type_registry: &AppTypeRegistry,
    map_id: &MapInstanceId,
    seed: u64,
    generation_version: u32,
) {
    let entity = registry.get(map_id);
    let map_dir = map_save_dir(&save_path.0, map_id);
    configure_map_from_meta(
        commands,
        entity,
        map_id,
        seed,
        generation_version,
        &map_dir,
        terrain_registry,
        type_registry,
    );
    let mut state = map_states
        .get_mut(entity)
        .expect("preflight target map must have MapLoadState");
    *state = MapLoadState::AwaitingEntities;
}

fn block_preflight_target(
    registry: &MapRegistry,
    map_states: &mut Query<&mut MapLoadState>,
    map_id: &MapInstanceId,
    rejection: MapPersistenceRejection,
) {
    let entity = registry.get(map_id);
    let mut state = map_states
        .get_mut(entity)
        .expect("preflight target map must have MapLoadState");
    *state = MapLoadState::Blocked(rejection);
}

fn clear_pending_switch_marker(commands: &mut Commands, kind: &MapPreflightKind) {
    let MapPreflightKind::MapSwitch { player_entity, .. } = kind else {
        trace!("startup preflight has no pending switch marker to clear");
        return;
    };
    commands
        .entity(*player_entity)
        .remove::<PendingMapSwitchPreflight>();
}

fn default_seed_for_map(map_id: &MapInstanceId) -> u64 {
    match map_id {
        MapInstanceId::Overworld => DEFAULT_OVERWORLD_SEED,
        MapInstanceId::Homebase { owner } => seed_from_nostr_public_key(*owner),
    }
}
