use avian3d::math::{AsF32, Vector};
use avian3d::prelude::*;
use bevy::prelude::*;
use leafwing_input_manager::prelude::*;
use lightyear::input::config::InputConfig;
use lightyear::prelude::input::leafwing::InputPlugin;
use lightyear::prelude::*;
use serde::{Deserialize, Serialize};

pub mod ability;
pub mod app_state;
pub mod auth;
pub mod billboard;
pub mod character;
pub mod diagnostics;
pub mod hit_detection;
pub mod map;
pub mod physics;
pub mod reflect_loader;
pub mod terrain;
pub mod transition;
pub mod vox_model;
pub mod world_object;

pub use ability::{
    ability_action_to_slot, AbilityAsset, AbilityBulletOf, AbilityBullets, AbilityCooldowns,
    AbilityDef, AbilityDefs, AbilityEffect, AbilityId, AbilityManifest, AbilityPhase,
    AbilityPhases, AbilityPlugin, AbilityProjectileSpawn, AbilitySlots, ActiveAbility, ActiveBuff,
    ActiveBuffs, ActiveShield, DefaultAbilitySlots, EffectTarget, EffectTrigger, ForceFrame,
    InputEffect, OnEndEffects, OnHitEffectDefs, OnHitEffects, OnInputEffects, OnTickEffects,
    ProjectileSpawnEffect, TickEffect, WhileActiveEffects,
};
pub use app_state::{
    AppState, AppStatePlugin, IdentityLoadComplete, RelayPoolReady, TrackedAssets,
};
pub use auth::{AuthChannel, IdentityChallenge, IdentityProof, NostrPublicKey, PlayerIdentity};
pub use character::{apply_movement, detect_grounded, update_facing};
pub use character::{
    CharacterMarker, CharacterPhysicsBundle, CharacterType, ColorComponent, DeathEvent,
    DummyTarget, Health, Invulnerable, IsGrounded, PlayerId, RespawnPoint, RespawnTimer,
    RespawnTimerConfig, CHARACTER_CAPSULE_HEIGHT, CHARACTER_CAPSULE_RADIUS, DEFAULT_RESPAWN_TICKS,
};
pub use hit_detection::{
    character_collision_layers, damageable_collision_layers, hitbox_collision_layers,
    projectile_collision_layers, terrain_collision_layers, GameLayer,
};
pub use map::{
    attach_chunk_colliders, ChunkChannel, ChunkDataSync, HomebasePayloadScope,
    HomebasePublicationAttestation, MapChannel, MapInstanceId, MapRegistry, MapSaveTarget,
    MapSwitchTarget, MapTransitionEnd, MapTransitionReady, MapTransitionStart, Owner,
    PendingTransition, PlayerMapSwitchRequest, SavedEntity, SavedEntityKind, SectionBlocksUpdate,
    TransitionReadySent, UnloadColumn, VoxelBrushEditRequest, VoxelChange, VoxelChannel,
    VoxelChunk, VoxelConcreteEditRequest, VoxelEditAck, VoxelEditBroadcast, VoxelEditReject,
    VoxelEditRequest, VoxelType,
};
pub use terrain::{TerrainDefRegistry, TerrainPlugin};
pub use transition::{MapTransitionEntity, TransitionPlugin};
pub use vox_model::{VoxModelAsset, VoxModelPlugin, VoxModelRegistry};
pub use world_object::{
    WorldObjectDefRegistry, WorldObjectDeleteAck, WorldObjectDeleteRequest, WorldObjectEditChannel,
    WorldObjectEditReject, WorldObjectEditRejectReason, WorldObjectId, WorldObjectMoveAck,
    WorldObjectMoveRequest, WorldObjectPlacementAck, WorldObjectPlacementChannel,
    WorldObjectPlacementReject, WorldObjectPlacementRejectReason, WorldObjectPlacementRequest,
    WorldObjectPlugin, WorldObjectRotateAck, WorldObjectRotateRequest, WorldObjectRotationSnapshot,
};

pub const PROTOCOL_ID: u64 = 0;
pub const PRIVATE_KEY: [u8; 32] = [0; 32];
pub const FIXED_TIMESTEP_HZ: f64 = 64.0;

/// Lightyear/Leafwing network input vocabulary for predicted player simulation.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Hash, Reflect)]
pub enum NetworkedPlayerActions {
    Move,
    CameraYaw,
    Jump,
    PlaceVoxel,
    RemoveVoxel,
    Ability1,
    Ability2,
    Ability3,
    Ability4,
}

impl Actionlike for NetworkedPlayerActions {
    fn input_control_kind(&self) -> InputControlKind {
        match self {
            Self::Move => InputControlKind::DualAxis,
            Self::CameraYaw => InputControlKind::Axis,
            _ => InputControlKind::Button,
        }
    }
}

#[cfg(feature = "test_utils")]
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Reflect, Event)]
pub struct TestTrigger {
    pub data: String,
}

pub struct ProtocolPlugin;

impl Plugin for ProtocolPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(InputPlugin::<NetworkedPlayerActions> {
            config: InputConfig::<NetworkedPlayerActions> {
                rebroadcast_inputs: true,
                // At 5, srv_tick_past_buffer_end occasionally hits +1,
                // causing axis values to persist (stuck movement) and discrete
                // transitions (JustPressed) to be missed.
                packet_redundancy: 20,
                ..default()
            },
        });

        // Voxel channel
        app.add_channel::<VoxelChannel>(ChannelSettings {
            mode: ChannelMode::OrderedReliable(ReliableSettings::default()),
            ..default()
        })
        .add_direction(NetworkDirection::Bidirectional);

        // Voxel messages
        app.register_message::<VoxelEditRequest>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<VoxelBrushEditRequest>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<VoxelConcreteEditRequest>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<VoxelEditBroadcast>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<VoxelEditAck>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<VoxelEditReject>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<SectionBlocksUpdate>()
            .add_direction(NetworkDirection::ServerToClient);

        // World object placement channel
        app.add_channel::<WorldObjectPlacementChannel>(ChannelSettings {
            mode: ChannelMode::OrderedReliable(ReliableSettings::default()),
            ..default()
        })
        .add_direction(NetworkDirection::Bidirectional);

        // World object placement messages
        app.register_message::<WorldObjectPlacementRequest>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<WorldObjectPlacementAck>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<WorldObjectPlacementReject>()
            .add_direction(NetworkDirection::ServerToClient);

        // World object edit channel
        app.add_channel::<WorldObjectEditChannel>(ChannelSettings {
            mode: ChannelMode::OrderedReliable(ReliableSettings::default()),
            ..default()
        })
        .add_direction(NetworkDirection::Bidirectional);

        // World object edit/delete messages
        app.register_message::<WorldObjectDeleteRequest>()
            .add_direction(NetworkDirection::ClientToServer)
            .add_map_entities();
        app.register_message::<WorldObjectDeleteAck>()
            .add_direction(NetworkDirection::ServerToClient)
            .add_map_entities();
        app.register_message::<WorldObjectMoveRequest>()
            .add_direction(NetworkDirection::ClientToServer)
            .add_map_entities();
        app.register_message::<WorldObjectMoveAck>()
            .add_direction(NetworkDirection::ServerToClient)
            .add_map_entities();
        app.register_message::<WorldObjectRotateRequest>()
            .add_direction(NetworkDirection::ClientToServer)
            .add_map_entities();
        app.register_message::<WorldObjectRotateAck>()
            .add_direction(NetworkDirection::ServerToClient)
            .add_map_entities();
        app.register_message::<WorldObjectEditReject>()
            .add_direction(NetworkDirection::ServerToClient);

        // Chunk streaming channel
        app.add_channel::<ChunkChannel>(ChannelSettings {
            mode: ChannelMode::UnorderedReliable(ReliableSettings::default()),
            ..default()
        })
        .add_direction(NetworkDirection::ServerToClient);

        // Chunk streaming messages
        app.register_message::<ChunkDataSync>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<UnloadColumn>()
            .add_direction(NetworkDirection::ServerToClient);

        // Map transition channel
        app.add_channel::<MapChannel>(ChannelSettings {
            mode: ChannelMode::OrderedReliable(ReliableSettings::default()),
            ..default()
        })
        .add_direction(NetworkDirection::Bidirectional);

        // Map transition messages
        app.register_message::<PlayerMapSwitchRequest>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<MapTransitionStart>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<MapTransitionReady>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<MapTransitionEnd>()
            .add_direction(NetworkDirection::ServerToClient);

        // Nostr post-connect authentication channel
        app.add_channel::<AuthChannel>(ChannelSettings {
            mode: ChannelMode::OrderedReliable(ReliableSettings::default()),
            ..default()
        })
        .add_direction(NetworkDirection::Bidirectional);

        app.register_message::<IdentityChallenge>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<IdentityProof>()
            .add_direction(NetworkDirection::ClientToServer);

        app.register_component::<PlayerIdentity>();
        app.register_type::<NostrPublicKey>();

        #[cfg(feature = "test_utils")]
        app.register_event::<TestTrigger>()
            .add_direction(NetworkDirection::Bidirectional);

        // Map instance identity
        app.register_component::<MapInstanceId>().add_prediction();
        app.register_component::<Owner>();

        // World objects — static, no prediction needed
        app.register_component::<world_object::WorldObjectId>();
        app.register_component::<world_object::VisualKind>();
        app.register_component::<world_object::ActiveTransformation>();

        // Marker components
        app.register_component::<PlayerId>();
        app.register_component::<ColorComponent>().add_prediction();
        app.register_component::<Name>();
        app.register_component::<CharacterMarker>().add_prediction();
        app.register_component::<DummyTarget>().add_prediction();
        app.register_component::<CharacterType>().add_prediction();
        app.register_component::<Health>().add_prediction();
        app.register_component::<Invulnerable>().add_prediction();
        app.register_component::<RespawnTimerConfig>();
        app.register_component::<RespawnTimer>().add_prediction();

        // Velocity prediction without visual correction
        app.register_component::<LinearVelocity>()
            .add_prediction()
            .add_should_rollback(linear_velocity_should_rollback);

        app.register_component::<AngularVelocity>()
            .add_prediction()
            .add_should_rollback(angular_velocity_should_rollback);

        // Ability components
        app.register_component::<AbilitySlots>();
        app.register_component::<ActiveAbility>()
            .add_prediction()
            .add_map_entities();
        app.register_component::<AbilityCooldowns>()
            .add_prediction();
        app.register_component::<ActiveShield>().add_prediction();
        app.register_component::<ActiveBuffs>().add_prediction();
        app.register_component::<AbilityProjectileSpawn>();

        // Position/Rotation with prediction + visual correction + interpolation
        app.register_component::<Position>()
            .add_prediction()
            .add_should_rollback(position_should_rollback)
            .add_linear_correction_fn()
            .add_linear_interpolation();

        app.register_component::<Rotation>()
            .add_prediction()
            .add_should_rollback(rotation_should_rollback)
            .add_linear_correction_fn()
            .add_linear_interpolation();
    }
}

fn position_should_rollback(this: &Position, that: &Position) -> bool {
    (this.0 - that.0).length() >= 0.01
}

fn rotation_should_rollback(this: &Rotation, that: &Rotation) -> bool {
    this.angle_between(*that) >= 0.01
}

fn linear_velocity_should_rollback(this: &LinearVelocity, that: &LinearVelocity) -> bool {
    (this.0 - that.0).length() >= 0.01
}

fn angular_velocity_should_rollback(this: &AngularVelocity, that: &AngularVelocity) -> bool {
    (this.0 - that.0).length() >= 0.01
}

pub struct SharedGameplayPlugin;

impl Plugin for SharedGameplayPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(AppStatePlugin);
        app.add_plugins(ProtocolPlugin);
        app.add_plugins(AbilityPlugin);
        app.add_plugins(terrain::TerrainPlugin);
        app.add_plugins(world_object::WorldObjectPlugin);
        app.add_plugins(vox_model::VoxModelPlugin);
        app.add_plugins(TransitionPlugin);

        app.add_plugins(lightyear::avian3d::plugin::LightyearAvianPlugin {
            replication_mode: lightyear::avian3d::plugin::AvianReplicationMode::Position,
            ..default()
        });

        app.add_plugins(
            PhysicsPlugins::default()
                .with_collision_hooks::<physics::MapCollisionHooks>()
                .build()
                .disable::<PhysicsTransformPlugin>()
                .disable::<PhysicsInterpolationPlugin>()
                .disable::<IslandSleepingPlugin>(),
        );
        app.insert_resource(Gravity(Vector::Y * -9.82 * 6.0));

        let ready = in_state(AppState::Ready);

        app.add_systems(FixedUpdate, update_facing.run_if(ready));
    }
}

/// Computes a [`Transform`] from avian [`Position`]/[`Rotation`].
///
/// [`PhysicsTransformPlugin`] is disabled ([`SharedGameplayPlugin`] above) —
/// lightyear handles position↔transform sync — so `Position` does not
/// auto-require `Transform`. Call this when attaching children that need a
/// parent with `GlobalTransform` before lightyear's `add_transform` runs in
/// `PostUpdate`.
pub fn transform_from_physics(
    pos: Option<&Position>,
    rot: Option<&avian3d::prelude::Rotation>,
) -> Transform {
    let mut t = Transform::default();
    if let Some(pos) = pos {
        t.translation = pos.f32();
    }
    if let Some(rot) = rot {
        t.rotation = rot.f32();
    }
    t
}

#[cfg(feature = "test_utils")]
pub mod test_utils;
