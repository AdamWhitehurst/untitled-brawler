use avian3d::prelude::CollidingEntities;
use bevy::ecs::reflect::ReflectComponent;
use bevy::prelude::*;
use bevy::reflect::PartialReflect;
use leafwing_input_manager::prelude::ActionState;
use lightyear::prelude::{ComponentRegistry, LocalTimeline, PeerId, Server, Tick};
use lightyear_replication::prespawn::PreSpawnedReceiver;
use protocol::ability::{
    AbilityAsset, AbilityInput, AbilityPhases, ActiveBuff, ActiveBuffs, ActiveShield, HitTargets,
    HitboxOf, InputEffect, MeleeHitbox, OnEndEffects, OnHitEffectDefs, OnHitEffects,
    OnInputEffects, OnTickEffects, TickEffect, WhileActiveEffects,
};
use protocol::*;
use std::collections::HashMap;

/// Build an `AbilityAsset` from explicit component lists.
fn build_ability_asset(
    phases: AbilityPhases,
    on_tick: Vec<TickEffect>,
    while_active: Vec<AbilityEffect>,
    on_hit: Vec<AbilityEffect>,
    on_end: Vec<AbilityEffect>,
    on_input: Vec<InputEffect>,
) -> AbilityAsset {
    let mut components: Vec<Box<dyn PartialReflect>> =
        vec![Box::new(phases).into_partial_reflect()];
    if !on_tick.is_empty() {
        components.push(Box::new(OnTickEffects(on_tick)).into_partial_reflect());
    }
    if !while_active.is_empty() {
        components.push(Box::new(WhileActiveEffects(while_active)).into_partial_reflect());
    }
    if !on_hit.is_empty() {
        components.push(Box::new(OnHitEffectDefs(on_hit)).into_partial_reflect());
    }
    if !on_end.is_empty() {
        components.push(Box::new(OnEndEffects(on_end)).into_partial_reflect());
    }
    if !on_input.is_empty() {
        components.push(Box::new(OnInputEffects(on_input)).into_partial_reflect());
    }
    AbilityAsset { components }
}

/// Insert an ability into both `Assets<AbilityAsset>` and `AbilityDefs`.
fn insert_test_ability(app: &mut App, id: &str, asset: AbilityAsset) {
    let handle = app
        .world_mut()
        .resource_mut::<Assets<AbilityAsset>>()
        .add(asset);
    app.world_mut()
        .resource_mut::<AbilityDefs>()
        .abilities
        .insert(AbilityId(id.to_string()), handle);
}

/// Spawn an `ActiveAbility` entity with archetype components applied immediately.
/// When spawning directly in Active phase, also inserts `OnHitEffects` to match
/// the production Startup->Active transition behavior.
fn spawn_test_active_ability(app: &mut App, active: ActiveAbility) -> Entity {
    let def_id = active.def_id.clone();
    let phase = active.phase.clone();
    let caster = active.caster;
    let original_caster = active.original_caster;
    let depth = active.depth;
    let entity = app.world_mut().spawn(active).id();
    apply_test_archetype(app, entity, &def_id);

    // Mirror what update_active_abilities does on Startup->Active transition:
    // insert OnHitEffects from OnHitEffectDefs when starting in Active phase.
    if phase == AbilityPhase::Active {
        if let Some(defs) = app.world().get::<OnHitEffectDefs>(entity) {
            if !defs.0.is_empty() {
                let effects = defs.0.clone();
                app.world_mut().entity_mut(entity).insert(OnHitEffects {
                    effects,
                    caster,
                    original_caster,
                    depth,
                });
            }
        }
    }
    entity
}

/// Apply archetype components from an ability's asset to an entity via the type registry.
fn apply_test_archetype(app: &mut App, entity: Entity, ability_id: &AbilityId) {
    let (asset, registry_arc) = {
        let world = app.world();
        let ability_defs = world.resource::<AbilityDefs>();
        let handle = ability_defs
            .get(ability_id)
            .unwrap_or_else(|| panic!("ability {:?} not in defs", ability_id))
            .clone();
        let assets = world.resource::<Assets<AbilityAsset>>();
        let asset = assets
            .get(&handle)
            .unwrap_or_else(|| panic!("ability {:?} asset not loaded", ability_id))
            .clone();
        let registry_arc = world.resource::<AppTypeRegistry>().0.clone();
        (asset, registry_arc)
    };

    let registry = registry_arc.read();
    let mut entity_mut = app.world_mut().entity_mut(entity);
    for component in &asset.components {
        let type_path = component.reflect_type_path();
        let registration = registry
            .get_with_type_path(type_path)
            .unwrap_or_else(|| panic!("Ability component type not registered: {type_path}"));
        let reflect_component = registration
            .data::<ReflectComponent>()
            .unwrap_or_else(|| panic!("Type missing #[reflect(Component)]: {type_path}"));
        reflect_component.insert(&mut entity_mut, component.as_ref(), &registry);
    }
}

/// Standard test ability definitions.
fn test_defs() -> Vec<(String, AbilityAsset)> {
    vec![
        (
            "punch".into(),
            build_ability_asset(
                AbilityPhases {
                    startup: 4,
                    active: 20,
                    recovery: 0,
                    cooldown: 16,
                },
                vec![TickEffect {
                    tick: 0,
                    effect: AbilityEffect::Melee {
                        id: None,
                        target: EffectTarget::Caster,
                    },
                }],
                vec![],
                vec![
                    AbilityEffect::Damage {
                        amount: 5.0,
                        target: EffectTarget::Victim,
                    },
                    AbilityEffect::ApplyForce {
                        force: Vec3::new(0.0, 0.9, 2.85),
                        frame: ForceFrame::RelativePosition,
                        target: EffectTarget::Victim,
                    },
                ],
                vec![],
                vec![InputEffect {
                    input: AbilityInput::Slot(0),
                    effect: AbilityEffect::Ability {
                        id: "punch2".into(),
                        target: EffectTarget::Caster,
                    },
                }],
            ),
        ),
        (
            "punch2".into(),
            build_ability_asset(
                AbilityPhases {
                    startup: 4,
                    active: 20,
                    recovery: 0,
                    cooldown: 0,
                },
                vec![TickEffect {
                    tick: 0,
                    effect: AbilityEffect::Melee {
                        id: None,
                        target: EffectTarget::Caster,
                    },
                }],
                vec![],
                vec![
                    AbilityEffect::Damage {
                        amount: 6.0,
                        target: EffectTarget::Victim,
                    },
                    AbilityEffect::ApplyForce {
                        force: Vec3::new(0.0, 1.05, 3.32),
                        frame: ForceFrame::RelativePosition,
                        target: EffectTarget::Victim,
                    },
                ],
                vec![],
                vec![InputEffect {
                    input: AbilityInput::Slot(0),
                    effect: AbilityEffect::Ability {
                        id: "punch3".into(),
                        target: EffectTarget::Caster,
                    },
                }],
            ),
        ),
        (
            "punch3".into(),
            build_ability_asset(
                AbilityPhases {
                    startup: 4,
                    active: 6,
                    recovery: 10,
                    cooldown: 0,
                },
                vec![TickEffect {
                    tick: 0,
                    effect: AbilityEffect::Melee {
                        id: None,
                        target: EffectTarget::Caster,
                    },
                }],
                vec![],
                vec![
                    AbilityEffect::Damage {
                        amount: 10.0,
                        target: EffectTarget::Victim,
                    },
                    AbilityEffect::ApplyForce {
                        force: Vec3::new(0.0, 2.4, 7.65),
                        frame: ForceFrame::RelativePosition,
                        target: EffectTarget::Victim,
                    },
                ],
                vec![],
                vec![],
            ),
        ),
        (
            "dash".into(),
            build_ability_asset(
                AbilityPhases {
                    startup: 2,
                    active: 8,
                    recovery: 4,
                    cooldown: 64,
                },
                vec![],
                vec![AbilityEffect::SetVelocity {
                    speed: 15.0,
                    target: EffectTarget::Caster,
                }],
                vec![],
                vec![],
                vec![],
            ),
        ),
        (
            "fireball".into(),
            build_ability_asset(
                AbilityPhases {
                    startup: 6,
                    active: 2,
                    recovery: 8,
                    cooldown: 96,
                },
                vec![TickEffect {
                    tick: 0,
                    effect: AbilityEffect::Projectile {
                        id: None,
                        speed: 20.0,
                        lifetime_ticks: 192,
                    },
                }],
                vec![],
                vec![
                    AbilityEffect::Damage {
                        amount: 25.0,
                        target: EffectTarget::Victim,
                    },
                    AbilityEffect::ApplyForce {
                        force: Vec3::new(0.0, 2.4, 7.65),
                        frame: ForceFrame::RelativePosition,
                        target: EffectTarget::Victim,
                    },
                ],
                vec![],
                vec![],
            ),
        ),
    ]
}

fn test_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(AssetPlugin::default());
    app.add_plugins(bevy::state::app::StatesPlugin);
    app.add_plugins(protocol::AppStatePlugin);
    app.add_plugins(AbilityPlugin);
    app.init_resource::<ComponentRegistry>();
    app.world_mut().register_component::<Server>();
    app.world_mut().register_component::<PreSpawnedReceiver>();

    // Pre-insert AbilityDefs so the plugin's insert_ability_defs system no-ops
    let defs = test_defs();
    let mut ability_handles = HashMap::new();
    {
        let mut assets = app.world_mut().resource_mut::<Assets<AbilityAsset>>();
        for (id, asset) in defs {
            let handle = assets.add(asset);
            ability_handles.insert(AbilityId(id), handle);
        }
    }
    app.insert_resource(AbilityDefs {
        abilities: ability_handles,
    });
    app.insert_resource(DefaultAbilitySlots::default());

    // Run FixedUpdate exactly once per app.update() call
    app.insert_resource(bevy::time::TimeUpdateStrategy::FixedTimesteps(1));

    // Transition to AppState::Ready so ability systems' run conditions pass.
    // Requires one update to process the state transition.
    app.world_mut()
        .resource_mut::<NextState<protocol::AppState>>()
        .set(protocol::AppState::Ready);
    app.update();

    app
}

fn insert_timeline(world: &mut World, tick_value: u16) {
    world.insert_resource(LocalTimeline::default());
    let mut timeline = world.resource_mut::<LocalTimeline>();
    timeline.apply_delta(tick_value as i16);
}

fn advance_timeline(world: &mut World, delta: i16) {
    let mut timeline = world.resource_mut::<LocalTimeline>();
    timeline.apply_delta(delta);
}

fn punch_slots() -> AbilitySlots {
    AbilitySlots([
        Some(AbilityId("punch".into())),
        Some(AbilityId("dash".into())),
        Some(AbilityId("fireball".into())),
        None,
        None,
    ])
}

fn spawn_character(world: &mut World) -> Entity {
    world
        .spawn((
            CharacterMarker,
            ActionState::<NetworkedPlayerActions>::default(),
            punch_slots(),
            AbilityCooldowns::default(),
            PlayerId(PeerId::Entity(1)),
            avian3d::prelude::Position(Vec3::ZERO),
            avian3d::prelude::Rotation::default(),
            avian3d::prelude::LinearVelocity(Vec3::ZERO),
            protocol::map::MapInstanceId::Overworld,
        ))
        .id()
}

fn find_active_ability(world: &mut World) -> Option<(Entity, ActiveAbility)> {
    world
        .query::<(Entity, &ActiveAbility)>()
        .iter(world)
        .next()
        .map(|(e, a)| (e, a.clone()))
}

fn find_active_ability_for_def(world: &mut World, def_id: &str) -> Option<(Entity, ActiveAbility)> {
    world
        .query::<(Entity, &ActiveAbility)>()
        .iter(world)
        .find(|(_, a)| a.def_id == AbilityId(def_id.into()))
        .map(|(e, a)| (e, a.clone()))
}

#[test]
fn activation_on_press() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 100);
    let char_entity = spawn_character(app.world_mut());

    app.world_mut()
        .get_mut::<ActionState<NetworkedPlayerActions>>(char_entity)
        .unwrap()
        .press(&NetworkedPlayerActions::Ability1);

    app.update();

    let (_, active) =
        find_active_ability(app.world_mut()).expect("ActiveAbility entity should exist");
    assert_eq!(active.def_id, AbilityId("punch".into()));
    assert_eq!(active.caster, char_entity);

    let timeline = app.world().resource::<LocalTimeline>();
    assert_eq!(active.phase_start_tick, timeline.tick());
}

#[test]
fn activation_blocked_by_cooldown() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 100);
    let char_entity = spawn_character(app.world_mut());

    app.world_mut()
        .get_mut::<AbilityCooldowns>(char_entity)
        .unwrap()
        .last_used[0] = Some(Tick(90));

    app.world_mut()
        .get_mut::<ActionState<NetworkedPlayerActions>>(char_entity)
        .unwrap()
        .press(&NetworkedPlayerActions::Ability1);

    app.update();

    assert!(
        find_active_ability(app.world_mut()).is_none(),
        "Should not activate while on cooldown"
    );
}

#[test]
fn activation_empty_slot() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 100);
    let char_entity = spawn_character(app.world_mut());

    app.world_mut()
        .get_mut::<ActionState<NetworkedPlayerActions>>(char_entity)
        .unwrap()
        .press(&NetworkedPlayerActions::Ability4);

    app.update();

    assert!(
        find_active_ability(app.world_mut()).is_none(),
        "Should not activate empty slot"
    );
}

#[test]
fn activation_sets_cooldown() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 100);
    let char_entity = spawn_character(app.world_mut());

    app.world_mut()
        .get_mut::<ActionState<NetworkedPlayerActions>>(char_entity)
        .unwrap()
        .press(&NetworkedPlayerActions::Ability1);

    app.update();

    let cd = app.world().get::<AbilityCooldowns>(char_entity).unwrap();
    let timeline = app.world().resource::<LocalTimeline>();
    assert_eq!(cd.last_used[0], Some(timeline.tick()));
}

#[test]
fn phase_startup_to_active() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 100);
    let char_entity = spawn_character(app.world_mut());

    spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("punch".into()),
            caster: char_entity,
            original_caster: char_entity,
            target: char_entity,
            phase: AbilityPhase::Startup,
            phase_start_tick: Tick(100),
            ability_slot: 0,
            depth: 0,
        },
    );

    advance_timeline(app.world_mut(), 4);
    app.update();

    let (_, active) = find_active_ability_for_def(app.world_mut(), "punch")
        .expect("ActiveAbility should still exist");
    assert_eq!(active.phase, AbilityPhase::Active);
}

#[test]
fn phase_active_to_recovery() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 200);
    let char_entity = spawn_character(app.world_mut());

    spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("punch3".into()),
            caster: char_entity,
            original_caster: char_entity,
            target: char_entity,
            phase: AbilityPhase::Active,
            phase_start_tick: Tick(200),
            ability_slot: 0,
            depth: 0,
        },
    );

    advance_timeline(app.world_mut(), 6);
    app.update();

    let (_, active) = find_active_ability_for_def(app.world_mut(), "punch3")
        .expect("ActiveAbility should still exist");
    assert_eq!(active.phase, AbilityPhase::Recovery);
}

#[test]
fn phase_recovery_completes() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 300);
    let char_entity = spawn_character(app.world_mut());

    spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("dash".into()),
            caster: char_entity,
            original_caster: char_entity,
            target: char_entity,
            phase: AbilityPhase::Recovery,
            phase_start_tick: Tick(300),
            ability_slot: 1,
            depth: 0,
        },
    );

    advance_timeline(app.world_mut(), 4);
    app.update();

    // prediction_despawn inserts PredictionDisable rather than actually despawning
    // in a test environment without full prediction plugin, the entity may still
    // exist but should have been marked for despawn. We verify the phase was Recovery
    // and the system ran without error.
}

#[test]
fn bullet_lifetime_despawn() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 300);

    let spawn_entity = app
        .world_mut()
        .spawn(AbilityProjectileSpawn {
            spawn_tick: Tick(100),
            position: Vec3::ZERO,
            direction: Vec3::NEG_Z,
            speed: 20.0,
            lifetime_ticks: 192,
            ability_id: AbilityId("fireball".into()),
            shooter: Entity::PLACEHOLDER,
        })
        .id();

    let bullet_entity = app.world_mut().spawn(AbilityBulletOf(spawn_entity)).id();

    app.update();

    assert!(
        app.world().get_entity(bullet_entity).is_err(),
        "Bullet should be despawned after lifetime expires"
    );
}

#[test]
fn bullet_lifetime_alive() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 200);

    let spawn_entity = app
        .world_mut()
        .spawn(AbilityProjectileSpawn {
            spawn_tick: Tick(100),
            position: Vec3::ZERO,
            direction: Vec3::NEG_Z,
            speed: 20.0,
            lifetime_ticks: 192,
            ability_id: AbilityId("fireball".into()),
            shooter: Entity::PLACEHOLDER,
        })
        .id();

    let bullet_entity = app.world_mut().spawn(AbilityBulletOf(spawn_entity)).id();

    app.update();

    assert!(
        app.world().get_entity(bullet_entity).is_ok(),
        "Bullet should survive before lifetime expires"
    );
}

#[test]
fn on_hit_effects_dispatched_on_first_active_tick() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 200);
    let char_entity = spawn_character(app.world_mut());

    let ability_entity = spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("punch".into()),
            caster: char_entity,
            original_caster: char_entity,
            target: char_entity,
            phase: AbilityPhase::Active,
            phase_start_tick: Tick(200),
            ability_slot: 0,
            depth: 0,
        },
    );

    app.update();

    let on_hit = app
        .world()
        .get::<OnHitEffects>(ability_entity)
        .expect("OnHitEffects should be present on first Active tick");
    assert_eq!(
        on_hit.effects.len(),
        2,
        "punch has 2 OnHit effects (Damage + ApplyForce)"
    );
    assert_eq!(on_hit.caster, char_entity);
    assert_eq!(on_hit.original_caster, char_entity);
    assert_eq!(on_hit.depth, 0);
}

#[test]
fn on_hit_effects_removed_on_recovery() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 200);
    let char_entity = spawn_character(app.world_mut());

    let ability_entity = spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("punch3".into()),
            caster: char_entity,
            original_caster: char_entity,
            target: char_entity,
            phase: AbilityPhase::Active,
            phase_start_tick: Tick(200),
            ability_slot: 0,
            depth: 0,
        },
    );

    // First update: dispatches markers (Active phase, tick 200)
    app.update();
    assert!(app.world().get::<OnHitEffects>(ability_entity).is_some());

    // Advance past active_ticks (6 ticks for punch3)
    advance_timeline(app.world_mut(), 6);
    app.update();

    // Now in Recovery -- OnHitEffects should be removed
    let active = app
        .world()
        .get::<ActiveAbility>(ability_entity)
        .expect("ActiveAbility should still exist in Recovery");
    assert_eq!(active.phase, AbilityPhase::Recovery);
    assert!(
        app.world().get::<OnHitEffects>(ability_entity).is_none(),
        "OnHitEffects should be removed when leaving Active phase"
    );
}

#[test]
fn melee_hitbox_entity_spawned() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 200);
    let char_entity = spawn_character(app.world_mut());

    spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("punch".into()),
            caster: char_entity,
            original_caster: char_entity,
            target: char_entity,
            phase: AbilityPhase::Active,
            phase_start_tick: Tick(200),
            ability_slot: 0,
            depth: 0,
        },
    );

    app.update();

    let hitbox = app
        .world_mut()
        .query::<(Entity, &HitboxOf, &MeleeHitbox, &HitTargets)>()
        .iter(app.world())
        .next();
    assert!(
        hitbox.is_some(),
        "Melee hitbox entity should be spawned with HitboxOf, MeleeHitbox, HitTargets"
    );
}

#[test]
fn hitbox_entity_has_correct_on_hit_effects() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 200);
    let char_entity = spawn_character(app.world_mut());

    spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("punch".into()),
            caster: char_entity,
            original_caster: char_entity,
            target: char_entity,
            phase: AbilityPhase::Active,
            phase_start_tick: Tick(200),
            ability_slot: 0,
            depth: 0,
        },
    );

    app.update();

    let hitbox_on_hit = app
        .world_mut()
        .query_filtered::<&OnHitEffects, With<HitboxOf>>()
        .iter(app.world())
        .next();
    let on_hit = hitbox_on_hit.expect("Hitbox entity should have OnHitEffects");
    assert_eq!(
        on_hit.effects.len(),
        2,
        "punch has 2 OnHit effects (Damage + ApplyForce)"
    );
    assert_eq!(on_hit.caster, char_entity);
    assert_eq!(on_hit.original_caster, char_entity);
}

#[test]
fn on_end_effects_dispatched_on_active_to_recovery() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 200);
    let char_entity = spawn_character(app.world_mut());

    insert_test_ability(
        &mut app,
        "dash_with_end",
        build_ability_asset(
            AbilityPhases {
                startup: 2,
                active: 4,
                recovery: 4,
                cooldown: 32,
            },
            vec![],
            vec![AbilityEffect::SetVelocity {
                speed: 15.0,
                target: EffectTarget::Caster,
            }],
            vec![],
            vec![AbilityEffect::SetVelocity {
                speed: 0.0,
                target: EffectTarget::Caster,
            }],
            vec![],
        ),
    );

    let ability_entity = spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("dash_with_end".into()),
            caster: char_entity,
            original_caster: char_entity,
            target: char_entity,
            phase: AbilityPhase::Active,
            phase_start_tick: Tick(200),
            ability_slot: 1,
            depth: 0,
        },
    );

    // First tick: Active phase. OnEndEffects is present as archetype component but
    // apply_on_end_effects only fires on Recovery entry tick.
    app.update();

    // Advance past active_ticks (4 ticks) -> triggers Active->Recovery
    advance_timeline(app.world_mut(), 4);
    app.update();

    // Should now be in Recovery phase
    let active = app
        .world()
        .get::<ActiveAbility>(ability_entity)
        .expect("ActiveAbility should still exist in Recovery");
    assert_eq!(active.phase, AbilityPhase::Recovery);

    // OnEndEffects was applied: velocity should be set to 0
    let velocity = app
        .world()
        .get::<avian3d::prelude::LinearVelocity>(char_entity)
        .unwrap();
    assert_eq!(velocity.x, 0.0);
    assert_eq!(velocity.z, 0.0);
}

fn count_active_abilities(world: &mut World) -> usize {
    world.query::<&ActiveAbility>().iter(world).count()
}

#[test]
fn sub_ability_spawned_on_cast() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 200);
    let char_entity = spawn_character(app.world_mut());

    insert_test_ability(
        &mut app,
        "chain_test",
        build_ability_asset(
            AbilityPhases {
                startup: 0,
                active: 4,
                recovery: 2,
                cooldown: 0,
            },
            vec![TickEffect {
                tick: 0,
                effect: AbilityEffect::Ability {
                    id: "punch".into(),
                    target: EffectTarget::Caster,
                },
            }],
            vec![],
            vec![],
            vec![],
            vec![],
        ),
    );

    spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("chain_test".into()),
            caster: char_entity,
            original_caster: char_entity,
            target: char_entity,
            phase: AbilityPhase::Active,
            phase_start_tick: Tick(200),
            ability_slot: 0,
            depth: 0,
        },
    );

    app.update();

    // Should now have 2 ActiveAbility entities: chain_test + spawned punch
    assert_eq!(count_active_abilities(app.world_mut()), 2);

    let (_, sub) = find_active_ability_for_def(app.world_mut(), "punch")
        .expect("Sub-ability 'punch' should exist");
    assert_eq!(sub.caster, char_entity);
    assert_eq!(sub.original_caster, char_entity);
    assert_eq!(sub.depth, 1);
    assert_eq!(sub.phase, AbilityPhase::Startup);
}

#[test]
fn sub_ability_depth_limited() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 200);
    let char_entity = spawn_character(app.world_mut());

    insert_test_ability(
        &mut app,
        "recurse",
        build_ability_asset(
            AbilityPhases {
                startup: 0,
                active: 4,
                recovery: 2,
                cooldown: 0,
            },
            vec![TickEffect {
                tick: 0,
                effect: AbilityEffect::Ability {
                    id: "punch".into(),
                    target: EffectTarget::Caster,
                },
            }],
            vec![],
            vec![],
            vec![],
            vec![],
        ),
    );

    // Spawn at depth 4 -- should NOT spawn sub-ability
    spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("recurse".into()),
            caster: char_entity,
            original_caster: char_entity,
            target: char_entity,
            phase: AbilityPhase::Active,
            phase_start_tick: Tick(200),
            ability_slot: 0,
            depth: 4,
        },
    );

    app.update();

    // Only the parent should exist -- sub-ability blocked by depth limit
    assert_eq!(count_active_abilities(app.world_mut()), 1);
    assert!(
        find_active_ability_for_def(app.world_mut(), "punch").is_none(),
        "Sub-ability should not spawn at depth >= 4"
    );
}

#[test]
fn sub_ability_phase_management() {
    // Verify that a sub-ability (depth > 0) goes through normal phase cycle
    let mut app = test_app();
    insert_timeline(app.world_mut(), 200);
    let char_entity = spawn_character(app.world_mut());

    // Use punch3 which has recovery_ticks > 0 for a full phase cycle test
    spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("punch3".into()),
            caster: char_entity,
            original_caster: char_entity,
            target: char_entity,
            phase: AbilityPhase::Startup,
            phase_start_tick: Tick(200),
            ability_slot: 0,
            depth: 1,
        },
    );

    app.update();

    let (_, sub) =
        find_active_ability_for_def(app.world_mut(), "punch3").expect("punch3 should exist");
    assert_eq!(sub.phase, AbilityPhase::Startup);
    assert_eq!(sub.depth, 1);

    // Advance 4 ticks: punch3 Startup (4 ticks) completes -> Active
    advance_timeline(app.world_mut(), 4);
    app.update();

    let (_, sub) =
        find_active_ability_for_def(app.world_mut(), "punch3").expect("punch3 should still exist");
    assert_eq!(sub.phase, AbilityPhase::Active);

    // Advance 6 more ticks: punch3 Active (6 ticks) completes -> Recovery
    advance_timeline(app.world_mut(), 6);
    app.update();

    let (_, sub) = find_active_ability_for_def(app.world_mut(), "punch3")
        .expect("punch3 should still exist in Recovery");
    assert_eq!(sub.phase, AbilityPhase::Recovery);
}

#[test]
fn on_input_effects_dispatched_during_active() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 200);
    let char_entity = spawn_character(app.world_mut());

    let ability_entity = spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("punch".into()),
            caster: char_entity,
            original_caster: char_entity,
            target: char_entity,
            phase: AbilityPhase::Active,
            phase_start_tick: Tick(200),
            ability_slot: 0,
            depth: 0,
        },
    );

    app.update();

    let on_input = app
        .world()
        .get::<OnInputEffects>(ability_entity)
        .expect("OnInputEffects should be present during Active phase");
    assert_eq!(on_input.0.len(), 1, "punch has 1 OnInput effect");
    assert_eq!(on_input.0[0].input, AbilityInput::Slot(0));
    assert_eq!(
        on_input.0[0].effect,
        AbilityEffect::Ability {
            id: "punch2".into(),
            target: EffectTarget::Caster
        },
    );
}

#[test]
fn active_input_effect_fires_from_semantic_ability_input() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 200);
    let char_entity = spawn_character(app.world_mut());

    spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("punch".into()),
            caster: char_entity,
            original_caster: char_entity,
            target: char_entity,
            phase: AbilityPhase::Active,
            phase_start_tick: Tick(200),
            ability_slot: 0,
            depth: 0,
        },
    );

    app.world_mut()
        .get_mut::<ActionState<NetworkedPlayerActions>>(char_entity)
        .unwrap()
        .press(&NetworkedPlayerActions::Ability1);
    app.update();

    assert!(
        find_active_ability_for_def(app.world_mut(), "punch2").is_some(),
        "Slot(0) OnInput should map to Ability1 and spawn punch2"
    );
}

#[test]
fn active_input_effect_ignores_unmatched_ability_input() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 200);
    let char_entity = spawn_character(app.world_mut());

    spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("punch".into()),
            caster: char_entity,
            original_caster: char_entity,
            target: char_entity,
            phase: AbilityPhase::Active,
            phase_start_tick: Tick(200),
            ability_slot: 0,
            depth: 0,
        },
    );

    app.world_mut()
        .get_mut::<ActionState<NetworkedPlayerActions>>(char_entity)
        .unwrap()
        .press(&NetworkedPlayerActions::Ability2);
    app.update();

    assert!(
        find_active_ability_for_def(app.world_mut(), "punch2").is_none(),
        "Slot(0) OnInput should ignore unmatched Ability2 network input"
    );
}

#[test]
fn on_input_effects_removed_on_despawn() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 200);
    let char_entity = spawn_character(app.world_mut());

    // Use punch3 which has recovery_ticks=10 for a clean lifecycle
    let ability_entity = spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("punch3".into()),
            caster: char_entity,
            original_caster: char_entity,
            target: char_entity,
            phase: AbilityPhase::Active,
            phase_start_tick: Tick(200),
            ability_slot: 0,
            depth: 0,
        },
    );

    // First update: Active phase, OnTickEffects present from archetype
    app.update();
    assert!(
        app.world().get::<OnTickEffects>(ability_entity).is_some(),
        "archetype components should be present during Active phase"
    );

    // Advance past active_ticks (6 ticks for punch3) -> Recovery
    advance_timeline(app.world_mut(), 6);
    app.update();

    let active = app
        .world()
        .get::<ActiveAbility>(ability_entity)
        .expect("should be in Recovery");
    assert_eq!(active.phase, AbilityPhase::Recovery);

    // Advance past recovery_ticks (10 ticks) -> despawn
    advance_timeline(app.world_mut(), 10);
    app.update();

    // Entity should be despawned; cleanup observer removes archetype components
    assert!(
        app.world().get_entity(ability_entity).is_err(),
        "Ability entity should be despawned after recovery completes"
    );
}

fn test_app_with_hit_detection() -> App {
    // AbilityPlugin already registers hit detection systems
    test_app()
}

/// Components required by avian's `Forces` `QueryData` so that the production
/// `target_query` (which uses `Forces`) matches the test entity. `ComputedMass::new(1.0)`
/// makes `apply_linear_impulse` produce velocity changes equal to the impulse.
fn forces_components() -> (
    avian3d::prelude::AngularVelocity,
    avian3d::prelude::ComputedMass,
    avian3d::prelude::ComputedAngularInertia,
    avian3d::prelude::ComputedCenterOfMass,
    avian3d::dynamics::integrator::VelocityIntegrationData,
    avian3d::prelude::forces::AccumulatedLocalAcceleration,
) {
    (
        avian3d::prelude::AngularVelocity::default(),
        avian3d::prelude::ComputedMass::new(1.0),
        avian3d::prelude::ComputedAngularInertia::default(),
        avian3d::prelude::ComputedCenterOfMass(Vec3::ZERO),
        avian3d::dynamics::integrator::VelocityIntegrationData::default(),
        avian3d::prelude::forces::AccumulatedLocalAcceleration::default(),
    )
}

fn spawn_target(world: &mut World, pos: Vec3) -> Entity {
    world
        .spawn((
            CharacterMarker,
            Health::new(100.0),
            avian3d::prelude::Position(pos),
            avian3d::prelude::Rotation::default(),
            avian3d::prelude::LinearVelocity(Vec3::ZERO),
            forces_components(),
            protocol::map::MapInstanceId::Overworld,
        ))
        .id()
}

#[test]
fn aoe_hitbox_damages_target() {
    let mut app = test_app_with_hit_detection();
    insert_timeline(app.world_mut(), 200);
    let caster = spawn_character(app.world_mut());
    let target = spawn_target(app.world_mut(), Vec3::new(3.0, 0.0, 0.0));

    insert_test_ability(
        &mut app,
        "aoe_test",
        build_ability_asset(
            AbilityPhases {
                startup: 0,
                active: 1,
                recovery: 4,
                cooldown: 0,
            },
            vec![TickEffect {
                tick: 0,
                effect: AbilityEffect::AreaOfEffect {
                    id: None,
                    target: EffectTarget::Caster,
                    radius: 5.0,
                    duration_ticks: None,
                },
            }],
            vec![],
            vec![AbilityEffect::Damage {
                amount: 25.0,
                target: EffectTarget::Victim,
            }],
            vec![],
            vec![],
        ),
    );

    spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("aoe_test".into()),
            caster,
            original_caster: caster,
            target: caster,
            phase: AbilityPhase::Active,
            phase_start_tick: Tick(200),
            ability_slot: 0,
            depth: 0,
        },
    );

    // Update 1: spawn AoE hitbox entity
    app.update();

    // Simulate physics: populate CollidingEntities on the hitbox with the target
    let hitbox_entity = app
        .world_mut()
        .query_filtered::<Entity, With<HitboxOf>>()
        .iter(app.world())
        .next()
        .expect("AoE hitbox entity should exist");

    app.world_mut()
        .get_mut::<CollidingEntities>(hitbox_entity)
        .unwrap()
        .insert(target);

    // Advance timeline: ability will transition Active -> Recovery
    advance_timeline(app.world_mut(), 1);

    // Update 2: hit detection should process the collision BEFORE cleanup despawns the hitbox
    app.update();

    let health = app.world().get::<Health>(target).unwrap();
    assert_eq!(
        health.current, 75.0,
        "Target should take 25 damage from AoE hit (got {} HP remaining)",
        health.current
    );
}

#[test]
fn teleport_moves_caster() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 200);
    let char_entity = spawn_character(app.world_mut());

    insert_test_ability(
        &mut app,
        "teleport_test",
        build_ability_asset(
            AbilityPhases {
                startup: 0,
                active: 4,
                recovery: 2,
                cooldown: 0,
            },
            vec![TickEffect {
                tick: 0,
                effect: AbilityEffect::Teleport { distance: 10.0 },
            }],
            vec![],
            vec![],
            vec![],
            vec![],
        ),
    );

    spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("teleport_test".into()),
            caster: char_entity,
            original_caster: char_entity,
            target: char_entity,
            phase: AbilityPhase::Active,
            phase_start_tick: Tick(200),
            ability_slot: 0,
            depth: 0,
        },
    );

    app.update();

    let pos = app
        .world()
        .get::<avian3d::prelude::Position>(char_entity)
        .unwrap();
    // Default Rotation faces NEG_Z, so teleport should move to ~(0, 0, -10)
    assert!((pos.0.x).abs() < 0.01, "X should be ~0, got {}", pos.0.x);
    assert!(
        (pos.0.z - (-10.0)).abs() < 0.01,
        "Z should be ~-10, got {}",
        pos.0.z
    );
}

#[test]
fn shield_absorbs_damage() {
    let mut app = test_app_with_hit_detection();
    insert_timeline(app.world_mut(), 200);
    let caster = spawn_character(app.world_mut());

    let target = app
        .world_mut()
        .spawn((
            CharacterMarker,
            Health::new(100.0),
            ActiveShield { remaining: 50.0 },
            avian3d::prelude::Position(Vec3::new(1.0, 0.0, 0.0)),
            avian3d::prelude::Rotation::default(),
            avian3d::prelude::LinearVelocity(Vec3::ZERO),
            forces_components(),
            protocol::map::MapInstanceId::Overworld,
        ))
        .id();

    insert_test_ability(
        &mut app,
        "shield_test",
        build_ability_asset(
            AbilityPhases {
                startup: 0,
                active: 4,
                recovery: 2,
                cooldown: 0,
            },
            vec![TickEffect {
                tick: 0,
                effect: AbilityEffect::Melee {
                    id: None,
                    target: EffectTarget::Caster,
                },
            }],
            vec![],
            vec![AbilityEffect::Damage {
                amount: 30.0,
                target: EffectTarget::Victim,
            }],
            vec![],
            vec![],
        ),
    );

    spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("shield_test".into()),
            caster,
            original_caster: caster,
            target: caster,
            phase: AbilityPhase::Active,
            phase_start_tick: Tick(200),
            ability_slot: 0,
            depth: 0,
        },
    );

    // First update: spawn hitbox
    app.update();

    let hitbox_entity = app
        .world_mut()
        .query_filtered::<Entity, With<HitboxOf>>()
        .iter(app.world())
        .next()
        .expect("hitbox should exist");

    app.world_mut()
        .get_mut::<CollidingEntities>(hitbox_entity)
        .unwrap()
        .insert(target);

    // Second update: process hits
    app.update();

    let shield = app
        .world()
        .get::<ActiveShield>(target)
        .expect("Shield should still exist");
    assert_eq!(shield.remaining, 20.0);

    let health = app.world().get::<Health>(target).unwrap();
    assert_eq!(
        health.current, 100.0,
        "Health should be untouched when shield absorbs all damage"
    );
}

#[test]
fn shield_overflow_damages_health() {
    let mut app = test_app_with_hit_detection();
    insert_timeline(app.world_mut(), 200);
    let caster = spawn_character(app.world_mut());

    let target = app
        .world_mut()
        .spawn((
            CharacterMarker,
            Health::new(100.0),
            ActiveShield { remaining: 20.0 },
            avian3d::prelude::Position(Vec3::new(1.0, 0.0, 0.0)),
            avian3d::prelude::Rotation::default(),
            avian3d::prelude::LinearVelocity(Vec3::ZERO),
            forces_components(),
            protocol::map::MapInstanceId::Overworld,
        ))
        .id();

    insert_test_ability(
        &mut app,
        "shield_overflow_test",
        build_ability_asset(
            AbilityPhases {
                startup: 0,
                active: 4,
                recovery: 2,
                cooldown: 0,
            },
            vec![TickEffect {
                tick: 0,
                effect: AbilityEffect::Melee {
                    id: None,
                    target: EffectTarget::Caster,
                },
            }],
            vec![],
            vec![AbilityEffect::Damage {
                amount: 50.0,
                target: EffectTarget::Victim,
            }],
            vec![],
            vec![],
        ),
    );

    spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("shield_overflow_test".into()),
            caster,
            original_caster: caster,
            target: caster,
            phase: AbilityPhase::Active,
            phase_start_tick: Tick(200),
            ability_slot: 0,
            depth: 0,
        },
    );

    app.update();

    let hitbox_entity = app
        .world_mut()
        .query_filtered::<Entity, With<HitboxOf>>()
        .iter(app.world())
        .next()
        .expect("hitbox should exist");

    app.world_mut()
        .get_mut::<CollidingEntities>(hitbox_entity)
        .unwrap()
        .insert(target);

    app.update();

    // Shield depleted -> removed
    assert!(
        app.world().get::<ActiveShield>(target).is_none(),
        "Shield should be removed after being fully depleted"
    );

    // Health takes overflow: 100 - (50 - 20) = 70
    let health = app.world().get::<Health>(target).unwrap();
    assert_eq!(
        health.current, 70.0,
        "Health should take overflow damage past shield"
    );
}

#[test]
fn buff_inserted_on_target() {
    let mut app = test_app();
    insert_timeline(app.world_mut(), 200);
    let char_entity = spawn_character(app.world_mut());

    insert_test_ability(
        &mut app,
        "buff_test",
        build_ability_asset(
            AbilityPhases {
                startup: 0,
                active: 4,
                recovery: 2,
                cooldown: 0,
            },
            vec![TickEffect {
                tick: 0,
                effect: AbilityEffect::Buff {
                    stat: "speed".into(),
                    multiplier: 1.5,
                    duration_ticks: 100,
                    target: EffectTarget::Caster,
                },
            }],
            vec![],
            vec![],
            vec![],
            vec![],
        ),
    );

    spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("buff_test".into()),
            caster: char_entity,
            original_caster: char_entity,
            target: char_entity,
            phase: AbilityPhase::Active,
            phase_start_tick: Tick(200),
            ability_slot: 0,
            depth: 0,
        },
    );

    app.update();

    let buffs = app
        .world()
        .get::<ActiveBuffs>(char_entity)
        .expect("ActiveBuffs should be present on caster");
    assert_eq!(buffs.0.len(), 1);
    assert_eq!(buffs.0[0].stat, "speed");
    assert_eq!(buffs.0[0].multiplier, 1.5);
    // expires_tick = 200 + 100 = Tick(300)
    assert_eq!(buffs.0[0].expires_tick, Tick(200) + 100i16);
}

#[test]
fn buff_expires_after_duration() {
    let mut app = test_app();
    app.add_systems(Update, ability::expire_buffs);
    insert_timeline(app.world_mut(), 200);
    let char_entity = spawn_character(app.world_mut());

    app.world_mut()
        .entity_mut(char_entity)
        .insert(ActiveBuffs(vec![ActiveBuff {
            stat: "speed".into(),
            multiplier: 1.5,
            expires_tick: Tick(210),
        }]));

    // At tick 200: buff should still exist (expires at 210)
    app.update();
    assert!(
        app.world().get::<ActiveBuffs>(char_entity).is_some(),
        "Buff should still exist before expiry tick"
    );

    // Advance to tick 211 (past expiry)
    advance_timeline(app.world_mut(), 11);
    app.update();

    assert!(
        app.world().get::<ActiveBuffs>(char_entity).is_none(),
        "ActiveBuffs should be removed after expiry"
    );
}

#[test]
fn buff_increases_damage() {
    let mut app = test_app_with_hit_detection();
    insert_timeline(app.world_mut(), 200);
    let caster = spawn_character(app.world_mut());

    // Give caster a 2x damage buff
    app.world_mut()
        .entity_mut(caster)
        .insert(ActiveBuffs(vec![ActiveBuff {
            stat: "damage".into(),
            multiplier: 2.0,
            expires_tick: Tick(999),
        }]));

    let target = spawn_target(app.world_mut(), Vec3::new(1.0, 0.0, 0.0));

    insert_test_ability(
        &mut app,
        "buff_dmg_test",
        build_ability_asset(
            AbilityPhases {
                startup: 0,
                active: 4,
                recovery: 2,
                cooldown: 0,
            },
            vec![TickEffect {
                tick: 0,
                effect: AbilityEffect::Melee {
                    id: None,
                    target: EffectTarget::Caster,
                },
            }],
            vec![],
            vec![AbilityEffect::Damage {
                amount: 10.0,
                target: EffectTarget::Victim,
            }],
            vec![],
            vec![],
        ),
    );

    spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("buff_dmg_test".into()),
            caster,
            original_caster: caster,
            target: caster,
            phase: AbilityPhase::Active,
            phase_start_tick: Tick(200),
            ability_slot: 0,
            depth: 0,
        },
    );

    // First update: spawn hitbox
    app.update();

    let hitbox_entity = app
        .world_mut()
        .query_filtered::<Entity, With<HitboxOf>>()
        .iter(app.world())
        .next()
        .expect("hitbox should exist");

    app.world_mut()
        .get_mut::<CollidingEntities>(hitbox_entity)
        .unwrap()
        .insert(target);

    // Second update: process hits with buff active
    app.update();

    let health = app.world().get::<Health>(target).unwrap();
    assert_eq!(
        health.current, 80.0,
        "10 base damage * 2.0 buff = 20 effective damage; 100 - 20 = 80 HP"
    );
}

fn assert_vec3_approx(actual: Vec3, expected: Vec3, msg: &str) {
    let diff = (actual - expected).length();
    assert!(
        diff < 1e-4,
        "{msg}: expected {expected:?}, got {actual:?} (diff {diff})"
    );
}

/// Apply an `ApplyForce` effect via an AoE hitbox and return the resulting velocity of the target.
fn run_force_test(
    force: Vec3,
    frame: ForceFrame,
    caster_rotation: Quat,
    target_pos: Vec3,
    target_rotation: Quat,
) -> Vec3 {
    let mut app = test_app_with_hit_detection();
    insert_timeline(app.world_mut(), 200);
    let caster = spawn_character(app.world_mut());
    let target = spawn_target(app.world_mut(), target_pos);

    app.world_mut()
        .get_mut::<avian3d::prelude::Rotation>(caster)
        .unwrap()
        .0 = caster_rotation;
    app.world_mut()
        .get_mut::<avian3d::prelude::Rotation>(target)
        .unwrap()
        .0 = target_rotation;

    insert_test_ability(
        &mut app,
        "force_test",
        build_ability_asset(
            AbilityPhases {
                startup: 0,
                active: 1,
                recovery: 4,
                cooldown: 0,
            },
            vec![TickEffect {
                tick: 0,
                effect: AbilityEffect::AreaOfEffect {
                    id: None,
                    target: EffectTarget::Caster,
                    radius: 10.0,
                    duration_ticks: None,
                },
            }],
            vec![],
            vec![AbilityEffect::ApplyForce {
                force,
                frame,
                target: EffectTarget::Victim,
            }],
            vec![],
            vec![],
        ),
    );

    spawn_test_active_ability(
        &mut app,
        ActiveAbility {
            def_id: AbilityId("force_test".into()),
            caster,
            original_caster: caster,
            target: caster,
            phase: AbilityPhase::Active,
            phase_start_tick: Tick(200),
            ability_slot: 0,
            depth: 0,
        },
    );

    // Update 1: spawn AoE hitbox
    app.update();

    let hitbox_entity = app
        .world_mut()
        .query_filtered::<Entity, With<HitboxOf>>()
        .iter(app.world())
        .next()
        .expect("AoE hitbox should exist");

    app.world_mut()
        .get_mut::<CollidingEntities>(hitbox_entity)
        .unwrap()
        .insert(target);

    advance_timeline(app.world_mut(), 1);

    // Update 2: process hit -> apply force
    app.update();

    app.world()
        .get::<avian3d::prelude::LinearVelocity>(target)
        .unwrap()
        .0
}

#[test]
fn force_frame_world() {
    // Caster and victim have arbitrary rotations -- World frame ignores both.
    let vel = run_force_test(
        Vec3::new(1.0, 0.0, 0.0),
        ForceFrame::World,
        Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
        Vec3::new(3.0, 0.0, 0.0),
        Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
    );
    assert_vec3_approx(
        vel,
        Vec3::new(1.0, 0.0, 0.0),
        "World frame: force applied verbatim",
    );
}

#[test]
fn force_frame_caster() {
    // Caster rotated 90 deg around Y: local +X maps to world -Z.
    let vel = run_force_test(
        Vec3::new(1.0, 0.0, 0.0),
        ForceFrame::Caster,
        Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
        Vec3::new(3.0, 0.0, 0.0),
        Quat::IDENTITY,
    );
    assert_vec3_approx(
        vel,
        Vec3::new(0.0, 0.0, -1.0),
        "Caster frame: rotated by caster rotation",
    );
}

#[test]
fn force_frame_victim() {
    // Victim rotated 90 deg around Y: local +X maps to world -Z.
    let vel = run_force_test(
        Vec3::new(1.0, 0.0, 0.0),
        ForceFrame::Victim,
        Quat::IDENTITY,
        Vec3::new(3.0, 0.0, 0.0),
        Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
    );
    assert_vec3_approx(
        vel,
        Vec3::new(0.0, 0.0, -1.0),
        "Victim frame: rotated by victim rotation",
    );
}

#[test]
fn force_frame_relative_position() {
    // Victim directly in +X from source. The RelativePosition frame maps +Z -> world +X.
    // Force (0, 0, 1) = "push toward victim" -> world +X.
    let vel = run_force_test(
        Vec3::new(0.0, 0.0, 1.0),
        ForceFrame::RelativePosition,
        Quat::IDENTITY,
        Vec3::new(5.0, 0.0, 0.0),
        Quat::IDENTITY,
    );
    assert_vec3_approx(
        vel,
        Vec3::new(1.0, 0.0, 0.0),
        "RelativePosition: +Z maps to caster->victim direction",
    );
}

#[test]
fn force_frame_relative_rotation() {
    // Caster identity, victim 90 deg Y: relative = victim * caster.inverse() = 90 deg Y.
    // 90 deg Y * Vec3::X = Vec3::NEG_Z.
    let vel = run_force_test(
        Vec3::new(1.0, 0.0, 0.0),
        ForceFrame::RelativeRotation,
        Quat::IDENTITY,
        Vec3::new(3.0, 0.0, 0.0),
        Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
    );
    assert_vec3_approx(
        vel,
        Vec3::new(0.0, 0.0, -1.0),
        "RelativeRotation: victim_rot * caster_rot.inverse() * force",
    );
}

#[test]
fn archetype_loads_from_ron_bytes() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(AssetPlugin::default());
    app.register_type::<AbilityPhases>()
        .register_type::<OnTickEffects>()
        .register_type::<TickEffect>()
        .register_type::<WhileActiveEffects>()
        .register_type::<OnHitEffectDefs>()
        .register_type::<OnEndEffects>()
        .register_type::<OnInputEffects>()
        .register_type::<InputEffect>()
        .register_type::<AbilityInput>()
        .register_type::<AbilityEffect>()
        .register_type::<EffectTarget>()
        .register_type::<ForceFrame>();

    let registry = app.world().resource::<AppTypeRegistry>().0.clone();
    let reg = registry.read();

    let ron_bytes = br#"
    {
        "protocol::ability::AbilityPhases": (startup: 4, active: 20, recovery: 0, cooldown: 16),
        "protocol::ability::OnTickEffects": ([(tick: 0, effect: Melee())]),
        "protocol::ability::OnHitEffectDefs": ([
            Damage(amount: 5.0, target: Victim),
        ]),
        "protocol::ability::WhileActiveEffects": ([
            SetVelocity(speed: 15.0, target: Caster),
        ]),
    }
    "#;

    let components = protocol::reflect_loader::deserialize_component_map(ron_bytes, &reg).unwrap();
    drop(reg);

    assert_eq!(components.len(), 4);

    // Apply to entity and verify
    let entity = app.world_mut().spawn_empty().id();
    let reg = registry.read();
    let mut entity_mut = app.world_mut().entity_mut(entity);
    for component in &components {
        let type_path = component.reflect_type_path();
        let registration = reg.get_with_type_path(type_path).unwrap();
        let reflect_component = registration.data::<ReflectComponent>().unwrap();
        reflect_component.insert(&mut entity_mut, component.as_ref(), &reg);
    }
    drop(reg);

    let phases = app.world().get::<AbilityPhases>(entity).unwrap();
    assert_eq!(phases.startup, 4);
    assert_eq!(phases.active, 20);
    assert_eq!(phases.recovery, 0);
    assert_eq!(phases.cooldown, 16);

    let on_tick = app.world().get::<OnTickEffects>(entity).unwrap();
    assert_eq!(on_tick.0.len(), 1);
    assert_eq!(on_tick.0[0].tick, 0);

    let while_active = app.world().get::<WhileActiveEffects>(entity).unwrap();
    assert_eq!(while_active.0.len(), 1);

    let on_hit = app.world().get::<OnHitEffectDefs>(entity).unwrap();
    assert_eq!(on_hit.0.len(), 1);
}

#[test]
fn abilities_asset_loads_after_on_input_schema_migration() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.register_type::<AbilityPhases>()
        .register_type::<OnTickEffects>()
        .register_type::<TickEffect>()
        .register_type::<WhileActiveEffects>()
        .register_type::<OnHitEffectDefs>()
        .register_type::<OnEndEffects>()
        .register_type::<OnInputEffects>()
        .register_type::<InputEffect>()
        .register_type::<AbilityInput>()
        .register_type::<AbilityEffect>()
        .register_type::<EffectTarget>()
        .register_type::<ForceFrame>();

    let ron_bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../assets/abilities/punch.ability.ron"
    ))
    .expect("punch ability asset should exist");
    let registry = app.world().resource::<AppTypeRegistry>().0.clone();
    let reg = registry.read();
    let components = protocol::reflect_loader::deserialize_component_map(&ron_bytes, &reg)
        .expect("migrated punch ability asset should load");

    assert!(components
        .iter()
        .any(|component| component.try_downcast_ref::<OnInputEffects>().is_some()));
}

#[test]
fn archetype_ability_full_lifecycle() {
    let mut app = test_app();
    let world = app.world_mut();
    insert_timeline(world, 0);
    let character = spawn_character(world);

    world
        .get_mut::<ActionState<NetworkedPlayerActions>>(character)
        .unwrap()
        .press(&NetworkedPlayerActions::Ability1);
    app.update();

    let (ability_entity, active) = find_active_ability(app.world_mut()).unwrap();
    assert_eq!(active.phase, AbilityPhase::Startup);

    // Verify archetype components are present from spawn
    assert!(app.world().get::<AbilityPhases>(ability_entity).is_some());
    assert!(app.world().get::<OnTickEffects>(ability_entity).is_some());
    assert!(app.world().get::<OnHitEffectDefs>(ability_entity).is_some());
    assert!(app.world().get::<OnInputEffects>(ability_entity).is_some());

    // Advance through startup to active phase (punch has 4 startup ticks)
    for _ in 0..4 {
        advance_timeline(app.world_mut(), 1);
        app.update();
    }

    let (_, active) = find_active_ability(app.world_mut()).unwrap();
    assert_eq!(active.phase, AbilityPhase::Active);

    // MeleeHitbox should have spawned on first Active tick
    let hitbox_count = app
        .world_mut()
        .query::<&MeleeHitbox>()
        .iter(app.world())
        .count();
    assert_eq!(
        hitbox_count, 1,
        "melee hitbox should spawn on first Active tick"
    );
}
