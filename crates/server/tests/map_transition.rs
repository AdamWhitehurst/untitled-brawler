use bevy::ecs::system::RunSystemOnce;
use bevy::prelude::*;
use lightyear::prelude::{Room, RoomEvent, RoomPlugin, RoomTarget};
use protocol::map::{MapInstanceId, PendingTransition};
use protocol::{MapRegistry, NostrPublicKey};
use server::map::preparation::ensure_map_exists;
use server::map::{MapLoadState, MapPreparation, PendingMapSwitchPreflight, RoomRegistry};
use server::persistence::WorldSavePath;
use voxel_map_engine::prelude::{ChunkTicket, MapDimensions, VoxelMapConfig};

fn owner(byte: u8) -> NostrPublicKey {
    NostrPublicKey([byte; 32])
}

#[test]
fn map_transition_registered_map_checking_persistence_is_not_usable() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.init_resource::<MapRegistry>();
    app.init_resource::<WorldSavePath>();
    let map_entity = app
        .world_mut()
        .spawn((MapInstanceId::Overworld, MapLoadState::CheckingPersistence))
        .id();
    app.world_mut()
        .resource_mut::<MapRegistry>()
        .insert(MapInstanceId::Overworld, map_entity);

    app.world_mut()
        .run_system_once(
            |mut commands: Commands,
             mut registry: ResMut<MapRegistry>,
             states: Query<Ref<MapLoadState>>,
             params: Query<(&VoxelMapConfig, &MapDimensions)>,
             save_path: Res<WorldSavePath>| {
                let preparation = ensure_map_exists(
                    &mut commands,
                    &mut registry,
                    &states,
                    &params,
                    &save_path,
                    &MapInstanceId::Overworld,
                );
                assert!(matches!(preparation, MapPreparation::Pending));
            },
        )
        .unwrap();
}

#[test]
fn map_transition_blocked_map_preparation_preserves_rejection() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.init_resource::<MapRegistry>();
    app.init_resource::<WorldSavePath>();
    let map_entity = app
        .world_mut()
        .spawn((
            MapInstanceId::Overworld,
            MapLoadState::Blocked(nostr_map_persistence::MapPersistenceRejection::Filesystem(
                "bad meta".to_string(),
            )),
        ))
        .id();
    app.world_mut()
        .resource_mut::<MapRegistry>()
        .insert(MapInstanceId::Overworld, map_entity);

    app.world_mut()
        .run_system_once(
            |mut commands: Commands,
             mut registry: ResMut<MapRegistry>,
             states: Query<Ref<MapLoadState>>,
             params: Query<(&VoxelMapConfig, &MapDimensions)>,
             save_path: Res<WorldSavePath>| {
                let preparation = ensure_map_exists(
                    &mut commands,
                    &mut registry,
                    &states,
                    &params,
                    &save_path,
                    &MapInstanceId::Overworld,
                );
                assert!(matches!(preparation, MapPreparation::Blocked(_)));
            },
        )
        .unwrap();
}

#[test]
fn map_transition_pending_switch_preflight_marker_does_not_start_transition() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    let entity = app
        .world_mut()
        .spawn(PendingMapSwitchPreflight {
            target_map_id: MapInstanceId::Homebase { owner: owner(7) },
            requested_at: 1.0,
        })
        .id();
    app.update();
    assert!(app
        .world()
        .get::<PendingMapSwitchPreflight>(entity)
        .is_some());
    assert!(app.world().get::<PendingTransition>(entity).is_none());
    assert!(app.world().get::<ChunkTicket>(entity).is_none());
}

#[test]
fn room_registry_creates_separate_rooms_for_different_maps() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(RoomPlugin);
    app.init_resource::<RoomRegistry>();

    app.world_mut()
        .run_system_once(
            |mut registry: ResMut<RoomRegistry>, mut commands: Commands| {
                let ow = registry.get_or_create(&MapInstanceId::Overworld, &mut commands);
                let hb = registry
                    .get_or_create(&MapInstanceId::Homebase { owner: owner(42) }, &mut commands);
                assert_ne!(ow, hb, "Different maps should have different rooms");

                let ow2 = registry.get_or_create(&MapInstanceId::Overworld, &mut commands);
                assert_eq!(ow, ow2, "Same map should return same room");
            },
        )
        .unwrap();
}

#[test]
fn room_transfer_moves_entity_between_rooms() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(RoomPlugin);

    let room_a = app.world_mut().spawn(Room::default()).id();
    let room_b = app.world_mut().spawn(Room::default()).id();
    let entity = app.world_mut().spawn_empty().id();

    app.world_mut().trigger(RoomEvent {
        room: room_a,
        target: RoomTarget::AddEntity(entity),
    });
    app.update();

    assert!(
        app.world()
            .get::<Room>(room_a)
            .unwrap()
            .entities
            .contains(&entity),
        "Entity should be in room A initially"
    );

    // Same-frame transfer
    app.world_mut().trigger(RoomEvent {
        room: room_a,
        target: RoomTarget::RemoveEntity(entity),
    });
    app.world_mut().trigger(RoomEvent {
        room: room_b,
        target: RoomTarget::AddEntity(entity),
    });
    app.update();

    assert!(
        !app.world()
            .get::<Room>(room_a)
            .unwrap()
            .entities
            .contains(&entity),
        "Entity should leave old room"
    );
    assert!(
        app.world()
            .get::<Room>(room_b)
            .unwrap()
            .entities
            .contains(&entity),
        "Entity should be in new room"
    );
}

#[test]
fn pending_transition_marker_can_be_added_and_removed() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);

    let entity = app
        .world_mut()
        .spawn(PendingTransition(MapInstanceId::Overworld))
        .id();
    app.update();
    assert!(app.world().get::<PendingTransition>(entity).is_some());

    app.world_mut()
        .entity_mut(entity)
        .remove::<PendingTransition>();
    app.update();
    assert!(app.world().get::<PendingTransition>(entity).is_none());
}

#[test]
fn different_homebase_owners_produce_distinct_map_ids() {
    assert_ne!(
        MapInstanceId::Homebase { owner: owner(111) },
        MapInstanceId::Homebase { owner: owner(222) },
        "Different owners must produce different homebase map ids"
    );
}
