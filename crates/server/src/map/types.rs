use bevy::prelude::*;
use nostr_map_persistence::MapPersistenceRejection;
use protocol::{MapInstanceId, NostrPublicKey, PlayerIdentity};

use crate::persistence::{MapMeta, ServerValidatedMapSave};

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
#[derive(Clone, Debug)]
pub struct MapTransitionParams {
    pub seed: u64,
    pub generation_version: u32,
    pub bounds: Option<IVec3>,
    pub chunk_size: u32,
    pub column_y_range: (i32, i32),
}

/// Indicates whether a map is ready for transition commit, still loading, or blocked.
#[derive(Clone, Debug)]
pub enum MapPreparation {
    Ready {
        entity: Entity,
        params: MapTransitionParams,
    },
    Pending,
    Blocked(MapPersistenceRejection),
}

/// Marker that prevents duplicate switch requests while map persistence preflight is pending.
#[derive(Component, Clone, Debug)]
pub struct PendingMapSwitchPreflight {
    pub target_map_id: MapInstanceId,
    pub requested_at: f64,
}

/// Authenticated login state waiting for overworld persistence preflight before character spawn.
#[derive(Component, Clone, Debug)]
pub struct PendingInitialSpawn {
    pub remote_id: lightyear::prelude::RemoteId,
    pub identity: PlayerIdentity,
    pub requested_at: f64,
}

/// Active map preflight task entity state.
#[derive(Component, Clone, Debug)]
pub struct ActiveMapPreflight {
    pub request: PendingMapPreflight,
    pub stage: MapPreflightStage,
}

/// Phase of the map preflight state machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MapPreflightStage {
    LoadFilesystemMeta,
    WaitingFilesystemMeta,
    WaitingRemoteDecision,
    CommitTransition,
}

/// Queue of requested map persistence preflights.
#[derive(Resource, Default)]
pub struct PendingMapPreflights(pub std::collections::VecDeque<PendingMapPreflight>);

/// Deterministically derive a homebase terrain seed from the owner's Nostr public key.
pub fn seed_from_nostr_public_key(owner: NostrPublicKey) -> u64 {
    u64::from_le_bytes(
        owner.0[0..8]
            .try_into()
            .expect("NostrPublicKey has 32 bytes"),
    )
}
