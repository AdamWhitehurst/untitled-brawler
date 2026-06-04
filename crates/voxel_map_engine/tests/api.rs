use std::sync::Arc;

use bevy::ecs::system::RunSystemOnce;
use bevy::prelude::*;
use ndshape::Shape;
use voxel_map_engine::prelude::*;

fn test_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(bevy::transform::TransformPlugin);
    app.init_resource::<Assets<Mesh>>();
    app.init_resource::<Assets<StandardMaterial>>();
    app.add_plugins(VoxelPlugin);
    app.insert_resource(ChunkGenerationEnabled);
    app
}

fn spawn_map(app: &mut App, spawning_distance: u32) -> Entity {
    spawn_map_with(
        app,
        spawning_distance,
        VoxelGenerator(Arc::new(FlatGenerator {
            chunk_size: 16,
            shape: RuntimeShape::<u32, 3>::new([18, 18, 18]),
        })),
    )
}

fn spawn_map_with(app: &mut App, spawning_distance: u32, generator: VoxelGenerator) -> Entity {
    app.world_mut()
        .spawn((
            VoxelMapInstance::new(5, 16),
            VoxelMapConfig::new(0, 0, spawning_distance, true),
            MapDimensions {
                chunk_size: 16,
                column_y_range: (-8, 8),
                tree_height: 5,
                bounds: None,
            },
            generator,
            Transform::default(),
        ))
        .id()
}

fn spawn_ticket(app: &mut App, map_entity: Entity, position: Vec3, distance: u32) -> Entity {
    app.world_mut()
        .spawn((
            ChunkTicket::new(map_entity, TicketType::Player, distance),
            Transform::from_translation(position),
            GlobalTransform::default(),
        ))
        .id()
}

fn tick(app: &mut App, n: usize) {
    for _ in 0..n {
        app.update();
    }
}

const MAX_TICKS: usize = 200;

fn tick_until(app: &mut App, condition: impl Fn(&App) -> bool) {
    for _ in 0..MAX_TICKS {
        app.update();
        if condition(app) {
            return;
        }
    }
    panic!("condition not met after {MAX_TICKS} ticks");
}

/// Test: set_voxel mutates octree directly, get_voxel returns the written value.
#[test]
fn set_get_voxel_round_trip() {
    let mut app = test_app();
    let map = spawn_map(&mut app, 1);
    spawn_ticket(&mut app, map, Vec3::ZERO, 0);

    // Wait for chunk at origin to load
    tick_until(&mut app, |app| has_loaded_chunk(app, map, IVec3::ZERO));

    // Use VoxelWorld::set_voxel via a one-shot system
    let edit_pos = IVec3::new(3, 5, 7);
    app.world_mut()
        .run_system_once(move |mut vw: VoxelWorld| {
            vw.set_voxel(map, edit_pos, WorldVoxel::Solid(42));
        })
        .unwrap();

    // Verify the edit is immediately visible in dirty_chunks
    let instance = app.world().get::<VoxelMapInstance>(map).unwrap();
    let chunk_pos = IVec3::ZERO; // edit_pos (3,5,7) is in chunk (0,0,0)
    assert!(instance.dirty_chunks.contains(&chunk_pos));
    assert!(instance.chunks_needing_remesh.contains(&chunk_pos));

    // Verify get_voxel returns the written value immediately (no flush needed)
    app.world_mut()
        .run_system_once(move |vw: VoxelWorld| {
            let voxel = vw.get_voxel(map, edit_pos);
            assert_eq!(voxel, WorldVoxel::Solid(42));
        })
        .unwrap();
}

/// Test: get_voxel falls back to SDF for unmodified positions.
#[test]
fn get_voxel_reads_sdf_for_unmodified() {
    let mut app = test_app();
    let map = spawn_map(&mut app, 1);
    tick(&mut app, 1);

    app.world_mut()
        .run_system_once(move |vw: VoxelWorld| {
            // Flat terrain: y <= 0 is solid, y > 0 is air
            let below = vw.get_voxel(map, IVec3::new(0, -1, 0));
            assert_eq!(below, WorldVoxel::Solid(0));

            let above = vw.get_voxel(map, IVec3::new(0, 1, 0));
            assert_eq!(above, WorldVoxel::Air);
        })
        .unwrap();
}

fn has_loaded_chunk(app: &App, map: Entity, pos: IVec3) -> bool {
    app.world()
        .get::<VoxelMapInstance>(map)
        .unwrap()
        .get_chunk_data(pos)
        .is_some()
}

/// Test: set_voxel marks the chunk for remesh, and the remesh system processes it.
#[test]
fn set_voxel_triggers_remesh() {
    let mut app = test_app();
    let map = spawn_map(&mut app, 1);
    spawn_ticket(&mut app, map, Vec3::ZERO, 0);

    // Wait for chunk to reach Full status (all pipeline stages complete)
    tick_until(&mut app, |app| {
        app.world()
            .get::<VoxelMapInstance>(map)
            .unwrap()
            .get_chunk_data(IVec3::ZERO)
            .is_some_and(|c| c.status == ChunkStatus::Full)
    });

    // Write a voxel inside the origin chunk
    let edit_pos = IVec3::new(8, 8, 8); // center of chunk (0,0,0)
    app.world_mut()
        .get_mut::<VoxelMapInstance>(map)
        .unwrap()
        .set_voxel(edit_pos, WorldVoxel::Solid(1));

    // Verify chunk is queued for remesh
    let instance = app.world().get::<VoxelMapInstance>(map).unwrap();
    assert!(
        instance.chunks_needing_remesh.contains(&IVec3::ZERO),
        "edited chunk should be queued for remesh"
    );

    // Tick to process remesh
    tick(&mut app, 3);

    // After remesh, chunks_needing_remesh should be drained
    let instance = app.world().get::<VoxelMapInstance>(map).unwrap();
    assert!(
        instance.chunks_needing_remesh.is_empty(),
        "remesh queue should be drained after processing"
    );

    // Chunk should still be loaded (not invalidated)
    assert!(
        instance
            .chunk_levels
            .contains_key(&chunk_to_column(IVec3::ZERO)),
        "chunk should remain loaded after in-place edit"
    );
}

/// Test: edited voxel data persists in the octree after edit (no chunk cycle needed).
#[test]
fn edited_voxel_persists_in_octree() {
    let mut app = test_app();
    let map = spawn_map(&mut app, 1);
    spawn_ticket(&mut app, map, Vec3::ZERO, 0);

    tick_until(&mut app, |app| has_loaded_chunk(app, map, IVec3::ZERO));

    let edit_pos = IVec3::new(8, 8, 8);
    app.world_mut()
        .get_mut::<VoxelMapInstance>(map)
        .unwrap()
        .set_voxel(edit_pos, WorldVoxel::Solid(99));

    // Verify the edit is readable from the octree
    app.world_mut()
        .run_system_once(move |vw: VoxelWorld| {
            let voxel = vw.get_voxel(map, edit_pos);
            assert_eq!(voxel, WorldVoxel::Solid(99));
        })
        .unwrap();
}

/// Test: raycast hits flat terrain from above.
#[test]
fn raycast_hits_flat_terrain() {
    let mut app = test_app();
    let map = spawn_map(&mut app, 1);
    tick(&mut app, 1);

    app.world_mut()
        .run_system_once(move |vw: VoxelWorld| {
            // Cast ray straight down from y=10 toward y=-10
            let ray = Ray3d::new(Vec3::new(0.5, 10.0, 0.5), Dir3::NEG_Y);
            let result = vw.raycast(map, ray, 50.0, |v| matches!(v, WorldVoxel::Solid(_)));
            let hit = result.expect("should hit flat terrain");

            // Flat terrain is solid at y <= 0, so first hit should be at y = 0
            assert_eq!(hit.position.y, 0, "should hit at y=0 (first solid)");
            assert_eq!(hit.voxel, WorldVoxel::Solid(0));
            assert_eq!(hit.normal, Some(Vec3::Y), "should enter from top face");
        })
        .unwrap();
}

/// Test: raycast misses when pointed at empty space.
#[test]
fn raycast_misses_empty_space() {
    let mut app = test_app();
    let map = spawn_map(&mut app, 1);
    tick(&mut app, 1);

    app.world_mut()
        .run_system_once(move |vw: VoxelWorld| {
            // Cast ray horizontally through air (y=10, well above terrain)
            let ray = Ray3d::new(Vec3::new(0.5, 10.0, 0.5), Dir3::X);
            let result = vw.raycast(map, ray, 20.0, |v| matches!(v, WorldVoxel::Solid(_)));
            assert!(result.is_none(), "should not hit anything above terrain");
        })
        .unwrap();
}

/// Test: get_voxel returns independent data per map instance.
#[test]
fn get_voxel_independent_between_instances() {
    let mut app = test_app();
    let map_a = spawn_map(&mut app, 1);
    let map_b = spawn_map(&mut app, 1);
    spawn_ticket(&mut app, map_a, Vec3::ZERO, 0);
    spawn_ticket(&mut app, map_b, Vec3::ZERO, 0);

    // Wait for both maps to load the origin chunk
    tick_until(&mut app, |app| {
        has_loaded_chunk(app, map_a, IVec3::ZERO) && has_loaded_chunk(app, map_b, IVec3::ZERO)
    });

    let edit_pos = IVec3::new(3, 5, 7);
    app.world_mut()
        .run_system_once(move |mut vw: VoxelWorld| {
            vw.set_voxel(map_a, edit_pos, WorldVoxel::Solid(42));
        })
        .unwrap();

    app.world_mut()
        .run_system_once(move |vw: VoxelWorld| {
            assert_eq!(
                vw.get_voxel(map_a, edit_pos),
                WorldVoxel::Solid(42),
                "map_a should have the written voxel"
            );
            assert_eq!(
                vw.get_voxel(map_b, edit_pos),
                WorldVoxel::Air,
                "map_b should be unaffected (y=5 is air in flat terrain)"
            );
        })
        .unwrap();
}

/// Test: set_voxel on one instance does not modify another instance's state.
#[test]
fn set_voxel_isolated_between_instances() {
    let mut app = test_app();
    let map_a = spawn_map(&mut app, 1);
    let map_b = spawn_map(&mut app, 1);
    spawn_ticket(&mut app, map_a, Vec3::ZERO, 0);
    spawn_ticket(&mut app, map_b, Vec3::ZERO, 0);

    // Wait for both maps to load the origin chunk
    tick_until(&mut app, |app| {
        has_loaded_chunk(app, map_a, IVec3::ZERO) && has_loaded_chunk(app, map_b, IVec3::ZERO)
    });

    // Snapshot map_a's dirty set before editing map_b.
    // Generation may have already dirtied chunks, so we compare before/after.
    let dirty_a_before = app
        .world()
        .get::<VoxelMapInstance>(map_a)
        .unwrap()
        .dirty_chunks
        .clone();

    let edit_pos = IVec3::new(2, 3, 4);
    app.world_mut()
        .run_system_once(move |mut vw: VoxelWorld| {
            vw.set_voxel(map_b, edit_pos, WorldVoxel::Solid(7));
        })
        .unwrap();

    let dirty_a_after = &app
        .world()
        .get::<VoxelMapInstance>(map_a)
        .unwrap()
        .dirty_chunks;
    assert_eq!(
        dirty_a_before, *dirty_a_after,
        "map_a dirty set should not change after editing map_b"
    );

    let instance_b = app.world().get::<VoxelMapInstance>(map_b).unwrap();
    assert!(
        instance_b.dirty_chunks.contains(&IVec3::ZERO),
        "map_b should have the edited chunk marked dirty"
    );

    // Verify the voxel is actually in map_b's octree
    app.world_mut()
        .run_system_once(move |vw: VoxelWorld| {
            assert_eq!(
                vw.get_voxel(map_b, edit_pos),
                WorldVoxel::Solid(7),
                "map_b should contain the edit"
            );
        })
        .unwrap();
}

/// Test: raycast on one map does not see another map's voxels.
#[test]
fn raycast_isolated_between_instances() {
    let mut app = test_app();
    let map_a = spawn_map(&mut app, 1);
    let map_b = spawn_map_with(&mut app, 1, VoxelGenerator(Arc::new(AirGenerator::new(16))));
    tick(&mut app, 1);

    app.world_mut()
        .run_system_once(move |vw: VoxelWorld| {
            let ray = Ray3d::new(Vec3::new(0.5, 10.0, 0.5), Dir3::NEG_Y);
            let filter = |v: WorldVoxel| matches!(v, WorldVoxel::Solid(_));

            let hit_a = vw.raycast(map_a, ray, 50.0, &filter);
            assert!(
                hit_a.is_some(),
                "map_a (flat terrain) should be hit by downward ray"
            );

            let hit_b = vw.raycast(map_b, ray, 50.0, &filter);
            assert!(
                hit_b.is_none(),
                "map_b (all air) should not be hit by any ray"
            );
        })
        .unwrap();
}

/// Test: set_voxels marks every touched loaded chunk dirty and queued for remesh.
#[test]
fn api_set_voxels_marks_each_touched_chunk_dirty_and_for_remesh() {
    let mut app = test_app();
    let map = spawn_map(&mut app, 1);
    spawn_ticket(&mut app, map, Vec3::ZERO, 1);

    tick_until(&mut app, |app| {
        has_loaded_chunk(app, map, IVec3::ZERO) && has_loaded_chunk(app, map, IVec3::X)
    });

    app.world_mut()
        .run_system_once(move |mut vw: VoxelWorld| {
            assert_eq!(
                vw.set_voxels(
                    map,
                    [
                        (IVec3::new(1, 2, 3), WorldVoxel::Solid(11)),
                        (IVec3::new(16, 2, 3), WorldVoxel::Solid(12)),
                    ],
                ),
                2
            );
        })
        .unwrap();

    let instance = app.world().get::<VoxelMapInstance>(map).unwrap();
    assert!(instance.dirty_chunks.contains(&IVec3::ZERO));
    assert!(instance.dirty_chunks.contains(&IVec3::X));
    assert!(instance.chunks_needing_remesh.contains(&IVec3::ZERO));
    assert!(instance.chunks_needing_remesh.contains(&IVec3::X));
}

/// Test: set_voxels updates neighbor padding and queues neighbor remesh at chunk boundaries.
#[test]
fn api_set_voxels_updates_boundary_padding_for_neighbor_chunks() {
    let mut app = test_app();
    let map = spawn_map(&mut app, 1);
    spawn_ticket(&mut app, map, Vec3::ZERO, 1);

    tick_until(&mut app, |app| {
        has_loaded_chunk(app, map, IVec3::ZERO) && has_loaded_chunk(app, map, IVec3::X)
    });

    app.world_mut()
        .run_system_once(move |mut vw: VoxelWorld| {
            vw.set_voxels(map, [(IVec3::new(15, 2, 3), WorldVoxel::Solid(77))]);
        })
        .unwrap();

    let instance = app.world().get::<VoxelMapInstance>(map).unwrap();
    assert!(instance.chunks_needing_remesh.contains(&IVec3::X));
    let neighbor = instance.get_chunk_data(IVec3::X).unwrap();
    let index = instance.shape.linearize([0, 3, 4]) as usize;
    assert_eq!(neighbor.voxels.get(index), WorldVoxel::Solid(77));
}

/// Test: set_voxels handles negative coordinates through euclidean chunk mapping.
#[test]
fn api_set_voxels_handles_negative_world_coordinates() {
    let mut app = test_app();
    let map = spawn_map(&mut app, 1);
    spawn_ticket(&mut app, map, Vec3::ZERO, 1);

    tick_until(&mut app, |app| has_loaded_chunk(app, map, -IVec3::X));

    app.world_mut()
        .run_system_once(move |mut vw: VoxelWorld| {
            assert_eq!(
                vw.set_voxels(map, [(IVec3::new(-1, 2, 3), WorldVoxel::Solid(88))]),
                1
            );
        })
        .unwrap();

    let instance = app.world().get::<VoxelMapInstance>(map).unwrap();
    assert!(instance.dirty_chunks.contains(&-IVec3::X));
    assert!(instance.chunks_needing_remesh.contains(&-IVec3::X));
}

#[test]
fn api_set_voxels_handles_negative_multi_edit_chunk_mapping() {
    let mut app = test_app();
    let map = spawn_map(&mut app, 1);
    spawn_ticket(&mut app, map, Vec3::ZERO, 1);

    tick_until(&mut app, |app| {
        has_loaded_chunk(app, map, -IVec3::X) && has_loaded_chunk(app, map, IVec3::new(-1, 0, -1))
    });

    app.world_mut()
        .run_system_once(move |mut vw: VoxelWorld| {
            assert_eq!(
                vw.set_voxels(
                    map,
                    [
                        (IVec3::new(-1, 2, 3), WorldVoxel::Solid(88)),
                        (IVec3::new(-1, 2, -1), WorldVoxel::Solid(89)),
                    ],
                ),
                2
            );
        })
        .unwrap();

    let instance = app.world().get::<VoxelMapInstance>(map).unwrap();
    assert!(instance.dirty_chunks.contains(&-IVec3::X));
    assert!(instance.dirty_chunks.contains(&IVec3::new(-1, 0, -1)));
}

#[test]
fn api_set_voxels_updates_both_boundary_neighbors_in_one_call() {
    let mut app = test_app();
    let map = spawn_map(&mut app, 1);
    spawn_ticket(&mut app, map, Vec3::ZERO, 1);

    tick_until(&mut app, |app| {
        has_loaded_chunk(app, map, -IVec3::X)
            && has_loaded_chunk(app, map, IVec3::ZERO)
            && has_loaded_chunk(app, map, IVec3::X)
    });

    app.world_mut()
        .run_system_once(move |mut vw: VoxelWorld| {
            assert_eq!(
                vw.set_voxels(
                    map,
                    [
                        (IVec3::new(0, 2, 3), WorldVoxel::Solid(91)),
                        (IVec3::new(15, 2, 3), WorldVoxel::Solid(92)),
                    ],
                ),
                2
            );
        })
        .unwrap();

    let instance = app.world().get::<VoxelMapInstance>(map).unwrap();
    assert!(instance.chunks_needing_remesh.contains(&-IVec3::X));
    assert!(instance.chunks_needing_remesh.contains(&IVec3::ZERO));
    assert!(instance.chunks_needing_remesh.contains(&IVec3::X));
}

#[test]
fn api_set_voxels_skips_unloaded_chunks_without_panicking() {
    let mut app = test_app();
    let map = spawn_map(&mut app, 1);
    spawn_ticket(&mut app, map, Vec3::ZERO, 0);

    tick_until(&mut app, |app| has_loaded_chunk(app, map, IVec3::ZERO));

    app.world_mut()
        .run_system_once(move |mut vw: VoxelWorld| {
            assert_eq!(
                vw.set_voxels(map, [(IVec3::new(1024, 2, 3), WorldVoxel::Solid(93))]),
                0
            );
        })
        .unwrap();
}
