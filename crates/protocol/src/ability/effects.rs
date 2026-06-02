use super::spawn::{spawn_aoe_hitbox, spawn_melee_hitbox, spawn_sub_ability};
use super::types::{
    AbilityAsset, AbilityDefs, AbilityEffect, AbilityPhase, ActiveAbility, ActiveShield,
    EffectTarget, ForceFrame, OnEndEffects, OnHitEffects, OnInputEffects, OnTickEffects,
    ProjectileSpawnEffect, WhileActiveEffects,
};
use crate::map::MapInstanceId;
use crate::{NetworkedPlayerActions, PlayerId};
use avian3d::prelude::*;
use bevy::prelude::*;
use leafwing_input_manager::prelude::ActionState;
use lightyear::prelude::{ControlledBy, LocalTimeline, Tick};

fn resolve_caster_target(target: &EffectTarget, active: &ActiveAbility) -> Entity {
    match target {
        EffectTarget::Caster => active.caster,
        EffectTarget::OriginalCaster => active.original_caster,
        other => {
            warn!(
                "EffectTarget::{:?} not valid in caster context, falling back to caster",
                other
            );
            active.caster
        }
    }
}

pub fn apply_on_tick_effects(
    mut commands: Commands,
    ability_defs: Res<AbilityDefs>,
    ability_assets: Res<Assets<AbilityAsset>>,
    registry: Res<AppTypeRegistry>,
    timeline: Res<LocalTimeline>,
    server_query: Query<&ControlledBy>,
    player_id_query: Query<&PlayerId>,
    query: Query<(
        Entity,
        &OnTickEffects,
        &ActiveAbility,
        Option<&OnHitEffects>,
    )>,
    mut caster_set: ParamSet<(
        Query<(&mut Position, &Rotation, &MapInstanceId)>,
        Query<Forces>,
    )>,
) {
    let tick = timeline.tick();
    for (entity, effects, active, on_hit_effects) in &query {
        if active.phase != AbilityPhase::Active {
            continue;
        }

        let active_offset = (tick - active.phase_start_tick) as u16;
        for tick_effect in &effects.0 {
            if tick_effect.tick != active_offset {
                continue;
            }
            match &tick_effect.effect {
                AbilityEffect::Melee { .. } => {
                    let caster_query = caster_set.p0();
                    spawn_melee_hitbox(
                        &mut commands,
                        entity,
                        active,
                        on_hit_effects,
                        &caster_query,
                    );
                }
                AbilityEffect::AreaOfEffect {
                    radius,
                    duration_ticks,
                    ..
                } => {
                    let caster_query = caster_set.p0();
                    spawn_aoe_hitbox(
                        &mut commands,
                        entity,
                        active,
                        on_hit_effects,
                        &caster_query,
                        *radius,
                        tick,
                        duration_ticks.unwrap_or(1),
                    );
                }
                AbilityEffect::Projectile {
                    speed,
                    lifetime_ticks,
                    ..
                } => {
                    commands.entity(entity).insert(ProjectileSpawnEffect {
                        speed: *speed,
                        lifetime_ticks: *lifetime_ticks,
                    });
                }
                AbilityEffect::Ability { id, target } => {
                    let target_entity = resolve_caster_target(target, active);
                    spawn_sub_ability(
                        &mut commands,
                        ability_defs.as_ref(),
                        ability_assets.as_ref(),
                        &registry.0,
                        id,
                        target_entity,
                        active.original_caster,
                        active.ability_slot,
                        active.depth,
                        tick,
                        &server_query,
                        &player_id_query,
                    );
                }
                AbilityEffect::Teleport { distance } => {
                    apply_teleport(&mut caster_set.p0(), active.caster, *distance);
                }
                AbilityEffect::Shield { absorb } => {
                    commands
                        .entity(active.caster)
                        .insert(ActiveShield { remaining: *absorb });
                }
                AbilityEffect::Buff {
                    stat,
                    multiplier,
                    duration_ticks,
                    target,
                } => {
                    apply_buff(
                        &mut commands,
                        resolve_caster_target(target, active),
                        stat,
                        *multiplier,
                        *duration_ticks,
                        tick,
                    );
                }
                AbilityEffect::ApplyForce {
                    force,
                    frame,
                    target,
                } => {
                    let target_entity = resolve_caster_target(target, active);
                    let rotation = caster_set
                        .p0()
                        .get(target_entity)
                        .map(|(_, r, _)| r.0)
                        .unwrap_or_default();
                    apply_caster_force(
                        &mut caster_set.p1(),
                        target_entity,
                        rotation,
                        *force,
                        frame,
                    );
                }
                _ => {
                    warn!("Unhandled OnTick effect: {:?}", tick_effect.effect);
                }
            }
        }
    }
}

/// Applies an impulse to a caster-target entity. `World` and rotation-derived
/// frames are honored; position-relative frames have no meaning when caster ==
/// target and fall back to the raw vector with a warning.
fn apply_caster_force(
    forces_query: &mut Query<Forces>,
    target_entity: Entity,
    rotation: Quat,
    force: Vec3,
    frame: &ForceFrame,
) {
    let world_force = match frame {
        ForceFrame::World => force,
        ForceFrame::Caster | ForceFrame::RelativeRotation => rotation * force,
        ForceFrame::Victim | ForceFrame::RelativePosition => {
            warn!(
                "ApplyForce frame {:?} not meaningful for caster target",
                frame
            );
            force
        }
    };
    let Ok(mut forces) = forces_query.get_mut(target_entity) else {
        warn!("ApplyForce target {:?} not a rigid body", target_entity);
        return;
    };
    forces.apply_linear_impulse(world_force);
}

pub fn apply_while_active_effects(
    query: Query<(&WhileActiveEffects, &ActiveAbility)>,
    mut caster_query: Query<(&Rotation, &mut LinearVelocity)>,
) {
    for (effects, active) in &query {
        if active.phase != AbilityPhase::Active {
            continue;
        }
        for effect in &effects.0 {
            match effect {
                AbilityEffect::SetVelocity { speed, target } => {
                    let target_entity = resolve_caster_target(target, active);
                    if let Ok((rotation, mut velocity)) = caster_query.get_mut(target_entity) {
                        let direction = super::types::facing_direction(rotation);
                        velocity.x = direction.x * speed;
                        velocity.z = direction.z * speed;
                    }
                }
                _ => {
                    warn!("Unhandled WhileActive effect: {:?}", effect);
                }
            }
        }
    }
}

pub fn apply_on_end_effects(
    mut commands: Commands,
    ability_defs: Res<AbilityDefs>,
    ability_assets: Res<Assets<AbilityAsset>>,
    registry: Res<AppTypeRegistry>,
    timeline: Res<LocalTimeline>,
    server_query: Query<&ControlledBy>,
    player_id_query: Query<&PlayerId>,
    query: Query<(Entity, &OnEndEffects, &ActiveAbility)>,
    mut caster_query: Query<(&mut Position, &Rotation, &mut LinearVelocity)>,
) {
    let tick = timeline.tick();
    for (_entity, effects, active) in &query {
        if active.phase != AbilityPhase::Recovery || active.phase_start_tick != tick {
            continue;
        }
        for effect in &effects.0 {
            match effect {
                AbilityEffect::SetVelocity { speed, target } => {
                    let target_entity = resolve_caster_target(target, active);
                    if let Ok((_, rotation, mut velocity)) = caster_query.get_mut(target_entity) {
                        let direction = super::types::facing_direction(rotation);
                        velocity.x = direction.x * speed;
                        velocity.z = direction.z * speed;
                    }
                }
                AbilityEffect::Ability { id, target } => {
                    let target_entity = resolve_caster_target(target, active);
                    spawn_sub_ability(
                        &mut commands,
                        ability_defs.as_ref(),
                        ability_assets.as_ref(),
                        &registry.0,
                        id,
                        target_entity,
                        active.original_caster,
                        active.ability_slot,
                        active.depth,
                        tick,
                        &server_query,
                        &player_id_query,
                    );
                }
                AbilityEffect::Teleport { distance } => {
                    let target_entity = resolve_caster_target(&EffectTarget::Caster, active);
                    if let Ok((mut position, rotation, _)) = caster_query.get_mut(target_entity) {
                        let direction = super::types::facing_direction(rotation);
                        position.0 += direction * *distance;
                    } else {
                        warn!(
                            "Teleport: caster {:?} missing Position/Rotation",
                            active.caster
                        );
                    }
                }
                AbilityEffect::Shield { absorb } => {
                    commands
                        .entity(active.caster)
                        .insert(ActiveShield { remaining: *absorb });
                }
                AbilityEffect::Buff {
                    stat,
                    multiplier,
                    duration_ticks,
                    target,
                } => {
                    apply_buff(
                        &mut commands,
                        resolve_caster_target(target, active),
                        stat,
                        *multiplier,
                        *duration_ticks,
                        tick,
                    );
                }
                _ => {
                    warn!("Unhandled OnEnd effect: {:?}", effect);
                }
            }
        }
    }
}

pub fn apply_on_input_effects(
    mut commands: Commands,
    ability_defs: Res<AbilityDefs>,
    ability_assets: Res<Assets<AbilityAsset>>,
    registry: Res<AppTypeRegistry>,
    timeline: Res<LocalTimeline>,
    server_query: Query<&ControlledBy>,
    player_id_query: Query<&PlayerId>,
    query: Query<(Entity, &OnInputEffects, &ActiveAbility)>,
    action_query: Query<&ActionState<NetworkedPlayerActions>>,
) {
    let tick = timeline.tick();
    for (_entity, effects, active) in &query {
        if active.phase != AbilityPhase::Active {
            continue;
        }
        let Ok(action_state) = action_query.get(active.caster) else {
            continue;
        };
        for input_effect in &effects.0 {
            let Some(action) = input_effect.input.to_networked_action() else {
                trace!(
                    input = ?input_effect.input,
                    "OnInput skipped: semantic input has no network action"
                );
                continue;
            };
            if !action_state.just_pressed(&action) {
                continue;
            }
            match &input_effect.effect {
                AbilityEffect::Ability { id, target } => {
                    let target_entity = resolve_caster_target(target, active);
                    spawn_sub_ability(
                        &mut commands,
                        ability_defs.as_ref(),
                        ability_assets.as_ref(),
                        &registry.0,
                        id,
                        target_entity,
                        active.original_caster,
                        active.ability_slot,
                        active.depth,
                        tick,
                        &server_query,
                        &player_id_query,
                    );
                }
                _ => {
                    warn!("Unhandled OnInput effect: {:?}", input_effect.effect);
                }
            }
        }
    }
}

fn apply_teleport(
    caster_query: &mut Query<(&mut Position, &Rotation, &MapInstanceId)>,
    caster: Entity,
    distance: f32,
) {
    if let Ok((mut position, rotation, _)) = caster_query.get_mut(caster) {
        let direction = super::types::facing_direction(rotation);
        position.0 += direction * distance;
    } else {
        warn!("Teleport: caster {:?} missing Position/Rotation", caster);
    }
}

fn apply_buff(
    commands: &mut Commands,
    target_entity: Entity,
    stat: &str,
    multiplier: f32,
    duration_ticks: u16,
    tick: Tick,
) {
    use super::types::{ActiveBuff, ActiveBuffs};
    let expires_tick = tick + duration_ticks as i16;
    commands
        .entity(target_entity)
        .insert(ActiveBuffs(vec![ActiveBuff {
            stat: stat.to_string(),
            multiplier,
            expires_tick,
        }]));
}
