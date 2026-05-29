use bevy::prelude::*;
use bevy::tasks::{futures::check_ready, IoTaskPool, Task};
use lightyear::prelude::MessageSender;
use nostr_map_persistence::{
    download_payloads, fetch_manifest_ancestors, latest_visible_manifest, validate_remote_map_save,
    verify_revision_chain, MapPersistenceRejection, RawSaveBase, RevisionDecision,
};
use persistence::{PendingStoreOps, StoreBackend};
use protocol::map::MapTransitionStart;
use protocol::{MapInstanceId, MapRegistry, RespawnPoint, TerrainDefRegistry};
use voxel_map_engine::prelude::{MapDimensions, VoxelMapConfig};

use crate::persistence::fs_map_meta::FsMapMetaStore;
use crate::persistence::{
    install_active_revision_store_backends, map_save_dir, materialize_validated_map_save,
    store_map_dir_for_loading, FakeRemoteMapRestores, MapMeta, RemoteMapPersistenceConfig,
    RemoteMapReadContext, ServerValidatedMapSave, WorldSavePath,
};

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
    mut already_waiting: Local<bool>,
) {
    if !active.is_empty() {
        if !*already_waiting {
            trace!("map preflight already active; waiting before spawning another");
            *already_waiting = true;
        }
        return;
    }
    *already_waiting = false;
    let Some(request) = queue.0.pop_front() else {
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
        Option<&mut RemotePreflightTask>,
    )>,
    meta_stores: Query<&StoreBackend<(), MapMeta, FsMapMetaStore>>,
    mut map_states: Query<&mut MapLoadState>,
    mut registry: ResMut<MapRegistry>,
    save_path: Res<WorldSavePath>,
    remote_config: Res<RemoteMapPersistenceConfig>,
    fake_remote_restores: Option<Res<FakeRemoteMapRestores>>,
    remote_read_context: Option<Res<RemoteMapReadContext>>,
    server_identity: Res<nostr_client::NostrKeys>,
    terrain_registry: Option<Res<TerrainDefRegistry>>,
    type_registry: Res<AppTypeRegistry>,
) {
    let Some(terrain_registry) = terrain_registry else {
        trace!("terrain registry not loaded; map persistence preflight is waiting");
        return;
    };

    for (entity, mut preflight, mut meta_ops, remote_task) in &mut active {
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
                if preflight.is_changed() {
                    trace!(?preflight.request.target_map_id, "waiting for filesystem metadata preflight load");
                }
                continue;
            }
            MapPreflightStage::WaitingFilesystemMeta => {
                let (_, loaded_meta) = meta_ops
                    .completed_loads
                    .pop()
                    .expect("checked completed filesystem metadata load exists");
                let owner = map_remote_owner(&preflight.request.target_map_id, &server_identity);
                match begin_remote_or_filesystem_decision(
                    &mut commands,
                    entity,
                    &preflight.request.target_map_id,
                    owner,
                    loaded_meta,
                    &remote_config,
                    fake_remote_restores.as_deref(),
                    remote_read_context.as_deref(),
                ) {
                    Some(decision) => finish_preflight_decision(
                        &mut commands,
                        &registry,
                        &mut map_states,
                        &save_path,
                        &terrain_registry,
                        &type_registry,
                        entity,
                        &mut preflight,
                        decision,
                    ),
                    None => preflight.stage = MapPreflightStage::WaitingRemoteDecision,
                }
            }
            MapPreflightStage::WaitingRemoteDecision => {
                let Some(mut remote_task) = remote_task else {
                    if preflight.is_changed() {
                        trace!(?preflight.request.target_map_id, "remote preflight task component not yet applied; waiting");
                    }
                    continue;
                };
                let Some(result) = check_ready(&mut remote_task.task) else {
                    if preflight.is_changed() {
                        trace!(?preflight.request.target_map_id, "waiting for remote map persistence preflight");
                    }
                    continue;
                };
                let decision = remote_decision_from_result(result, remote_task.loaded_meta.take());
                commands.entity(entity).remove::<RemotePreflightTask>();
                finish_preflight_decision(
                    &mut commands,
                    &registry,
                    &mut map_states,
                    &save_path,
                    &terrain_registry,
                    &type_registry,
                    entity,
                    &mut preflight,
                    decision,
                );
            }
            MapPreflightStage::CommitTransition => {
                trace!(?preflight.request.target_map_id, "map preflight ready for transition commit");
            }
        }
    }
}

fn map_remote_owner(
    map_id: &MapInstanceId,
    server_identity: &nostr_client::NostrKeys,
) -> protocol::NostrPublicKey {
    match map_id {
        MapInstanceId::Overworld => server_identity.protocol_public_key(),
        MapInstanceId::Homebase { owner } => *owner,
    }
}

/// Async remote restore preflight handle plus the filesystem meta to fall back to.
#[derive(Component)]
pub struct RemotePreflightTask {
    loaded_meta: Option<MapMeta>,
    task: Task<Result<Option<ServerValidatedMapSave>, MapPersistenceRejection>>,
}

/// Resolves the filesystem-only decision synchronously or spawns the async remote read.
///
/// Returns `Some(decision)` when no remote read is needed; returns `None` after inserting a
/// [`RemotePreflightTask`] whose result must be polled in [`MapPreflightStage::WaitingRemoteDecision`].
#[allow(clippy::too_many_arguments)]
fn begin_remote_or_filesystem_decision(
    commands: &mut Commands,
    entity: Entity,
    target_map_id: &MapInstanceId,
    owner: protocol::NostrPublicKey,
    loaded_meta: Option<MapMeta>,
    remote_config: &RemoteMapPersistenceConfig,
    fake_remote_restores: Option<&FakeRemoteMapRestores>,
    remote_read_context: Option<&RemoteMapReadContext>,
) -> Option<MapPersistencePreflightDecision> {
    if !remote_config.enabled {
        return Some(
            loaded_meta
                .map(MapPersistencePreflightDecision::UseFilesystem)
                .unwrap_or(MapPersistencePreflightDecision::Missing),
        );
    }

    if let Some(save) = fake_remote_restores.and_then(|remote| remote.0.get(target_map_id).cloned())
    {
        return Some(MapPersistencePreflightDecision::UseRemote(save));
    }

    let Some(remote_read_context) = remote_read_context else {
        trace!(
            ?target_map_id,
            "remote persistence enabled but no fake or real remote read context is configured; falling back"
        );
        return Some(
            loaded_meta
                .map(MapPersistencePreflightDecision::UseFilesystem)
                .unwrap_or(MapPersistencePreflightDecision::RemoteUnavailable),
        );
    };

    if matches!(target_map_id, MapInstanceId::Homebase { .. }) {
        warn!(
            ?target_map_id,
            "temporary insecure Phase 3 homebase remote import path is enabled; Phase 5 must require server attestation"
        );
    }

    let task = spawn_remote_preflight_task(remote_read_context, owner, target_map_id.clone());
    commands
        .entity(entity)
        .insert(RemotePreflightTask { loaded_meta, task });
    None
}

/// Spawns the Nostr manifest lookup, Blossom download, validation, and save assembly on the IO pool.
fn spawn_remote_preflight_task(
    remote_read_context: &RemoteMapReadContext,
    owner: protocol::NostrPublicKey,
    target_map_id: MapInstanceId,
) -> Task<Result<Option<ServerValidatedMapSave>, MapPersistenceRejection>> {
    let event_client = remote_read_context.event_client.clone();
    let query_policy = remote_read_context.query_policy.clone();
    let persistence_policy = remote_read_context.persistence_policy.clone();
    IoTaskPool::get().spawn(async move {
        let Some(head) =
            latest_visible_manifest(&event_client, owner, &target_map_id, query_policy.clone())
                .await?
        else {
            return Ok(None);
        };
        let chain =
            fetch_manifest_ancestors(&event_client, &head, None, query_policy.clone()).await?;
        match verify_revision_chain(&chain, None)? {
            RevisionDecision::AtAcceptedHead => return Ok(None),
            RevisionDecision::Descendant(_) => {}
        }
        let payloads = download_payloads(&chain, persistence_policy.clone()).await?;
        let raw_save = validate_remote_map_save(
            chain,
            payloads,
            persistence_policy.clone(),
            RawSaveBase::Empty,
        )?;
        let save = raw_save.try_into()?;
        Ok::<_, MapPersistenceRejection>(Some(save))
    })
}

/// Maps a finished remote preflight task result to a backend decision, falling back to filesystem meta.
fn remote_decision_from_result(
    result: Result<Option<ServerValidatedMapSave>, MapPersistenceRejection>,
    loaded_meta: Option<MapMeta>,
) -> MapPersistencePreflightDecision {
    match result {
        Ok(Some(save)) => MapPersistencePreflightDecision::UseRemote(save),
        Ok(None) => loaded_meta
            .map(MapPersistencePreflightDecision::UseFilesystem)
            .unwrap_or(MapPersistencePreflightDecision::Missing),
        Err(MapPersistenceRejection::Unavailable(_)) => loaded_meta
            .map(MapPersistencePreflightDecision::UseFilesystem)
            .unwrap_or(MapPersistencePreflightDecision::RemoteUnavailable),
        Err(rejection) => MapPersistencePreflightDecision::Blocked(rejection),
    }
}

/// Applies a preflight decision, then despawns startup preflights or advances switches to commit.
#[allow(clippy::too_many_arguments)]
fn finish_preflight_decision(
    commands: &mut Commands,
    registry: &MapRegistry,
    map_states: &mut Query<&mut MapLoadState>,
    save_path: &WorldSavePath,
    terrain_registry: &TerrainDefRegistry,
    type_registry: &AppTypeRegistry,
    entity: Entity,
    preflight: &mut ActiveMapPreflight,
    decision: MapPersistencePreflightDecision,
) {
    apply_preflight_result(
        commands,
        registry,
        map_states,
        save_path,
        terrain_registry,
        type_registry,
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

/// Commits map switches after preflight-selected filesystem loading reaches Ready.
#[allow(clippy::too_many_arguments)]
pub fn commit_ready_map_preflights(
    mut commands: Commands,
    active: Query<(Entity, Ref<ActiveMapPreflight>)>,
    mut registry: ResMut<MapRegistry>,
    map_state_query: Query<Ref<MapLoadState>>,
    map_params_query: Query<(&VoxelMapConfig, &MapDimensions)>,
    save_path: Res<WorldSavePath>,
    mut room_registry: ResMut<RoomRegistry>,
    mut senders: Query<&mut MessageSender<MapTransitionStart>>,
    respawn_query: Query<(&avian3d::prelude::Position, &MapInstanceId), With<RespawnPoint>>,
) {
    for (entity, preflight) in &active {
        if preflight.stage != MapPreflightStage::CommitTransition {
            if preflight.is_changed() {
                trace!(?preflight.stage, "active preflight is not ready to commit transition");
            }
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
        MapPersistencePreflightDecision::UseRemote(save) => {
            let map_entity = registry.get(&request.target_map_id);
            let map_save_dir = map_save_dir(&save_path.0, &request.target_map_id);
            if let Err(error) = materialize_validated_map_save(&map_save_dir, &save) {
                block_preflight_target(
                    registry,
                    map_states,
                    &request.target_map_id,
                    MapPersistenceRejection::Filesystem(format!(
                        "materialize remote save: {error}"
                    )),
                );
                return;
            }
            if let Err(error) =
                install_active_revision_store_backends(commands, map_entity, &map_save_dir)
            {
                block_preflight_target(
                    registry,
                    map_states,
                    &request.target_map_id,
                    MapPersistenceRejection::Filesystem(format!(
                        "install active revision stores: {error}"
                    )),
                );
                return;
            }
            configure_preflight_map(
                commands,
                registry,
                map_states,
                save_path,
                terrain_registry,
                type_registry,
                &request.target_map_id,
                save.meta.seed,
                save.meta.generation_version,
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
    let canonical_map_dir = map_save_dir(&save_path.0, map_id);
    let map_dir = store_map_dir_for_loading(&canonical_map_dir)
        .expect("active map revision pointer must be valid before map configuration");
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
