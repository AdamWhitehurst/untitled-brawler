mod activation;
mod effects;
mod input;
mod lifecycle;
mod spawn;
mod types;

pub mod loader;
pub mod loading;
pub mod plugin;

pub(crate) use spawn::spawn_sub_ability;

pub use activation::{ability_action_to_slot, ability_activation, slot_to_ability_action};
pub use input::AbilityInput;
pub use lifecycle::expire_buffs;
pub use loading::DefaultAbilitySlots;
pub use plugin::AbilityPlugin;
pub use types::{
    facing_direction, AbilityAsset, AbilityBulletOf, AbilityBullets, AbilityCooldowns, AbilityDef,
    AbilityDefs, AbilityEffect, AbilityId, AbilityManifest, AbilityPhase, AbilityPhases,
    AbilityProjectileSpawn, AbilitySlots, ActiveAbility, ActiveAbilityHitboxes, ActiveBuff,
    ActiveBuffs, ActiveShield, AoEHitbox, Condition, ConditionalEffect, ConditionalEffects,
    EffectTarget, EffectTrigger, ForceFrame, HitTargets, HitboxOf, InputEffect, MeleeHitbox,
    OnEndEffects, OnHitEffectDefs, OnHitEffects, OnInputEffects, OnTickEffects,
    ProjectileSpawnEffect, TickEffect, WhileActiveEffects,
};
