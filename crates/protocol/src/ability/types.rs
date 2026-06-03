use super::input::AbilityInput;
use avian3d::prelude::Rotation;
use bevy::ecs::entity::{EntityMapper, MapEntities};
use bevy::prelude::*;
use bevy::reflect::PartialReflect;
use lightyear::prelude::Tick;
use lightyear::utils::collections::EntityHashSet;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

/// Returns the normalized facing direction from a rotation.
pub fn facing_direction(rotation: &Rotation) -> Vec3 {
    (rotation.0 * Vec3::NEG_Z).normalize()
}

/// String-based ability identifier.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Reflect)]
#[type_path = "protocol::ability"]
pub struct AbilityId(pub String);

/// Specifies who receives an effect.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Reflect, Default)]
#[type_path = "protocol::ability"]
pub enum EffectTarget {
    #[default]
    Caster,
    Victim,
    OriginalCaster,
}

/// Coordinate frame used to interpret a force vector in [`AbilityEffect::ApplyForce`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, Reflect)]
#[type_path = "protocol::ability"]
pub enum ForceFrame {
    #[default]
    World,
    Caster,
    Victim,
    RelativePosition,
    RelativeRotation,
}

/// What an ability does when it activates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Reflect)]
#[type_path = "protocol::ability"]
pub enum AbilityEffect {
    Melee {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        target: EffectTarget,
    },
    Projectile {
        #[serde(default)]
        id: Option<String>,
        speed: f32,
        lifetime_ticks: u16,
    },
    SetVelocity {
        speed: f32,
        target: EffectTarget,
    },
    Damage {
        amount: f32,
        target: EffectTarget,
    },
    ApplyForce {
        force: Vec3,
        #[serde(default)]
        frame: ForceFrame,
        target: EffectTarget,
    },
    AreaOfEffect {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        target: EffectTarget,
        radius: f32,
        #[serde(default)]
        duration_ticks: Option<u16>,
    },
    Ability {
        id: String,
        target: EffectTarget,
    },
    Teleport {
        distance: f32,
    },
    Shield {
        absorb: f32,
    },
    Buff {
        stat: String,
        multiplier: f32,
        duration_ticks: u16,
        target: EffectTarget,
    },
}

/// Controls when an effect fires during an ability's lifecycle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Reflect)]
#[type_path = "protocol::ability"]
pub enum EffectTrigger {
    OnTick {
        #[serde(default)]
        tick: u16,
        effect: AbilityEffect,
    },
    WhileActive(AbilityEffect),
    OnHit(AbilityEffect),
    OnEnd(AbilityEffect),
    OnInput {
        input: AbilityInput,
        effect: AbilityEffect,
    },
}

/// Definition of a single ability, loaded from an individual `.ability.ron` file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Reflect, Asset)]
#[type_path = "protocol::ability"]
pub struct AbilityDef {
    pub startup_ticks: u16,
    pub active_ticks: u16,
    pub recovery_ticks: u16,
    pub cooldown_ticks: u16,
    pub effects: Vec<EffectTrigger>,
}

impl AbilityDef {
    pub fn phase_duration(&self, phase: &AbilityPhase) -> u16 {
        match phase {
            AbilityPhase::Startup => self.startup_ticks,
            AbilityPhase::Active => self.active_ticks,
            AbilityPhase::Recovery => self.recovery_ticks,
        }
    }
}

/// Tick-based phase durations and cooldown. Loaded from RON archetype.
#[derive(Component, Clone, Debug, PartialEq, Reflect, Serialize, Deserialize, Default)]
#[type_path = "protocol::ability"]
#[reflect(Component, Serialize, Deserialize)]
pub struct AbilityPhases {
    pub startup: u16,
    pub active: u16,
    pub recovery: u16,
    pub cooldown: u16,
}

impl AbilityPhases {
    pub fn phase_duration(&self, phase: &AbilityPhase) -> u16 {
        match phase {
            AbilityPhase::Startup => self.startup,
            AbilityPhase::Active => self.active,
            AbilityPhase::Recovery => self.recovery,
        }
    }
}

/// Manifest listing ability IDs, used by WASM builds where `load_folder` is unavailable.
#[derive(Clone, Debug, Serialize, Deserialize, Asset, TypePath)]
#[serde(transparent)]
pub struct AbilityManifest(pub Vec<String>);

/// Resource holding loaded ability asset handles, keyed by `AbilityId`.
#[derive(Resource, Clone, Debug, Default)]
pub struct AbilityDefs {
    pub abilities: HashMap<AbilityId, Handle<AbilityAsset>>,
}

impl AbilityDefs {
    pub fn get(&self, id: &AbilityId) -> Option<&Handle<AbilityAsset>> {
        self.abilities.get(id)
    }
}

/// Per-character ability loadout (up to 5 slots; slot 4 reserved for Jump).
#[derive(Component, Clone, Debug, PartialEq, Serialize, Deserialize, Asset, TypePath)]
#[type_path = "protocol::ability"]
#[derive(Default)]
pub struct AbilitySlots(pub [Option<AbilityId>; 5]);

/// Which phase of an ability is currently executing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Reflect)]
#[type_path = "protocol::ability"]
pub enum AbilityPhase {
    Startup,
    Active,
    Recovery,
}

/// Tracks an executing ability as a standalone predicted entity.
#[derive(Component, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActiveAbility {
    pub def_id: AbilityId,
    pub caster: Entity,
    pub original_caster: Entity,
    pub target: Entity,
    pub phase: AbilityPhase,
    pub phase_start_tick: Tick,
    pub ability_slot: u8,
    pub depth: u8,
}

impl MapEntities for ActiveAbility {
    fn map_entities<M: EntityMapper>(&mut self, entity_mapper: &mut M) {
        self.caster = entity_mapper.get_mapped(self.caster);
        self.original_caster = entity_mapper.get_mapped(self.original_caster);
        self.target = entity_mapper.get_mapped(self.target);
    }
}

/// Per-slot cooldown tracking.
#[derive(Component, Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct AbilityCooldowns {
    pub last_used: [Option<Tick>; 5],
}

impl AbilityCooldowns {
    pub fn is_on_cooldown(&self, slot: usize, current_tick: Tick, cooldown_ticks: u16) -> bool {
        self.last_used[slot]
            .map(|last| (current_tick - last).unsigned_abs() <= cooldown_ticks)
            .unwrap_or(false)
    }
}

/// One-shot: inserted by apply_on_tick_effects when processing Projectile.
#[derive(Component, Clone, Debug, PartialEq)]
pub struct ProjectileSpawnEffect {
    pub speed: f32,
    pub lifetime_ticks: u16,
}

/// Relationship: hitbox entity belongs to an ActiveAbility entity.
#[derive(Component, Debug)]
#[relationship(relationship_target = ActiveAbilityHitboxes)]
pub struct HitboxOf(#[entities] pub Entity);

/// Relationship target: ActiveAbility's spawned hitbox entities.
#[derive(Component, Debug, Default)]
#[relationship_target(relationship = HitboxOf, linked_spawn)]
pub struct ActiveAbilityHitboxes(Vec<Entity>);

/// Marker on hitbox entities that need to track caster position each tick.
#[derive(Component, Clone, Debug)]
pub struct MeleeHitbox;

/// Tracks spawn tick and duration for AoE hitbox lifetime management.
#[derive(Component, Clone, Debug)]
pub struct AoEHitbox {
    pub spawn_tick: Tick,
    pub duration_ticks: u16,
}

/// Tracks entities already hit by this hitbox to prevent duplicate effects.
#[derive(Component, Clone, Debug, Default)]
pub struct HitTargets(pub EntityHashSet);

/// Carried on ActiveAbility entities (for melee) and bullet entities (for projectiles).
#[derive(Component, Clone, Debug)]
pub struct OnHitEffects {
    pub effects: Vec<AbilityEffect>,
    pub caster: Entity,
    pub original_caster: Entity,
    pub depth: u8,
}

/// Active-phase tick effect with offset metadata.
#[derive(Clone, Debug, PartialEq, Reflect, Serialize, Deserialize)]
#[type_path = "protocol::ability"]
pub struct TickEffect {
    #[serde(default)]
    pub tick: u16,
    pub effect: AbilityEffect,
}

/// Archetype component: all tick-triggered effects with their offsets.
#[derive(Component, Clone, Debug, PartialEq, Reflect, Serialize, Deserialize, Default)]
#[type_path = "protocol::ability"]
#[reflect(Component, Serialize, Deserialize)]
pub struct OnTickEffects(pub Vec<TickEffect>);

/// Archetype component: effects that fire every tick during Active phase.
#[derive(Component, Clone, Debug, PartialEq, Reflect, Serialize, Deserialize, Default)]
#[type_path = "protocol::ability"]
#[reflect(Component, Serialize, Deserialize)]
pub struct WhileActiveEffects(pub Vec<AbilityEffect>);

/// Archetype component: effects that fire when Active → Recovery.
#[derive(Component, Clone, Debug, PartialEq, Reflect, Serialize, Deserialize, Default)]
#[type_path = "protocol::ability"]
#[reflect(Component, Serialize, Deserialize)]
pub struct OnEndEffects(pub Vec<AbilityEffect>);

/// Input-triggered effect with semantic ability input metadata.
#[derive(Clone, Debug, PartialEq, Reflect, Serialize, Deserialize)]
#[type_path = "protocol::ability"]
pub struct InputEffect {
    pub input: AbilityInput,
    pub effect: AbilityEffect,
}

/// Archetype component: input-triggered effects during Active phase.
#[derive(Component, Clone, Debug, PartialEq, Reflect, Serialize, Deserialize, Default)]
#[type_path = "protocol::ability"]
#[reflect(Component, Serialize, Deserialize)]
pub struct OnInputEffects(pub Vec<InputEffect>);

/// Archetype component: effects applied when a hitbox/projectile hits a target.
#[derive(Component, Clone, Debug, PartialEq, Reflect, Serialize, Deserialize, Default)]
#[type_path = "protocol::ability"]
#[reflect(Component, Serialize, Deserialize)]
pub struct OnHitEffectDefs(pub Vec<AbilityEffect>);

/// Caster-state condition evaluated at ability activation.
#[derive(Clone, Debug, PartialEq, Reflect, Serialize, Deserialize)]
#[type_path = "protocol::ability"]
pub enum Condition {
    Grounded,
    Airborne,
}

/// One conditional branch: if `condition` matches the caster at activation,
/// `effect` is appended to the spawned ability's `OnTickEffects` at tick 0.
#[derive(Clone, Debug, PartialEq, Reflect, Serialize, Deserialize)]
#[type_path = "protocol::ability"]
pub struct ConditionalEffect {
    pub condition: Condition,
    pub effect: AbilityEffect,
}

/// If present and no entry's condition matches at activation, the ability
/// is refused (no spawn, no cooldown consumption). All matching entries fire.
#[derive(Component, Clone, Debug, PartialEq, Reflect, Serialize, Deserialize, Default)]
#[type_path = "protocol::ability"]
#[reflect(Component, Serialize, Deserialize)]
pub struct ConditionalEffects(pub Vec<ConditionalEffect>);

/// A bundle of reflected components loaded from a `.ability.ron` file.
#[derive(Asset, TypePath)]
pub struct AbilityAsset {
    pub components: Vec<Box<dyn PartialReflect>>,
}

impl Clone for AbilityAsset {
    fn clone(&self) -> Self {
        Self {
            components: self
                .components
                .iter()
                .map(|c| {
                    c.reflect_clone()
                        .expect("ability component must be cloneable")
                        .into_partial_reflect()
                })
                .collect(),
        }
    }
}

impl fmt::Debug for AbilityAsset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AbilityAsset")
            .field(
                "components",
                &self
                    .components
                    .iter()
                    .map(|c| c.reflect_type_path())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Damage absorption shield on a character. Intercepts damage before Health.
#[derive(Component, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActiveShield {
    pub remaining: f32,
}

/// Temporary stat modifiers on a character. Tick-based expiry.
#[derive(Component, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActiveBuffs(pub Vec<ActiveBuff>);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActiveBuff {
    pub stat: String,
    pub multiplier: f32,
    pub expires_tick: Tick,
}

/// Marker on a ProjectileSpawn entity -- stores spawn parameters.
#[derive(Component, Clone, Debug, PartialEq, Serialize, Deserialize, Reflect)]
#[type_path = "protocol::ability"]
pub struct AbilityProjectileSpawn {
    pub spawn_tick: Tick,
    pub position: Vec3,
    pub direction: Vec3,
    pub speed: f32,
    pub lifetime_ticks: u16,
    pub ability_id: AbilityId,
    pub shooter: Entity,
}

/// Relationship: projectile belongs to a character.
#[derive(Component, Debug)]
#[relationship(relationship_target = AbilityBullets)]
pub struct AbilityBulletOf(#[entities] pub Entity);

/// Relationship target: character's active projectiles.
#[derive(Component, Debug, Default)]
#[relationship_target(relationship = AbilityBulletOf, linked_spawn)]
pub struct AbilityBullets(Vec<Entity>);
