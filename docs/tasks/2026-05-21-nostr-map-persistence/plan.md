# Implementation Plan

## Overview

Map/layout persistence should choose the latest visible valid save chain across local filesystem and Nostr/Blossom without weakening server-authoritative gameplay. Remote data is fetched and validated into backend-agnostic save data, materialized into a crash-safe staged filesystem revision, atomically promoted, and then loaded by the current map/chunk pipeline.

## Global Implementation Rules

- Before any `cargo build`, `cargo check`, or `cargo test`, verify no other build/check/test is running:

```bash
if pgrep -af 'cargo (build|check|test|make)|cargo-make|rustc' | grep -v pgrep; then
  echo 'A Rust build/check/test is already running; wait or stop it first.' >&2
  exit 1
fi
```

- Keep `voxel_map_engine` free of Nostr/Blossom/protocol types.
- Keep `crates/nostr_client` map-agnostic: it owns generic Nostr relay/event signing/query helpers and generic Blossom/blob upload/download/hash verification only.
- Put all map-specific Nostr/Blossom integration in a new `crates/nostr_map_persistence` crate: map manifests, descriptor roots, revision-chain validation, map payload policies, remote read/publish helpers, and Store adapters.
- Do not invent new persistence backend traits or task wrappers until first trying `bevy-persistence::Store`, `StoreBackend`, and `PendingStoreOps`; any exception must document the exact gap that `Store<K, V>` cannot express. If the gap is async browser/network I/O, extend `bevy-persistence` with an async Store abstraction instead of bypassing Store orchestration.
- Keep domain DTOs separate from transport plumbing: `protocol` owns generic game/network identifiers plus wire DTOs that must cross Lightyear messages; `nostr_client` owns generic Nostr/Blossom primitives; `nostr_map_persistence` owns map-specific persistence DTOs, validation/signing helpers, and reusable Store adapters; server/client code owns authority decisions and concrete payload decoding.
- Remote Nostr/Blossom Store adapters must support native and web through one shared async API. Use Bevy-style conditional send bounds (`ConditionalSend`/`ConditionalSendFuture`) for async Store futures so native remains `Send` while browser `wasm32` Fetch/WebSocket futures are not forced to be `Send`.
- Treat unavailable remote services as graceful fallback, but treat invalid, incomplete, or divergent available chains as blocked/quarantined states.
- Add `trace!` before every expected wait/early-out. Use `panic!`, `expect`, or explicit rejection for impossible/invalid state.
- Do not change runtime gameplay authority: edits are still server-validated, server-applied, acked/replicated, and then persisted.

## Type Minimization Result

- Keep ECS/game-flow types that express gameplay or transition state: `MapLoadState`, `PendingMapPreflight`, `MapPreflightKind`, `MapTransitionParams`, `MapPreparation`, and `RemoteMapPublishWorker`.
- Keep shared map-persistence DTOs in `crates/nostr_map_persistence`: `MapRevision`, `RawValidatedMapSave`, `RawValidatedMapDelta`, `PayloadSlotState`, raw payload DTOs, map manifests/descriptors, map policies, and `MapPersistenceRejection`. Keep generic blob refs/verified blobs in `crates/nostr_client`; keep game/network identity primitives and any Lightyear wire DTOs in `crates/protocol`; server-local decoded records such as `ServerValidatedMapSave`, `ServerValidatedMapDelta`, publish journals, and quarantine records stay in server persistence modules.
- Use `MapPersistenceRejection` as the single detailed failure type; do not add parallel failure-kind or block-reason enums unless a caller needs a separate stable public code.
- Use `Store<K, V>` adapters plus `StoreBackend`/`PendingStoreOps` for concrete synchronous/blocking persistence such as accepted head, filesystem metadata/snapshots, fake remote heads/manifests/payloads, and publish journals. Preflight orchestration itself belongs in systems/resources that drive concrete stores; do not model the whole preflight decision as a read-only pseudo-Store.
- Use a new async Store abstraction plus async pending-op polling for Nostr/Blossom reads, blob upload, and manifest publication. The async Store should mirror the existing Store/PendingStoreOps ECS shape but await `load`/`save` internally and use Bevy's conditional-send futures for web compatibility.

---

## Phase 1: Preflight Persistence Before Transitions

### Changes

#### 1. Split only the map modules needed for preflight

**Files**: `crates/server/src/map.rs`, `crates/server/src/map/mod.rs`, `crates/server/src/map/types.rs`, `crates/server/src/map/preflight.rs`, `crates/server/src/map/preparation.rs`, `crates/server/src/map/switching.rs` **Action**: rename/create/modify

Do the minimal split needed to keep new persistence/preflight logic out of the already-large `map.rs`; do not front-load unrelated cleanup.

- rename `crates/server/src/map.rs` to `crates/server/src/map/mod.rs`; `crates/server/src/lib.rs` can keep `pub mod map;` unchanged;
- keep `ServerMapPlugin`, resource registration, system scheduling, existing voxel-edit/world-object-edit/room/chunk-push/save systems, and broad compatibility re-exports in `map/mod.rs`;
- move only shared map lifecycle/transition-preparation/preflight types into `map/types.rs` and re-export them from `map/mod.rs` with `pub use types::*;`;
- put persistence preflight state-machine systems in `map/preflight.rs`;
- move only `ensure_map_exists`, `spawn_homebase`, and helpers directly needed by preflight-driven preparation into `map/preparation.rs`;
- move only `handle_map_switch_requests` and `resolve_switch_target` into `map/switching.rs` if required to separate request capture from transition commit.

Later phases should add focused modules such as `map/remote_publish.rs`, `map/homebase_publication.rs`, and `map/diagnostics.rs`. Defer broader `map.rs` cleanup until behavior tests pass.

#### 2. Server map persistence state and preflight types

**Files**: `Cargo.toml`, `crates/server/Cargo.toml`, `crates/nostr_map_persistence/Cargo.toml`, `crates/nostr_map_persistence/src/lib.rs`, `crates/server/src/persistence/mod.rs`, `crates/server/src/map/types.rs` **Action**: create/modify

Create and wire a minimal `nostr_map_persistence` crate before moving these types so Phase 1 compiles. Add it to the workspace and to `server` dependencies with compile-shaped `MapPersistenceRejection`, `RawValidatedMapSave`, and `PayloadSlotState` DTOs. Also add the server-local `ServerValidatedMapSave` struct in `crates/server/src/persistence/mod.rs` before `MapPersistencePreflightDecision` references it. Then move existing `MapLoadState` and `MapTransitionParams` from the old `map.rs` into this file and add transition-preflight data structures beside them. Re-export these types from `map/mod.rs`.

```rust
/// Tracks a map entity's server-side load/persistence lifecycle.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub enum MapLoadState {
    CheckingPersistence,
    AwaitingMeta,
    AwaitingEntities,
    Blocked(MapPersistenceRejection),
    Ready,
}

/// Describes the backend choice made by map persistence preflight.
#[derive(Clone, Debug)]
pub enum MapPersistencePreflightDecision {
    UseFilesystem(MapMeta),
    UseRemote(ServerValidatedMapSave),
    Missing,
    RemoteUnavailable,
    Blocked(MapPersistenceRejection),
}

/// Identifies why a map preflight is running.
#[derive(Clone, Debug)]
pub enum MapPreflightKind {
    StartupOverworld,
    MapSwitch {
        client_entity: Entity,
        player_entity: Entity,
        current_map_id: MapInstanceId,
        requested_at: f64,
    },
}

/// Captures startup or map-switch intent while persistence preflight runs.
#[derive(Clone, Debug)]
pub struct PendingMapPreflight {
    pub target_map_id: MapInstanceId,
    pub kind: MapPreflightKind,
}

/// Completed persistence preflight decision produced by the preflight state machine.
#[derive(Clone, Debug)]
pub struct MapPersistencePreflightResult {
    pub target_map_id: MapInstanceId,
    pub kind: MapPreflightKind,
    pub decision: MapPersistencePreflightDecision,
}

/// Seed, generation version, and bounds for a map transition message.
pub struct MapTransitionParams {
    pub seed: u64,
    pub generation_version: u32,
    pub bounds: Option<IVec3>,
    pub chunk_size: u32,
    pub column_y_range: (i32, i32),
}

/// Indicates whether a map is ready for transition commit, still loading, or blocked.
pub enum MapPreparation {
    Ready { entity: Entity, params: MapTransitionParams },
    Pending,
    Blocked(MapPersistenceRejection),
}
```

In Phase 1, `RawValidatedMapSave`, `PayloadSlotState`, `MapPersistenceRejection`, and the `ServerValidatedMapSave` struct should be compile-shaped even if remote fields are unused until Phase 2. `MapPersistenceRejection` must already include the `Filesystem(String)` variant used by filesystem-only preflight. Do not model preflight itself as a read-only `Store`. Preflight systems should own the state machine and spawn concrete loads through existing stores such as `FsMapMetaStore` and, in later phases, `FsAcceptedMapHeadStore`, in-memory remote stores, and async Nostr/Blossom stores.

#### 3. Start and poll unified startup/switch preflight before loading or transitioning

**Files**: `crates/server/src/map/preflight.rs`, `crates/server/src/map/switching.rs`, `crates/server/src/map/mod.rs`, `crates/server/src/auth.rs`, `crates/server/src/gameplay.rs` **Action**: create/modify

Add a preflight state machine that is used for both overworld startup and player map switches. Systems orchestrate concrete stores; do not add a read-only `MapPreflightStore` whose `save` is unsupported.

Startup path:

- `init_overworld_entity` should register the overworld in `MapRegistry` with `MapLoadState::CheckingPersistence` instead of immediately entering `AwaitingMeta`;
- enqueue `PendingMapPreflight { target_map_id: MapInstanceId::Overworld, kind: StartupOverworld }`;
- do not spawn local metadata/entity/chunk loads until preflight selects `UseFilesystem`, `Missing`, `RemoteUnavailable`, or a materialized `UseRemote` result.

Map-switch path:

- replace direct calls to `crate::transition::start_map_transition` inside `handle_map_switch_requests` with insertion/enqueueing of `PendingMapPreflight { kind: MapSwitch { ... } }`;
- the request handler must only resolve player identity/current map and store intent;
- it must insert a per-player/client `PendingMapSwitchPreflight` marker so repeat requests are ignored or replace the prior request explicitly; do not rely on `PendingTransition` for duplicate gating while preflight is pending;
- `PendingMapSwitchPreflight` must be removed when the preflight commits, blocks, is cancelled, or is explicitly replaced by a newer request;
- it must not relocate, freeze, attach `ChunkTicket`, remove room senders, or send `MapTransitionStart`.

Initial authenticated login path:

- split current `spawn_authenticated_character` so auth success records `PlayerIdentity` plus a `PendingInitialSpawn` on the client entity instead of immediately spawning the character;
- `PendingInitialSpawn` must wait until the Overworld preflight has reached `MapPreparation::Ready` before creating the character, attaching `ChunkTicket`, inserting `TransitionPending`, joining the overworld room, or sending `MapTransitionStart`;
- the same prepared-map transition payload helper should be used for initial spawn and later map switches so there is no second path that can bypass persistence preflight.

The Phase 1 preflight state machine is filesystem-only. It should spawn concrete `FsMapMetaStore` loads through `StoreBackend`/`PendingStoreOps`, map results to `MapPersistencePreflightDecision`, and preserve the same polling/error-drain shape as existing map metadata/chunk persistence. Once Phase 2/3 remote lookup exists, preflight must treat filesystem state as the local baseline/accepted head, query remote before selecting a final decision, and choose filesystem only when remote is disabled, missing, unavailable, or has no accepted newer descendant.

Add `poll_map_persistence_preflight` to drive the state machine and drain completed loads/errors. It should:

- leave the task entity/state in place and `trace!` while concrete store ops are pending;
- remove the completed preflight task/state after its result or error is drained;
- remove the matching `PendingMapSwitchPreflight` marker on commit/block/cancel so future legitimate switches are not permanently gated;
- materialize remote data only for `UseRemote` once Phase 2 exists;
- for startup overworld: advance the map entity to `AwaitingMeta`/`AwaitingEntities` only after the selected backend data is ready for the normal filesystem loaders;
- for pending initial spawns: create/spawn the authenticated character only when `ensure_map_exists(Overworld, ...)` returns `MapPreparation::Ready`;
- for map switches: call the new transition commit function only when `ensure_map_exists(...)` returns `MapPreparation::Ready`;
- use filesystem/default metadata only for `UseFilesystem`, `Missing`, or `RemoteUnavailable`, where `UseFilesystem` means remote comparison has already decided local state wins;
- set/keep `MapLoadState::Blocked(_)` for `MapPersistencePreflightDecision::Blocked(_)`;
- convert unexpected `PendingStoreOps::load_errors` into a loud `MapPersistenceRejection::Filesystem(...)` block.

Register startup preflight before `poll_map_meta` can spawn filesystem loads, and register switch-preflight polling before `complete_map_transition`.

Concrete ECS shape for Phase 1 should look like this rather than an implicit queue:

```rust
#[derive(Resource, Default)]
pub struct PendingMapPreflights(pub VecDeque<PendingMapPreflight>);

#[derive(Component, Clone, Debug)]
pub struct PendingMapSwitchPreflight {
    pub target_map_id: MapInstanceId,
    pub requested_at: f64,
}

#[derive(Component, Clone, Debug)]
pub struct PendingInitialSpawn {
    pub remote_id: RemoteId,
    pub identity: PlayerIdentity,
    pub requested_at: f64,
}

#[derive(Component)]
pub struct ActiveMapPreflight {
    pub request: PendingMapPreflight,
    pub stage: MapPreflightStage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MapPreflightStage {
    LoadFilesystemMeta,
    WaitingFilesystemMeta,
    DecideFilesystemOnly,
    MaterializeRemote,
    PrepareMap,
    CommitTransition,
}

pub fn enqueue_startup_overworld_preflight(
    mut queue: ResMut<PendingMapPreflights>,
    registry: Res<MapRegistry>,
) {
    let Some(_) = registry.0.get(&MapInstanceId::Overworld) else {
        panic!("overworld must be registered before startup persistence preflight");
    };
    queue.0.push_back(PendingMapPreflight {
        target_map_id: MapInstanceId::Overworld,
        kind: MapPreflightKind::StartupOverworld,
    });
}

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
        ActiveMapPreflight { request, stage: MapPreflightStage::LoadFilesystemMeta },
        PendingStoreOps::<(), MapMeta>::default(),
    ));
}

pub fn process_pending_initial_spawns(
    mut commands: Commands,
    mut pending_clients: Query<(Entity, &PendingInitialSpawn)>,
    mut map_registry: ResMut<MapRegistry>,
    map_state_query: Query<&MapLoadState>,
    map_params_query: Query<(&VoxelMapConfig, &MapDimensions)>,
    mut transition_context: InitialSpawnTransitionContext,
) {
    for (client_entity, pending_spawn) in &mut pending_clients {
        let preparation = ensure_map_exists(
            &mut commands,
            &mut map_registry,
            &map_state_query,
            &map_params_query,
            &MapInstanceId::Overworld,
        );
        match preparation {
            MapPreparation::Ready { entity: overworld_entity, params } => {
                let character_entity = spawn_authenticated_character_entity(&mut commands, client_entity, pending_spawn);
                commit_initial_overworld_spawn(
                    &mut commands,
                    character_entity,
                    client_entity,
                    overworld_entity,
                    params,
                    &mut transition_context,
                );
                commands.entity(client_entity).remove::<PendingInitialSpawn>();
            }
            MapPreparation::Pending => {
                trace!(?client_entity, "initial spawn waiting for overworld persistence preflight");
            }
            MapPreparation::Blocked(reason) => {
                warn!(?client_entity, ?reason, "initial spawn blocked by overworld persistence preflight");
            }
        }
    }
}
```

The filesystem-only poll path should make the result mapping explicit:

```rust
pub fn poll_map_persistence_preflight(
    mut commands: Commands,
    mut active: Query<(Entity, &mut ActiveMapPreflight, &mut PendingStoreOps<(), MapMeta>)>,
    meta_stores: Query<&StoreBackend<(), MapMeta, FsMapMetaStore>>,
    mut map_states: Query<&mut MapLoadState>,
    mut registry: ResMut<MapRegistry>,
) {
    for (entity, mut preflight, mut meta_ops) in &mut active {
        meta_ops.poll();
        if let Some((_, error)) = meta_ops.load_errors.pop() {
            let rejection = MapPersistenceRejection::Filesystem(error.to_string());
            block_preflight_target(&registry, &mut map_states, &preflight.request.target_map_id, rejection);
            commands.entity(entity).despawn();
            continue;
        }
        match preflight.stage {
            MapPreflightStage::LoadFilesystemMeta => {
                let Some(map_entity) = ensure_preflight_target_registered(
                    &mut commands,
                    &mut registry,
                    &preflight.request.target_map_id,
                ) else {
                    trace!(?preflight.request.target_map_id, "preflight target placeholder was just spawned; waiting for commands to apply");
                    continue;
                };
                let store = meta_stores.get(map_entity)
                    .expect("preflight target must have FsMapMetaStore backend before metadata load");
                meta_ops.spawn_load(&store.0, ());
                preflight.stage = MapPreflightStage::WaitingFilesystemMeta;
            }
            MapPreflightStage::WaitingFilesystemMeta if meta_ops.completed_loads.is_empty() => {
                trace!(?preflight.request.target_map_id, "waiting for filesystem metadata preflight load");
                continue;
            }
            MapPreflightStage::WaitingFilesystemMeta => {
                let (_, loaded_meta) = meta_ops.completed_loads.pop()
                    .expect("checked completed filesystem metadata load exists");
                let decision = loaded_meta
                    .map(MapPersistencePreflightDecision::UseFilesystem)
                    .unwrap_or(MapPersistencePreflightDecision::Missing);
                apply_preflight_result(&mut commands, &registry, &mut map_states, preflight.request.clone(), decision);
                commands.entity(entity).despawn();
            }
            MapPreflightStage::DecideFilesystemOnly
            | MapPreflightStage::MaterializeRemote
            | MapPreflightStage::PrepareMap
            | MapPreflightStage::CommitTransition => {
                trace!(?preflight.stage, "preflight stage is handled by later phase systems");
                continue;
            }
        }
    }
}

fn ensure_preflight_target_registered(
    commands: &mut Commands,
    registry: &mut MapRegistry,
    target_map_id: &MapInstanceId,
) -> Option<Entity> {
    if let Some(&entity) = registry.0.get(target_map_id) {
        return Some(entity);
    }
    match target_map_id {
        MapInstanceId::Overworld => panic!("overworld must be registered before preflight"),
        MapInstanceId::Homebase { owner } => {
            let entity = spawn_homebase_preflight_placeholder_with_stores(
                commands,
                *owner,
                MapLoadState::CheckingPersistence,
            );
            registry.0.insert(target_map_id.clone(), entity);
            None
        }
    }
}
```

#### 4. Make map existence distinct from map usability

**File**: `crates/server/src/map/preparation.rs` **Action**: create/modify

Change `ensure_map_exists(...) -> (Entity, MapTransitionParams)` to `ensure_map_exists(...) -> MapPreparation`.

Rules:

- Existing registered maps with `MapLoadState::Ready` and `VoxelMapConfig + MapDimensions` return `Ready`.
- Existing registered maps with `CheckingPersistence`, `AwaitingMeta`, or `AwaitingEntities` return `Pending` and log `trace!`.
- Existing registered maps with `Blocked(reason)` return `Blocked(reason.clone())`.
- Missing overworld still `panic!`s because overworld must be registered at `AppState::Ready`.
- Missing homebase first gets a `CheckingPersistence` placeholder registered for preflight with filesystem store backends rooted at the canonical homebase save directory; selected/preflight metadata later installs `VoxelMapConfig`, dimensions, and generator, then advances it to `AwaitingEntities` or `Ready` only after map entities are loaded.

```rust
pub fn ensure_map_exists(
    commands: &mut Commands,
    registry: &mut MapRegistry,
    map_state_query: &Query<&MapLoadState>,
    map_params_query: &Query<(&VoxelMapConfig, &MapDimensions)>,
    map_id: &MapInstanceId,
) -> MapPreparation {
    if let Some(&entity) = registry.0.get(map_id) {
        let state = map_state_query
            .get(entity)
            .expect("registered map entity must have MapLoadState");
        match state {
            MapLoadState::Ready => {
                let (config, dimensions) = map_params_query
                    .get(entity)
                    .expect("ready map entity must have VoxelMapConfig + MapDimensions");
                return MapPreparation::Ready { entity, params: transition_params(config, dimensions) };
            }
            MapLoadState::Blocked(reason) => return MapPreparation::Blocked(reason.clone()),
            MapLoadState::CheckingPersistence | MapLoadState::AwaitingMeta | MapLoadState::AwaitingEntities => {
                trace!(?map_id, ?state, "map exists but is not ready for transition yet");
                return MapPreparation::Pending;
            }
        }
    }

    match map_id {
        MapInstanceId::Overworld => panic!("overworld must be registered before map preparation"),
        MapInstanceId::Homebase { owner } => {
            let entity = spawn_homebase_preflight_placeholder_with_stores(
                commands,
                *owner,
                MapLoadState::CheckingPersistence,
            );
            registry.0.insert(map_id.clone(), entity);
            trace!(?map_id, ?entity, "spawned homebase placeholder pending persistence preflight");
            MapPreparation::Pending
        }
    }
}
```

#### 5. Split transition start into preflight and commit

**File**: `crates/server/src/transition.rs` **Action**: modify

Rename the existing `start_map_transition` body to `commit_map_transition` and take already-prepared map information.

```rust
#[allow(clippy::too_many_arguments)]
pub fn commit_map_transition(
    commands: &mut Commands,
    player_entity: Entity,
    client_entity: Entity,
    current_map_id: &MapInstanceId,
    target_map_id: &MapInstanceId,
    prepared_map: Entity,
    params: MapTransitionParams,
    room_registry: &mut RoomRegistry,
    senders: &mut Query<&mut MessageSender<MapTransitionStart>>,
    respawn_query: &Query<(&Position, &MapInstanceId), With<RespawnPoint>>,
) {
    let old_room = room_registry.get_or_create(current_map_id, commands);
    let new_room = room_registry.get_or_create(target_map_id, commands);
    let spawn_position = respawn_query
        .iter()
        .find(|(_, map_id)| *map_id == target_map_id)
        .map(|(position, _)| position.0)
        .unwrap_or(crate::gameplay::DEFAULT_SPAWN_POS);

    commands.trigger(RoomEvent {
        room: old_room,
        target: RoomTarget::RemoveSender(client_entity),
    });
    relocation::relocate_remove(commands, player_entity, old_room, target_map_id, Some(spawn_position));
    commands.entity(player_entity).insert((
        LinearVelocity(Vec3::ZERO),
        DisableRollback,
        ColliderDisabled,
        RigidBodyDisabled,
        ChunkTicket::player(prepared_map),
    ));

    let mut sender = senders
        .get_mut(client_entity)
        .expect("transitioning client must have MapTransitionStart sender");
    sender.send::<MapChannel>(MapTransitionStart {
        target: target_map_id.clone(),
        seed: params.seed,
        generation_version: params.generation_version,
        bounds: params.bounds,
        spawn_position,
        chunk_size: params.chunk_size,
        column_y_range: params.column_y_range,
        readiness_radius: TRANSITION_READINESS_RADIUS,
    });
    commands.entity(player_entity).insert(TransitionPending {
        client_entity,
        target_map_id: target_map_id.clone(),
        new_room,
        relocated_entities: vec![player_entity],
    });
}
```

The only public transition function that relocates/freezes/sends `MapTransitionStart` should be `commit_map_transition`. It must be called only from `poll_map_persistence_preflight` after map preparation is ready.

#### 6. Transition tests

**File**: `crates/server/tests/map_transition.rs` **Action**: modify

Add focused unit/system tests for startup and transition gates:

- overworld startup enters `CheckingPersistence` and does not spawn filesystem meta/entity/chunk loads until preflight chooses a backend;
- auth success inserts `PendingInitialSpawn` but does not spawn a character, insert `ChunkTicket`/`TransitionPending`, or send `MapTransitionStart` while Overworld is still `CheckingPersistence`;
- repeat switch requests while `PendingMapSwitchPreflight` exists do not enqueue duplicate stale transitions;
- pending switch preflight does not insert `PendingTransition`, `ColliderDisabled`, `RigidBodyDisabled`, or `ChunkTicket`;
- pending preflight does not send `MapTransitionStart`;
- completed filesystem preflight commits transition and inserts the expected markers;
- invalid/divergent preflight records `MapLoadState::Blocked(_)` and leaves the player on the current map;
- waiting paths are explicit and observable through state, not implied by map entity existence.

Use direct systems/resources where Lightyear senders are hard to drive; assert ECS state before/after `app.update()`.

Example gate assertion shape:

```rust
#[test]
fn pending_preflight_does_not_start_transition() {
    let mut app = server_map_test_app();
    app.world_mut().resource_mut::<PendingMapPreflights>().0.push_back(PendingMapPreflight {
        target_map_id: MapInstanceId::Homebase { owner: test_owner() },
        kind: MapPreflightKind::MapSwitch {
            client_entity: test_client_entity(&mut app),
            player_entity: test_player_entity(&mut app),
            current_map_id: MapInstanceId::Overworld,
            requested_at: 1.0,
        },
    });

    app.update();

    let world = app.world();
    assert!(world.query::<&PendingTransition>().iter(world).next().is_none());
    assert!(world.query::<&ChunkTicket>().iter(world).next().is_none());
    assert_no_transition_start_messages(world);
}
```

#### 7. Persistence tests for fallback/block states

**File**: `crates/server/tests/world_persistence.rs` **Action**: modify

Add tests for:

- local valid `map.meta.bin` gives `UseFilesystem`;
- no local meta gives `Missing` and homebase seed fallback remains deterministic;
- simulated remote unavailable gives filesystem/default fallback;
- invalid preflight does not overwrite local files.

### Verification

#### Automated

- [x] `if pgrep -af 'cargo (build|check|test)|cargo-make|rustc' | grep -v pgrep; then echo busy >&2; exit 1; fi`
- [x] `cargo test -p server map_transition`
- [x] `cargo test -p server world_persistence`

#### Manual

- [x] Start `cargo server` and confirm overworld startup logs preflight before normal filesystem loading; then run `cargo client`, request a homebase switch, and confirm the player is not frozen/relocated until the server has selected seed/dimensions.
- [x] With remote disabled/unavailable, confirm valid filesystem worlds still transition normally.

---

## Phase 2: Fake Remote Restore Materialization

### Changes

#### 1. Shared accepted-save DTOs and revision metadata

**Files**: `Cargo.toml`, `crates/nostr_client/src/blobs.rs`, `crates/nostr_map_persistence/Cargo.toml`, `crates/nostr_map_persistence/src/lib.rs`, `crates/nostr_map_persistence/src/manifest.rs`, `crates/server/src/persistence/mod.rs` **Action**: create/modify

Add or complete the dedicated `nostr_map_persistence` workspace crate so map-specific Nostr persistence types are shared by server and client without teaching `nostr_client` about maps. The new crate depends on `protocol` for `MapInstanceId`/`NostrPublicKey`, on `nostr_client` for the generic `BlobRef` DTO, and on `persistence` when Store adapters are added. Define the real `NostrMapManifest`, `ManifestPayloadDescriptor`, `PayloadClass`, `PayloadKey`, and raw validated payload DTOs here in Phase 2 so fake remote restore tests use production-shaped manifest DTOs before Phase 3 adds real relay/event I/O. Server persistence adds decoded `ServerValidatedMapSave`/`ServerValidatedMapDelta` structs plus fallible conversion helpers around existing backend types, but neither `nostr_client` nor `nostr_map_persistence` may depend on `crates/server`.

```rust
pub type ManifestHash = [u8; 32];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum PayloadClass {
    MapMeta,
    TerrainChunk,
    ChunkEntities,
    MapEntities,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum PayloadKey {
    Singleton,
    Chunk { x: i32, y: i32, z: i32 },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ManifestPayloadSlot {
    Present { blob: BlobRef },
    Empty,
    Absent,
    Tombstoned,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestPayloadDescriptor {
    pub class: PayloadClass,
    pub key: PayloadKey,
    pub slot: ManifestPayloadSlot,
    pub schema_version: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NostrMapManifest {
    pub map_id: MapInstanceId,
    pub owner: NostrPublicKey,
    pub revision: u64,
    pub previous_hash: Option<ManifestHash>,
    pub payloads: Vec<ManifestPayloadDescriptor>,
    pub schema_version: u32,
    pub descriptor_root: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct RawMapMetaPayload { pub bytes: Vec<u8> }
#[derive(Clone, Debug)]
pub struct RawChunkPayload { pub bytes: Vec<u8> }
#[derive(Clone, Debug)]
pub struct RawChunkEntitiesPayload { pub bytes: Vec<u8> }
#[derive(Clone, Debug)]
pub struct RawMapEntitiesPayload { pub bytes: Vec<u8> }

#[derive(Clone, Debug)]
pub struct RawMapPayloads {
    pub present_payloads: Vec<(ManifestPayloadDescriptor, Vec<u8>)>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MapRevision {
    pub revision: u64,
    pub previous_hash: Option<ManifestHash>,
    pub manifest_hash: ManifestHash,
}

#[derive(Clone, Debug)]
pub struct RawValidatedMapSave {
    pub meta: RawMapMetaPayload,
    pub chunks: Vec<(PayloadKey, RawChunkPayload)>,
    pub chunk_entities: Vec<(PayloadKey, RawChunkEntitiesPayload)>,
    pub map_entities: Option<RawMapEntitiesPayload>,
    pub revision: MapRevision,
}

#[derive(Clone, Debug)]
pub enum RawSaveBase {
    Empty,
    Snapshot(RawValidatedMapSave),
}

// In crates/server/src/persistence/mod.rs:
#[derive(Clone, Debug)]
pub struct ServerValidatedMapSave {
    pub meta: MapMeta,
    pub chunks: Vec<(IVec3, ChunkFileEnvelope)>,
    pub chunk_entities: Vec<(IVec3, Vec<WorldObjectSpawn>)>,
    pub map_entities: Option<Vec<SavedEntity>>,
    pub revision: MapRevision,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MapPersistenceRejection {
    Filesystem(String),
    Invalid(String),
    Incomplete(String),
    Divergent(String),
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PayloadSlotState<T> {
    Present(T),
    Empty,
    Absent,
    Tombstoned,
}

#[derive(Clone, Debug)]
pub struct RawValidatedMapDelta {
    pub revision: MapRevision,
    pub meta: PayloadSlotState<RawMapMetaPayload>,
    pub chunks: Vec<(PayloadKey, PayloadSlotState<RawChunkPayload>)>,
    pub chunk_entities: Vec<(PayloadKey, PayloadSlotState<RawChunkEntitiesPayload>)>,
    pub map_entities: PayloadSlotState<RawMapEntitiesPayload>,
}

// In crates/server/src/persistence/mod.rs:
#[derive(Clone, Debug)]
pub struct ServerValidatedMapDelta {
    pub revision: MapRevision,
    pub meta: PayloadSlotState<MapMeta>,
    pub chunks: Vec<(IVec3, PayloadSlotState<ChunkFileEnvelope>)>,
    pub chunk_entities: Vec<(IVec3, PayloadSlotState<Vec<WorldObjectSpawn>>)>,
    pub map_entities: PayloadSlotState<Vec<SavedEntity>>,
}
```

#### 2. Minimal remote config, accepted head store, and legacy bootstrap

**File**: `crates/server/src/persistence/mod.rs` **Action**: modify

Introduce the minimal runtime config needed before fake/real remote selection:

```rust
#[derive(Resource, Clone, Debug)]
pub struct RemoteMapPersistenceConfig {
    pub enabled: bool,
    pub fallback_timeout: Duration,
}
```

Default config preserves current filesystem-only behavior when remote persistence is disabled. Quarantine paths and diagnostics fields are added later.

Add two head files under each active map revision: `accepted_head.bin` for the last remote/materialized-or-successfully-published revision and `local_head.bin` for the newest local filesystem revision marker. `accepted_head.bin` stores a real `MapRevision` and is updated only after materialization succeeds or remote publish succeeds, with one explicit exception: legacy bootstrap may initialize it from existing filesystem bytes when no head files exist yet. `local_head.bin` stores a `LocalMapHead` that does not require a Nostr manifest hash, so it can advance immediately after local filesystem saves succeed and before publish-draft finalization. Implement both as bevy-persistence stores, not bespoke load/save APIs.

```rust
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
        let revision = bincode::deserialize(&bytes)
            .map_err(|e| PersistenceError::Deserialize(format!("deserialize accepted head: {e}")))?;
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
    pub fn path(&self) -> PathBuf { self.map_dir.join("accepted_head.bin") }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalMapHead {
    pub local_revision_number: u64,
    pub active_content_hash: [u8; 32],
    pub accepted_remote_manifest_hash: Option<ManifestHash>,
}

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
    pub fn path(&self) -> PathBuf { self.map_dir.join("local_head.bin") }
}

pub fn bootstrap_filesystem_revision(
    save_dir: &Path,
    map_id: &MapInstanceId,
) -> Result<MapRevision, PersistenceError> {
    fn collect_single(files: &mut Vec<String>, save_dir: &Path, relative: &str) -> Result<(), PersistenceError> {
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

    fn collect_dir(files: &mut Vec<String>, save_dir: &Path, dir: &str, suffix: &str) -> Result<(), PersistenceError> {
        let path = save_dir.join(dir);
        if !path.exists() {
            trace!(?path, "legacy persistence directory is absent during bootstrap");
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
            let entry = entry
                .map_err(|e| PersistenceError::Deserialize(format!("read legacy {dir} entry: {e}")))?;
            let file_type = entry
                .file_type()
                .map_err(|e| PersistenceError::Deserialize(format!("stat legacy {dir} entry: {e}")))?;
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

    fn hash_len_prefixed(hasher: &mut sha2::Sha256, bytes: &[u8]) {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }

    let mut files = Vec::new();
    collect_single(&mut files, save_dir, "map.meta.bin")?;
    collect_single(&mut files, save_dir, "entities.bin")?;
    collect_dir(&mut files, save_dir, "terrain", ".bin")?;
    collect_dir(&mut files, save_dir, "entities", ".entities.bin")?;
    files.sort();

    let mut hasher = sha2::Sha256::new();
    hasher.update(b"bevy-lightyear-template:legacy-map-revision:v1");
    let map_id_bytes = bincode::serialize(map_id)
        .map_err(|e| PersistenceError::Serialize(format!("serialize map id for legacy hash: {e}")))?;
    hash_len_prefixed(&mut hasher, &map_id_bytes);

    for relative in files {
        hash_len_prefixed(&mut hasher, relative.as_bytes());
        let bytes = fs::read(save_dir.join(&relative))
            .map_err(|e| PersistenceError::Deserialize(format!("read legacy persistence file {relative}: {e}")))?;
        hash_len_prefixed(&mut hasher, &bytes);
    }

    Ok(MapRevision {
        revision: 0,
        previous_hash: None,
        manifest_hash: hasher.finalize().into(),
    })
}
```

The bootstrap hash must be deterministic: sort relative file paths lexicographically and hash `(relative_path, bytes)` for `map.meta.bin`, `entities.bin`, `terrain/*.bin`, and `entities/*.entities.bin`.

Use one exact on-disk layout for all later sections:

```text
<save_root>/<map-kind>/<map-id>/
  active_revision          # UTF-8 revision directory name, tmp-renamed
  revisions/
    rev-0000000000000007-<manifest_hash_hex>/
      accepted_head.bin
      local_head.bin
      map.meta.bin
      entities.bin
      terrain/chunk_<x>_<y>_<z>.bin
      entities/chunk_<x>_<y>_<z>.entities.bin
  staging/
    rev-0000000000000008-<manifest_hash_hex>.staging-<nonce>/
```

```rust
pub const ACTIVE_REVISION_FILE: &str = "active_revision";
pub const REVISIONS_DIR: &str = "revisions";
pub const STAGING_DIR: &str = "staging";

pub fn manifest_hash_hex(hash: ManifestHash) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn revision_dir_name(revision: &MapRevision) -> String {
    format!("rev-{:016}-{}", revision.revision, manifest_hash_hex(revision.manifest_hash))
}

pub fn active_pointer_path(map_save_dir: &Path) -> PathBuf {
    map_save_dir.join(ACTIVE_REVISION_FILE)
}

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

pub fn store_map_dir_for_loading(map_save_dir: &Path) -> Result<PathBuf, PersistenceError> {
    resolve_active_map_dir(map_save_dir).map(|active| active.unwrap_or_else(|| map_save_dir.to_path_buf()))
}
```

#### 3. Materialize accepted remote data through a staged revision

**File**: `crates/server/src/persistence/mod.rs` **Action**: modify

Materialize a complete accepted save into a revision staging directory, validate it through existing filesystem store formats, then atomically promote that staged revision by switching the active revision pointer/head. Do not write remote data file-by-file into the live active directory.

```rust
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
                fs::remove_dir_all(&path)
                    .map_err(|e| PersistenceError::Serialize(format!("remove staging dir {}: {e}", path.display())))?;
            } else {
                fs::remove_file(&path)
                    .map_err(|e| PersistenceError::Serialize(format!("remove staging file {}: {e}", path.display())))?;
            }
        }
    } else {
        trace!(?staging_dir, "no materialization staging directory to clean up");
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

pub fn materialize_validated_map_save(
    save_dir: &Path,
    save: &ServerValidatedMapSave,
) -> Result<(), PersistenceError> {
    let staging_dir = create_revision_staging_dir(save_dir, &save.revision)?;
    write_full_revision_to_staging(&staging_dir, save)?;
    validate_staged_revision(&staging_dir, save)?;
    FsAcceptedMapHeadStore { map_dir: Arc::new(staging_dir.clone()) }.save(&(), &save.revision)?;
    FsLocalMapHeadStore { map_dir: Arc::new(staging_dir.clone()) }.save(&(), &local_head_from_remote_save(save)?)?;
    atomically_promote_staged_revision(save_dir, &staging_dir, &save.revision)?;
    Ok(())
}
```

Requirements:

Promotion should be the only place that changes the active pointer:

```rust
pub fn atomically_promote_staged_revision(
    map_save_dir: &Path,
    staging_dir: &Path,
    revision: &MapRevision,
) -> Result<PathBuf, PersistenceError> {
    let final_dir = map_save_dir.join(REVISIONS_DIR).join(revision_dir_name(revision));
    fs::create_dir_all(final_dir.parent().expect("revision dir has parent"))
        .map_err(|e| PersistenceError::Serialize(format!("mkdir revisions: {e}")))?;
    if final_dir.exists() {
        validate_revision_directory_identity(&final_dir, revision)?;
        if staging_dir.exists() {
            fs::remove_dir_all(staging_dir)
                .map_err(|e| PersistenceError::Serialize(format!("remove duplicate staging dir: {e}")))?;
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

pub fn validate_staged_revision(
    staging_dir: &Path,
    expected: &ServerValidatedMapSave,
) -> Result<(), PersistenceError> {
    let meta = FsMapMetaStore { map_dir: Arc::new(staging_dir.to_path_buf()) }.load(&())?
        .ok_or_else(|| PersistenceError::Deserialize("materialized revision missing map metadata".into()))?;
    if meta.seed != expected.meta.seed {
        return Err(PersistenceError::Deserialize("staged meta seed does not match validated save".into()));
    }

    for (chunk_pos, expected_chunk) in &expected.chunks {
        let actual = FsChunkStore { map_dir: Arc::new(staging_dir.to_path_buf()) }.load(chunk_pos)?
            .ok_or_else(|| PersistenceError::Deserialize(format!("materialized revision missing terrain chunk {chunk_pos}")))?;
        if actual.version != expected_chunk.version {
            return Err(PersistenceError::VersionMismatch { expected: expected_chunk.version, actual: actual.version });
        }
    }

    Ok(())
}
```

`map_save_dir(...)` callers that construct `FsMapMetaStore`, `FsMapEntitiesStore`, `FsChunkStore`, and `FsChunkEntitiesStore` must first call `store_map_dir_for_loading(...)` so legacy saves keep working until an active pointer exists. Once promoted, the active directory is the mutable filesystem working copy for normal save systems; `accepted_head.bin` records the last accepted remote/base revision, and later local mutations are represented by publish drafts instead of treating the directory name as the live content hash.

1. reuse existing `Store` serialization/deserialization logic where practical;
2. represent `Tombstoned` and unlisted slots by omitting/deleting them from the staged revision before promotion;
3. preserve authoritative empty entity files separately from absent files;
4. fix `FsMapEntitiesStore::load` so an existing empty `entities.bin` returns `Some(vec![])` instead of collapsing to `None`;
5. leave the active revision pointer/head untouched until the staged revision is fully written and validated;
6. on startup, remove incomplete staging directories and validate that the active pointer/head references a complete staged revision before normal map loading.

#### 4. Fake remote store adapter seams and integration-test fixtures

**Files**: `crates/nostr_map_persistence/src/stores.rs`, `crates/server/src/persistence/mod.rs`, `crates/server/tests/world_persistence.rs` **Action**: modify

Add public key/result seams needed for remote lookup, but avoid test-only manifest DTOs or fake-only key types. Integration tests may define in-memory `Store` implementations such as `FakeManifestHeadStore`, `FakeManifestByHashStore`, and `FakeBlobFetchStore`, but their values must use the real `nostr_map_persistence` manifest type (`NostrMapManifest`) and the same key types as production. Do not hide fakes behind library `#[cfg(test)]`, because `crates/server/tests/...` compiles the server crate as a normal dependency. Do not add a new `RemoteMapPersistence` trait; model lookup through concrete `Store<K, V>` adapters so tests and production use the same persistence orchestration shape.

```rust
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ManifestHeadQuery {
    pub owner: NostrPublicKey,
    pub map_id: MapInstanceId,
    pub accepted_head: Option<ManifestHash>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlobFetchRequest {
    pub blob: BlobRef,
    pub max_bytes: u64,
}

// In crates/server/tests/world_persistence.rs:
#[derive(Clone, Default)]
struct FakeManifestHeadStore {
    manifests: Arc<HashMap<ManifestHash, NostrMapManifest>>,
    heads: Arc<HashMap<(NostrPublicKey, MapInstanceId), ManifestHash>>,
}

impl Store<ManifestHeadQuery, NostrMapManifest> for FakeManifestHeadStore {
    fn load(&self, key: &ManifestHeadQuery) -> Result<Option<NostrMapManifest>, PersistenceError> {
        let Some(head_hash) = self.heads.get(&(key.owner, key.map_id.clone())) else {
            trace!(?key.map_id, ?key.owner, "fake remote has no visible head");
            return Ok(None);
        };
        let manifest = self.manifests.get(head_hash).cloned().ok_or_else(|| {
            PersistenceError::Deserialize(format!("fake head references missing manifest {head_hash:?}"))
        })?;
        if manifest.owner != key.owner || manifest.map_id != key.map_id {
            return Err(PersistenceError::Deserialize("fake head manifest owner/map_id mismatch".into()));
        }
        Ok(Some(manifest))
    }

    fn save(&self, _key: &ManifestHeadQuery, _value: &NostrMapManifest) -> Result<(), PersistenceError> {
        Err(PersistenceError::Serialize("FakeManifestHeadStore is read-only; construct its maps directly".into()))
    }
}

#[derive(Clone, Default)]
struct FakeManifestByHashStore {
    manifests: Arc<HashMap<ManifestHash, NostrMapManifest>>,
}

impl Store<ManifestHash, NostrMapManifest> for FakeManifestByHashStore {
    fn load(&self, key: &ManifestHash) -> Result<Option<NostrMapManifest>, PersistenceError> {
        let manifest = self.manifests.get(key).cloned();
        if manifest.is_none() {
            trace!(?key, "fake manifest by hash lookup missed");
        }
        Ok(manifest)
    }

    fn save(&self, _key: &ManifestHash, _value: &NostrMapManifest) -> Result<(), PersistenceError> {
        Err(PersistenceError::Serialize("FakeManifestByHashStore is read-only; construct its maps directly".into()))
    }
}

#[derive(Clone, Default)]
struct FakeBlobFetchStore {
    payloads: Arc<HashMap<[u8; 32], Vec<u8>>>,
}

impl Store<BlobFetchRequest, Vec<u8>> for FakeBlobFetchStore {
    fn load(&self, key: &BlobFetchRequest) -> Result<Option<Vec<u8>>, PersistenceError> {
        if key.blob.size > key.max_bytes {
            return Err(PersistenceError::Deserialize(format!(
                "fake blob {} bytes exceeds max {}",
                key.blob.size, key.max_bytes
            )));
        }
        let Some(bytes) = self.payloads.get(&key.blob.sha256).cloned() else {
            trace!(sha256 = ?key.blob.sha256, "fake blob lookup missed");
            return Ok(None);
        };
        if bytes.len() as u64 != key.blob.size {
            return Err(PersistenceError::Deserialize(format!(
                "fake blob size mismatch: expected {}, got {}",
                key.blob.size,
                bytes.len()
            )));
        }
        let actual: [u8; 32] = sha2::Sha256::digest(&bytes).into();
        if actual != key.blob.sha256 {
            return Err(PersistenceError::Deserialize("fake blob sha256 mismatch".into()));
        }
        Ok(Some(bytes))
    }

    fn save(&self, _key: &BlobFetchRequest, _value: &Vec<u8>) -> Result<(), PersistenceError> {
        Err(PersistenceError::Serialize("FakeBlobFetchStore is read-only; construct its payload map directly".into()))
    }
}
```

`NostrMapManifest` should be easy to construct directly in tests because it is a `nostr_map_persistence` DTO, not a Nostr SDK event wrapper. Missing data should use `Ok(None)`; invalid, incomplete, divergent, or unavailable data should be surfaced as explicit `MapPersistenceRejection` values by the assembly/preflight layer rather than hidden behind a parallel lookup enum.

Keep all fake store implementations inside integration tests and expose only production-shaped public seams (`ManifestHeadQuery`, `ManifestHash`, `BlobFetchRequest`, `NostrMapManifest`, `RawValidatedMapSave`, `ServerValidatedMapSave`, and assembly/conversion functions). Integration tests can drive those seams by installing test-local `StoreBackend` or `PendingStoreOps` components that use the same key types as production; do not introduce parallel test-only manifest, head-query, manifest-key, or blob-key types.

#### 5. Chain assembly before materialization

**File**: `crates/server/src/persistence/mod.rs` **Action**: modify

Add assembly functions that fetch ancestors by hash, replay deltas from a base, and reject missing required slots before materialization.

```rust
pub enum SaveBase {
    Empty,
    Snapshot(ServerValidatedMapSave),
}

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

    fn apply_keyed_required_slot<K: PartialEq + std::fmt::Debug, T>(
        slots: &mut Vec<(K, T)>,
        key: K,
        slot: PayloadSlotState<T>,
        class: PayloadClass,
    ) -> Result<(), MapPersistenceRejection> {
        match slot {
            PayloadSlotState::Present(value) => upsert_slot(slots, key, value),
            PayloadSlotState::Absent => trace!(?class, ?key, "keyed delta slot absent; preserving previous value"),
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
            PayloadSlotState::Absent => trace!(?class, ?key, "keyed entity delta slot absent; preserving previous value"),
            PayloadSlotState::Tombstoned => remove_slot(slots, &key),
        }
    }

    let (mut meta, mut chunks, mut chunk_entities, mut map_entities, mut revision, mut expected_previous_hash) = match base {
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
                delta.revision.revision,
                delta.revision.previous_hash,
                expected_previous_hash
            )));
        }
        apply_required_slot(&mut meta, delta.meta, PayloadClass::MapMeta)?;
        for (chunk_pos, slot) in delta.chunks {
            apply_keyed_required_slot(&mut chunks, chunk_pos, slot, PayloadClass::TerrainChunk)?;
        }
        for (chunk_pos, slot) in delta.chunk_entities {
            apply_keyed_entity_slot(&mut chunk_entities, chunk_pos, slot, PayloadClass::ChunkEntities);
        }
        apply_entity_slot(&mut map_entities, delta.map_entities, PayloadClass::MapEntities);
        expected_previous_hash = Some(delta.revision.manifest_hash);
        revision = Some(delta.revision);
    }

    let revision = revision.ok_or_else(|| {
        MapPersistenceRejection::Incomplete("cannot assemble map save without a base or delta revision".into())
    })?;
    let meta = meta.ok_or_else(|| {
        MapPersistenceRejection::Incomplete("assembled map save is missing metadata".into())
    })?;
    Ok(ServerValidatedMapSave { meta, chunks, chunk_entities, map_entities, revision })
}
```

Semantics:

Replay must handle slot classes explicitly so `Empty` cannot erase required terrain/meta data:

```rust
fn apply_required_slot<T>(
    current: &mut Option<T>,
    slot: PayloadSlotState<T>,
    class: PayloadClass,
) -> Result<(), MapPersistenceRejection> {
    match slot {
        PayloadSlotState::Present(value) => *current = Some(value),
        PayloadSlotState::Absent => trace!(?class, "delta slot absent; preserving previous value"),
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
        PayloadSlotState::Absent => trace!(?class, "entity delta slot absent; preserving previous value"),
        PayloadSlotState::Tombstoned => *current = None,
    }
}
```

- `Present(value)` writes/updates a slot;
- `Empty` writes an authoritative empty value for entity slots;
- `Tombstoned` deletes the slot from the assembled save;
- `Absent` means no change in that delta, not missing content.

#### 6. Integrate fake restore with map preflight

**Files**: `crates/server/src/map/preflight.rs`, `crates/server/src/map/preparation.rs` **Action**: modify

Teach the preflight state-machine systems to select a backend by orchestrating concrete accepted-head, filesystem snapshot, manifest-head, manifest-by-hash, and blob-fetch stores in this order:

- load accepted head via `FsAcceptedMapHeadStore` and local head via `FsLocalMapHeadStore`; bootstrap filesystem revision as accepted head plus a matching `LocalMapHead` only for legacy saves with no head files;
- query fake remote `Store` adapters when configured in tests before deciding `UseFilesystem`;
- hash the active filesystem revision before remote materialization; if active files differ from both `accepted_head` and `local_head`, treat them as unpublished local state, prefer filesystem, write/rebuild an unpublished-save draft, then resume publish instead of overwriting them;
- if `local_head` is newer than `accepted_head` or the publish journal has `Pending`/`InFlight` entries, prefer filesystem and resume publish; if it has `Failed` entries, prefer filesystem and leave remote publish blocked until an explicit retry/admin clear; a remote save may only be treated as already-successful publish progress when it matches a prepared journal entry's `new_manifest_hash`;
- compare visible remote descendants to `accepted_head` only when there are no unpublished local saves;
- return `UseRemote(save)` after full chain assembly only when remote has an accepted descendant of `accepted_head` and no unpublished local head/journal entry would be overwritten;
- return `UseFilesystem` when remote is missing/unavailable, has no accepted newer descendant, or local unpublished changes must be preserved;
- call `materialize_validated_map_save` before existing map load proceeds;
- for startup overworld, only advance from `CheckingPersistence` to normal meta/entity/chunk loading after the selected staged revision is active.

After promotion, rebuild or reinsert all filesystem `StoreBackend`s for the map entity so they point at the active revision directory, not the preflight baseline directory captured before promotion:

```rust
pub fn install_active_revision_store_backends(
    commands: &mut Commands,
    map_entity: Entity,
    map_save_dir: &Path,
) -> Result<(), PersistenceError> {
    let active_dir = store_map_dir_for_loading(map_save_dir)?;
    let map_dir = Arc::new(active_dir);
    commands.entity(map_entity).insert((
        StoreBackend::new(FsMapMetaStore { map_dir: map_dir.clone() }),
        StoreBackend::new(FsMapEntitiesStore { map_dir: map_dir.clone() }),
        StoreBackend::new(FsChunkStore { map_dir: map_dir.clone() }),
        StoreBackend::new(FsChunkEntitiesStore { map_dir }),
    ));
    Ok(())
}
```

Keep production remote disabled in Phase 2 unless tests install fake `Store` adapters. `SaveBase` should use already-loaded local snapshot data, not filesystem paths, so assembly is backend-agnostic.

#### 7. Store read/write helpers for staged validation

**Files**: `crates/server/src/persistence/fs_map_meta.rs`, `crates/server/src/persistence/fs_map_entities.rs`, `crates/voxel_map_engine/src/persistence/fs_chunk.rs`, `crates/voxel_map_engine/src/persistence/fs_chunk_entities.rs` **Action**: modify

Add small public/internal helpers so staged materialization can validate specific staged files without duplicating version/deserialization logic, or factor private read/validate logic into reusable functions. Include an explicit `FsMapEntitiesStore::load` behavior change: missing `entities.bin` returns `None`, but an existing empty envelope returns `Some(vec![])`.

```rust
pub(crate) fn load_map_meta_file(path: &Path) -> Result<MapMeta, PersistenceError> {
    let bytes = fs::read(path).map_err(|e| PersistenceError::Deserialize(format!("read meta: {e}")))?;
    let meta: MapMeta = bincode::deserialize(&bytes)
        .map_err(|e| PersistenceError::Deserialize(format!("deserialize meta: {e}")))?;
    if meta.version != META_VERSION {
        return Err(PersistenceError::VersionMismatch { expected: META_VERSION, actual: meta.version });
    }
    Ok(meta)
}
```

#### 8. End-to-end restore tests

**File**: `crates/server/tests/world_persistence.rs` **Action**: modify

Add tests named with `remote_restore` in the test name so they can be filtered by `cargo test -p server remote_restore`:

- `remote_restore_missing_local_save_materializes_meta_chunks_and_entities`;
- `remote_restore_delta_chain_replays_from_filesystem_base`;
- `remote_restore_incomplete_slot_rejected`;
- `remote_restore_tombstone_removes_slot`;
- `remote_restore_accepted_head_written_after_files`;
- `remote_restore_staging_cleanup_removes_interrupted_revisions`;
- `remote_restore_divergent_chain_preserves_filesystem`.

#### 9. Existing voxel persistence regression tests

**File**: `crates/server/tests/voxel_persistence.rs` **Action**: modify

Add or update tests to prove materialized chunks are loadable by `FsChunkStore` and dirty saves after restore still write through the normal filesystem store.

### Verification

#### Automated

- [x] `if pgrep -af 'cargo (build|check|test)|cargo-make|rustc' | grep -v pgrep; then echo busy >&2; exit 1; fi`
- [x] `cargo test -p server remote_restore`
- [x] `cargo test -p server world_persistence`
- [x] `cargo test -p server voxel_persistence`

#### Manual

- [x] Create a temp `worlds/` directory with no local save, enable the fake remote test resource in a local harness, and confirm `map.meta.bin`, chunk files, entity files, and `accepted_head.bin` appear only after a complete valid chain.
- [x] Interrupt a materialization run while a staging directory exists and confirm the next startup removes incomplete staging data before loading.

---

## Phase 3: Real Nostr/Blossom Read Path

### Changes

#### 0. Wire `bevy-persistence` to the local feature branch

**File**: `Cargo.toml` **Action**: modify

Before editing `git/bevy-persistence`, switch the workspace dependency to the local path for this feature branch:

```toml
persistence = { package = "bevy-persistence", path = "git/bevy-persistence" }
```

Do this because `git/bevy-persistence` is currently excluded from the workspace and the active dependency points at GitHub. Update `Cargo.lock` after the dependency change. When the async Store changes are upstreamed, this can be reverted to a pinned git revision.

#### 1. Keep `nostr_client` generic and add `nostr_map_persistence`

**Files**: `Cargo.toml`, `crates/nostr_client/src/lib.rs`, `crates/nostr_map_persistence/Cargo.toml`, `crates/nostr_map_persistence/src/lib.rs` **Action**: modify/create

`nostr_client` should add generic Nostr event/query and Blossom blob modules alongside its existing auth, identity, announcement, plugin, and relay APIs. It must not export a map persistence module or any `MapInstanceId`/manifest-specific type.

```rust
// crates/nostr_client/src/lib.rs
pub mod blobs;
pub mod events;
pub mod relay_pool;

pub use blobs::{BlobRef, BlobReadError, BlobWriteError, VerifiedBlob};
pub use events::{NostrEventDraft, NostrEventKind, VerifiedNostrEvent};
```

Ensure the already-created map-specific integration crate exports the Phase 3 read/store modules and has the needed dependencies; Phase 4 adds the publish module.

```toml
# crates/nostr_map_persistence/Cargo.toml
[package]
name = "nostr_map_persistence"
version = "0.1.0"
edition = "2021"

[dependencies]
protocol = { workspace = true }
nostr_client = { workspace = true }
persistence = { workspace = true }
serde = { workspace = true, features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
thiserror = "2"
url = "2"
```

```rust
// crates/nostr_map_persistence/src/lib.rs
pub mod manifest;
pub mod policy;
pub mod read;
pub mod stores;

pub use manifest::{ManifestPayloadDescriptor, NostrMapManifest, VerifiedManifest};
pub use nostr_client::BlobRef;
pub use policy::{MapPersistencePolicy, NostrMapQueryPolicy};
```

#### 2. Add dependencies for generic blob HTTP and map hashing

**Files**: `crates/nostr_client/Cargo.toml`, `crates/nostr_map_persistence/Cargo.toml` **Action**: modify

Add cross-target HTTP/blob dependencies to `nostr_client`; keep map hashing and policy dependencies in `nostr_map_persistence`.

```toml
# crates/nostr_client/Cargo.toml
sha2 = "0.10"
url = "2"
thiserror = "2"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }

# crates/nostr_map_persistence/Cargo.toml
sha2 = "0.10"
thiserror = "2"
url = "2"
```

Do not broadly gate production Nostr/Blossom reads behind `not(wasm32)`. `reqwest` uses browser Fetch on `wasm32`; only truly native-only knobs such as redirect policy, native TLS/proxy/Tor controls, or blocking APIs should be `cfg(not(target_arch = "wasm32"))`. Tests use fake stores/helpers and must not require real network.

Before adding real remote adapters, extend the now-path-backed `bevy-persistence` with an async Store API and async pending-op component.

**Additional files**: `Cargo.toml`, `Cargo.lock`, `git/bevy-persistence/src/store.rs`, `git/bevy-persistence/src/async_ops.rs`, `git/bevy-persistence/src/lib.rs` **Action**: modify/create

The trait should mirror `Store<K, V>` but return Bevy conditional-send futures, for example `BoxedFuture<'a, Result<Option<V>, PersistenceError>>` where the boxed future is `Send` on native and not forced to be `Send` on `wasm32`. The pending-op component should mirror `PendingStoreOps` completion/error queues and spawn through Bevy task pools, relying on Bevy's single-threaded `spawn_local` behavior on WASM rather than adding a separate web-only path.

Keep the existing sync `Store` and `PendingStoreOps` intact for blocking filesystem backends. Add async primitives alongside them rather than replacing the current filesystem API in this phase.

The async API should be concrete enough to compile on native and wasm:

```rust
// git/bevy-persistence/src/store.rs
#[cfg(not(target_arch = "wasm32"))]
pub type BoxedStoreFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
#[cfg(target_arch = "wasm32")]
pub type BoxedStoreFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

#[cfg(not(target_arch = "wasm32"))]
pub trait AsyncStoreThreadBounds: Send + Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + Sync> AsyncStoreThreadBounds for T {}

#[cfg(target_arch = "wasm32")]
pub trait AsyncStoreThreadBounds {}
#[cfg(target_arch = "wasm32")]
impl<T> AsyncStoreThreadBounds for T {}

pub trait AsyncStore<K, V>: Clone + AsyncStoreThreadBounds + 'static
where
    K: Clone + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    fn load<'a>(&'a self, key: &'a K) -> BoxedStoreFuture<'a, Result<Option<V>, PersistenceError>>;
    fn save<'a>(&'a self, key: &'a K, value: &'a V) -> BoxedStoreFuture<'a, Result<(), PersistenceError>>;
}

#[derive(Component, Clone)]
pub struct AsyncStoreBackend<K, V, S: AsyncStore<K, V>>(pub S, pub PhantomData<(K, V)>);
```

```rust
// git/bevy-persistence/src/async_ops.rs
#[derive(Component)]
pub struct PendingAsyncStoreOps<K, V> {
    pub loads: Vec<Task<(K, Result<Option<V>, PersistenceError>)>>,
    pub saves: Vec<Task<(K, Result<(), PersistenceError>)>>,
    pub completed_loads: Vec<(K, Option<V>)>,
    pub load_errors: Vec<(K, PersistenceError)>,
    pub completed_saves: Vec<K>,
    pub save_errors: Vec<(K, PersistenceError)>,
}

impl<K, V> PendingAsyncStoreOps<K, V>
where
    K: Clone + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    pub fn spawn_load<S: AsyncStore<K, V>>(&mut self, store: &S, key: K) {
        let store = store.clone();
        let task = AsyncComputeTaskPool::get().spawn(async move {
            let result = store.load(&key).await;
            (key, result)
        });
        self.loads.push(task);
    }
}
```

If Bevy's wasm task-pool type rejects the native `Task` shape for non-`Send` futures, split only the spawn helper behind `cfg(target_arch = "wasm32")`; keep the public `AsyncStore` and pending result queues identical.

#### 3. Signed manifest, payload descriptors, policy, and query policy

**Files**: `crates/nostr_map_persistence/src/manifest.rs`, `crates/nostr_map_persistence/src/policy.rs` **Action**: create/modify

Extend the Phase 2 map-specific manifest/descriptors/policy DTOs in `nostr_map_persistence` with real Nostr event verification helpers. The manifest content should serialize as JSON and remain independent of server ECS types except for protocol-safe identity/map types. `nostr_client` should see this only as opaque event content and generic blob references.

```rust
use nostr_client::BlobRef;
use protocol::{MapInstanceId, NostrPublicKey};

pub const NOSTR_KIND_MAP_MANIFEST: u16 = 30079;
pub const MAP_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const MAP_TAG: &str = "m";
pub const MANIFEST_HASH_TAG: &str = "x";
pub const PREVIOUS_MANIFEST_HASH_TAG: &str = "y";

pub fn map_tag_value(owner: NostrPublicKey, map_id: &MapInstanceId) -> String {
    let owner_key = nostr_client::npub_from_nostr_public_key(owner);
    let map_key = match map_id {
        MapInstanceId::Overworld => "overworld".to_owned(),
        MapInstanceId::Homebase { owner } => {
            format!("homebase-{}", nostr_client::npub_from_nostr_public_key(*owner))
        }
    };
    format!("{owner_key}:{map_key}")
}

pub fn manifest_event_tags(manifest: &NostrMapManifest) -> Result<Vec<NostrTag>, ManifestVerificationError> {
    verify_descriptor_root(manifest)?;
    let manifest_hash = compute_manifest_hash(manifest)?;
    let map_key = map_tag_value(manifest.owner, &manifest.map_id);
    let manifest_hash_text = manifest_hash_hex(manifest_hash);
    let mut tags = vec![
        NostrTag::new("d", format!("{map_key}:{manifest_hash_text}")),
        NostrTag::new(MAP_TAG, map_key),
        NostrTag::new(MANIFEST_HASH_TAG, manifest_hash_text),
        NostrTag::new("r", manifest.revision.to_string()),
    ];
    if let Some(previous_hash) = manifest.previous_hash {
        tags.push(NostrTag::new(PREVIOUS_MANIFEST_HASH_TAG, manifest_hash_hex(previous_hash)));
    }
    Ok(tags)
}

```

Add policy structs:

```rust
pub struct MapPersistencePolicy {
    pub max_blob_bytes: u64,
    pub max_manifest_bytes: usize,
    pub max_payloads: usize,
    pub allowed_payload_classes: BTreeSet<PayloadClass>,
    pub allowed_blossom_hosts: BTreeSet<String>,
}

pub struct NostrMapQueryPolicy {
    pub relays: Vec<String>,
    pub timeout: Duration,
    pub limit: usize,
    pub tie_break: ManifestTieBreak,
}
```

#### 4. Manifest verification and descriptor root

**File**: `crates/nostr_map_persistence/src/manifest.rs` **Action**: modify

Implement verification for both event content and required indexed tags. Because kind `30079` is addressable, every revision must use a unique `d` tag that includes the manifest hash; do not reuse the same `d` tag for every revision or relays may replace ancestors. Latest lookup queries by `author + kind + #m=<map_tag_value>` and ancestor lookup queries by `author + kind + #x=<previous_hash_hex>`.

Implement:

```rust
pub fn verify_manifest_event(
    event_json: &str,
    expected_owner: NostrPublicKey,
    expected_map_id: &MapInstanceId,
) -> Result<NostrMapManifest, ManifestVerificationError> {
    let event = nostr_client::events::verify_event_json(event_json)?;
    if event.kind != NostrEventKind::Custom(NOSTR_KIND_MAP_MANIFEST) {
        return Err(ManifestVerificationError::Invalid(format!(
            "expected kind {NOSTR_KIND_MAP_MANIFEST}, got {:?}",
            event.kind
        )));
    }
    if event.pubkey != expected_owner {
        return Err(ManifestVerificationError::Invalid("manifest signer does not match expected owner".into()));
    }
    let manifest: NostrMapManifest = serde_json::from_str(&event.content)?;
    if manifest.owner != expected_owner || &manifest.map_id != expected_map_id {
        return Err(ManifestVerificationError::Invalid("manifest owner/map id does not match query".into()));
    }
    if manifest.schema_version != MAP_MANIFEST_SCHEMA_VERSION {
        return Err(ManifestVerificationError::Invalid(format!(
            "unsupported manifest schema version {}",
            manifest.schema_version
        )));
    }
    verify_descriptor_root(&manifest)?;
    verify_manifest_event_tags(&event, &manifest)?;
    Ok(manifest)
}

pub fn verify_manifest_event_tags(
    event: &VerifiedNostrEvent,
    manifest: &NostrMapManifest,
) -> Result<(), ManifestVerificationError> {
    let manifest_hash = compute_manifest_hash(manifest)?;
    let manifest_hash_text = manifest_hash_hex(manifest_hash);
    let map_key = map_tag_value(manifest.owner, &manifest.map_id);
    let expected_d = format!("{map_key}:{manifest_hash_text}");

    fn tag_values<'a>(event: &'a VerifiedNostrEvent, name: &str) -> Vec<&'a str> {
        event.tags.iter()
            .filter(|tag| tag.name == name)
            .map(|tag| tag.value.as_str())
            .collect()
    }

    fn require_one(event: &VerifiedNostrEvent, name: &str, expected: &str) -> Result<(), ManifestVerificationError> {
        let values = tag_values(event, name);
        if values.len() != 1 {
            return Err(ManifestVerificationError::Invalid(format!(
                "manifest event requires exactly one #{name} tag"
            )));
        }
        if values[0] != expected {
            return Err(ManifestVerificationError::Invalid(format!(
                "manifest event #{name} tag mismatch: expected {expected}, got {}",
                values[0]
            )));
        }
        Ok(())
    }

    require_one(event, "d", &expected_d)?;
    require_one(event, MAP_TAG, &map_key)?;
    require_one(event, MANIFEST_HASH_TAG, &manifest_hash_text)?;
    match manifest.previous_hash {
        Some(previous_hash) => require_one(event, PREVIOUS_MANIFEST_HASH_TAG, &manifest_hash_hex(previous_hash))?,
        None => {
            if !tag_values(event, PREVIOUS_MANIFEST_HASH_TAG).is_empty() {
                return Err(ManifestVerificationError::Invalid(
                    "genesis manifest must not include previous-hash tag".into(),
                ));
            }
        }
    }
    Ok(())
}

pub fn manifest_payload_descriptor_order(descriptor: &ManifestPayloadDescriptor) -> Vec<u8> {
    serde_json::to_vec(descriptor).expect("manifest payload descriptors must serialize for deterministic ordering")
}

pub fn verify_descriptor_root(manifest: &NostrMapManifest) -> Result<(), ManifestVerificationError> {
    let mut payloads = manifest.payloads.iter().collect::<Vec<_>>();
    payloads.sort_by(|a, b| manifest_payload_descriptor_order(a).cmp(&manifest_payload_descriptor_order(b)));

    let mut hasher = Sha256::new();
    hasher.update(DESCRIPTOR_ROOT_DOMAIN);
    hasher.update((payloads.len() as u64).to_be_bytes());
    for descriptor in payloads {
        hasher.update(serde_json::to_vec(&descriptor.class).map_err(ManifestVerificationError::CanonicalSerialization)?);
        hasher.update(serde_json::to_vec(&descriptor.key).map_err(ManifestVerificationError::CanonicalSerialization)?);
        hasher.update(descriptor.schema_version.to_be_bytes());
        match &descriptor.slot {
            ManifestPayloadSlot::Present { blob } => {
                hasher.update([0]);
                hasher.update(blob.sha256);
                hasher.update(blob.size.to_be_bytes());
            }
            ManifestPayloadSlot::Empty => hasher.update([1]),
            ManifestPayloadSlot::Absent => hasher.update([2]),
            ManifestPayloadSlot::Tombstoned => hasher.update([3]),
        }
    }

    let actual: [u8; 32] = hasher.finalize().into();
    if actual != manifest.descriptor_root {
        return Err(ManifestVerificationError::Invalid("manifest descriptor root mismatch".into()));
    }
    Ok(())
}
```

Descriptor root hashing must be domain-separated, deterministic, and covered by tests that mutate one field at a time.

Define the hash inputs once and reuse them for restore, accepted heads, and publish retry idempotency:

```rust
pub const DESCRIPTOR_ROOT_DOMAIN: &[u8] = b"untitled-brawler/map-payload-descriptor/v1";
pub const MANIFEST_HASH_DOMAIN: &[u8] = b"untitled-brawler/map-manifest/v1";

#[derive(Clone, Debug)]
pub struct VerifiedManifest {
    pub manifest: NostrMapManifest,
    pub manifest_hash: ManifestHash,
    pub raw_event_json: String,
}

pub fn canonical_manifest_bytes(manifest: &NostrMapManifest) -> Result<Vec<u8>, ManifestVerificationError> {
    serde_json::to_vec(manifest).map_err(ManifestVerificationError::CanonicalSerialization)
}

pub fn compute_manifest_hash(manifest: &NostrMapManifest) -> Result<ManifestHash, ManifestVerificationError> {
    let mut hasher = Sha256::new();
    hasher.update(MANIFEST_HASH_DOMAIN);
    hasher.update(canonical_manifest_bytes(manifest)?);
    Ok(hasher.finalize().into())
}

pub fn verify_manifest_event_with_hash(
    event_json: &str,
    expected_owner: NostrPublicKey,
    expected_map_id: &MapInstanceId,
) -> Result<VerifiedManifest, ManifestVerificationError> {
    let manifest = verify_manifest_event(event_json, expected_owner, expected_map_id)?;
    let manifest_hash = compute_manifest_hash(&manifest)?;
    Ok(VerifiedManifest { manifest, manifest_hash, raw_event_json: event_json.to_owned() })
}
```

#### 5. Revision-chain verification and ancestor fetch

**File**: `crates/nostr_map_persistence/src/read.rs` **Action**: modify

Implement:

```rust
pub enum RevisionDecision {
    AtAcceptedHead,
    Descendant(Vec<NostrMapManifest>),
}

pub fn verify_revision_chain(
    manifest_chain: &[NostrMapManifest],
    accepted_head: Option<MapRevision>,
) -> Result<RevisionDecision, MapPersistenceRejection> {
    if manifest_chain.is_empty() {
        return Err(MapPersistenceRejection::Incomplete("manifest chain is empty".into()));
    }

    let head_hash = compute_manifest_hash(&manifest_chain[0])
        .map_err(|error| MapPersistenceRejection::Invalid(error.to_string()))?;
    if let Some(accepted) = &accepted_head {
        if head_hash == accepted.manifest_hash {
            if manifest_chain[0].revision != accepted.revision {
                return Err(MapPersistenceRejection::Invalid(
                    "accepted manifest hash has mismatched revision number".into(),
                ));
            }
            return Ok(RevisionDecision::AtAcceptedHead);
        }
        if manifest_chain[0].revision <= accepted.revision {
            return Err(MapPersistenceRejection::Divergent(
                "remote candidate is not newer than accepted head".into(),
            ));
        }
    }

    for pair in manifest_chain.windows(2) {
        let child = &pair[0];
        let parent = &pair[1];
        let parent_hash = compute_manifest_hash(parent)
            .map_err(|error| MapPersistenceRejection::Invalid(error.to_string()))?;
        if child.previous_hash != Some(parent_hash) {
            return Err(MapPersistenceRejection::Divergent(
                "manifest chain previous_hash does not match parent hash".into(),
            ));
        }
        if parent.revision >= child.revision {
            return Err(MapPersistenceRejection::Invalid(
                "manifest revision numbers must strictly increase".into(),
            ));
        }
    }

    let tail = manifest_chain.last().expect("manifest_chain was checked non-empty");
    match accepted_head {
        Some(accepted) if tail.previous_hash != Some(accepted.manifest_hash) => {
            Err(MapPersistenceRejection::Divergent(
                "manifest chain does not descend from accepted head".into(),
            ))
        }
        None if tail.previous_hash.is_some() => Err(MapPersistenceRejection::Incomplete(
            "manifest chain without accepted head must include genesis".into(),
        )),
        _ => Ok(RevisionDecision::Descendant(manifest_chain.to_vec())),
    }
}

pub async fn fetch_manifest_ancestors(
    client: &nostr_client::events::NostrEventClient,
    head: &NostrMapManifest,
    accepted_head: Option<MapRevision>,
) -> Result<Vec<NostrMapManifest>, RemotePersistenceError> {
    let mut chain = vec![head.clone()];
    let mut current = head.clone();
    let mut seen = BTreeSet::new();
    seen.insert(compute_manifest_hash(&current)?);

    loop {
        let current_hash = compute_manifest_hash(&current)?;
        if accepted_head.as_ref().is_some_and(|accepted| accepted.manifest_hash == current_hash) {
            return Ok(chain);
        }

        let Some(previous_hash) = current.previous_hash else {
            if accepted_head.is_some() {
                return Err(RemotePersistenceError::Divergent(
                    "remote chain reached genesis before accepted head".into(),
                ));
            }
            return Ok(chain);
        };
        if accepted_head.as_ref().is_some_and(|accepted| accepted.manifest_hash == previous_hash) {
            return Ok(chain);
        }

        let events = client.query(
            NostrEventQuery::new()
                .author(current.owner)
                .kind(NostrEventKind::Custom(NOSTR_KIND_MAP_MANIFEST))
                .tag(MANIFEST_HASH_TAG, manifest_hash_hex(previous_hash))
                .limit(2),
        ).await?;

        let mut matches = Vec::new();
        for event_json in events {
            let verified = verify_manifest_event_with_hash(&event_json, current.owner, &current.map_id)?;
            if verified.manifest_hash == previous_hash {
                matches.push(verified.manifest);
            }
        }
        if matches.is_empty() {
            return Err(RemotePersistenceError::MissingAncestor(previous_hash));
        }
        if matches.len() > 1 {
            return Err(RemotePersistenceError::AmbiguousAncestor(previous_hash));
        }
        let ancestor = matches.pop().expect("ancestor match exists");
        if ancestor.revision >= current.revision {
            return Err(RemotePersistenceError::Invalid("ancestor revision must be lower than child revision".into()));
        }
        if !seen.insert(previous_hash) {
            return Err(RemotePersistenceError::Invalid("manifest chain contains a hash cycle".into()));
        }
        current = ancestor.clone();
        chain.push(ancestor);
    }
}
```

Reject rollback, missing ancestors, ambiguous forks, and divergent heads. Latestness is only relative to configured query policy and local accepted head. The addressable event kind is retained, but hash-indexed tags are the chain lookup source of truth; tests must prove an ancestor can be fetched by `#x` even when it is not the latest revision for a map.

#### 6. Generic Blossom URL and byte verification

**File**: `crates/nostr_client/src/blobs.rs` **Action**: modify

Implement generic blob URL, byte, upload, and download helpers. Keep this module free of map terminology; `nostr_map_persistence` supplies map-specific policy values and payload descriptors. Do not add a separate `BlobTransport` trait unless the async Store adapter `AsyncStore<BlobFetchRequest, VerifiedBlob>` in Phase 3.8 cannot express the operation.

```rust
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct BlobRef {
    pub sha256: [u8; 32],
    pub size: u64,
    pub content_type: String,
    pub urls: Vec<String>,
}

pub struct VerifiedBlob {
    pub sha256: [u8; 32],
    pub bytes: Vec<u8>,
}

pub struct BlobFetchPolicy {
    pub max_bytes: u64,
    pub allowed_hosts: BTreeSet<String>,
}

pub fn verify_blob_url(url: &url::Url, policy: &BlobFetchPolicy) -> Result<(), BlobReadError> {
    if url.scheme() != "https" {
        return Err(BlobReadError::InvalidUrl(format!("blob URL must use https: {url}")));
    }
    let host = url.host_str().ok_or(BlobReadError::MissingHost)?;
    if !policy.allowed_hosts.contains(host) {
        return Err(BlobReadError::ForbiddenHost(host.to_owned()));
    }
    Ok(())
}

pub fn verify_blob_bytes(
    expected_sha256: [u8; 32],
    expected_size: Option<u64>,
    bytes: Vec<u8>,
) -> Result<VerifiedBlob, BlobReadError> {
    if let Some(expected_size) = expected_size {
        let actual_size = bytes.len() as u64;
        if actual_size != expected_size {
            return Err(BlobReadError::SizeMismatch { expected: expected_size, actual: actual_size });
        }
    }
    let actual = sha2::Sha256::digest(&bytes);
    if actual.as_slice() != expected_sha256 {
        return Err(BlobReadError::HashMismatch { expected: expected_sha256, actual: actual.into() });
    }
    Ok(VerifiedBlob { sha256: expected_sha256, bytes })
}
```

`fetch_and_verify_blob` must try policy-approved URLs only and accept the first byte body that matches hash and size.

#### 7. Map remote helper functions

**File**: `crates/nostr_map_persistence/src/read.rs` **Action**: modify

Add map-specific pure/async helper functions around the generic `nostr_client` event/query/blob API. Do not add map logic to `nostr_client`; server and client boundaries wrap these helpers in async Store adapters.

```rust
pub async fn latest_visible_manifest(
    client: &nostr_client::events::NostrEventClient,
    owner: NostrPublicKey,
    map_id: &MapInstanceId,
    policy: NostrMapQueryPolicy,
) -> Result<Option<NostrMapManifest>, RemotePersistenceError> {
    let map_key = map_tag_value(owner, map_id);
    let events = client.query(
        NostrEventQuery::new()
            .author(owner)
            .kind(NostrEventKind::Custom(NOSTR_KIND_MAP_MANIFEST))
            .tag(MAP_TAG, map_key)
            .limit(policy.limit)
            .timeout(policy.timeout),
    ).await?;

    let mut manifests = Vec::new();
    for event_json in events {
        manifests.push(verify_manifest_event_with_hash(&event_json, owner, map_id)?);
    }
    manifests.sort_by(|left, right| {
        left.manifest.revision
            .cmp(&right.manifest.revision)
            .then_with(|| left.manifest_hash.cmp(&right.manifest_hash))
    });
    Ok(manifests.pop().map(|verified| verified.manifest))
}

pub async fn download_payloads(
    manifest_chain: &[NostrMapManifest],
    policy: MapPersistencePolicy,
) -> Result<RawMapPayloads, RemotePersistenceError> {
    let descriptor_count = manifest_chain.iter().map(|manifest| manifest.payloads.len()).sum::<usize>();
    if descriptor_count > policy.max_payloads {
        return Err(RemotePersistenceError::Invalid(format!(
            "manifest chain references {descriptor_count} payloads; limit is {}",
            policy.max_payloads
        )));
    }

    let blob_policy = nostr_client::blobs::BlobFetchPolicy {
        max_bytes: policy.max_blob_bytes,
        allowed_hosts: policy.allowed_blossom_hosts.clone(),
    };
    let mut present_payloads = Vec::with_capacity(descriptor_count);
    for manifest in manifest_chain {
        verify_descriptor_root(manifest)?;
        for descriptor in &manifest.payloads {
            if !policy.allowed_payload_classes.contains(&descriptor.class) {
                return Err(RemotePersistenceError::Invalid(format!("payload class {:?} is not allowed", descriptor.class)));
            }
            match &descriptor.slot {
                ManifestPayloadSlot::Present { blob } => {
                    let verified = nostr_client::blobs::fetch_and_verify_blob(blob, &blob_policy).await?;
                    present_payloads.push((descriptor.clone(), verified.bytes));
                }
                ManifestPayloadSlot::Empty | ManifestPayloadSlot::Absent | ManifestPayloadSlot::Tombstoned => {
                    trace!(?descriptor.class, ?descriptor.key, "manifest descriptor has no blob to download");
                }
            }
        }
    }
    Ok(RawMapPayloads { present_payloads })
}

pub fn validate_remote_map_save(
    manifest_chain: Vec<NostrMapManifest>,
    payloads: RawMapPayloads,
    policy: MapPersistencePolicy,
    base: RawSaveBase,
) -> Result<RawValidatedMapSave, MapPersistenceRejection> {
    if manifest_chain.is_empty() {
        return Err(MapPersistenceRejection::Incomplete("remote manifest chain is empty".into()));
    }

    let mut bytes_by_descriptor = BTreeMap::new();
    for (descriptor, bytes) in payloads.present_payloads {
        let ManifestPayloadSlot::Present { blob } = &descriptor.slot else {
            return Err(MapPersistenceRejection::Invalid("raw payload bytes supplied for non-present descriptor".into()));
        };
        let verified = nostr_client::blobs::verify_blob_bytes(blob.sha256, Some(blob.size), bytes)
            .map_err(|error| MapPersistenceRejection::Invalid(error.to_string()))?;
        bytes_by_descriptor.insert((descriptor.class.clone(), descriptor.key.clone(), blob.sha256), verified.bytes);
    }

    let mut deltas = Vec::new();
    for manifest in manifest_chain.into_iter().rev() {
        verify_descriptor_root(&manifest).map_err(|error| MapPersistenceRejection::Invalid(error.to_string()))?;
        if manifest.payloads.len() > policy.max_payloads {
            return Err(MapPersistenceRejection::Invalid("manifest payload count exceeds policy".into()));
        }
        deltas.push(raw_delta_from_manifest(manifest, &policy, &bytes_by_descriptor)?);
    }
    assemble_raw_validated_map_save(base, deltas)
}
```

Keep manifest verification and chain-verification helpers separate from network helpers so they remain easy to unit test. When the verified chain starts after a local accepted head, the preflight layer must pass that accepted materialized raw save as `RawSaveBase::Snapshot`; use `RawSaveBase::Empty` only for genesis chains whose first delta has `previous_hash == None`. Server code converts the returned raw save into `ServerValidatedMapSave` after shared validation succeeds.

Raw payload conversion belongs in `crates/server/src/persistence/mod.rs`, not in `nostr_client` or `nostr_map_persistence`, and should be explicit about existing envelope formats:

```rust
impl TryFrom<RawValidatedMapSave> for ServerValidatedMapSave {
    type Error = MapPersistenceRejection;

    fn try_from(raw: RawValidatedMapSave) -> Result<Self, Self::Error> {
        let meta = decode_map_meta_payload(raw.meta)?;
        let chunks = raw.chunks.into_iter()
            .map(|(key, payload)| Ok((chunk_key_to_ivec3(key)?, decode_chunk_envelope(payload)?)))
            .collect::<Result<Vec<_>, MapPersistenceRejection>>()?;
        let chunk_entities = raw.chunk_entities.into_iter()
            .map(|(key, payload)| Ok((chunk_key_to_ivec3(key)?, decode_chunk_entities(payload)?)))
            .collect::<Result<Vec<_>, MapPersistenceRejection>>()?;
        let map_entities = raw.map_entities.map(decode_map_entities).transpose()?;
        Ok(ServerValidatedMapSave { meta, chunks, chunk_entities, map_entities, revision: raw.revision })
    }
}

impl TryFrom<RawValidatedMapDelta> for ServerValidatedMapDelta {
    type Error = MapPersistenceRejection;

    fn try_from(raw: RawValidatedMapDelta) -> Result<Self, Self::Error> {
        Ok(Self {
            revision: raw.revision,
            meta: try_map_slot(raw.meta, decode_map_meta_payload)?,
            chunks: try_map_keyed_slots(raw.chunks, decode_payload_key, decode_chunk_envelope)?,
            chunk_entities: try_map_keyed_slots(raw.chunk_entities, decode_payload_key, decode_chunk_entities)?,
            map_entities: try_map_slot(raw.map_entities, decode_map_entities)?,
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
    decode_key: impl Fn(PayloadKey) -> Result<IVec3, MapPersistenceRejection>,
    decode_value: impl Fn(T) -> Result<U, MapPersistenceRejection>,
) -> Result<Vec<(IVec3, PayloadSlotState<U>)>, MapPersistenceRejection> {
    slots.into_iter()
        .map(|(key, slot)| Ok((decode_key(key)?, try_map_slot(slot, &decode_value)?)))
        .collect()
}

fn decode_chunk_envelope(payload: RawChunkPayload) -> Result<ChunkFileEnvelope, MapPersistenceRejection> {
    let envelope = zstd_bincode_decode::<ChunkFileEnvelope>(&payload.bytes)?;
    if envelope.version != CHUNK_SAVE_VERSION {
        return Err(MapPersistenceRejection::Invalid(format!(
            "chunk payload version mismatch: expected {CHUNK_SAVE_VERSION}, got {}",
            envelope.version
        )));
    }
    Ok(envelope)
}
```

#### 8. Server boundary integration

**Files**: `crates/nostr_map_persistence/src/stores.rs`, `crates/server/src/persistence/mod.rs` **Action**: modify

Add reusable bevy-persistence async Store adapters in `nostr_map_persistence` around the map read helpers. Server code should only configure/install these adapters and convert raw validated payloads into `ServerValidatedMapDelta`/`ServerValidatedMapSave` without leaking Nostr/Blossom types into voxel engine. Keep authority-specific policy enforcement at the server boundary for map bounds, entity allowlists, quota, and class allowlists.

```rust
#[derive(Clone)]
pub struct NostrManifestStore {
    pub client: nostr_client::events::NostrEventClient,
    pub policy: NostrMapQueryPolicy,
}

impl AsyncStore<ManifestHeadQuery, NostrMapManifest> for NostrManifestStore {
    fn load<'a>(&'a self, key: &'a ManifestHeadQuery) -> BoxedStoreFuture<'a, Result<Option<NostrMapManifest>, PersistenceError>> {
        Box::pin(async move {
            latest_visible_manifest(&self.client, key.owner, &key.map_id, self.policy.clone())
                .await
                .map_err(|error| PersistenceError::Deserialize(error.to_string()))
        })
    }

    fn save<'a>(&'a self, _key: &'a ManifestHeadQuery, _value: &'a NostrMapManifest) -> BoxedStoreFuture<'a, Result<(), PersistenceError>> {
        Box::pin(async { Err(PersistenceError::Serialize("NostrManifestStore is read-only".into())) })
    }
}

#[derive(Clone)]
pub struct NostrManifestByHashStore {
    pub client: nostr_client::events::NostrEventClient,
    pub policy: NostrMapQueryPolicy,
}

impl AsyncStore<ManifestHash, NostrMapManifest> for NostrManifestByHashStore {
    fn load<'a>(&'a self, key: &'a ManifestHash) -> BoxedStoreFuture<'a, Result<Option<NostrMapManifest>, PersistenceError>> {
        Box::pin(async move {
            fetch_manifest_by_hash(&self.client, *key, self.policy.clone())
                .await
                .map_err(|error| PersistenceError::Deserialize(error.to_string()))
        })
    }

    fn save<'a>(&'a self, _key: &'a ManifestHash, _value: &'a NostrMapManifest) -> BoxedStoreFuture<'a, Result<(), PersistenceError>> {
        Box::pin(async { Err(PersistenceError::Serialize("NostrManifestByHashStore is read-only".into())) })
    }
}

#[derive(Clone)]
pub struct BlossomBlobStore {
    pub policy: MapPersistencePolicy,
}

impl AsyncStore<BlobFetchRequest, VerifiedBlob> for BlossomBlobStore {
    fn load<'a>(&'a self, key: &'a BlobFetchRequest) -> BoxedStoreFuture<'a, Result<Option<VerifiedBlob>, PersistenceError>> {
        Box::pin(async move {
            let policy = nostr_client::blobs::BlobFetchPolicy {
                max_bytes: key.max_bytes.min(self.policy.max_blob_bytes),
                allowed_hosts: self.policy.allowed_blossom_hosts.clone(),
            };
            nostr_client::blobs::fetch_and_verify_blob(&key.blob, &policy)
                .await
                .map(Some)
                .map_err(|error| PersistenceError::Deserialize(error.to_string()))
        })
    }

    fn save<'a>(&'a self, _key: &'a BlobFetchRequest, _value: &'a VerifiedBlob) -> BoxedStoreFuture<'a, Result<(), PersistenceError>> {
        Box::pin(async { Err(PersistenceError::Serialize("BlossomBlobStore read-path adapter is read-only".into())) })
    }
}
```

Native server code may additionally provide sync `Store` wrappers that block on these async helpers when useful, but the canonical remote-network adapters should be async so browser WASM can use the same persistence shape through async pending-op polling.

#### 9. Preflight uses real remote when configured

**File**: `crates/server/src/map/preflight.rs` **Action**: modify

Wire the async remote Store adapters into preflight behind optional server resources/config. The preflight state-machine systems should orchestrate concrete filesystem, accepted-head, manifest, and blob stores; do not reintroduce a read-only `MapPreflightStore`. Preserve Phase 2 backend-selection semantics:

- remote disabled or no stores configured => filesystem/default behavior;
- remote configured => query remote stores before returning `UseFilesystem`;
- missing remote head => compare/load filesystem;
- timeout/unreachable => `RemoteUnavailable` and filesystem fallback;
- verified descendant => materialize then load;
- invalid/incomplete/divergent => blocked.

Temporary Homebase security exception: Phase 3 may allow Homebase remote import before server-signed `HomebasePublicationAttestation` exists because the product direction explicitly accepts this temporary violation. Mark this path as temporary/insecure in code comments, tests, and diagnostics, and remove the exception in Phase 5 when attestation enforcement lands.

Timeout and late-result handling must be stateful, not implied by dropping a task. In Phase 3, extend `ActiveMapPreflight` with remote attempt fields instead of storing timeout state out-of-band:

```rust
#[derive(Component)]
pub struct ActiveMapPreflight {
    pub request: PendingMapPreflight,
    pub stage: MapPreflightStage,
    pub remote_attempt: Option<RemotePreflightAttempt>,
    pub decision: Option<MapPersistencePreflightDecision>,
    pub ignored_remote_generation: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct RemotePreflightAttempt {
    pub started_at: Instant,
    pub deadline: Instant,
    pub generation: u64,
}

fn poll_remote_preflight_timeout(
    time: Res<Time>,
    config: Res<RemoteMapPersistenceConfig>,
    mut active: Query<&mut ActiveMapPreflight>,
) {
    for mut preflight in &mut active {
        let Some(attempt) = preflight.remote_attempt.as_ref() else {
            trace!("preflight has no remote attempt to time out");
            continue;
        };
        if Instant::now() < attempt.deadline {
            trace!(?preflight.request.target_map_id, "remote preflight still within fallback timeout");
            continue;
        }
        preflight.decision = Some(MapPersistencePreflightDecision::RemoteUnavailable);
        preflight.ignored_remote_generation = Some(attempt.generation);
    }
}

fn drain_late_remote_results(
    mut active: Query<&mut ActiveMapPreflight>,
    mut results: ResMut<RemotePreflightResults>,
    mut quarantine: EventWriter<MapPersistenceQuarantineRequested>,
) {
    while let Some(result) = results.0.pop_front() {
        let Some(mut preflight) = active.iter_mut()
            .find(|preflight| preflight.request.target_map_id == result.target_map_id)
        else {
            trace!(?result.target_map_id, result.generation, "discarding remote result for inactive preflight");
            continue;
        };

        let stale = preflight.ignored_remote_generation
            .is_some_and(|ignored| result.generation <= ignored);
        if stale || preflight.decision.is_some() {
            match &result.result {
                Err(MapPersistenceRejection::Invalid(_))
                | Err(MapPersistenceRejection::Incomplete(_))
                | Err(MapPersistenceRejection::Divergent(_)) => {
                    quarantine.write(MapPersistenceQuarantineRequested {
                        map_id: result.target_map_id.clone(),
                        rejection: result.result.clone().expect_err("matched rejected remote result"),
                    });
                }
                _ => trace!(?result.target_map_id, result.generation, "discarding stale remote result"),
            }
            continue;
        }

        preflight.decision = Some(match result.result {
            Ok(Some(save)) => MapPersistencePreflightDecision::UseRemote(save),
            Ok(None) => MapPersistencePreflightDecision::Missing,
            Err(MapPersistenceRejection::Unavailable(_)) => MapPersistencePreflightDecision::RemoteUnavailable,
            Err(rejection) => MapPersistencePreflightDecision::Blocked(rejection),
        });
    }
}
```

#### 10. Relay pool support

**File**: `crates/nostr_client/src/relay_pool.rs` **Action**: modify

Expose enough generic event-query capability for `nostr_map_persistence` manifest store adapters, without changing relay readiness semantics. If `RelayPool` already owns a `nostr_sdk::Client`, wrap/clone it behind a map-agnostic event client helper.

```rust
impl RelayPool {
    pub fn event_client(&self) -> nostr_client::events::NostrEventClient {
        nostr_client::events::NostrEventClient::from(self.client.clone())
    }
}
```

### Verification

#### Automated

- [x] `if pgrep -af 'cargo (build|check|test)|cargo-make|rustc' | grep -v pgrep; then echo busy >&2; exit 1; fi`
- [x] `cargo test -p nostr_map_persistence map_persistence`
- [x] `cargo test -p nostr_client`
- [x] `cargo test -p server remote_restore`

#### Manual

- [x] Review test fixtures to confirm they cover one-field-at-a-time tampering for signature, pubkey, map id, kind, tags, revision, previous hash, descriptor slot, blob hash, and blob size.
- [x] Confirm no test requires an external relay, Blossom server, local HTTP server, or network access.

---

## Phase 4: Server-Owned Overworld Dual-Write

### Changes

#### 1. Publish journal and status model

**File**: `crates/server/src/persistence/mod.rs` **Action**: modify

Add persisted publish journal types.

```rust
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum RemotePublishStatus {
    Pending,
    InFlight,
    Published,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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

#[derive(Clone, Debug)]
pub struct FsRemotePublishJournalStore {
    pub save_root: PathBuf,
}

#[derive(Component, Clone, Debug, Serialize, Deserialize, Default)]
pub struct RemotePublishJournal {
    pub entries: Vec<RemotePublishJournalEntry>,
}

impl Store<MapInstanceId, RemotePublishJournal> for FsRemotePublishJournalStore {
    fn load(&self, map_id: &MapInstanceId) -> Result<Option<RemotePublishJournal>, PersistenceError> {
        let path = map_save_dir(&self.save_root, map_id).join("remote_publish_journal.bin");
        if !path.exists() {
            trace!(?map_id, "remote publish journal file is absent");
            return Ok(None);
        }
        let bytes = fs::read(&path)
            .map_err(|error| PersistenceError::Deserialize(format!("read remote publish journal {}: {error}", path.display())))?;
        let journal = bincode::deserialize(&bytes)
            .map_err(|error| PersistenceError::Deserialize(format!("deserialize remote publish journal {}: {error}", path.display())))?;
        Ok(Some(journal))
    }

    fn save(
        &self,
        map_id: &MapInstanceId,
        journal: &RemotePublishJournal,
    ) -> Result<(), PersistenceError> {
        let map_dir = map_save_dir(&self.save_root, map_id);
        fs::create_dir_all(&map_dir)
            .map_err(|error| PersistenceError::Serialize(format!("mkdir remote publish journal dir {}: {error}", map_dir.display())))?;
        let path = map_dir.join("remote_publish_journal.bin");
        let tmp_path = path.with_extension("bin.tmp");
        let bytes = bincode::serialize(journal)
            .map_err(|error| PersistenceError::Serialize(format!("serialize remote publish journal: {error}")))?;
        fs::write(&tmp_path, bytes)
            .map_err(|error| PersistenceError::Serialize(format!("write remote publish journal tmp {}: {error}", tmp_path.display())))?;
        fs::rename(&tmp_path, &path)
            .map_err(|error| PersistenceError::Serialize(format!(
                "rename remote publish journal tmp {} -> {}: {error}",
                tmp_path.display(),
                path.display()
            )))?;
        Ok(())
    }
}
```

Persist journal entries under server-controlled per-map journal files via `FsRemotePublishJournalStore` plus `StoreBackend`/`PendingStoreOps`.

#### 2. Publish helpers and server store adapters

**Files**: `crates/nostr_client/src/blobs.rs`, `crates/nostr_client/src/events.rs`, `crates/nostr_map_persistence/src/publish.rs`, `crates/nostr_map_persistence/src/stores.rs`, `crates/server/src/map/remote_publish.rs` **Action**: modify/create

Keep generic Nostr event publication and Blossom HTTP upload helpers in `nostr_client`; put map manifest construction/signing orchestration and reusable publish Store adapters in `nostr_map_persistence`; server code configures these adapters and owns journal orchestration. Do not add generic `BlobPublisher`/`RemotePublisher` traits unless async Store cannot model the operation.

```rust
// crates/nostr_client/src/blobs.rs and src/events.rs
pub struct NostrKeys(nostr_sdk::Keys);
pub struct BlossomAuth { /* signs BUD-11 kind 24242 upload tokens */ }
pub async fn upload_blob(upload_url: &str, bytes: Vec<u8>, auth: &BlossomAuth) -> Result<BlobRef, BlobWriteError>;
pub async fn publish_event(client: &NostrEventClient, event_json: String) -> Result<(), NostrEventError>;

// crates/nostr_map_persistence/src/publish.rs
pub fn build_signed_map_manifest_event(
    identity: &impl MapManifestSigner,
    mut manifest: NostrMapManifest,
) -> Result<(ManifestHash, String), RemotePersistenceError> {
    let signer = identity.public_key();
    if manifest.owner != signer {
        return Err(RemotePersistenceError::Invalid(format!(
            "manifest owner {:?} does not match signer {:?}",
            manifest.owner, signer
        )));
    }

    manifest.descriptor_root = compute_descriptor_root(&manifest.payloads)?;
    let manifest_hash = compute_manifest_hash(&manifest)?;
    let content = String::from_utf8(canonical_manifest_bytes(&manifest)?)
        .expect("canonical JSON manifest bytes are valid UTF-8");
    let event_json = identity.sign_map_manifest_event(NostrEventDraft {
        kind: NostrEventKind::Custom(NOSTR_KIND_MAP_MANIFEST),
        content,
        tags: manifest_event_tags(&manifest)?,
    })?;

    let verified = verify_manifest_event_with_hash(&event_json, manifest.owner, &manifest.map_id)?;
    if verified.manifest_hash != manifest_hash {
        return Err(RemotePersistenceError::Invalid(
            "signed manifest event hash does not match prepared manifest hash".into(),
        ));
    }
    Ok((manifest_hash, event_json))
}
```

```rust
// crates/nostr_map_persistence/src/stores.rs
#[derive(Clone)]
pub struct BlossomBlobPutStore {
    pub upload_url: String,
    pub auth: nostr_client::BlossomAuth,
}

impl AsyncStore<BlobRef, Vec<u8>> for BlossomBlobPutStore {
    fn load<'a>(&'a self, _key: &'a BlobRef) -> BoxedStoreFuture<'a, Result<Option<Vec<u8>>, PersistenceError>> {
        Box::pin(async { Err(PersistenceError::Deserialize("BlossomBlobPutStore does not support load".into())) })
    }

    fn save<'a>(&'a self, key: &'a BlobRef, value: &'a Vec<u8>) -> BoxedStoreFuture<'a, Result<(), PersistenceError>> {
        let upload_url = self.upload_url.clone();
        let expected = key.clone();
        let bytes = value.clone();
        Box::pin(async move {
            nostr_client::blobs::verify_blob_bytes(expected.sha256, Some(expected.size), bytes.clone())
                .map_err(|error| PersistenceError::Serialize(format!("verify upload blob bytes: {error}")))?;
            let uploaded = nostr_client::blobs::upload_blob(&upload_url, bytes, &self.auth)
                .await
                .map_err(|error| PersistenceError::Serialize(format!("upload Blossom blob: {error}")))?;
            if uploaded.sha256 != expected.sha256 || uploaded.size != expected.size {
                return Err(PersistenceError::Serialize(format!(
                    "uploaded blob ref mismatch: expected {} bytes {:?}, got {} bytes {:?}",
                    expected.size, expected.sha256, uploaded.size, uploaded.sha256
                )));
            }
            Ok(())
        })
    }
}

#[derive(Clone)]
pub struct NostrManifestPublishStore {
    pub client: nostr_client::events::NostrEventClient,
}

impl AsyncStore<ManifestHash, String> for NostrManifestPublishStore {
    fn load<'a>(&'a self, _key: &'a ManifestHash) -> BoxedStoreFuture<'a, Result<Option<String>, PersistenceError>> {
        Box::pin(async { Err(PersistenceError::Deserialize("NostrManifestPublishStore does not support load".into())) })
    }

    fn save<'a>(&'a self, key: &'a ManifestHash, value: &'a String) -> BoxedStoreFuture<'a, Result<(), PersistenceError>> {
        let client = self.client.clone();
        let expected_hash = *key;
        let event_json = value.clone();
        Box::pin(async move {
            let actual_hash = manifest_hash_from_signed_event_json(&event_json)
                .map_err(|error| PersistenceError::Serialize(format!("verify manifest publish event: {error}")))?;
            if actual_hash != expected_hash {
                return Err(PersistenceError::Serialize(format!(
                    "manifest publish hash mismatch: expected {:?}, got {:?}",
                    expected_hash, actual_hash
                )));
            }
            nostr_client::events::publish_event(&client, event_json)
                .await
                .map_err(|error| PersistenceError::Serialize(format!("publish Nostr manifest event: {error}")))?;
            Ok(())
        })
    }
}

```

Server journal construction stays in `crates/server/src/map/remote_publish.rs` because it uses server-local draft and journal types:

```rust
async fn upload_publish_slot<T>(
    payloads: &mut Vec<ManifestPayloadDescriptor>,
    blob_store: &impl AsyncStore<BlobRef, Vec<u8>>,
    public_blossom_base_url: &url::Url,
    class: PayloadClass,
    key: PayloadKey,
    schema_version: u32,
    slot: PayloadSlotState<T>,
    encode: impl FnOnce(T) -> Result<Vec<u8>, RemotePersistenceError>,
) -> Result<(), RemotePersistenceError> {
    let manifest_slot = match slot {
        PayloadSlotState::Present(value) => {
            let bytes = encode(value)?;
            let sha256: [u8; 32] = sha2::Sha256::digest(&bytes).into();
            let mut get_url = public_blossom_base_url.clone();
            get_url.set_path(&hex::encode(sha256));
            let blob = BlobRef {
                sha256,
                size: bytes.len() as u64,
                content_type: "application/octet-stream".to_string(),
                urls: vec![get_url.to_string()],
            };
            blob_store.save(&blob, &bytes).await?;
            ManifestPayloadSlot::Present { blob }
        }
        PayloadSlotState::Empty => ManifestPayloadSlot::Empty,
        PayloadSlotState::Absent => ManifestPayloadSlot::Absent,
        PayloadSlotState::Tombstoned => ManifestPayloadSlot::Tombstoned,
    };
    payloads.push(ManifestPayloadDescriptor { class, key, slot: manifest_slot, schema_version });
    Ok(())
}

pub async fn prepare_server_map_publish_entry(
    identity: &ServerIdentity,
    draft: ServerMapPublishDraft,
    previous_remote_manifest_hash: Option<ManifestHash>,
    blob_store: &impl AsyncStore<BlobRef, Vec<u8>>,
    public_blossom_base_url: &url::Url,
) -> Result<RemotePublishJournalEntry, RemotePersistenceError> {
    let map_id = MapInstanceId::Overworld;
    let owner = identity.nostr_public_key();
    let mut payloads = Vec::new();

    upload_publish_slot(&mut payloads, blob_store, public_blossom_base_url, PayloadClass::MapMeta, PayloadKey::Singleton, MAP_META_SCHEMA_VERSION, draft.meta, encode_map_meta_payload).await?;
    for (chunk_pos, slot) in draft.chunks {
        upload_publish_slot(
            &mut payloads,
            blob_store,
            public_blossom_base_url,
            PayloadClass::TerrainChunk,
            PayloadKey::Chunk { x: chunk_pos.x, y: chunk_pos.y, z: chunk_pos.z },
            CHUNK_SAVE_VERSION,
            slot,
            encode_chunk_payload,
        ).await?;
    }
    for (chunk_pos, slot) in draft.chunk_entities {
        upload_publish_slot(
            &mut payloads,
            blob_store,
            public_blossom_base_url,
            PayloadClass::ChunkEntities,
            PayloadKey::Chunk { x: chunk_pos.x, y: chunk_pos.y, z: chunk_pos.z },
            CHUNK_ENTITY_SAVE_VERSION,
            slot,
            encode_chunk_entities_payload,
        ).await?;
    }
    upload_publish_slot(&mut payloads, blob_store, public_blossom_base_url, PayloadClass::MapEntities, PayloadKey::Singleton, MAP_ENTITIES_SCHEMA_VERSION, draft.map_entities, encode_map_entities_payload).await?;

    payloads.sort_by(manifest_payload_descriptor_order);
    let descriptor_root = compute_descriptor_root(&payloads)?;
    let manifest = NostrMapManifest {
        map_id: map_id.clone(),
        owner,
        revision: draft.local_revision_number,
        previous_hash: previous_remote_manifest_hash,
        payloads: payloads.clone(),
        schema_version: MAP_MANIFEST_SCHEMA_VERSION,
        descriptor_root,
    };
    let (new_manifest_hash, signed_event_json) = nostr_map_persistence::publish::build_signed_map_manifest_event(identity, manifest)?;
    let local_revision = MapRevision {
        revision: draft.local_revision_number,
        previous_hash: previous_remote_manifest_hash,
        manifest_hash: new_manifest_hash,
    };

    Ok(RemotePublishJournalEntry {
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
```

Use deterministic serialization and deterministic manifest hashes so retrying an entry republishes the same manifest hash. Use draft payload-slot semantics before the manifest hash exists; convert the draft into a finalized `MapRevision` only after descriptor root and manifest hash are computed. The publish worker first turns queued drafts into pending journal entries, then publishes journal entries through async pending ops rather than blocking the ECS schedule.

#### 3. Server publish worker and save integration

**Files**: `git/bevy-persistence/src/ops.rs`, `crates/voxel_map_engine/src/lifecycle.rs`, `crates/server/src/map/remote_publish.rs`, `crates/server/src/map/mod.rs` **Action**: create/modify

After existing filesystem saves succeed in `save_dirty_chunks_debounced`/chunk entity save paths, persist a reconstructable local unpublished-save draft for the affected payload slots, then advance `local_head.bin` with a `LocalMapHead`. A journal-preparation system converts persisted drafts into `RemotePublishJournalEntry { status: Pending, signed_event_json: Some(...) }`; only `poll_remote_publish_journal` publishes manifest events. If publish later fails, preflight must preserve the newer local head and keep the failed journal entry as a filesystem-preserving blocker until explicit retry/admin clear, rather than materializing an older remote save over local filesystem data. Startup recovery must rebuild pending journal entries from persisted unpublished-save drafts, from a `local_head` ahead of `accepted_head`, or from the active filesystem revision when the active filesystem content hash differs from the recorded heads.

The enqueue point needs to be tied to drained save success, not dirty-state observation alone, and it must not exist only in memory between local filesystem save and journal persistence. First extend the synchronous `PendingStoreOps` save result model with explicit operation ids that appear on both success and failure:

```rust
// git/bevy-persistence/src/ops.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SaveOpId(pub u64);

#[derive(Resource, Default)]
pub struct SaveOpIdAllocator {
    next: u64,
}

impl SaveOpIdAllocator {
    pub fn allocate(&mut self) -> SaveOpId {
        self.next += 1;
        SaveOpId(self.next)
    }
}

#[derive(Clone, Debug)]
pub struct SaveCompletion<K> {
    pub id: Option<SaveOpId>,
    pub key: K,
}

#[derive(Debug)]
pub struct SaveFailure<K> {
    pub id: Option<SaveOpId>,
    pub key: K,
    pub error: PersistenceError,
}

pub struct PendingStoreOps<K, V> {
    pub completed_saves: Vec<SaveCompletion<K>>,
    pub save_errors: Vec<SaveFailure<K>>,
    // existing load fields and task queues stay unchanged
}

impl<K, V> PendingStoreOps<K, V> {
    pub fn spawn_save(&mut self, store: &impl Store<K, V>, key: K, value: V) {
        self.spawn_save_inner(None, store, key, value);
    }

    pub fn spawn_save_with_id(&mut self, store: &impl Store<K, V>, id: SaveOpId, key: K, value: V) {
        self.spawn_save_inner(Some(id), store, key, value);
    }
}
```

Then make the engine-owned chunk save drainer expose generic completion events without depending on server or Nostr code:

```rust
// crates/voxel_map_engine/src/lifecycle.rs
pub struct PendingSave {
    pub position: IVec3,
    pub envelope: ChunkFileEnvelope,
    pub save_id: Option<SaveOpId>,
}

#[derive(Event, Clone, Debug)]
pub struct ChunkSaveCompleted {
    pub map_entity: Entity,
    pub position: IVec3,
    pub save_id: Option<SaveOpId>,
}

#[derive(Event, Clone, Debug)]
pub struct ChunkSaveFailed {
    pub map_entity: Entity,
    pub position: IVec3,
    pub save_id: Option<SaveOpId>,
    pub error: String,
}

pub fn drain_pending_saves(
    mut map_query: Query<(Entity, &mut PendingSaves, &StoreBackend<IVec3, ChunkFileEnvelope, FsChunkStore>, &mut PendingStoreOps<IVec3, ChunkFileEnvelope>)>,
    mut completed: EventWriter<ChunkSaveCompleted>,
    mut failed: EventWriter<ChunkSaveFailed>,
) {
    for (map_entity, mut pending, chunk_store, mut chunk_ops) in &mut map_query {
        chunk_ops.poll();
        for completion in chunk_ops.completed_saves.drain(..) {
            completed.write(ChunkSaveCompleted { map_entity, position: completion.key, save_id: completion.id });
        }
        for failure in chunk_ops.save_errors.drain(..) {
            error!(?failure.key, error = %failure.error, "failed to save chunk data");
            failed.write(ChunkSaveFailed {
                map_entity,
                position: failure.key,
                save_id: failure.id,
                error: failure.error.to_string(),
            });
        }

        let mut spawned = 0;
        while !pending.queue.is_empty() && spawned < MAX_SAVE_SPAWNS_PER_FRAME {
            let save = pending.queue.pop_front().expect("pending save queue checked non-empty");
            match save.save_id {
                Some(save_id) => chunk_ops.spawn_save_with_id(&chunk_store.0, save_id, save.position, save.envelope),
                None => chunk_ops.spawn_save(&chunk_store.0, save.position, save.envelope),
            }
            spawned += 1;
        }
        if spawned >= MAX_SAVE_SPAWNS_PER_FRAME {
            trace!(spawned, remaining = pending.queue.len(), "chunk save spawn limit reached for frame");
        }
    }
}
```

Then normalize all persisted payload-class save completions into a server-local event. Terrain chunk saves arrive from the generic voxel event above; map metadata, map entities, and chunk-entity save pollers are server-owned and should emit this event directly from their existing single-drainer systems.

```rust
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MapPayloadSaveKey {
    MapMeta,
    MapEntities,
    TerrainChunk(IVec3),
    ChunkEntities(IVec3),
}

#[derive(Event, Clone, Debug)]
pub struct MapPayloadSaveCompleted {
    pub map_entity: Entity,
    pub save_id: SaveOpId,
    pub key: MapPayloadSaveKey,
}

#[derive(Event, Clone, Debug)]
pub struct MapPayloadSaveFailed {
    pub map_entity: Entity,
    pub save_id: SaveOpId,
    pub key: MapPayloadSaveKey,
    pub error: String,
}
```

Then bind server publish drafts to those ids from server-owned systems that consume the normalized events:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerMapPublishDraft {
    pub local_revision_number: u64,
    pub meta: PayloadSlotState<MapMeta>,
    pub chunks: Vec<(IVec3, PayloadSlotState<ChunkFileEnvelope>)>,
    pub chunk_entities: Vec<(IVec3, PayloadSlotState<Vec<WorldObjectSpawn>>)>,
    pub map_entities: PayloadSlotState<Vec<SavedEntity>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalUnpublishedPublishDraft {
    pub map_id: MapInstanceId,
    pub draft: ServerMapPublishDraft,
    pub save_id: SaveOpId,
}

#[derive(Clone, Debug)]
pub struct FsLocalUnpublishedPublishDraftStore {
    pub map_dir: Arc<PathBuf>,
}

impl Store<SaveOpId, LocalUnpublishedPublishDraft> for FsLocalUnpublishedPublishDraftStore {
    fn load(&self, save_id: &SaveOpId) -> Result<Option<LocalUnpublishedPublishDraft>, PersistenceError> {
        load_optional_bincode(self.map_dir.join("unpublished").join(format!("{}.bin", save_id.0)))
    }

    fn save(&self, save_id: &SaveOpId, draft: &LocalUnpublishedPublishDraft) -> Result<(), PersistenceError> {
        atomic_save_bincode(self.map_dir.join("unpublished").join(format!("{}.bin", save_id.0)), draft)
    }
}

impl FsLocalUnpublishedPublishDraftStore {
    pub fn load_all(&self) -> Result<Vec<LocalUnpublishedPublishDraft>, PersistenceError> {
        load_all_bincode_files_sorted(self.map_dir.join("unpublished"))
    }
}

#[derive(Component, Default)]
pub struct PendingRemotePublishDeltas(pub VecDeque<ServerMapPublishDraft>);

#[derive(Resource, Default)]
pub struct PendingPublishBySaveId(pub HashMap<SaveOpId, ServerMapPublishDraft>);

pub fn handle_completed_map_payload_save_for_publish(
    mut completed: EventReader<MapPayloadSaveCompleted>,
    map_ids: Query<&MapInstanceId>,
    draft_stores: Query<&StoreBackend<SaveOpId, LocalUnpublishedPublishDraft, FsLocalUnpublishedPublishDraftStore>>,
    local_head_stores: Query<&StoreBackend<(), LocalMapHead, FsLocalMapHeadStore>>,
    mut pending_by_save_id: ResMut<PendingPublishBySaveId>,
    mut deltas: Query<&mut PendingRemotePublishDeltas>,
) {
    for event in completed.read() {
        let map_id = map_ids.get(event.map_entity).expect("payload save event map entity must have MapInstanceId");
        if !matches!(map_id, MapInstanceId::Overworld) {
            trace!(?map_id, ?event.key, "remote publish skipped for non-overworld server-owned path");
            continue;
        }
        let draft = pending_by_save_id.0.remove(&event.save_id)
            .expect("completed overworld save id must have a matching publish draft");
        let persisted = LocalUnpublishedPublishDraft { map_id: map_id.clone(), draft: draft.clone(), save_id: event.save_id };
        draft_stores.get(event.map_entity)
            .expect("publishable map must have local unpublished draft store")
            .0.save(&event.save_id, &persisted)
            .expect("local unpublished publish draft must persist before local_head advances");
        local_head_stores.get(event.map_entity)
            .expect("publishable map must have local head store")
            .0.save(&(), &local_head_from_unpublished_draft(&persisted))
            .expect("local head must advance after local publish draft persists");
        deltas.get_mut(event.map_entity)
            .expect("map with publishable save must have PendingRemotePublishDeltas")
            .0.push_back(draft);
    }
}
```

Only the system that owns a given `PendingStoreOps` may call `poll()` or drain `completed_saves`/`save_errors`. Existing save pollers must be extended in place: voxel chunk `drain_pending_saves` emits generic terrain chunk completion/failure events that the server normalizes to `MapPayloadSaveCompleted/Failed`, while server-owned metadata, map-entity, and chunk-entity pollers emit `MapPayloadSaveCompleted/Failed` directly. Server publish systems consume those events and update publish state. Do not add a second system that races to drain the same queues. Legacy `spawn_save(...)` completions/errors carry `id: None` and keep existing logging/error behavior.

Add a `RemoteMapPublishWorker` resource/system:

```rust
#[derive(Resource, Default)]
pub struct RemoteMapPublishWorker {
    pub in_flight_by_map: HashSet<MapInstanceId>,
}

pub fn apply_publish_results(
    map_id: &MapInstanceId,
    journal: &mut RemotePublishJournal,
    worker: &mut RemoteMapPublishWorker,
    publish_ops: &mut PendingAsyncStoreOps<ManifestHash, String>,
    accepted_head_store: &FsAcceptedMapHeadStore,
    local_head_store: &FsLocalMapHeadStore,
    journal_store: &FsRemotePublishJournalStore,
) -> Result<(), PersistenceError> {
    while let Some(&manifest_hash) = publish_ops.completed_saves.first() {
        let entry_index = journal.entries.iter()
            .position(|entry| entry.new_manifest_hash == manifest_hash && entry.status == RemotePublishStatus::InFlight)
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
        let entry_index = journal.entries.iter()
            .position(|entry| entry.new_manifest_hash == manifest_hash && entry.status == RemotePublishStatus::InFlight)
            .expect("failed manifest publish must match an in-flight journal entry");
        let old_retry_count = journal.entries[entry_index].retry_count;
        journal.entries[entry_index].status = RemotePublishStatus::Failed;
        journal.entries[entry_index].retry_count = journal.entries[entry_index].retry_count.saturating_add(1);
        if let Err(save_error) = journal_store.save(map_id, journal) {
            journal.entries[entry_index].status = RemotePublishStatus::InFlight;
            journal.entries[entry_index].retry_count = old_retry_count;
            return Err(save_error);
        }
        let (_, error) = publish_ops.save_errors.remove(0);
        error!(?map_id, ?manifest_hash, retry_count = journal.entries[entry_index].retry_count, "remote manifest publish failed: {error}");
        worker.in_flight_by_map.remove(map_id);
    }
    Ok(())
}
```

Publish polling should preserve ordering even when multiple maps have work:

```rust
pub fn poll_remote_publish_journal(
    mut worker: ResMut<RemoteMapPublishWorker>,
    mut journals: Query<(
        &MapInstanceId,
        &StoreBackend<MapInstanceId, RemotePublishJournal, FsRemotePublishJournalStore>,
        &StoreBackend<(), MapRevision, FsAcceptedMapHeadStore>,
        &StoreBackend<(), LocalMapHead, FsLocalMapHeadStore>,
        &AsyncStoreBackend<ManifestHash, String, NostrManifestPublishStore>,
        &mut RemotePublishJournal,
        &mut PendingAsyncStoreOps<ManifestHash, String>,
    )>,
) {
    for (map_id, journal_store, accepted_head_store, local_head_store, publish_store, mut journal, mut publish_ops) in &mut journals {
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
            error!(?map_id, ?error, "failed to persist remote publish result state");
            continue;
        }

        if worker.in_flight_by_map.contains(map_id) {
            trace!(?map_id, "remote publish already in flight for map");
            continue;
        }
        if journal.entries.iter().any(|entry| entry.status == RemotePublishStatus::Failed) {
            trace!(?map_id, "remote publish blocked by earlier failed journal entry");
            continue;
        }
        let Some(entry) = journal.entries.iter_mut().find(|entry| entry.status == RemotePublishStatus::Pending) else {
            trace!(?map_id, "no pending remote publish journal entry");
            continue;
        };
        entry.status = RemotePublishStatus::InFlight;
        worker.in_flight_by_map.insert(map_id.clone());
        let event_json = entry.signed_event_json.clone()
            .expect("pending publish journal entry must contain signed event JSON");
        publish_ops.spawn_save(&publish_store.0, entry.new_manifest_hash, event_json);
        journal_store.0.save(map_id, &journal)
            .expect("remote publish journal should persist after marking entry in-flight");
    }
}
```

Rules:

- only one in-flight publish per map;
- later pending entries never publish past an earlier failed entry;
- remote/accepted head advances only after publish success;
- local head advances as `LocalMapHead` in the save-completion handler only after a reconstructable unpublished-save draft has been persisted, so startup can resume publish if the process exits before journal preparation;
- later pending entries may be squashed only if their `previous_remote_manifest_hash` is recomputed against the current remote head;
- overworld manifests must be signed by configured server identity.

#### 4. Server persistence journal helpers

**File**: `crates/server/src/persistence/mod.rs` **Action**: modify

Add journal load/save/recovery helpers. On startup, reset `InFlight` to `Pending` so interrupted publishes retry deterministically, and load all `FsLocalUnpublishedPublishDraftStore::load_all()` records into `PendingRemotePublishDeltas` before remote preflight can choose a backend. Preflight startup must inspect the journal and unpublished-draft store before remote selection; any `Pending` or `InFlight` entry means local unpublished data exists and publish should resume, while any `Failed` entry means local unpublished data exists but remote publishing remains blocked until explicit retry/admin clear. If a remote event already equals a pending entry's `new_manifest_hash`, mark that entry published and advance `accepted_head` instead of rematerializing older remote data.

```rust
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
                return Err(PersistenceError::Deserialize("unpublished draft map id does not match owning map".into()));
            }
            deltas.0.push_back(persisted.draft);
        }
    }
    Ok(())
}
```

#### 5. Voxel publish tests

**File**: `crates/server/tests/voxel_persistence.rs` **Action**: modify

Add `remote_publish`-filtered tests:

- publish N fails while N+1 is queued;
- N+1 does not publish before N succeeds;
- retry of N uses the same deterministic manifest hash;
- remote already has manifest hash counts as success;
- local chunk file exists even when remote publish fails;
- restart preflight with `local_head` ahead of `accepted_head` chooses filesystem and reloads persisted unpublished drafts instead of materializing the older remote head.

#### 6. World-object publish tests

**File**: `crates/server/tests/world_object_edit.rs` **Action**: modify

Add tests proving chunk-entity changes enqueue/publish alongside terrain and preserve authoritative empty chunk-entity files.

### Verification

#### Automated

- [x] `if pgrep -af 'cargo (build|check|test)|cargo-make|rustc' | grep -v pgrep; then echo busy >&2; exit 1; fi`
- [x] `cargo test -p server remote_publish`
- [x] `cargo test -p server voxel_persistence`
- [x] `cargo test -p server world_object_edit`

#### Manual

- [x] Run a server with remote publishing enabled (`SERVER_MAP_REMOTE_PUBLISH=1`, `SERVER_BLOSSOM_UPLOAD_URL`, `SERVER_BLOSSOM_PUBLIC_BASE_URL`, `NOSTR_RELAYS`), make two overworld edits quickly, and confirm logs show only one in-flight publish for `Overworld`.
- [x] Force the first manifest publish to fail with `SERVER_MAP_REMOTE_PUBLISH_FAIL_FIRST=1` and confirm later entries wait while local filesystem saves still advance.

---

## Phase 5: Player-Owned Homebase Publication

Phase 5 removes the temporary Phase 3 Homebase import security exception by requiring server-signed attestation and progression-bearing rejection for accepted Homebase imports.

### Changes

#### 1. Homebase attestation type

**Files**: `crates/protocol/src/map/homebase_publication.rs`, `crates/nostr_map_persistence/src/attestation.rs` **Action**: modify/create

Add server-signed homebase publication attestation wire DTOs in `protocol` if they cross Lightyear client/server messages, and put canonical serialization/signing/verification helpers in `nostr_map_persistence`. Keep both layers free of Nostr SDK types; client and server should not import any map-specific attestation type from `nostr_client`.

```rust
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct HomebasePublicationAttestation {
    pub owner: NostrPublicKey,
    pub map_id: MapInstanceId,
    pub server_revision: u64,
    pub previous_manifest_hash: Option<[u8; 32]>,
    pub descriptor_root: [u8; 32],
    pub payload_scope: HomebasePayloadScope,
    pub expires_at: u64,
    pub server_pubkey: NostrPublicKey,
    pub server_signature: Vec<u8>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct HomebasePayloadScope {
    pub terrain_chunks: Vec<IVec3>,
    pub chunk_entities: Vec<IVec3>,
    pub includes_meta: bool,
    pub includes_map_entities: bool,
}
```

Use a small `nostr_map_persistence` serializable chunk coordinate struct if `IVec3` would force Bevy/glam dependencies into shared persistence DTOs; convert at server/client boundaries.

#### 2. Client publication queue and completeness tracking

**File**: `crates/client/src/map.rs` **Action**: modify

Track authoritative replicated homebase data eligible for publication.

```rust
#[derive(Resource, Default)]
pub struct HomebasePublicationQueue {
    pub pending: VecDeque<ClientHomebaseUpdateDraft>,
}

#[derive(Clone, Debug, Default)]
pub struct HomebaseReplicaCompleteness {
    pub has_meta: bool,
    pub terrain_chunks: HashSet<IVec3>,
    pub chunk_entities: HashSet<IVec3>,
    pub has_map_entities: bool,
}
```

Update completeness only from authoritative server messages/acks/replication, not from speculative client prediction. Do not include progression-bearing state.

Represent draft data separately from speculative local edits:

```rust
#[derive(Clone, Debug)]
pub struct ClientHomebaseUpdateDraft {
    pub map_id: MapInstanceId,
    pub owner: NostrPublicKey,
    pub completeness: HomebaseReplicaCompleteness,
    pub terrain_chunks: HashMap<IVec3, ChunkFileEnvelope>,
    pub chunk_entities: HashMap<IVec3, Vec<WorldObjectSpawn>>,
    pub map_meta: Option<MapMetaSnapshot>,
    pub map_entities: Option<Vec<SavedEntitySnapshot>>,
}

pub fn mark_authoritative_chunk_publishable(
    mut completeness: ResMut<HomebaseReplicaCompleteness>,
    message: EventReader<AuthoritativeChunkReplicated>,
) {
    for event in message.read() {
        if !matches!(event.map_id, MapInstanceId::Homebase { .. }) {
            trace!(?event.map_id, "replicated chunk is not homebase publishable");
            continue;
        }
        completeness.terrain_chunks.insert(event.chunk_pos);
    }
}
```

#### 3. Transition/start completeness hooks

**File**: `crates/client/src/transition.rs` **Action**: modify

When entering a homebase transition, initialize/reset the publication completeness tracker for that `MapInstanceId`. When transition readiness completes and authoritative chunks/entities are present, mark publishable slots.

#### 4. Client publish unit and attested publish

**Files**: `crates/client/Cargo.toml`, `crates/nostr_map_persistence/src/publish.rs`, `crates/client/src/map_publication.rs` **Action**: modify/create

Add client-owned homebase publish data and keep store orchestration at the client boundary. `nostr_map_persistence` should build map manifests and use generic `nostr_client` event/blob helpers; client code should use bevy-persistence async Store adapters for blob upload and manifest publication so native and web share the same Nostr/Blossom persistence path.

In this phase, extend `NostrMapManifest` with `pub homebase_attestation: Option<HomebasePublicationAttestation>`. Existing overworld and pre-attestation manifest initializers must set it to `None`; homebase manifests must set it to `Some(attestation)` and include the field in canonical manifest hashing/signature verification.

```rust
pub struct ClientHomebaseUpdate {
    pub owner: NostrPublicKey,
    pub map_id: MapInstanceId,
    pub payloads: Vec<ManifestPayloadDescriptor>,
    pub previous_revision: Option<MapRevision>,
    pub attestation: HomebasePublicationAttestation,
}

pub fn build_homebase_manifest_event(
    signer: &impl MapManifestSigner,
    update: ClientHomebaseUpdate,
) -> Result<(ManifestHash, String), RemotePersistenceError> {
    if signer.public_key() != update.owner {
        return Err(RemotePersistenceError::Invalid("homebase manifest signer must equal owner".into()));
    }
    if !matches!(update.map_id, MapInstanceId::Homebase { owner } if owner == update.owner) {
        return Err(RemotePersistenceError::Invalid("client may only publish own homebase manifest".into()));
    }
    if update.attestation.owner != update.owner
        || update.attestation.map_id != update.map_id
        || update.attestation.previous_manifest_hash != update.previous_revision.as_ref().map(|revision| revision.manifest_hash)
    {
        return Err(RemotePersistenceError::Invalid("homebase attestation does not match update".into()));
    }

    let descriptor_root = compute_descriptor_root(&update.payloads)?;
    if descriptor_root != update.attestation.descriptor_root {
        return Err(RemotePersistenceError::Invalid("homebase descriptor root does not match attestation".into()));
    }
    let manifest = NostrMapManifest {
        map_id: update.map_id,
        owner: update.owner,
        revision: update.attestation.server_revision,
        previous_hash: update.attestation.previous_manifest_hash,
        payloads: update.payloads,
        schema_version: MAP_MANIFEST_SCHEMA_VERSION,
        descriptor_root,
        homebase_attestation: Some(update.attestation),
    };
    build_signed_map_manifest_event(signer, manifest)
}
```

`crates/client/src/map_publication.rs` should configure and use the shared `nostr_map_persistence` `AsyncStore<BlobRef, Vec<u8>>` and `AsyncStore<ManifestHash, String>` adapters, then enqueue/poll uploads through the async pending-op component. Do not require unconditional `Send` for these futures on `wasm32`; follow Bevy's conditional-send model.

#### 5. Server attestation request/verification

**Files**: `crates/nostr_map_persistence/src/attestation.rs`, `crates/server/src/map/homebase_publication.rs` **Action**: create/modify

Add shared attestation canonical serialization/signing/verification helpers in `nostr_map_persistence`, and add server-side logic to verify a descriptor root against authoritative homebase state before calling those helpers. Server-specific identity types stay in server code and implement shared signer/verifier traits.

```rust
pub fn verify_homebase_publication_attestation_request(
    signer: &impl AttestationSigner,
    owner: NostrPublicKey,
    map_id: &MapInstanceId,
    descriptor_root: [u8; 32],
    payload_scope: &HomebasePayloadScope,
    authoritative_state: &AuthoritativeHomebaseState,
    now_unix: u64,
    ttl_seconds: u64,
) -> Result<HomebasePublicationAttestation, MapPersistenceRejection> {
    let MapInstanceId::Homebase { owner: map_owner } = map_id else {
        return Err(MapPersistenceRejection::Invalid("server only attests homebase publication requests".into()));
    };
    if *map_owner != owner {
        return Err(MapPersistenceRejection::Invalid("homebase attestation owner does not match map owner".into()));
    }
    if authoritative_state.owner != owner || authoritative_state.map_id != *map_id {
        return Err(MapPersistenceRejection::Invalid("attestation request does not match authoritative homebase state".into()));
    }
    if authoritative_state.descriptor_root != descriptor_root {
        return Err(MapPersistenceRejection::Invalid("attestation descriptor root does not match authoritative state".into()));
    }
    if authoritative_state.payload_scope != *payload_scope {
        return Err(MapPersistenceRejection::Incomplete("attestation payload scope does not match authoritative state".into()));
    }
    validate_homebase_import_payload_scope(map_id, owner, &authoritative_state.validated_save, &authoritative_state.entitlements)?;
    let expires_at = now_unix.checked_add(ttl_seconds)
        .ok_or_else(|| MapPersistenceRejection::Invalid("attestation expiry overflow".into()))?;

    sign_homebase_attestation(signer, HomebasePublicationAttestation {
        owner,
        map_id: map_id.clone(),
        server_revision: authoritative_state.server_revision,
        previous_manifest_hash: authoritative_state.previous_manifest_hash,
        descriptor_root,
        payload_scope: payload_scope.clone(),
        expires_at,
        server_pubkey: signer.public_key(),
        server_signature: Vec::new(),
    })
}
```

Only sign attestations for `MapInstanceId::Homebase { owner }`. Reject overworld and foreign-owner requests.

Attestation signing must define canonical bytes and signer inputs:

```rust
pub trait AttestationSigner {
    fn public_key(&self) -> NostrPublicKey;
    fn sign_attestation_payload(&self, payload: &[u8]) -> Result<Vec<u8>, MapPersistenceRejection>;
}

pub const HOMEBASE_ATTESTATION_SIGNING_DOMAIN: &[u8] = b"untitled-brawler/homebase-publication-attestation/v1";

pub trait AttestationVerifier {
    fn verify_attestation_payload(
        &self,
        pubkey: NostrPublicKey,
        payload: &[u8],
        signature: &[u8],
    ) -> Result<(), MapPersistenceRejection>;
}

pub fn attestation_signing_payload(attestation: &HomebasePublicationAttestation) -> Result<Vec<u8>, MapPersistenceRejection> {
    let unsigned = HomebasePublicationAttestation {
        server_signature: Vec::new(),
        ..attestation.clone()
    };
    let json = serde_json::to_vec(&unsigned)
        .map_err(|e| MapPersistenceRejection::Invalid(format!("serialize attestation payload: {e}")))?;
    let mut payload = Vec::with_capacity(HOMEBASE_ATTESTATION_SIGNING_DOMAIN.len() + 1 + json.len());
    payload.extend_from_slice(HOMEBASE_ATTESTATION_SIGNING_DOMAIN);
    payload.push(0);
    payload.extend_from_slice(&json);
    Ok(payload)
}

pub fn sign_homebase_attestation(
    signer: &impl AttestationSigner,
    mut attestation: HomebasePublicationAttestation,
) -> Result<HomebasePublicationAttestation, MapPersistenceRejection> {
    attestation.server_pubkey = signer.public_key();
    attestation.server_signature.clear();
    let payload = attestation_signing_payload(&attestation)?;
    attestation.server_signature = signer.sign_attestation_payload(&payload)?;
    if attestation.server_signature.is_empty() {
        return Err(MapPersistenceRejection::Invalid("homebase attestation signature is empty".into()));
    }
    Ok(attestation)
}

pub fn verify_homebase_attestation(
    verifier: &impl AttestationVerifier,
    attestation: &HomebasePublicationAttestation,
    now_unix: u64,
) -> Result<(), MapPersistenceRejection> {
    if now_unix > attestation.expires_at {
        return Err(MapPersistenceRejection::Invalid("homebase attestation expired".into()));
    }
    if attestation.server_signature.is_empty() {
        return Err(MapPersistenceRejection::Invalid("homebase attestation signature is empty".into()));
    }
    let payload = attestation_signing_payload(attestation)?;
    verifier.verify_attestation_payload(attestation.server_pubkey, &payload, &attestation.server_signature)
}
```

Tests should mutate owner, map id, descriptor root, expiry, and signature independently.

#### 6. Import validation for homebase manifests

**Files**: `crates/nostr_map_persistence/src/validation.rs`, `crates/server/src/map/homebase_publication.rs` **Action**: modify

Shared import validation in `nostr_map_persistence` accepts player-owned homebase data only if:

- player signature is valid;
- manifest signer equals owner;
- map id is `Homebase { owner }`;
- server attestation signature is valid;
- attestation owner/map/revision/descriptor root matches manifest;
- revision descends from accepted head;
- payloads pass hash/schema/completeness validation.

Server import policy then applies authority-specific bounds, quota, entitlement, and entity allowlist checks in `crates/server/src/map/homebase_publication.rs` after raw payloads are decoded to server types.

#### 7. Server import policy rejects progression-bearing data

**File**: `crates/server/src/map/homebase_publication.rs` **Action**: modify

Add server policy checks that reject progression-bearing objects, earned inventory, character state, relationships, breeding state, rewards, unentitled furnishings/toys/eggs/rewards, and all client-published overworld data.

Use an allowlist-style validator so new progression-bearing components fail closed:

```rust
pub fn validate_homebase_import_payload_scope(
    map_id: &MapInstanceId,
    owner: NostrPublicKey,
    payloads: &ServerValidatedMapSave,
    entitlements: &PlayerEntitlements,
) -> Result<(), MapPersistenceRejection> {
    if !matches!(map_id, MapInstanceId::Homebase { owner: map_owner } if *map_owner == owner) {
        return Err(MapPersistenceRejection::Invalid("client may only import own homebase map".into()));
    }
    for entity in payloads.map_entities.iter().flatten() {
        validate_publishable_saved_entity(entity, entitlements)?;
    }
    for (chunk_pos, spawns) in &payloads.chunk_entities {
        for spawn in spawns {
            validate_publishable_world_object_spawn(*chunk_pos, spawn, entitlements)?;
        }
    }
    Ok(())
}

fn validate_publishable_world_object_spawn(
    chunk_pos: IVec3,
    spawn: &WorldObjectSpawn,
    entitlements: &PlayerEntitlements,
) -> Result<(), MapPersistenceRejection> {
    if spawn.persisted_components.contains_progression_or_character_state() {
        return Err(MapPersistenceRejection::Invalid(format!(
            "client-published chunk entity at {chunk_pos:?} contains progression-bearing data"
        )));
    }
    if !entitlements.allows_world_object(&spawn.kind) {
        return Err(MapPersistenceRejection::Invalid(format!("unentitled homebase object: {:?}", spawn.kind)));
    }
    Ok(())
}
```

#### 8. Client publication tests

**File**: `crates/client/tests/map_transition.rs` **Action**: modify

Replace the existing placeholder-only file with tests named `homebase_publication_*` where practical, or add a new test module in the same file. Cover:

- homebase transition initializes completeness tracking;
- speculative prediction alone does not mark a slot publishable;
- ack/replicated authoritative state can mark a slot publishable;
- publish queue waits for attestation.

### Verification

#### Automated

- [ ] `if pgrep -af 'cargo (build|check|test)|cargo-make|rustc' | grep -v pgrep; then echo busy >&2; exit 1; fi`
- [ ] `cargo test -p client homebase_publication`
- [ ] `cargo test -p server remote_restore`

#### Manual

- [ ] Run server and client, edit a homebase, wait for server ack/replication, request server attestation, publish from client, then restart/import and confirm import succeeds only with both valid player signature and valid server attestation.
- [ ] Try to publish an overworld or foreign-owner homebase manifest from a client identity and confirm it is rejected.

---

## Phase 6: Quarantine, Rollback, and Diagnostics

### Changes

#### 1. Quarantine and runtime remote config extension

**File**: `crates/server/src/persistence/mod.rs` **Action**: modify

Extend the minimal Phase 2 `RemoteMapPersistenceConfig` with quarantine fields, and add quarantine record/config filesystem helpers.

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QuarantinedMapSave {
    pub map_id: MapInstanceId,
    pub owner: Option<NostrPublicKey>,
    pub reason: MapPersistenceRejection,
    pub manifest_hash: Option<ManifestHash>,
}

// Extend the Phase 2 config:
#[derive(Resource, Clone, Debug)]
pub struct RemoteMapPersistenceConfig {
    pub enabled: bool,
    pub fallback_timeout: Duration,
    pub quarantine_dir: PathBuf,
}

pub fn quarantine_rejected_map_save(
    config: &RemoteMapPersistenceConfig,
    record: &QuarantinedMapSave,
    raw_manifest: Option<&[u8]>,
) -> Result<(), PersistenceError> {
    let map_dir = config.quarantine_dir.join(quarantine_map_dir_component(&record.map_id));
    fs::create_dir_all(&map_dir)
        .map_err(|e| PersistenceError::Serialize(format!("mkdir quarantine dir: {e}")))?;

    let record_name = record.manifest_hash
        .map(manifest_hash_hex)
        .unwrap_or_else(|| format!("local-invalid-{}", unix_timestamp_millis()));
    let record_path = map_dir.join(format!("{record_name}.quarantine.ron"));
    let record_text = ron::ser::to_string_pretty(record, ron::ser::PrettyConfig::default())
        .map_err(|e| PersistenceError::Serialize(format!("serialize quarantine record: {e}")))?;
    write_quarantine_file_atomically(&record_path, record_text.as_bytes())?;

    if let Some(raw_manifest) = raw_manifest {
        let manifest_path = map_dir.join(format!("{record_name}.manifest.bin"));
        write_quarantine_file_atomically(&manifest_path, raw_manifest)?;
    } else {
        trace!(?record.map_id, ?record.manifest_hash, "quarantined rejected map save without raw manifest bytes");
    }
    Ok(())
}

fn write_quarantine_file_atomically(path: &Path, bytes: &[u8]) -> Result<(), PersistenceError> {
    let file_name = path.file_name()
        .and_then(|name| name.to_str())
        .expect("quarantine file path must have UTF-8 file name");
    let tmp_path = path.with_file_name(format!("{file_name}.{}.tmp", std::process::id()));
    fs::write(&tmp_path, bytes)
        .map_err(|e| PersistenceError::Serialize(format!("write quarantine tmp: {e}")))?;
    fs::rename(&tmp_path, path)
        .map_err(|e| PersistenceError::Serialize(format!("rename quarantine file: {e}")))?;
    Ok(())
}

fn quarantine_map_dir_component(map_id: &MapInstanceId) -> String {
    match map_id {
        MapInstanceId::Overworld => "overworld".to_string(),
        MapInstanceId::Homebase { owner } => format!("homebase_{}", nostr_client::npub_from_nostr_public_key(*owner)),
    }
}
```

Default config should preserve current filesystem-only behavior when remote persistence is disabled.

#### 2. Server diagnostics and startup recovery

**Files**: `crates/server/src/map/diagnostics.rs`, `crates/server/src/map/mod.rs` **Action**: create/modify

Add startup/update systems that:

- remove leftover incomplete materialization staging directories and stale pointer tmp files;
- reset publish journal `InFlight` entries to `Pending`;
- validate active revision pointers and accepted-head references point to complete materialized revisions;
- quarantine/fallback if the active pointer or accepted head points to missing/invalid materialized data;
- emit structured logs for map id, owner, selected backend, revision, manifest hash, remote head, local accepted head, query policy, and failure class.

Every expected wait/fallback path must log with `trace!`; invalid/divergent/incomplete paths should use `warn!` or `error!` with the rejection reason.

Startup recovery should be an explicit system with loud invalid-state handling:

```rust
pub fn recover_map_persistence_startup(
    config: Res<RemoteMapPersistenceConfig>,
    map_dirs: Res<MapSaveDirectories>,
    mut journals: Query<(&MapInstanceId, &mut RemotePublishJournal)>,
) {
    for map_dir in map_dirs.iter() {
        if let Err(error) = cleanup_materialization_staging(map_dir) {
            error!(?map_dir, ?error, "failed to clean map materialization staging directory");
        }
        match validate_active_revision_pointer(map_dir) {
            Ok(()) => trace!(?map_dir, "active map revision pointer validated"),
            Err(rejection) => {
                warn!(?map_dir, ?rejection, "active revision pointer invalid; quarantining remote materialization state");
                quarantine_rejected_map_save(&config, &QuarantinedMapSave::from_rejection(map_dir, rejection), None)
                    .expect("quarantine record should be writable during startup recovery");
            }
        }
    }

    for (map_id, mut journal) in &mut journals {
        for entry in &mut journal.entries {
            if entry.status == RemotePublishStatus::InFlight {
                trace!(?map_id, ?entry.new_manifest_hash, "resetting interrupted publish to pending");
                entry.status = RemotePublishStatus::Pending;
            }
        }
    }
}
```

#### 3. Nostr diagnostics classification

**File**: `crates/nostr_map_persistence/src/diagnostics.rs` **Action**: modify

Ensure remote errors classify distinctly as:

- unavailable/timeout;
- manifest verification failure;
- descriptor root mismatch;
- revision-chain divergence;
- incomplete payload set;
- Blossom URL policy rejection;
- blob hash/size mismatch.

These variants feed server quarantine/block decisions.

#### 4. README documentation

**File**: `README.md` **Action**: modify

Add a concise `Nostr/Blossom Map Persistence` subsection under Development or Nostr configuration covering:

- v1 scope: map/layout persistence for Overworld and Homebase only;
- latestness limitation: latest visible valid descendant under configured query policy and local accepted head, not global latest;
- remote disabled mode and filesystem fallback behavior;
- quarantine directory and what invalid/divergent means;
- active revision pointer, accepted head file, staged revision directories, and safe rollback/manual recovery path;
- no progression-bearing client-published state in v1.

#### 5. Update task structure notes after implementation

**File**: `docs/tasks/2026-05-21-nostr-map-persistence/structure.md` **Action**: modify

After Phase 6 implementation, add a short implementation note or checklist result that records the final remote-disable, quarantine, rollback, and diagnostics behavior. Do not change the phase order or design decisions.

### Verification

#### Automated

- [ ] `if pgrep -af 'cargo (build|check|test)|cargo-make|rustc' | grep -v pgrep; then echo busy >&2; exit 1; fi`
- [ ] `cargo check-all`
- [ ] `cargo test-all`

#### Manual

- [ ] Existing filesystem-only worlds load unchanged with remote persistence disabled.
- [ ] Remote can be disabled without migration or save-directory changes.
- [ ] Divergent remote data is quarantined without overwriting valid filesystem state.
- [ ] Logs distinguish relay/Blossom unavailable, invalid data, incomplete data, and divergent chain.
- [ ] README recovery steps are sufficient to locate quarantine records, inspect the active revision pointer and accepted head, disable remote, and roll back to filesystem state.
