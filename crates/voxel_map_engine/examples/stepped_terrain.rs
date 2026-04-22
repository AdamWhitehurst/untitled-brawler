use std::sync::Arc;

use bevy::prelude::*;
use voxel_map_engine::prelude::*;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(VoxelPlugin)
        .insert_resource(ChunkGenerationEnabled)
        .add_systems(Startup, setup)
        .add_systems(Update, move_camera)
        .run();
}

fn setup(mut commands: Commands) {
    info!(
        "stepped_terrain controls: WASD move | Space up | Shift down | \
         Q/E yaw left/right | T teleport above Y-boundary staircase"
    );

    let mut instance = VoxelMapInstance::new(5, 16);
    instance.debug_colors = true;

    let map_entity = commands
        .spawn((
            instance,
            VoxelMapConfig::new(0, 0, 5, true).with_meshing_mode(MeshingMode::Sloped),
            MapDimensions {
                chunk_size: 16,
                column_y_range: (-8, 8),
                tree_height: 5,
                bounds: None,
            },
            VoxelGenerator(Arc::new(SteppedGenerator {
                chunk_size: 16,
                shape: RuntimeShape::<u32, 3>::new([18, 18, 18]),
            })),
            PendingChunks::default(),
            Transform::default(),
        ))
        .id();

    commands.spawn((
        Camera3d::default(),
        Transform::from_translation(Vec3::new(30.0, 20.0, 30.0)).looking_at(Vec3::ZERO, Vec3::Y),
        ChunkTicket::new(map_entity, TicketType::Player, 5),
    ));

    commands.spawn((
        DirectionalLight {
            illuminance: 10000.0,
            shadows_enabled: true,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.5, 0.5, 0.0)),
    ));
}

fn move_camera(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    mut query: Query<&mut Transform, With<Camera3d>>,
) {
    let Ok(mut transform) = query.single_mut() else {
        return;
    };

    let speed = 40.0 * time.delta_secs();
    let forward = Vec3::new(transform.forward().x, 0.0, transform.forward().z).normalize_or_zero();
    let right = Vec3::new(transform.right().x, 0.0, transform.right().z).normalize_or_zero();

    if keys.pressed(KeyCode::KeyW) {
        transform.translation += forward * speed;
    }
    if keys.pressed(KeyCode::KeyS) {
        transform.translation -= forward * speed;
    }
    if keys.pressed(KeyCode::KeyA) {
        transform.translation -= right * speed;
    }
    if keys.pressed(KeyCode::KeyD) {
        transform.translation += right * speed;
    }
    if keys.pressed(KeyCode::Space) {
        transform.translation.y += speed;
    }
    if keys.pressed(KeyCode::ShiftLeft) {
        transform.translation.y -= speed;
    }

    let yaw_speed = 1.5 * time.delta_secs();
    if keys.pressed(KeyCode::KeyQ) {
        transform.rotate_y(yaw_speed);
    }
    if keys.pressed(KeyCode::KeyE) {
        transform.rotate_y(-yaw_speed);
    }

    // Teleport above the Y-boundary staircase so its top (world_y up to 19)
    // is visible from above. Looks down at the staircase center.
    if keys.just_pressed(KeyCode::KeyT) {
        *transform = Transform::from_translation(Vec3::new(-25.0, 40.0, -16.0))
            .looking_at(Vec3::new(-25.0, 17.0, -16.5), Vec3::Y);
    }
}
