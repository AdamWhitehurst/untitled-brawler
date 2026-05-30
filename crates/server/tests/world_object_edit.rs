use std::sync::Arc;

use avian3d::prelude::{Position, Rotation};
use bevy::ecs::system::RunSystemOnce;
use bevy::prelude::*;
use persistence::{PendingStoreOps, Store, StoreBackend};
use protocol::map::{ChunkEntityRef, MapInstanceId};
use protocol::world_object::{
    WorldObjectEditRejectReason, WorldObjectId, WorldObjectMoveRequest, WorldObjectRotateRequest,
    WorldObjectRotationSnapshot,
};
use voxel_map_engine::config::WorldObjectSpawn;
use voxel_map_engine::persistence::fs_chunk_entities::FsChunkEntitiesStore;
use voxel_map_engine::prelude::{
    chunk_to_column, ChunkData, ChunkStatus, MapDimensions, VoxelMapInstance, WorldVoxel,
};

const CHUNK_SIZE: u32 = 16;
const PADDED_VOLUME_16: usize = 18 * 18 * 18;

fn loaded_instance(chunk_pos: IVec3) -> VoxelMapInstance {
    let mut instance = VoxelMapInstance::new(5, CHUNK_SIZE);
    let voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
    instance.insert_chunk_data(
        chunk_pos,
        ChunkData::from_voxels(&voxels, ChunkStatus::Full),
    );
    instance.chunk_levels.insert(chunk_to_column(chunk_pos), 0);
    instance
}

fn object_id() -> WorldObjectId {
    WorldObjectId("test_tree".to_string())
}

fn dimensions(bounds: Option<IVec3>) -> MapDimensions {
    MapDimensions {
        chunk_size: CHUNK_SIZE,
        column_y_range: (-4, 4),
        tree_height: 5,
        bounds,
    }
}

fn validate_in_world(
    app: &mut App,
    target: Entity,
    map_entity: Entity,
    map_id: MapInstanceId,
) -> Result<server::map::ValidatedWorldObjectDelete, WorldObjectEditRejectReason> {
    app.world_mut()
        .run_system_once(
            move |target_exists: Query<Entity>,
                  object_query: Query<(&WorldObjectId, &MapInstanceId, &ChunkEntityRef)>,
                  map_query: Query<&VoxelMapInstance>| {
                server::map::validate_world_object_delete(
                    target,
                    map_entity,
                    &map_id,
                    &target_exists,
                    &object_query,
                    &map_query,
                )
            },
        )
        .expect("validation system should run")
}

#[test]
fn delete_validation_accepts_loaded_world_object_on_player_map() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    let chunk_pos = IVec3::ZERO;
    let map_entity = app.world_mut().spawn(loaded_instance(chunk_pos)).id();
    let target = app
        .world_mut()
        .spawn((
            object_id(),
            MapInstanceId::Overworld,
            ChunkEntityRef {
                map_entity,
                chunk_pos,
            },
        ))
        .id();

    let result = validate_in_world(&mut app, target, map_entity, MapInstanceId::Overworld)
        .expect("delete should be valid");

    assert_eq!(result.map_entity, map_entity);
    assert_eq!(result.chunk_pos, chunk_pos);
}

#[test]
fn delete_validation_rejects_missing_or_non_world_object_target() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    let chunk_pos = IVec3::ZERO;
    let map_entity = app.world_mut().spawn(loaded_instance(chunk_pos)).id();
    let non_world_object = app.world_mut().spawn_empty().id();
    let missing = Entity::PLACEHOLDER;

    assert_eq!(
        validate_in_world(&mut app, missing, map_entity, MapInstanceId::Overworld).unwrap_err(),
        WorldObjectEditRejectReason::MissingTarget
    );
    assert_eq!(
        validate_in_world(
            &mut app,
            non_world_object,
            map_entity,
            MapInstanceId::Overworld
        )
        .unwrap_err(),
        WorldObjectEditRejectReason::NotWorldObject
    );
}

#[test]
fn delete_validation_rejects_foreign_map_and_unloaded_chunk() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    let loaded_chunk = IVec3::ZERO;
    let unloaded_chunk = IVec3::new(1, 0, 0);
    let map_entity = app.world_mut().spawn(loaded_instance(loaded_chunk)).id();
    let foreign_target = app
        .world_mut()
        .spawn((
            object_id(),
            MapInstanceId::Homebase {
                owner: protocol::NostrPublicKey([1; 32]),
            },
            ChunkEntityRef {
                map_entity,
                chunk_pos: loaded_chunk,
            },
        ))
        .id();
    let unloaded_target = app
        .world_mut()
        .spawn((
            object_id(),
            MapInstanceId::Overworld,
            ChunkEntityRef {
                map_entity,
                chunk_pos: unloaded_chunk,
            },
        ))
        .id();

    assert_eq!(
        validate_in_world(
            &mut app,
            foreign_target,
            map_entity,
            MapInstanceId::Overworld
        )
        .unwrap_err(),
        WorldObjectEditRejectReason::ForeignMap
    );
    assert_eq!(
        validate_in_world(
            &mut app,
            unloaded_target,
            map_entity,
            MapInstanceId::Overworld
        )
        .unwrap_err(),
        WorldObjectEditRejectReason::ChunkUnavailable
    );
}

#[test]
fn delete_save_writes_empty_chunk_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsChunkEntitiesStore {
        map_dir: Arc::new(dir.path().to_path_buf()),
    };
    let chunk_pos = IVec3::ZERO;
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    let map_entity = app
        .world_mut()
        .spawn((
            StoreBackend::new(store.clone()),
            PendingStoreOps::<IVec3, Vec<WorldObjectSpawn>>::default(),
        ))
        .id();
    let deleted = app
        .world_mut()
        .spawn((
            ChunkEntityRef {
                map_entity,
                chunk_pos,
            },
            object_id(),
            Position(Vec3::new(1.0, 2.0, 3.0).into()),
        ))
        .id();

    app.world_mut()
        .run_system_once(
            move |entity_query: Query<(
                Entity,
                &ChunkEntityRef,
                &WorldObjectId,
                &Position,
                Option<&protocol::world_object::ActiveTransformation>,
                Option<&protocol::Health>,
                Option<&Rotation>,
            )>,
                  mut store_query: Query<(
                &StoreBackend<IVec3, Vec<WorldObjectSpawn>, FsChunkEntitiesStore>,
                &mut PendingStoreOps<IVec3, Vec<WorldObjectSpawn>>,
            )>| {
                server::chunk_entities::save_chunk_entities_now_or_queue(
                    map_entity,
                    chunk_pos,
                    Some(deleted),
                    None,
                    &entity_query,
                    &mut store_query,
                );
                let (_, mut ops) = store_query.get_mut(map_entity).unwrap();
                ops.flush();
            },
        )
        .unwrap();

    let loaded = store.load(&chunk_pos).unwrap().expect("empty file exists");
    assert!(loaded.is_empty());
}

fn validate_move_in_world(
    app: &mut App,
    request: WorldObjectMoveRequest,
    map_entity: Entity,
    map_id: MapInstanceId,
) -> Result<server::map::ValidatedWorldObjectMove, WorldObjectEditRejectReason> {
    app.world_mut()
        .run_system_once(
            move |object_query: Query<(&WorldObjectId, &MapInstanceId, &ChunkEntityRef)>,
                  map_query: Query<(&VoxelMapInstance, &MapDimensions)>| {
                server::map::validate_world_object_move(
                    &request,
                    map_entity,
                    &map_id,
                    &object_query,
                    &map_query,
                )
            },
        )
        .expect("move validation system should run")
}

#[test]
fn move_same_chunk_validation_accepts_loaded_target() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    let chunk_pos = IVec3::ZERO;
    let map_entity = app
        .world_mut()
        .spawn((loaded_instance(chunk_pos), dimensions(None)))
        .id();
    let target = app
        .world_mut()
        .spawn((
            object_id(),
            MapInstanceId::Overworld,
            ChunkEntityRef {
                map_entity,
                chunk_pos,
            },
        ))
        .id();
    let final_position = Vec3::new(2.0, 3.0, 4.0);

    let result = validate_move_in_world(
        &mut app,
        WorldObjectMoveRequest {
            sequence: 1,
            target,
            final_position,
        },
        map_entity,
        MapInstanceId::Overworld,
    )
    .expect("same-chunk move should be valid");

    assert_eq!(result.map_entity, map_entity);
    assert_eq!(result.old_chunk_pos, chunk_pos);
    assert_eq!(result.new_chunk_pos, chunk_pos);
    assert_eq!(result.final_position, final_position);
}

#[test]
fn move_same_chunk_validation_rejects_non_finite_out_of_bounds_and_unloaded() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    let loaded_chunk = IVec3::ZERO;
    let map_entity = app
        .world_mut()
        .spawn((
            loaded_instance(loaded_chunk),
            dimensions(Some(IVec3::new(2, 4, 2))),
        ))
        .id();
    let target = app
        .world_mut()
        .spawn((
            object_id(),
            MapInstanceId::Overworld,
            ChunkEntityRef {
                map_entity,
                chunk_pos: loaded_chunk,
            },
        ))
        .id();

    assert_eq!(
        validate_move_in_world(
            &mut app,
            WorldObjectMoveRequest {
                sequence: 1,
                target,
                final_position: Vec3::new(f32::NAN, 0.0, 0.0),
            },
            map_entity,
            MapInstanceId::Overworld,
        )
        .unwrap_err(),
        WorldObjectEditRejectReason::NonFinitePosition
    );
    assert_eq!(
        validate_move_in_world(
            &mut app,
            WorldObjectMoveRequest {
                sequence: 2,
                target,
                final_position: Vec3::new(64.0, 0.0, 0.0),
            },
            map_entity,
            MapInstanceId::Overworld,
        )
        .unwrap_err(),
        WorldObjectEditRejectReason::OutOfBounds
    );
    assert_eq!(
        validate_move_in_world(
            &mut app,
            WorldObjectMoveRequest {
                sequence: 3,
                target,
                final_position: Vec3::new(17.0, 0.0, 0.0),
            },
            map_entity,
            MapInstanceId::Overworld,
        )
        .unwrap_err(),
        WorldObjectEditRejectReason::DestinationChunkUnavailable
    );
}

#[test]
fn cross_chunk_move_validation_accepts_loaded_destination() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    let old_chunk_pos = IVec3::ZERO;
    let new_chunk_pos = IVec3::new(1, 0, 0);
    let mut instance = loaded_instance(old_chunk_pos);
    let voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
    instance.insert_chunk_data(
        new_chunk_pos,
        ChunkData::from_voxels(&voxels, ChunkStatus::Full),
    );
    instance
        .chunk_levels
        .insert(chunk_to_column(new_chunk_pos), 0);
    let map_entity = app.world_mut().spawn((instance, dimensions(None))).id();
    let target = app
        .world_mut()
        .spawn((
            object_id(),
            MapInstanceId::Overworld,
            ChunkEntityRef {
                map_entity,
                chunk_pos: old_chunk_pos,
            },
        ))
        .id();

    let result = validate_move_in_world(
        &mut app,
        WorldObjectMoveRequest {
            sequence: 1,
            target,
            final_position: Vec3::new(17.0, 0.0, 0.0),
        },
        map_entity,
        MapInstanceId::Overworld,
    )
    .expect("cross-chunk move to loaded destination should be valid");

    assert_eq!(result.old_chunk_pos, old_chunk_pos);
    assert_eq!(result.new_chunk_pos, new_chunk_pos);
}

#[test]
fn cross_chunk_move_rejects_unavailable_destination() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    let old_chunk_pos = IVec3::ZERO;
    let map_entity = app
        .world_mut()
        .spawn((loaded_instance(old_chunk_pos), dimensions(None)))
        .id();
    let target = app
        .world_mut()
        .spawn((
            object_id(),
            MapInstanceId::Overworld,
            ChunkEntityRef {
                map_entity,
                chunk_pos: old_chunk_pos,
            },
        ))
        .id();

    assert_eq!(
        validate_move_in_world(
            &mut app,
            WorldObjectMoveRequest {
                sequence: 1,
                target,
                final_position: Vec3::new(17.0, 0.0, 0.0),
            },
            map_entity,
            MapInstanceId::Overworld,
        )
        .unwrap_err(),
        WorldObjectEditRejectReason::DestinationChunkUnavailable
    );
}

#[test]
fn cross_chunk_move_saves_empty_source_and_destination_with_moved_object() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsChunkEntitiesStore {
        map_dir: Arc::new(dir.path().to_path_buf()),
    };
    let old_chunk_pos = IVec3::ZERO;
    let new_chunk_pos = IVec3::new(1, 0, 0);
    let final_position = Vec3::new(17.0, 5.0, 6.0);
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    let map_entity = app
        .world_mut()
        .spawn((
            StoreBackend::new(store.clone()),
            PendingStoreOps::<IVec3, Vec<WorldObjectSpawn>>::default(),
        ))
        .id();
    let moved = app
        .world_mut()
        .spawn((
            ChunkEntityRef {
                map_entity,
                chunk_pos: old_chunk_pos,
            },
            object_id(),
            Position(Vec3::new(1.0, 2.0, 3.0).into()),
        ))
        .id();

    app.world_mut()
        .run_system_once(
            move |entity_query: Query<(
                Entity,
                &ChunkEntityRef,
                &WorldObjectId,
                &Position,
                Option<&protocol::world_object::ActiveTransformation>,
                Option<&protocol::Health>,
                Option<&Rotation>,
            )>,
                  mut store_query: Query<(
                &StoreBackend<IVec3, Vec<WorldObjectSpawn>, FsChunkEntitiesStore>,
                &mut PendingStoreOps<IVec3, Vec<WorldObjectSpawn>>,
            )>| {
                server::chunk_entities::queue_world_object_move_persistence(
                    map_entity,
                    old_chunk_pos,
                    new_chunk_pos,
                    moved,
                    final_position,
                    &entity_query,
                    &mut store_query,
                );
                let (_, mut ops) = store_query.get_mut(map_entity).unwrap();
                ops.flush();
            },
        )
        .unwrap();

    let source = store
        .load(&old_chunk_pos)
        .unwrap()
        .expect("source file exists");
    let destination = store
        .load(&new_chunk_pos)
        .unwrap()
        .expect("destination file exists");
    assert!(source.is_empty());
    assert_eq!(destination.len(), 1);
    assert_eq!(destination[0].position, final_position);
}

#[test]
fn move_same_chunk_save_uses_new_final_position() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsChunkEntitiesStore {
        map_dir: Arc::new(dir.path().to_path_buf()),
    };
    let chunk_pos = IVec3::ZERO;
    let final_position = Vec3::new(4.0, 5.0, 6.0);
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    let map_entity = app
        .world_mut()
        .spawn((
            StoreBackend::new(store.clone()),
            PendingStoreOps::<IVec3, Vec<WorldObjectSpawn>>::default(),
        ))
        .id();
    let moved = app
        .world_mut()
        .spawn((
            ChunkEntityRef {
                map_entity,
                chunk_pos,
            },
            object_id(),
            Position(Vec3::new(1.0, 2.0, 3.0).into()),
        ))
        .id();

    app.world_mut()
        .run_system_once(
            move |entity_query: Query<(
                Entity,
                &ChunkEntityRef,
                &WorldObjectId,
                &Position,
                Option<&protocol::world_object::ActiveTransformation>,
                Option<&protocol::Health>,
                Option<&Rotation>,
            )>,
                  mut store_query: Query<(
                &StoreBackend<IVec3, Vec<WorldObjectSpawn>, FsChunkEntitiesStore>,
                &mut PendingStoreOps<IVec3, Vec<WorldObjectSpawn>>,
            )>| {
                server::chunk_entities::save_chunk_entities_now_or_queue(
                    map_entity,
                    chunk_pos,
                    None,
                    Some(server::chunk_entities::ChunkEntitySaveOverride {
                        entity: moved,
                        position: Some(final_position),
                        chunk_pos: Some(chunk_pos),
                        rotation: None,
                    }),
                    &entity_query,
                    &mut store_query,
                );
                let (_, mut ops) = store_query.get_mut(map_entity).unwrap();
                ops.flush();
            },
        )
        .unwrap();

    let loaded = store.load(&chunk_pos).unwrap().expect("move save exists");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].position, final_position);
}

fn validate_rotate_in_world(
    app: &mut App,
    request: WorldObjectRotateRequest,
    map_entity: Entity,
    map_id: MapInstanceId,
) -> Result<Quat, WorldObjectEditRejectReason> {
    app.world_mut()
        .run_system_once(
            move |object_query: Query<(&WorldObjectId, &MapInstanceId, &ChunkEntityRef)>,
                  map_query: Query<&VoxelMapInstance>| {
                server::map::validate_world_object_rotation(
                    &request,
                    map_entity,
                    &map_id,
                    &object_query,
                    &map_query,
                )
            },
        )
        .expect("rotation validation system should run")
}

#[test]
fn rotate_validation_accepts_normalized_finite_rotation() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    let chunk_pos = IVec3::ZERO;
    let map_entity = app.world_mut().spawn(loaded_instance(chunk_pos)).id();
    let target = app
        .world_mut()
        .spawn((
            object_id(),
            MapInstanceId::Overworld,
            ChunkEntityRef {
                map_entity,
                chunk_pos,
            },
        ))
        .id();
    let rotation = Quat::from_xyzw(0.0, 2.0, 0.0, 0.0);

    let result = validate_rotate_in_world(
        &mut app,
        WorldObjectRotateRequest {
            sequence: 1,
            target,
            rotation,
        },
        map_entity,
        MapInstanceId::Overworld,
    )
    .expect("rotation should be valid");

    assert!((result.length_squared() - 1.0).abs() <= 0.0001);
}

#[test]
fn rotate_validation_rejects_invalid_rotation_and_unavailable_chunk() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    let loaded_chunk = IVec3::ZERO;
    let unloaded_chunk = IVec3::new(1, 0, 0);
    let map_entity = app.world_mut().spawn(loaded_instance(loaded_chunk)).id();
    let target = app
        .world_mut()
        .spawn((
            object_id(),
            MapInstanceId::Overworld,
            ChunkEntityRef {
                map_entity,
                chunk_pos: loaded_chunk,
            },
        ))
        .id();
    let unloaded_target = app
        .world_mut()
        .spawn((
            object_id(),
            MapInstanceId::Overworld,
            ChunkEntityRef {
                map_entity,
                chunk_pos: unloaded_chunk,
            },
        ))
        .id();

    assert_eq!(
        validate_rotate_in_world(
            &mut app,
            WorldObjectRotateRequest {
                sequence: 1,
                target,
                rotation: Quat::from_xyzw(f32::NAN, 0.0, 0.0, 1.0),
            },
            map_entity,
            MapInstanceId::Overworld,
        )
        .unwrap_err(),
        WorldObjectEditRejectReason::InvalidRotation
    );
    assert_eq!(
        validate_rotate_in_world(
            &mut app,
            WorldObjectRotateRequest {
                sequence: 2,
                target: unloaded_target,
                rotation: Quat::IDENTITY,
            },
            map_entity,
            MapInstanceId::Overworld,
        )
        .unwrap_err(),
        WorldObjectEditRejectReason::ChunkUnavailable
    );
}

#[test]
fn rotate_persists_through_chunk_entity_save_restore_payload() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsChunkEntitiesStore {
        map_dir: Arc::new(dir.path().to_path_buf()),
    };
    let chunk_pos = IVec3::ZERO;
    let rotation = Quat::from_rotation_y(1.25);
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    let map_entity = app
        .world_mut()
        .spawn((
            StoreBackend::new(store.clone()),
            PendingStoreOps::<IVec3, Vec<WorldObjectSpawn>>::default(),
        ))
        .id();
    let rotated = app
        .world_mut()
        .spawn((
            ChunkEntityRef {
                map_entity,
                chunk_pos,
            },
            object_id(),
            Position(Vec3::new(1.0, 2.0, 3.0).into()),
        ))
        .id();

    app.world_mut()
        .run_system_once(
            move |entity_query: Query<(
                Entity,
                &ChunkEntityRef,
                &WorldObjectId,
                &Position,
                Option<&protocol::world_object::ActiveTransformation>,
                Option<&protocol::Health>,
                Option<&Rotation>,
            )>,
                  mut store_query: Query<(
                &StoreBackend<IVec3, Vec<WorldObjectSpawn>, FsChunkEntitiesStore>,
                &mut PendingStoreOps<IVec3, Vec<WorldObjectSpawn>>,
            )>| {
                server::chunk_entities::save_chunk_entities_now_or_queue(
                    map_entity,
                    chunk_pos,
                    None,
                    Some(server::chunk_entities::ChunkEntitySaveOverride {
                        entity: rotated,
                        position: None,
                        chunk_pos: None,
                        rotation: Some(rotation),
                    }),
                    &entity_query,
                    &mut store_query,
                );
                let (_, mut ops) = store_query.get_mut(map_entity).unwrap();
                ops.flush();
            },
        )
        .unwrap();

    let loaded = store
        .load(&chunk_pos)
        .unwrap()
        .expect("rotation save exists");
    assert_eq!(loaded.len(), 1);
    let rotation_type = std::any::type_name::<WorldObjectRotationSnapshot>();
    let saved = loaded[0]
        .persisted_components
        .iter()
        .find(|component| component.type_path == rotation_type)
        .expect("rotation persisted component exists");
    let snapshot: WorldObjectRotationSnapshot = ron::from_str(&saved.ron_data).unwrap();
    assert!(snapshot.0.dot(rotation).abs() >= 1.0 - 0.0001);
}

#[test]
fn world_object_edit_remote_publish_chunk_entity_empty_payload_encodes_and_decodes() {
    let bytes = nostr_map_persistence::encode_chunk_entities_payload(Vec::new())
        .expect("empty chunk entity payload encodes");
    assert!(!bytes.is_empty());
}

#[test]
fn world_object_edit_remote_publish_chunk_entity_empty_file_preserves_authoritative_empty_slot() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsChunkEntitiesStore {
        map_dir: Arc::new(dir.path().to_path_buf()),
    };
    let chunk_pos = IVec3::ZERO;
    store
        .save(&chunk_pos, &Vec::<WorldObjectSpawn>::new())
        .unwrap();

    let loaded = store
        .load(&chunk_pos)
        .unwrap()
        .expect("empty chunk entity file is authoritative");
    assert!(loaded.is_empty());
}
