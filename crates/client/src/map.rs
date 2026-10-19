#[cfg(feature = "spawn-panel")]
use crate::input::editor::WorldObjectCommandIntent;
use crate::input::gestures::ClientPointerGestureState;
use crate::input::ownership::{ClientInputOwnershipSnapshot, PointerInputOwner};
use crate::input::raw::RawClientActions;
#[cfg(feature = "spawn-panel")]
use crate::world_object::{preview_visual_from_def, DefaultVoxModelMaterial};
#[cfg(feature = "spawn-panel")]
use avian3d::prelude::{Position, Rotation, SpatialQuery, SpatialQueryFilter};
use bevy::{prelude::*, window::PrimaryWindow};
#[cfg(feature = "spawn-panel")]
use dev::panels::spawn::{
    NearbyWorldObject, PendingWorldObjectDelete, PendingWorldObjectMove,
    PendingWorldObjectPlacement, PendingWorldObjectRotation, SpawnPanelUi,
    WorldObjectSelectionSource,
};
#[cfg(feature = "spawn-panel")]
use dev::DevInspectorState;
use dev::EditingMode;
#[cfg(feature = "spawn-panel")]
use dev::{
    AcknowledgedVoxelChange, TerrainBrushSettings, TerrainBrushStrokeMode, TerrainEditHistory,
    TerrainEditRecord,
};
use leafwing_input_manager::prelude::*;
#[cfg(feature = "spawn-panel")]
use lightyear::prelude::Replicated;
use lightyear::prelude::{Controlled, MessageReceiver, MessageSender, Predicted};
#[cfg(feature = "spawn-panel")]
use protocol::vox_model::{VoxModelAsset, VoxModelRegistry};
#[cfg(feature = "spawn-panel")]
use protocol::world_object::{
    PlacementOffset, WorldObjectDef, WorldObjectDefRegistry, WorldObjectDeleteAck,
    WorldObjectDeleteRequest, WorldObjectEditChannel, WorldObjectEditReject, WorldObjectId,
    WorldObjectMoveAck, WorldObjectMoveRequest, WorldObjectPlacementAck,
    WorldObjectPlacementChannel, WorldObjectPlacementReject, WorldObjectPlacementRequest,
    WorldObjectRotateAck, WorldObjectRotateRequest,
};
#[cfg(not(feature = "spawn-panel"))]
use protocol::VoxelEditRequest;
use protocol::{
    CharacterMarker, ChunkDataSync, MapInstanceId, MapRegistry, SectionBlocksUpdate, UnloadColumn,
    VoxelChannel, VoxelEditAck, VoxelEditBroadcast, VoxelEditReject, VoxelType,
};
#[cfg(feature = "spawn-panel")]
use protocol::{VoxelBrushEditRequest, VoxelChange, VoxelConcreteEditRequest};
#[cfg(feature = "spawn-panel")]
use voxel_map_engine::prelude::{brush_anchor, brush_footprint, TerrainBrushMode};
use voxel_map_engine::prelude::{
    chunk_to_column, column_to_chunks, ChunkData, ChunkStatus, ChunkTicket, MapDimensions,
    VoxelMapInstance, VoxelPlugin, VoxelWorld, WorldVoxel,
};

const RAYCAST_MAX_DISTANCE: f32 = 200.0;

fn in_editing_mode(mode: EditingMode) -> impl Fn(Res<EditingMode>) -> bool + Clone {
    move |current_mode: Res<EditingMode>| *current_mode == mode
}

fn effective_pointer_owner(
    ownership: &ClientInputOwnershipSnapshot,
    gesture: &ClientPointerGestureState,
) -> PointerInputOwner {
    gesture.effective_owner(ownership.pointer)
}

#[cfg(feature = "spawn-panel")]
/// Marker for local-only world-object placement preview entities.
#[derive(Component)]
pub struct WorldObjectPlacementPreview {
    /// Placement request sequence for accepted/pending previews; `None` marks the hover preview.
    pub sequence: Option<u32>,
    /// Object definition id rendered by this local preview.
    pub object_id: WorldObjectId,
}

#[cfg(feature = "spawn-panel")]
/// Marker for local-only world-object edit preview entities.
#[derive(Component)]
pub struct WorldObjectEditPreview {
    pub sequence: Option<u32>,
    pub target: Entity,
    pub object_id: WorldObjectId,
}

/// Current world-object placement target derived from the active camera ray.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlacementTarget {
    pub base_position: Vec3,
    pub hit_normal: IVec3,
}

/// Computes the current terrain-adjacent world-object placement target.
pub fn current_placement_target(
    player_query: &Query<&ChunkTicket, (With<Predicted>, With<Controlled>, With<CharacterMarker>)>,
    voxel_world: &mut VoxelWorld,
    camera_query: &Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    window_query: &Query<&Window, With<PrimaryWindow>>,
) -> Option<PlacementTarget> {
    let Ok(chunk_ticket) = player_query.single() else {
        trace!("current_placement_target: no predicted player with ChunkTicket");
        return None;
    };
    let Some(ray) = camera_ray(camera_query, window_query) else {
        trace!("current_placement_target: no camera ray");
        return None;
    };
    let Some(hit) = voxel_world.raycast(chunk_ticket.map_entity, ray, RAYCAST_MAX_DISTANCE, |v| {
        matches!(v, WorldVoxel::Solid(_))
    }) else {
        trace!("current_placement_target: raycast hit nothing");
        return None;
    };
    let Some(normal) = hit.normal else {
        trace!("current_placement_target: hit has no normal");
        return None;
    };
    let hit_normal = normal.as_ivec3();
    Some(PlacementTarget {
        base_position: (hit.position + hit_normal).as_vec3(),
        hit_normal,
    })
}

/// Local-only terrain brush preview footprint.
#[derive(Resource, Default)]
pub struct TerrainBrushPreview {
    pub positions: Vec<IVec3>,
}

/// Plane captured at stroke start so brush drags ignore terrain created during the stroke.
#[derive(Clone, Copy)]
pub struct TerrainBrushPlaneLock {
    pub point: Vec3,
    pub normal: Vec3,
    pub base_anchor: IVec3,
}

/// Tracks held brush strokes and repeat gating.
#[derive(Resource, Default)]
pub struct TerrainBrushStrokeState {
    pub active: bool,
    pub last_anchor: Option<IVec3>,
    pub last_cursor_position: Option<Vec2>,
    pub frames_since_last_application: u32,
    pub plane_lock: Option<TerrainBrushPlaneLock>,
    pub current_gesture_id: Option<u32>,
    pub next_gesture_id: u32,
}

/// Buffers ChunkDataSync messages that arrive before the client player is ready.
/// Lightyear clears MessageReceiver each frame in Last, so we must drain and
/// Tracks pending predictions for block edits awaiting server acknowledgment.
#[derive(Resource, Default)]
pub struct VoxelPredictionState {
    pub next_sequence: u32,
    pub pending: Vec<VoxelPrediction>,
}

/// A pending terrain edit prediction awaiting server acknowledgment.
pub struct VoxelPrediction {
    pub sequence: u32,
    pub map_id: MapInstanceId,
    pub changes: Vec<PredictedVoxelChange>,
    pub kind: TerrainPredictionKind,
    pub gesture_id: Option<u32>,
}

/// Source operation for a pending terrain edit prediction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerrainPredictionKind {
    NewEdit,
    Undo,
    Redo,
}

/// One locally predicted voxel change.
pub struct PredictedVoxelChange {
    pub position: IVec3,
    pub old_voxel: VoxelType,
    pub new_voxel: VoxelType,
}

impl VoxelPredictionState {
    /// Returns the next sequence number, incrementing the counter.
    pub fn next(&mut self) -> u32 {
        let seq = self.next_sequence;
        self.next_sequence += 1;
        seq
    }
}

/// Plugin managing client-side voxel map functionality.
pub struct ClientMapPlugin;

impl Plugin for ClientMapPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(VoxelPlugin)
            .init_resource::<MapRegistry>()
            .init_resource::<TerrainBrushPreview>()
            .init_resource::<TerrainBrushStrokeState>()
            .init_resource::<VoxelPredictionState>()
            .init_resource::<EditingMode>()
            // handle_chunk_data_sync, handle_unload_column,
            // attach_chunk_ticket_to_player, and attach_chunk_colliders are
            // registered in ClientTransitionPlugin's chain (after
            // on_transition_start_received + ApplyDeferred) to guarantee
            // the map entity is in the registry before chunk sync runs.
            .add_systems(
                Update,
                (
                    handle_voxel_broadcasts,
                    handle_section_blocks_update,
                    handle_voxel_edit_ack,
                    handle_voxel_edit_reject,
                    #[cfg(feature = "spawn-panel")]
                    sync_terrain_edit_history_active_map,
                    #[cfg(feature = "spawn-panel")]
                    handle_world_object_placement_ack,
                    #[cfg(feature = "spawn-panel")]
                    handle_world_object_placement_reject,
                    #[cfg(feature = "spawn-panel")]
                    handle_world_object_delete_ack,
                    #[cfg(feature = "spawn-panel")]
                    handle_world_object_move_ack,
                    #[cfg(feature = "spawn-panel")]
                    handle_world_object_rotate_ack,
                    #[cfg(feature = "spawn-panel")]
                    handle_world_object_edit_reject,
                )
                    .run_if(in_state(ui::ClientState::InGame)),
            )
            .add_systems(
                PostUpdate,
                (
                    #[cfg(feature = "spawn-panel")]
                    update_terrain_brush_preview.run_if(in_editing_mode(EditingMode::Terrain)),
                    #[cfg(feature = "spawn-panel")]
                    handle_terrain_brush_input.run_if(in_editing_mode(EditingMode::Terrain)),
                    #[cfg(feature = "spawn-panel")]
                    handle_terrain_undo_redo_input.run_if(in_editing_mode(EditingMode::Terrain)),
                    #[cfg(not(feature = "spawn-panel"))]
                    handle_voxel_input.run_if(in_editing_mode(EditingMode::Terrain)),
                    #[cfg(feature = "spawn-panel")]
                    update_world_object_nearby_selection
                        .run_if(in_editing_mode(EditingMode::SelectEdit)),
                    #[cfg(feature = "spawn-panel")]
                    handle_world_object_cursor_pick_input
                        .run_if(in_editing_mode(EditingMode::SelectEdit)),
                    #[cfg(feature = "spawn-panel")]
                    handle_world_object_delete_input
                        .run_if(in_editing_mode(EditingMode::SelectEdit)),
                    #[cfg(feature = "spawn-panel")]
                    handle_world_object_move_input.run_if(in_editing_mode(EditingMode::SelectEdit)),
                    #[cfg(feature = "spawn-panel")]
                    handle_world_object_rotate_input
                        .run_if(in_editing_mode(EditingMode::SelectEdit)),
                    #[cfg(feature = "spawn-panel")]
                    handle_world_object_placement_input
                        .run_if(in_editing_mode(EditingMode::PlaceDefinition)),
                    #[cfg(feature = "spawn-panel")]
                    update_world_object_placement_preview
                        .run_if(in_editing_mode(EditingMode::PlaceDefinition)),
                    #[cfg(feature = "spawn-panel")]
                    cleanup_stale_world_object_edit_previews
                        .run_if(in_editing_mode(EditingMode::SelectEdit)),
                    #[cfg(feature = "spawn-panel")]
                    update_world_object_edit_preview
                        .run_if(in_editing_mode(EditingMode::SelectEdit)),
                    #[cfg(feature = "spawn-panel")]
                    reconcile_placement_preview_on_replication,
                    #[cfg(feature = "spawn-panel")]
                    reconcile_edit_preview_on_transform_replication,
                )
                    .chain()
                    .run_if(in_state(ui::ClientState::InGame))
                    .after(TransformSystems::Propagate),
            );
    }
}

#[cfg(feature = "spawn-panel")]
fn sync_terrain_edit_history_active_map(
    mut history: ResMut<TerrainEditHistory>,
    player_query: Query<&MapInstanceId, (With<Predicted>, With<Controlled>, With<CharacterMarker>)>,
) {
    let Ok(map_id) = player_query.single() else {
        trace!("sync_terrain_edit_history_active_map: no predicted controlled player map");
        history.active_map = None;
        return;
    };
    history.active_map = Some(map_id.clone());
}

pub fn attach_chunk_ticket_to_player(
    mut commands: Commands,
    registry: Res<MapRegistry>,
    players: Query<
        (Entity, &MapInstanceId),
        (With<Predicted>, With<CharacterMarker>, Without<ChunkTicket>),
    >,
) {
    for (entity, map_id) in &players {
        let Some(&map_entity) = registry.0.get(map_id) else {
            trace!(
                "attach_chunk_ticket_to_player: map {map_id:?} not yet registered, expected during transition"
            );
            continue;
        };
        trace!("Attaching ChunkTicket to player {entity:?} on map {map_id:?}");
        commands
            .entity(entity)
            .insert(ChunkTicket::player(map_entity));
    }
}

/// Receives chunk data from server and queues async meshing via the remesh pipeline.
pub fn handle_chunk_data_sync(
    mut receivers: Query<&mut MessageReceiver<ChunkDataSync>>,
    mut map_query: Query<&mut VoxelMapInstance>,
    registry: Res<MapRegistry>,
) {
    let mut incoming: Vec<ChunkDataSync> = Vec::new();
    for mut receiver in &mut receivers {
        incoming.extend(receiver.receive());
    }

    if incoming.is_empty() {
        return;
    }

    for sync in incoming {
        let Some(&map_entity) = registry.0.get(&sync.map_id) else {
            continue;
        };
        let Ok(mut instance) = map_query.get_mut(map_entity) else {
            continue;
        };

        if sync.chunk_size != instance.chunk_size {
            error!(
                "ChunkDataSync chunk_size mismatch for {:?}: server={}, client={}",
                sync.map_id, sync.chunk_size, instance.chunk_size
            );
            continue;
        }

        let chunk_data = ChunkData::from_voxels(&sync.data.to_voxels(), ChunkStatus::Full);

        instance.insert_chunk_data(sync.chunk_pos, chunk_data);
        instance
            .chunk_levels
            .entry(chunk_to_column(sync.chunk_pos))
            .or_insert(0);
        instance.chunks_needing_remesh.insert(sync.chunk_pos);
    }
}

/// Handle server's UnloadColumn message — remove chunk data for all chunks in the column.
/// Mesh entity cleanup is handled by the existing `despawn_out_of_range_chunks` system
/// which checks `chunk_levels.contains_key()`.
pub fn handle_unload_column(
    mut receivers: Query<&mut MessageReceiver<UnloadColumn>>,
    registry: Res<MapRegistry>,
    mut map_query: Query<(&mut VoxelMapInstance, &MapDimensions)>,
) {
    for mut receiver in &mut receivers {
        for unload in receiver.receive() {
            let Some(&map_entity) = registry.0.get(&unload.map_id) else {
                continue;
            };
            let Ok((mut instance, dimensions)) = map_query.get_mut(map_entity) else {
                continue;
            };
            let col = unload.column;
            for chunk_pos in column_to_chunks(col, dimensions.column_y_range) {
                instance.remove_chunk_data(chunk_pos);
            }
            instance.chunk_levels.remove(&col);
        }
    }
}

/// Applies voxel edit broadcasts from the server, skipping positions with pending predictions.
fn handle_voxel_broadcasts(
    mut receiver: Query<&mut MessageReceiver<VoxelEditBroadcast>>,
    registry: Res<MapRegistry>,
    mut voxel_world: VoxelWorld,
    prediction_state: Res<VoxelPredictionState>,
) {
    for mut message_receiver in receiver.iter_mut() {
        for broadcast in message_receiver.receive() {
            let Some(&map_entity) = registry.0.get(&broadcast.map_id) else {
                trace!(
                    "handle_voxel_broadcasts: map {:?} not registered",
                    broadcast.map_id
                );
                continue;
            };
            let has_pending_prediction =
                has_pending_prediction_at(&prediction_state, &broadcast.map_id, broadcast.position);
            if has_pending_prediction {
                trace!(
                    "handle_voxel_broadcasts: skipping broadcast at {:?} (pending prediction)",
                    broadcast.position
                );
                continue;
            }

            trace!(
                "handle_voxel_broadcasts: applying broadcast at {:?} voxel={:?}",
                broadcast.position,
                broadcast.voxel
            );
            voxel_world.set_voxel(
                map_entity,
                broadcast.position,
                WorldVoxel::from(broadcast.voxel),
            );
        }
    }
}

/// Handles batched block updates from server.
fn handle_section_blocks_update(
    mut receivers: Query<&mut MessageReceiver<SectionBlocksUpdate>>,
    registry: Res<MapRegistry>,
    mut voxel_world: VoxelWorld,
    prediction_state: Res<VoxelPredictionState>,
) {
    for mut receiver in receivers.iter_mut() {
        for update in receiver.receive() {
            let Some(&map_entity) = registry.0.get(&update.map_id) else {
                trace!(
                    "handle_section_blocks_update: map {:?} not registered",
                    update.map_id
                );
                continue;
            };
            for (pos, voxel) in &update.changes {
                let has_pending_prediction =
                    has_pending_prediction_at(&prediction_state, &update.map_id, *pos);
                if has_pending_prediction {
                    trace!(
                        "handle_section_blocks_update: skipping change at {:?} (pending prediction)",
                        pos
                    );
                    continue;
                }
                voxel_world.set_voxel(map_entity, *pos, WorldVoxel::from(*voxel));
            }
        }
    }
}

fn has_pending_prediction_at(
    prediction_state: &VoxelPredictionState,
    map_id: &MapInstanceId,
    position: IVec3,
) -> bool {
    prediction_state
        .pending
        .iter()
        .filter(|prediction| prediction.map_id == *map_id)
        .flat_map(|prediction| prediction.changes.iter())
        .any(|change| change.position == position)
}

#[cfg(feature = "spawn-panel")]
fn raycast_terrain_brush_anchor(
    chunk_ticket: &ChunkTicket,
    voxel_world: &VoxelWorld,
    camera_query: &Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    window_query: &Query<&Window, With<PrimaryWindow>>,
    settings: &TerrainBrushSettings,
) -> Option<(IVec3, TerrainBrushPlaneLock)> {
    let Some(ray) = camera_ray(camera_query, window_query) else {
        trace!("raycast_terrain_brush_anchor: no camera ray");
        return None;
    };
    let Some(hit) = voxel_world.raycast(chunk_ticket.map_entity, ray, RAYCAST_MAX_DISTANCE, |v| {
        matches!(v, WorldVoxel::Solid(_))
    }) else {
        trace!("raycast_terrain_brush_anchor: raycast hit nothing");
        return None;
    };
    let Some(anchor) = brush_anchor(&hit, settings.mode) else {
        trace!("raycast_terrain_brush_anchor: hit has no usable brush anchor");
        return None;
    };
    let Some(hit_normal) = hit.normal else {
        trace!("raycast_terrain_brush_anchor: hit has no usable plane normal");
        return None;
    };
    Some((
        anchor,
        TerrainBrushPlaneLock {
            point: ray.origin + *ray.direction * RAYCAST_MAX_DISTANCE * hit.t,
            normal: hit_normal,
            base_anchor: anchor,
        },
    ))
}

#[cfg(feature = "spawn-panel")]
fn projected_terrain_brush_anchor(
    plane_lock: TerrainBrushPlaneLock,
    camera_query: &Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    window_query: &Query<&Window, With<PrimaryWindow>>,
) -> Option<IVec3> {
    let Some(ray) = camera_ray(camera_query, window_query) else {
        trace!("projected_terrain_brush_anchor: no camera ray");
        return None;
    };
    let denominator = plane_lock.normal.dot(*ray.direction);
    if denominator.abs() <= f32::EPSILON {
        trace!("projected_terrain_brush_anchor: ray is parallel to stroke plane");
        return None;
    }
    let distance = (plane_lock.point - ray.origin).dot(plane_lock.normal) / denominator;
    if distance < 0.0 {
        trace!("projected_terrain_brush_anchor: projected point is behind camera");
        return None;
    }
    Some(anchor_from_projected_plane_point(
        ray.origin + *ray.direction * distance,
        plane_lock,
    ))
}

#[cfg(feature = "spawn-panel")]
fn anchor_from_projected_plane_point(point: Vec3, plane_lock: TerrainBrushPlaneLock) -> IVec3 {
    let mut anchor = point.floor().as_ivec3();
    let normal = plane_lock.normal.as_ivec3();
    for axis in 0..3 {
        if normal[axis] != 0 {
            anchor[axis] = plane_lock.base_anchor[axis];
        }
    }
    anchor
}

#[cfg(feature = "spawn-panel")]
fn update_terrain_brush_preview(
    player_query: Query<&ChunkTicket, (With<Predicted>, With<Controlled>, With<CharacterMarker>)>,
    voxel_world: VoxelWorld,
    camera_query: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    window_query: Query<&Window, With<PrimaryWindow>>,
    settings: Res<TerrainBrushSettings>,
    stroke_state: Res<TerrainBrushStrokeState>,
    mut preview: ResMut<TerrainBrushPreview>,
    mut gizmos: Gizmos,
) {
    if !settings.active {
        trace!("update_terrain_brush_preview: terrain brush inactive");
        preview.positions.clear();
        return;
    }
    let Ok(chunk_ticket) = player_query.single() else {
        trace!("update_terrain_brush_preview: no predicted player with ChunkTicket");
        preview.positions.clear();
        return;
    };
    let anchor = if let Some(plane_lock) = stroke_state.plane_lock {
        projected_terrain_brush_anchor(plane_lock, &camera_query, &window_query)
    } else {
        raycast_terrain_brush_anchor(
            chunk_ticket,
            &voxel_world,
            &camera_query,
            &window_query,
            &settings,
        )
        .map(|(anchor, _)| anchor)
    };
    let Some(anchor) = anchor else {
        preview.positions.clear();
        return;
    };
    let anchor = if settings.stroke_mode == TerrainBrushStrokeMode::Continuous {
        stroke_state
            .plane_lock
            .and_then(|plane_lock| {
                brush_anchor_with_changes(
                    chunk_ticket.map_entity,
                    &voxel_world,
                    anchor,
                    &settings,
                    plane_lock,
                )
                .map(|(anchor, _)| anchor)
            })
            .unwrap_or(anchor)
    } else {
        anchor
    };
    preview.positions = brush_footprint(anchor, settings.shape, settings.width, settings.height);
    for pos in &preview.positions {
        gizmos.cube(
            Transform::from_translation(pos.as_vec3() + Vec3::splat(0.5))
                .with_scale(Vec3::splat(1.0)),
            Color::srgb(0.2, 0.9, 1.0),
        );
    }
}

#[cfg(feature = "spawn-panel")]
fn predict_brush_changes(
    map_entity: Entity,
    voxel_world: &VoxelWorld,
    anchor: IVec3,
    settings: &TerrainBrushSettings,
) -> Vec<PredictedVoxelChange> {
    brush_footprint(anchor, settings.shape, settings.width, settings.height)
        .into_iter()
        .filter_map(|position| {
            let old = voxel_world.get_voxel(map_entity, position);
            let new_voxel = predicted_voxel_for_brush_mode(old, settings.mode, settings.material)?;
            Some(PredictedVoxelChange {
                position,
                old_voxel: old.into(),
                new_voxel,
            })
        })
        .collect()
}

#[cfg(feature = "spawn-panel")]
fn predicted_voxel_for_brush_mode(
    old: WorldVoxel,
    mode: TerrainBrushMode,
    material: u8,
) -> Option<VoxelType> {
    match mode {
        TerrainBrushMode::FillAir if matches!(old, WorldVoxel::Air | WorldVoxel::Unset) => {
            Some(VoxelType::Solid(material))
        }
        TerrainBrushMode::Remove if matches!(old, WorldVoxel::Solid(_)) => Some(VoxelType::Air),
        TerrainBrushMode::PaintExisting if matches!(old, WorldVoxel::Solid(_)) => {
            Some(VoxelType::Solid(material))
        }
        TerrainBrushMode::ReplaceAll => Some(VoxelType::Solid(material)),
        _ => None,
    }
}

#[cfg(feature = "spawn-panel")]
fn handle_terrain_brush_input(
    player_query: Query<
        (&ChunkTicket, &MapInstanceId),
        (With<Predicted>, With<Controlled>, With<CharacterMarker>),
    >,
    mut voxel_world: VoxelWorld,
    camera_query: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    window_query: Query<&Window, With<PrimaryWindow>>,
    action_query: Query<&ActionState<RawClientActions>, With<Controlled>>,
    ownership: Res<ClientInputOwnershipSnapshot>,
    gesture: Res<ClientPointerGestureState>,
    settings: Res<TerrainBrushSettings>,
    mut stroke_state: ResMut<TerrainBrushStrokeState>,
    mut message_sender: Query<&mut MessageSender<VoxelBrushEditRequest>>,
    mut prediction_state: ResMut<VoxelPredictionState>,
) {
    if !settings.active {
        trace!("handle_terrain_brush_input: terrain brush inactive");
        reset_brush_stroke_state(&mut stroke_state);
        return;
    }
    let Ok((chunk_ticket, map_id)) = player_query.single() else {
        trace!(
            "handle_terrain_brush_input: no predicted player with ChunkTicket and MapInstanceId"
        );
        reset_brush_stroke_state(&mut stroke_state);
        return;
    };
    let Ok(action_state) = action_query.single() else {
        trace!("handle_terrain_brush_input: no entity with ActionState + Controlled");
        reset_brush_stroke_state(&mut stroke_state);
        return;
    };

    let pointer_owner = effective_pointer_owner(&ownership, &gesture);
    if !pointer_owner.allows_terrain() {
        trace!(
            ?pointer_owner,
            "handle_terrain_brush_input: terrain input suppressed by pointer ownership"
        );
        reset_brush_stroke_state(&mut stroke_state);
        return;
    }

    let pressed = action_state.pressed(&RawClientActions::PlaceVoxel);
    if !pressed {
        trace!("handle_terrain_brush_input: no terrain brush action held");
        reset_brush_stroke_state(&mut stroke_state);
        return;
    }

    let Some(cursor_position) = cursor_screen_position(&window_query) else {
        trace!("handle_terrain_brush_input: no cursor position");
        reset_brush_stroke_state(&mut stroke_state);
        return;
    };
    if should_suppress_brush_repeat(&settings, &mut stroke_state, cursor_position) {
        trace!("handle_terrain_brush_input: stroke repeat suppressed");
        return;
    }

    let Some((anchor, plane_lock)) = current_stroke_anchor(
        chunk_ticket,
        &voxel_world,
        &camera_query,
        &window_query,
        &settings,
        &stroke_state,
    ) else {
        reset_brush_stroke_state(&mut stroke_state);
        return;
    };

    let Some((anchor, changes)) = brush_anchor_with_changes(
        chunk_ticket.map_entity,
        &voxel_world,
        anchor,
        &settings,
        plane_lock,
    ) else {
        trace!("handle_terrain_brush_input: brush produced no changes");
        mark_brush_application(&mut stroke_state, anchor, cursor_position, plane_lock);
        return;
    };

    let sequence = prediction_state.next();
    let gesture_id = current_or_start_terrain_gesture(&mut stroke_state);
    voxel_world.set_voxels(
        chunk_ticket.map_entity,
        changes
            .iter()
            .map(|change| (change.position, WorldVoxel::from(change.new_voxel))),
    );
    prediction_state.pending.push(VoxelPrediction {
        sequence,
        map_id: map_id.clone(),
        changes,
        kind: TerrainPredictionKind::NewEdit,
        gesture_id: Some(gesture_id),
    });

    for mut sender in message_sender.iter_mut() {
        sender.send::<VoxelChannel>(VoxelBrushEditRequest {
            sequence,
            map_id: map_id.clone(),
            anchor,
            shape: settings.shape,
            width: settings.width,
            height: settings.height,
            mode: settings.mode,
            material: settings.material,
        });
    }
    mark_brush_application(&mut stroke_state, anchor, cursor_position, plane_lock);
}

#[cfg(feature = "spawn-panel")]
fn brush_anchor_with_changes(
    map_entity: Entity,
    voxel_world: &VoxelWorld,
    anchor: IVec3,
    settings: &TerrainBrushSettings,
    plane_lock: TerrainBrushPlaneLock,
) -> Option<(IVec3, Vec<PredictedVoxelChange>)> {
    let changes = predict_brush_changes(map_entity, voxel_world, anchor, settings);
    if !changes.is_empty() || settings.stroke_mode == TerrainBrushStrokeMode::Discrete {
        return (!changes.is_empty()).then_some((anchor, changes));
    }

    let direction = continuous_brush_direction(settings.mode, plane_lock.normal)?;
    for offset in 1..=64 {
        let candidate = anchor + direction * offset;
        let changes = predict_brush_changes(map_entity, voxel_world, candidate, settings);
        if !changes.is_empty() {
            return Some((candidate, changes));
        }
    }
    trace!("brush_anchor_with_changes: continuous stroke found no editable voxels");
    None
}

#[cfg(feature = "spawn-panel")]
fn continuous_brush_direction(mode: TerrainBrushMode, normal: Vec3) -> Option<IVec3> {
    let normal = normal.as_ivec3();
    match mode {
        TerrainBrushMode::FillAir | TerrainBrushMode::ReplaceAll => Some(normal),
        TerrainBrushMode::Remove | TerrainBrushMode::PaintExisting => Some(-normal),
    }
}

#[cfg(feature = "spawn-panel")]
fn current_stroke_anchor(
    chunk_ticket: &ChunkTicket,
    voxel_world: &VoxelWorld,
    camera_query: &Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    window_query: &Query<&Window, With<PrimaryWindow>>,
    settings: &TerrainBrushSettings,
    stroke_state: &TerrainBrushStrokeState,
) -> Option<(IVec3, TerrainBrushPlaneLock)> {
    if let Some(plane_lock) = stroke_state.plane_lock {
        return projected_terrain_brush_anchor(plane_lock, camera_query, window_query)
            .map(|anchor| (anchor, plane_lock));
    }
    raycast_terrain_brush_anchor(
        chunk_ticket,
        voxel_world,
        camera_query,
        window_query,
        settings,
    )
}

#[cfg(feature = "spawn-panel")]
fn should_suppress_brush_repeat(
    settings: &TerrainBrushSettings,
    stroke_state: &mut TerrainBrushStrokeState,
    cursor_position: Vec2,
) -> bool {
    if !stroke_state.active {
        return false;
    }

    match settings.stroke_mode {
        TerrainBrushStrokeMode::Discrete => stroke_state
            .last_cursor_position
            .is_some_and(|last| last.distance_squared(cursor_position) < 0.25),
        TerrainBrushStrokeMode::Continuous => {
            stroke_state.frames_since_last_application =
                stroke_state.frames_since_last_application.saturating_add(1);
            stroke_state.frames_since_last_application < settings.continuous_every_n_frames.max(1)
        }
    }
}

#[cfg(feature = "spawn-panel")]
fn mark_brush_application(
    stroke_state: &mut TerrainBrushStrokeState,
    anchor: IVec3,
    cursor_position: Vec2,
    plane_lock: TerrainBrushPlaneLock,
) {
    stroke_state.active = true;
    stroke_state.last_anchor = Some(anchor);
    stroke_state.last_cursor_position = Some(cursor_position);
    stroke_state.frames_since_last_application = 0;
    stroke_state.plane_lock = Some(plane_lock);
}

#[cfg(feature = "spawn-panel")]
fn reset_brush_stroke_state(stroke_state: &mut TerrainBrushStrokeState) {
    stroke_state.active = false;
    stroke_state.last_anchor = None;
    stroke_state.last_cursor_position = None;
    stroke_state.frames_since_last_application = 0;
    stroke_state.plane_lock = None;
    stroke_state.current_gesture_id = None;
}

#[cfg(feature = "spawn-panel")]
fn current_or_start_terrain_gesture(stroke_state: &mut TerrainBrushStrokeState) -> u32 {
    if let Some(gesture_id) = stroke_state.current_gesture_id {
        return gesture_id;
    }
    let gesture_id = stroke_state.next_gesture_id;
    stroke_state.next_gesture_id = stroke_state.next_gesture_id.wrapping_add(1);
    stroke_state.current_gesture_id = Some(gesture_id);
    gesture_id
}

#[cfg(not(feature = "spawn-panel"))]
fn handle_voxel_input(
    player_query: Query<
        (&ChunkTicket, &MapInstanceId),
        (With<Predicted>, With<Controlled>, With<CharacterMarker>),
    >,
    mut voxel_world: VoxelWorld,
    camera_query: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    window_query: Query<&Window, With<PrimaryWindow>>,
    action_query: Query<&ActionState<RawClientActions>, With<Controlled>>,
    ownership: Res<ClientInputOwnershipSnapshot>,
    gesture: Res<ClientPointerGestureState>,
    mut message_sender: Query<&mut MessageSender<VoxelEditRequest>>,
    mut prediction_state: ResMut<VoxelPredictionState>,
) {
    let Ok((chunk_ticket, map_id)) = player_query.single() else {
        trace!("handle_voxel_input: no predicted player with ChunkTicket and MapInstanceId");
        return;
    };
    let Ok(action_state) = action_query.single() else {
        trace!("handle_voxel_input: no entity with ActionState + Controlled");
        return;
    };

    let pointer_owner = effective_pointer_owner(&ownership, &gesture);
    if !pointer_owner.allows_terrain() {
        trace!(
            ?pointer_owner,
            "handle_voxel_input: terrain input suppressed by pointer ownership"
        );
        return;
    }

    let removing = action_state.just_pressed(&RawClientActions::RemoveVoxel);
    let placing = action_state.just_pressed(&RawClientActions::PlaceVoxel);
    if !removing && !placing {
        trace!("handle_voxel_input: no voxel edit action pressed");
        return;
    }

    let Some(ray) = camera_ray(&camera_query, &window_query) else {
        warn!("handle_voxel_input: no camera ray (no cursor position?)");
        return;
    };

    let Some(hit) = voxel_world.raycast(chunk_ticket.map_entity, ray, RAYCAST_MAX_DISTANCE, |v| {
        matches!(v, WorldVoxel::Solid(_))
    }) else {
        trace!("handle_voxel_input: raycast hit nothing");
        return;
    };

    let (position, voxel) = if removing {
        (hit.position, VoxelType::Air)
    } else if let Some(normal) = hit.normal {
        (hit.position + normal.as_ivec3(), VoxelType::Solid(0))
    } else {
        trace!("handle_voxel_input: place hit has no normal");
        return;
    };

    let sequence = prediction_state.next();
    let old_voxel = voxel_world
        .get_voxel(chunk_ticket.map_entity, position)
        .into();

    voxel_world.set_voxel(chunk_ticket.map_entity, position, WorldVoxel::from(voxel));

    prediction_state.pending.push(VoxelPrediction {
        sequence,
        map_id: map_id.clone(),
        changes: vec![PredictedVoxelChange {
            position,
            old_voxel,
            new_voxel: voxel,
        }],
        kind: TerrainPredictionKind::NewEdit,
        gesture_id: None,
    });

    for mut sender in message_sender.iter_mut() {
        trace!("Sending voxel edit request to server: {:?}", position);
        sender.send::<VoxelChannel>(VoxelEditRequest {
            position,
            voxel,
            sequence,
        });
    }
}

#[cfg(feature = "spawn-panel")]
/// Computes the display transform for a local placement preview.
pub fn preview_transform(def: &WorldObjectDef, base_position: Vec3) -> Transform {
    let offset = def
        .components
        .iter()
        .find_map(|c| c.try_downcast_ref::<PlacementOffset>())
        .map(|offset| offset.0)
        .unwrap_or(Vec3::ZERO);
    Transform::from_translation(base_position + offset)
}

#[cfg(feature = "spawn-panel")]
/// Spawns a local-only placement preview parent and optional visual child.
pub fn spawn_world_object_placement_preview(
    commands: &mut Commands,
    sequence: Option<u32>,
    object_id: WorldObjectId,
    transform: Transform,
    def: &WorldObjectDef,
    vox_registry: &VoxModelRegistry,
    vox_assets: &Assets<VoxModelAsset>,
    default_material: &DefaultVoxModelMaterial,
) -> Entity {
    let entity = commands
        .spawn((
            WorldObjectPlacementPreview {
                sequence,
                object_id,
            },
            transform,
            Visibility::default(),
            Name::new("world-object-placement-preview"),
        ))
        .id();
    preview_visual_from_def(
        commands,
        entity,
        def,
        vox_registry,
        vox_assets,
        default_material,
    );
    entity
}

#[cfg(feature = "spawn-panel")]
/// Computes the current final move target for an existing world object.
pub fn current_world_object_move_target(
    def: &WorldObjectDef,
    player_query: &Query<&ChunkTicket, (With<Predicted>, With<Controlled>, With<CharacterMarker>)>,
    voxel_world: &mut VoxelWorld,
    camera_query: &Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    window_query: &Query<&Window, With<PrimaryWindow>>,
) -> Option<Vec3> {
    current_placement_target(player_query, voxel_world, camera_query, window_query)
        .map(|target| preview_transform(def, target.base_position).translation)
}

#[cfg(feature = "spawn-panel")]
/// Spawns a local-only edit preview parent and optional visual child.
pub fn spawn_world_object_edit_preview(
    commands: &mut Commands,
    sequence: Option<u32>,
    target: Entity,
    object_id: WorldObjectId,
    transform: Transform,
    def: &WorldObjectDef,
    vox_registry: &VoxModelRegistry,
    vox_assets: &Assets<VoxModelAsset>,
    default_material: &DefaultVoxModelMaterial,
) -> Entity {
    let entity = commands
        .spawn((
            WorldObjectEditPreview {
                sequence,
                target,
                object_id,
            },
            transform,
            Visibility::default(),
            Name::new("world-object-edit-preview"),
        ))
        .id();
    preview_visual_from_def(
        commands,
        entity,
        def,
        vox_registry,
        vox_assets,
        default_material,
    );
    entity
}

#[cfg(feature = "spawn-panel")]
/// Returns replicated world objects in radius on the current map sorted by distance.
pub fn nearby_world_objects_in_radius(
    origin: Vec3,
    radius: f32,
    objects: &Query<
        (Entity, &WorldObjectId, &Position, Option<&MapInstanceId>),
        (With<WorldObjectId>, With<Replicated>),
    >,
    current_map: Option<&MapInstanceId>,
) -> Vec<NearbyWorldObject> {
    let radius_sq = radius * radius;
    let mut nearby: Vec<NearbyWorldObject> = objects
        .iter()
        .filter(|(_, _, _, object_map)| match (current_map, object_map) {
            (Some(current), Some(object_map)) => *object_map == current,
            _ => true,
        })
        .filter_map(|(entity, object_id, position, _)| {
            let dist_sq = position.0.distance_squared(origin);
            (dist_sq <= radius_sq).then(|| NearbyWorldObject {
                entity,
                object_id: object_id.clone(),
                distance: dist_sq.sqrt(),
            })
        })
        .collect();
    nearby.sort_by(|a, b| a.distance.total_cmp(&b.distance));
    nearby
}

#[cfg(feature = "spawn-panel")]
/// Returns the nearest replicated world object under the cursor ray.
pub fn current_world_object_pick(
    camera_query: &Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    window_query: &Query<&Window, With<PrimaryWindow>>,
    object_query: &Query<(), (With<WorldObjectId>, With<Replicated>)>,
    spatial_query: &SpatialQuery,
) -> Option<Entity> {
    let Some(ray) = camera_ray(camera_query, window_query) else {
        trace!("current_world_object_pick: no camera ray");
        return None;
    };
    pick_world_object_collider_from_ray(ray, object_query, spatial_query)
}

#[cfg(feature = "spawn-panel")]
/// Picks the closest replicated world-object collider under the cursor ray.
pub fn pick_world_object_collider_from_ray(
    ray: Ray3d,
    object_query: &Query<(), (With<WorldObjectId>, With<Replicated>)>,
    spatial_query: &SpatialQuery,
) -> Option<Entity> {
    spatial_query
        .cast_ray_predicate(
            ray.origin,
            ray.direction,
            RAYCAST_MAX_DISTANCE,
            true,
            &SpatialQueryFilter::default(),
            &|entity| object_query.contains(entity),
        )
        .map(|hit| hit.entity)
}

#[cfg(feature = "spawn-panel")]
fn update_world_object_nearby_selection(
    mut ui_state: ResMut<SpawnPanelUi>,
    player_query: Query<
        (&Position, &MapInstanceId),
        (With<Predicted>, With<Controlled>, With<CharacterMarker>),
    >,
    object_query: Query<
        (Entity, &WorldObjectId, &Position, Option<&MapInstanceId>),
        (With<WorldObjectId>, With<Replicated>),
    >,
) {
    if !ui_state.selection.nearby_scan_requested {
        trace!("update_world_object_nearby_selection: scan not requested");
        return;
    }
    ui_state.selection.nearby_scan_requested = false;
    let Ok((player_position, player_map)) = player_query.single() else {
        trace!("update_world_object_nearby_selection: no predicted controlled player position");
        ui_state.selection.nearby_objects.clear();
        return;
    };
    ui_state.selection.nearby_objects = nearby_world_objects_in_radius(
        player_position.0,
        ui_state.selection.nearby_radius,
        &object_query,
        Some(player_map),
    );
}

#[cfg(feature = "spawn-panel")]
fn handle_world_object_cursor_pick_input(
    mut ui_state: ResMut<SpawnPanelUi>,
    inspector_state: Option<Res<DevInspectorState>>,
    mut command_reader: MessageReader<WorldObjectCommandIntent>,
    camera_query: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    window_query: Query<&Window, With<PrimaryWindow>>,
    object_query: Query<(), (With<WorldObjectId>, With<Replicated>)>,
    spatial_query: SpatialQuery,
) {
    let pick_requested = command_reader
        .read()
        .any(|intent| matches!(intent, WorldObjectCommandIntent::Pick));
    if !ui_state.selection.cursor_pick_armed {
        trace!("handle_world_object_cursor_pick_input: cursor pick is not armed");
        return;
    }
    let Some(inspector_state) = inspector_state else {
        trace!("handle_world_object_cursor_pick_input: dev inspector state missing");
        return;
    };
    if !inspector_state.enabled || !inspector_state.panels.spawn_panel {
        trace!("handle_world_object_cursor_pick_input: spawn panel is not active");
        return;
    }
    if !pick_requested {
        trace!("handle_world_object_cursor_pick_input: pick intent not received");
        return;
    }
    let Some(picked) =
        current_world_object_pick(&camera_query, &window_query, &object_query, &spatial_query)
    else {
        trace!("handle_world_object_cursor_pick_input: no world object under cursor");
        return;
    };
    ui_state.selection.cursor_pick_armed = false;
    ui_state.selection.selected = Some(picked);
    ui_state.selection.selection_source = Some(WorldObjectSelectionSource::Cursor);
}

#[cfg(feature = "spawn-panel")]
fn handle_world_object_delete_input(
    mut ui_state: ResMut<SpawnPanelUi>,
    mut command_reader: MessageReader<WorldObjectCommandIntent>,
    mut message_sender: Query<&mut MessageSender<WorldObjectDeleteRequest>>,
) {
    if !command_reader
        .read()
        .any(|intent| matches!(intent, WorldObjectCommandIntent::Delete))
    {
        trace!("handle_world_object_delete_input: delete intent not received");
        return;
    }
    let Some(target) = ui_state.selection.selected else {
        trace!("handle_world_object_delete_input: no selected world object");
        return;
    };
    let sequence = ui_state.selection.next_sequence();
    let request = WorldObjectDeleteRequest { sequence, target };

    let mut sent = false;
    for mut sender in &mut message_sender {
        sender.send::<WorldObjectEditChannel>(request.clone());
        sent = true;
    }
    if !sent {
        trace!("handle_world_object_delete_input: no WorldObjectDeleteRequest sender");
        return;
    }

    ui_state
        .selection
        .pending_deletes
        .push(PendingWorldObjectDelete {
            sequence,
            target,
            accepted: false,
        });
}

#[cfg(feature = "spawn-panel")]
fn handle_world_object_move_input(
    mut ui_state: ResMut<SpawnPanelUi>,
    mut command_reader: MessageReader<WorldObjectCommandIntent>,
    player_query: Query<&ChunkTicket, (With<Predicted>, With<Controlled>, With<CharacterMarker>)>,
    mut voxel_world: VoxelWorld,
    camera_query: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    window_query: Query<&Window, With<PrimaryWindow>>,
    registry: Option<Res<WorldObjectDefRegistry>>,
    target_query: Query<
        (&WorldObjectId, Option<&Position>, Option<&MapInstanceId>),
        With<Replicated>,
    >,
    map_registry: Option<Res<MapRegistry>>,
    map_query: Query<&MapDimensions>,
    mut message_sender: Query<&mut MessageSender<WorldObjectMoveRequest>>,
) {
    if !ui_state.selection.move_armed {
        trace!("handle_world_object_move_input: move is not armed");
        return;
    }
    if !command_reader
        .read()
        .any(|intent| matches!(intent, WorldObjectCommandIntent::Move))
    {
        trace!("handle_world_object_move_input: move intent not received");
        return;
    }
    let Some(target) = ui_state.selection.selected else {
        trace!("handle_world_object_move_input: move armed without selected object");
        return;
    };
    let Ok((object_id, current_position, target_map_id)) = target_query.get(target) else {
        trace!("handle_world_object_move_input: selected object no longer exists");
        return;
    };
    let Some(registry) = registry else {
        trace!("handle_world_object_move_input: WorldObjectDefRegistry not loaded");
        return;
    };
    let Some(def) = registry.get(object_id) else {
        trace!("handle_world_object_move_input: selected object definition missing");
        return;
    };
    let Some(final_position) = current_world_object_move_target(
        def,
        &player_query,
        &mut voxel_world,
        &camera_query,
        &window_query,
    ) else {
        trace!("handle_world_object_move_input: no move target");
        return;
    };

    let (old_chunk_pos, new_chunk_pos) = world_object_move_chunk_display(
        current_position,
        target_map_id,
        final_position,
        map_registry.as_deref(),
        &map_query,
    );
    let sequence = ui_state.selection.next_sequence();
    let request = WorldObjectMoveRequest {
        sequence,
        target,
        final_position,
    };
    let mut sent = false;
    for mut sender in &mut message_sender {
        sender.send::<WorldObjectEditChannel>(request.clone());
        sent = true;
    }
    if !sent {
        trace!("handle_world_object_move_input: no WorldObjectMoveRequest sender");
        return;
    }
    ui_state.selection.last_reject = None;
    ui_state
        .selection
        .pending_moves
        .push(PendingWorldObjectMove {
            sequence,
            target,
            final_position,
            old_chunk_pos,
            new_chunk_pos,
            accepted: false,
        });
}

#[cfg(feature = "spawn-panel")]
fn world_object_move_chunk_display(
    current_position: Option<&Position>,
    target_map_id: Option<&MapInstanceId>,
    final_position: Vec3,
    map_registry: Option<&MapRegistry>,
    map_query: &Query<&MapDimensions>,
) -> (Option<IVec3>, Option<IVec3>) {
    let Some(current_position) = current_position else {
        trace!("world_object_move_chunk_display: selected object has no Position");
        return (None, None);
    };
    let Some(target_map_id) = target_map_id else {
        trace!("world_object_move_chunk_display: selected object has no MapInstanceId");
        return (None, None);
    };
    let Some(map_registry) = map_registry else {
        trace!("world_object_move_chunk_display: MapRegistry not loaded");
        return (None, None);
    };
    let Some(map_entity) = map_registry.0.get(target_map_id).copied() else {
        trace!("world_object_move_chunk_display: target map id is not registered");
        return (None, None);
    };
    let Ok(dimensions) = map_query.get(map_entity) else {
        trace!("world_object_move_chunk_display: target map has no MapDimensions");
        return (None, None);
    };
    let old_chunk_pos =
        voxel_map_engine::lifecycle::world_to_chunk_pos(current_position.0, dimensions.chunk_size);
    let new_chunk_pos =
        voxel_map_engine::lifecycle::world_to_chunk_pos(final_position, dimensions.chunk_size);
    (Some(old_chunk_pos), Some(new_chunk_pos))
}

#[cfg(feature = "spawn-panel")]
fn handle_world_object_rotate_input(
    mut ui_state: ResMut<SpawnPanelUi>,
    mut command_reader: MessageReader<WorldObjectCommandIntent>,
    target_query: Query<(), (With<WorldObjectId>, With<Replicated>)>,
    mut message_sender: Query<&mut MessageSender<WorldObjectRotateRequest>>,
) {
    let Some(yaw_delta) = command_reader.read().find_map(|intent| match intent {
        WorldObjectCommandIntent::Rotate { yaw_delta } => Some(*yaw_delta),
        _ => None,
    }) else {
        trace!("handle_world_object_rotate_input: rotate intent not received");
        return;
    };
    let Some(target) = ui_state.selection.selected else {
        trace!("handle_world_object_rotate_input: no selected world object");
        return;
    };
    if target_query.get(target).is_err() {
        trace!("handle_world_object_rotate_input: selected object no longer exists");
        return;
    }
    let rotation = Quat::from_rotation_y(yaw_delta.to_radians());
    let sequence = ui_state.selection.next_sequence();
    let request = WorldObjectRotateRequest {
        sequence,
        target,
        rotation,
    };
    let mut sent = false;
    for mut sender in &mut message_sender {
        sender.send::<WorldObjectEditChannel>(request.clone());
        sent = true;
    }
    if !sent {
        trace!("handle_world_object_rotate_input: no WorldObjectRotateRequest sender");
        return;
    }
    ui_state
        .selection
        .pending_rotations
        .push(PendingWorldObjectRotation {
            sequence,
            target,
            rotation,
            accepted: false,
        });
}

#[cfg(feature = "spawn-panel")]
fn handle_world_object_placement_input(
    mut ui_state: ResMut<SpawnPanelUi>,
    mut command_reader: MessageReader<WorldObjectCommandIntent>,
    player_query: Query<&ChunkTicket, (With<Predicted>, With<Controlled>, With<CharacterMarker>)>,
    mut voxel_world: VoxelWorld,
    camera_query: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    window_query: Query<&Window, With<PrimaryWindow>>,
    mut message_sender: Query<&mut MessageSender<WorldObjectPlacementRequest>>,
) {
    if !ui_state.placement.armed {
        trace!("handle_world_object_placement_input: placement is not armed");
        return;
    }
    if !command_reader
        .read()
        .any(|intent| matches!(intent, WorldObjectCommandIntent::Place))
    {
        trace!("handle_world_object_placement_input: place intent not received");
        return;
    }
    let Some(object_id) = ui_state.selected_object.clone() else {
        trace!("handle_world_object_placement_input: placement armed without selected object");
        return;
    };
    let Some(target) = current_placement_target(
        &player_query,
        &mut voxel_world,
        &camera_query,
        &window_query,
    ) else {
        trace!("handle_world_object_placement_input: no placement target");
        return;
    };

    let sequence = ui_state.placement.next_sequence();
    let request = WorldObjectPlacementRequest {
        sequence,
        object_id: object_id.clone(),
        base_position: target.base_position,
    };

    let mut sent = false;
    for mut sender in message_sender.iter_mut() {
        sender.send::<WorldObjectPlacementChannel>(request.clone());
        sent = true;
    }
    if !sent {
        trace!("handle_world_object_placement_input: no WorldObjectPlacementRequest sender");
        return;
    }

    ui_state
        .placement
        .pending
        .push(PendingWorldObjectPlacement {
            sequence,
            object_id,
            base_position: target.base_position,
            accepted_final_position: None,
        });
}

#[cfg(feature = "spawn-panel")]
/// Maintains hover and pending local placement previews from spawn-panel state.
fn update_world_object_placement_preview(
    mut commands: Commands,
    ui_state: Res<SpawnPanelUi>,
    // Optional because definitions are loaded asynchronously by the world-object plugin.
    registry: Option<Res<WorldObjectDefRegistry>>,
    vox_registry: Res<VoxModelRegistry>,
    vox_assets: Res<Assets<VoxModelAsset>>,
    default_material: Res<DefaultVoxModelMaterial>,
    player_query: Query<&ChunkTicket, (With<Predicted>, With<Controlled>, With<CharacterMarker>)>,
    mut voxel_world: VoxelWorld,
    camera_query: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    window_query: Query<&Window, With<PrimaryWindow>>,
    mut preview_query: Query<(Entity, &mut Transform, &WorldObjectPlacementPreview)>,
) {
    for (entity, _, preview) in &mut preview_query {
        if let Some(sequence) = preview.sequence {
            let still_pending = ui_state
                .placement
                .pending
                .iter()
                .any(|pending| pending.sequence == sequence);
            if !still_pending {
                trace!(
                    "update_world_object_placement_preview: despawning stale sequence preview {}",
                    sequence
                );
                commands.entity(entity).despawn();
            }
        }
    }

    let Some(registry) = registry else {
        trace!("update_world_object_placement_preview: WorldObjectDefRegistry not loaded");
        return;
    };

    if !ui_state.placement.armed {
        for (entity, _, preview) in &mut preview_query {
            if preview.sequence.is_none() {
                trace!("update_world_object_placement_preview: despawning disarmed hover preview");
                commands.entity(entity).despawn();
            }
        }
        trace!("update_world_object_placement_preview: placement is not armed");
        return;
    }

    let Some(selected_object) = ui_state.selected_object.clone() else {
        trace!("update_world_object_placement_preview: placement armed without selected object");
        for (entity, _, preview) in &mut preview_query {
            if preview.sequence.is_none() {
                commands.entity(entity).despawn();
            }
        }
        return;
    };

    let Some(selected_def) = registry.get(&selected_object) else {
        trace!(
            "update_world_object_placement_preview: unknown selected object {:?}",
            selected_object.0
        );
        return;
    };

    if let Some(target) = current_placement_target(
        &player_query,
        &mut voxel_world,
        &camera_query,
        &window_query,
    ) {
        let transform = preview_transform(selected_def, target.base_position);
        let mut hover_entity = None;
        for (entity, mut preview_transform, preview) in &mut preview_query {
            if preview.sequence.is_some() {
                trace!(
                    "update_world_object_placement_preview: skipping sequence preview while updating hover preview"
                );
                continue;
            }
            if hover_entity.is_some() || preview.object_id != selected_object {
                trace!("update_world_object_placement_preview: despawning duplicate hover preview");
                commands.entity(entity).despawn();
                continue;
            }
            *preview_transform = transform;
            hover_entity = Some(entity);
        }
        if hover_entity.is_none() {
            spawn_world_object_placement_preview(
                &mut commands,
                None,
                selected_object.clone(),
                transform,
                selected_def,
                &vox_registry,
                &vox_assets,
                &default_material,
            );
        }
    } else {
        trace!("update_world_object_placement_preview: no current placement target");
    }

    for pending in &ui_state.placement.pending {
        let Some(def) = registry.get(&pending.object_id) else {
            trace!(
                "update_world_object_placement_preview: pending object id {:?} is unknown",
                pending.object_id.0
            );
            continue;
        };
        let transform = pending
            .accepted_final_position
            .map(Transform::from_translation)
            .unwrap_or_else(|| preview_transform(def, pending.base_position));
        let mut sequence_entity = None;
        for (entity, mut preview_transform, preview) in &mut preview_query {
            if preview.sequence == Some(pending.sequence) {
                *preview_transform = transform;
                sequence_entity = Some(entity);
            }
        }
        if sequence_entity.is_none() {
            spawn_world_object_placement_preview(
                &mut commands,
                Some(pending.sequence),
                pending.object_id.clone(),
                transform,
                def,
                &vox_registry,
                &vox_assets,
                &default_material,
            );
        }
    }
}

#[cfg(feature = "spawn-panel")]
/// Removes stale local edit previews whose target or pending request no longer exists.
pub fn cleanup_stale_world_object_edit_previews(
    mut commands: Commands,
    ui_state: Res<SpawnPanelUi>,
    target_query: Query<Entity, (With<WorldObjectId>, With<Replicated>)>,
    preview_query: Query<(Entity, &WorldObjectEditPreview)>,
) {
    for (preview_entity, preview) in &preview_query {
        let target_exists = target_query.get(preview.target).is_ok();
        let pending_move = preview.sequence.is_some_and(|sequence| {
            ui_state
                .selection
                .pending_moves
                .iter()
                .any(|pending| pending.sequence == sequence)
        });
        let pending_rotation = preview.sequence.is_some_and(|sequence| {
            ui_state
                .selection
                .pending_rotations
                .iter()
                .any(|pending| pending.sequence == sequence)
        });
        let hover = preview.sequence.is_none() && ui_state.selection.move_armed;
        if !target_exists || (!pending_move && !pending_rotation && !hover) {
            trace!(
                "cleanup_stale_world_object_edit_previews: despawning stale preview {preview_entity:?}"
            );
            commands.entity(preview_entity).despawn();
        }
    }
}

#[cfg(feature = "spawn-panel")]
/// Maintains hover and pending local edit previews from spawn-panel move state.
#[allow(clippy::too_many_arguments)]
fn update_world_object_edit_preview(
    mut commands: Commands,
    ui_state: Res<SpawnPanelUi>,
    registry: Option<Res<WorldObjectDefRegistry>>,
    vox_registry: Res<VoxModelRegistry>,
    vox_assets: Res<Assets<VoxModelAsset>>,
    default_material: Res<DefaultVoxModelMaterial>,
    player_query: Query<&ChunkTicket, (With<Predicted>, With<Controlled>, With<CharacterMarker>)>,
    mut voxel_world: VoxelWorld,
    camera_query: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    window_query: Query<&Window, With<PrimaryWindow>>,
    target_query: Query<
        (Entity, &WorldObjectId, Option<&Position>, Option<&Rotation>),
        With<Replicated>,
    >,
    mut preview_query: Query<(Entity, &mut Transform, &WorldObjectEditPreview)>,
) {
    for (entity, _, preview) in &mut preview_query {
        let target_exists = target_query.get(preview.target).is_ok();
        let still_pending = preview.sequence.is_some_and(|sequence| {
            ui_state
                .selection
                .pending_moves
                .iter()
                .any(|pending| pending.sequence == sequence)
                || ui_state
                    .selection
                    .pending_rotations
                    .iter()
                    .any(|pending| pending.sequence == sequence)
        });
        let hover = preview.sequence.is_none() && ui_state.selection.move_armed;
        if !target_exists || (!still_pending && !hover) {
            trace!("update_world_object_edit_preview: despawning stale edit preview");
            commands.entity(entity).despawn();
        }
    }

    let Some(registry) = registry else {
        trace!("update_world_object_edit_preview: WorldObjectDefRegistry not loaded");
        return;
    };
    let Some(target) = ui_state.selection.selected else {
        trace!("update_world_object_edit_preview: no selected world object");
        return;
    };
    let Ok((_, object_id, current_position, current_rotation)) = target_query.get(target) else {
        trace!("update_world_object_edit_preview: selected object is missing or not replicated");
        return;
    };
    let Some(def) = registry.get(object_id) else {
        trace!("update_world_object_edit_preview: selected object definition missing");
        return;
    };

    if ui_state.selection.move_armed {
        if let Some(final_position) = current_world_object_move_target(
            def,
            &player_query,
            &mut voxel_world,
            &camera_query,
            &window_query,
        ) {
            upsert_world_object_edit_preview(
                &mut commands,
                None,
                target,
                object_id.clone(),
                Transform {
                    translation: final_position,
                    rotation: current_or_pending_world_object_rotation(
                        target,
                        current_rotation,
                        &ui_state,
                    ),
                    ..default()
                },
                def,
                &vox_registry,
                &vox_assets,
                &default_material,
                &mut preview_query,
            );
        } else {
            trace!("update_world_object_edit_preview: no current move target");
        }
    }

    for pending in &ui_state.selection.pending_moves {
        upsert_world_object_edit_preview(
            &mut commands,
            Some(pending.sequence),
            pending.target,
            object_id.clone(),
            Transform {
                translation: pending.final_position,
                rotation: current_or_pending_world_object_rotation(
                    pending.target,
                    current_rotation,
                    &ui_state,
                ),
                ..default()
            },
            def,
            &vox_registry,
            &vox_assets,
            &default_material,
            &mut preview_query,
        );
    }

    if let Some(current_position) = current_position {
        for pending in &ui_state.selection.pending_rotations {
            upsert_world_object_edit_preview(
                &mut commands,
                Some(pending.sequence),
                pending.target,
                object_id.clone(),
                Transform {
                    translation: current_position.0,
                    rotation: pending.rotation,
                    ..default()
                },
                def,
                &vox_registry,
                &vox_assets,
                &default_material,
                &mut preview_query,
            );
        }
    } else {
        trace!(
            "update_world_object_edit_preview: selected object has no Position for rotation preview"
        );
    }
}

#[cfg(feature = "spawn-panel")]
fn current_or_pending_world_object_rotation(
    target: Entity,
    current_rotation: Option<&Rotation>,
    ui_state: &SpawnPanelUi,
) -> Quat {
    ui_state
        .selection
        .pending_rotations
        .iter()
        .rev()
        .find(|pending| pending.target == target)
        .map(|pending| pending.rotation)
        .or_else(|| current_rotation.map(|rotation| rotation.0))
        .unwrap_or(Quat::IDENTITY)
}

#[cfg(feature = "spawn-panel")]
#[allow(clippy::too_many_arguments)]
fn upsert_world_object_edit_preview(
    commands: &mut Commands,
    sequence: Option<u32>,
    target: Entity,
    object_id: WorldObjectId,
    transform: Transform,
    def: &WorldObjectDef,
    vox_registry: &VoxModelRegistry,
    vox_assets: &Assets<VoxModelAsset>,
    default_material: &DefaultVoxModelMaterial,
    preview_query: &mut Query<(Entity, &mut Transform, &WorldObjectEditPreview)>,
) {
    let mut preview_entity = None;
    for (entity, mut preview_transform, preview) in preview_query.iter_mut() {
        if preview.sequence == sequence && preview.target == target {
            *preview_transform = transform;
            preview_entity = Some(entity);
        }
    }
    if preview_entity.is_none() {
        spawn_world_object_edit_preview(
            commands,
            sequence,
            target,
            object_id,
            transform,
            def,
            vox_registry,
            vox_assets,
            default_material,
        );
    }
}

#[cfg(feature = "spawn-panel")]
/// Removes accepted local edit previews once replicated position or rotation matches the accepted edit.
pub fn reconcile_edit_preview_on_transform_replication(
    mut commands: Commands,
    mut ui_state: ResMut<SpawnPanelUi>,
    target_query: Query<
        (Entity, Option<&Position>, Option<&Rotation>),
        (
            With<WorldObjectId>,
            Or<(Changed<Position>, Changed<Rotation>)>,
        ),
    >,
    preview_query: Query<(Entity, &WorldObjectEditPreview, &Transform)>,
) {
    for (target, position, rotation) in &target_query {
        for (preview_entity, preview, preview_transform) in &preview_query {
            let Some(sequence) = preview.sequence else {
                trace!("reconcile_edit_preview_on_transform_replication: skipping hover preview");
                continue;
            };
            if preview.target != target {
                trace!(
                    "reconcile_edit_preview_on_transform_replication: preview target does not match changed target"
                );
                continue;
            }
            let move_matches = position.is_some_and(|position| {
                positions_match(preview_transform.translation, position.0)
                    && ui_state
                        .selection
                        .pending_moves
                        .iter()
                        .any(|pending| pending.sequence == sequence)
            });
            let rotation_matches = rotation.is_some_and(|rotation| {
                rotations_match(preview_transform.rotation, rotation.0)
                    && ui_state
                        .selection
                        .pending_rotations
                        .iter()
                        .any(|pending| pending.sequence == sequence)
            });
            if !move_matches && !rotation_matches {
                trace!(
                    "reconcile_edit_preview_on_transform_replication: preview does not match changed target"
                );
                continue;
            }
            commands.entity(preview_entity).despawn();
            ui_state
                .selection
                .pending_moves
                .retain(|pending| pending.sequence != sequence);
            ui_state
                .selection
                .pending_rotations
                .retain(|pending| pending.sequence != sequence);
        }
    }
}

#[cfg(feature = "spawn-panel")]
/// Removes accepted local previews once the matching replicated object appears.
pub fn reconcile_placement_preview_on_replication(
    mut commands: Commands,
    mut ui_state: ResMut<SpawnPanelUi>,
    replicated_query: Query<(&WorldObjectId, &Position), Added<Replicated>>,
    preview_query: Query<(Entity, &WorldObjectPlacementPreview, &Transform)>,
) {
    for (replicated_id, replicated_position) in &replicated_query {
        let replicated_position = replicated_position.0;
        for (preview_entity, preview, preview_transform) in &preview_query {
            let Some(sequence) = preview.sequence else {
                trace!("reconcile_placement_preview_on_replication: skipping hover preview");
                continue;
            };
            if &preview.object_id != replicated_id {
                trace!(
                    "reconcile_placement_preview_on_replication: preview object id does not match replicated object"
                );
                continue;
            }
            if positions_match(preview_transform.translation, replicated_position) {
                commands.entity(preview_entity).despawn();
                ui_state
                    .placement
                    .pending
                    .retain(|pending| pending.sequence != sequence);
            }
        }
    }
}

#[cfg(feature = "spawn-panel")]
/// Returns true when two world positions are close enough for preview reconciliation.
pub fn positions_match(a: Vec3, b: Vec3) -> bool {
    a.distance_squared(b) <= 0.01 * 0.01
}

#[cfg(feature = "spawn-panel")]
/// Returns true when two rotations are close enough for preview reconciliation.
pub fn rotations_match(a: Quat, b: Quat) -> bool {
    a.dot(b).abs() >= 1.0 - 0.0001
}

fn cursor_screen_position(window_query: &Query<&Window, With<PrimaryWindow>>) -> Option<Vec2> {
    window_query.single().ok()?.cursor_position()
}

fn camera_ray(
    camera_query: &Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    window_query: &Query<&Window, With<PrimaryWindow>>,
) -> Option<Ray3d> {
    let (camera, camera_transform) = camera_query.single().ok()?;
    let cursor_pos = cursor_screen_position(window_query)?;
    let viewport_pos = if let Some(rect) = camera.logical_viewport_rect() {
        cursor_pos - rect.min
    } else {
        cursor_pos
    };

    camera
        .viewport_to_world(camera_transform, viewport_pos)
        .ok()
}

#[cfg(feature = "spawn-panel")]
fn handle_world_object_placement_ack(
    mut receivers: Query<&mut MessageReceiver<WorldObjectPlacementAck>>,
    mut ui_state: ResMut<SpawnPanelUi>,
) {
    for mut receiver in &mut receivers {
        for ack in receiver.receive() {
            let previous_len = ui_state.placement.pending.len();
            ui_state
                .placement
                .pending
                .retain(|pending| pending.sequence != ack.sequence);
            if ui_state.placement.pending.len() == previous_len {
                trace!(
                    "handle_world_object_placement_ack: ack seq={} had no pending placement",
                    ack.sequence
                );
                continue;
            }
            ui_state.placement.last_reject = None;
        }
    }
}

#[cfg(feature = "spawn-panel")]
fn handle_world_object_placement_reject(
    mut receivers: Query<&mut MessageReceiver<WorldObjectPlacementReject>>,
    mut ui_state: ResMut<SpawnPanelUi>,
) {
    for mut receiver in &mut receivers {
        for reject in receiver.receive() {
            trace!(
                "handle_world_object_placement_reject: reject seq={} reason={:?}",
                reject.sequence,
                reject.reason,
            );
            ui_state
                .placement
                .pending
                .retain(|pending| pending.sequence != reject.sequence);
            ui_state.placement.last_reject = Some(reject.reason);
        }
    }
}

#[cfg(feature = "spawn-panel")]
fn handle_world_object_delete_ack(
    mut receivers: Query<&mut MessageReceiver<WorldObjectDeleteAck>>,
    mut ui_state: ResMut<SpawnPanelUi>,
) {
    for mut receiver in &mut receivers {
        for ack in receiver.receive() {
            let previous_len = ui_state.selection.pending_deletes.len();
            ui_state
                .selection
                .pending_deletes
                .retain(|pending| pending.sequence != ack.sequence);
            if ui_state.selection.pending_deletes.len() == previous_len {
                trace!(
                    "handle_world_object_delete_ack: ack seq={} had no pending delete",
                    ack.sequence
                );
                continue;
            }
            ui_state.selection.last_reject = None;
        }
    }
}

#[cfg(feature = "spawn-panel")]
fn handle_world_object_move_ack(
    mut receivers: Query<&mut MessageReceiver<WorldObjectMoveAck>>,
    mut ui_state: ResMut<SpawnPanelUi>,
) {
    for mut receiver in &mut receivers {
        for ack in receiver.receive() {
            let previous_len = ui_state.selection.pending_moves.len();
            ui_state
                .selection
                .pending_moves
                .retain(|pending| pending.sequence != ack.sequence);
            if ui_state.selection.pending_moves.len() == previous_len {
                trace!(
                    "handle_world_object_move_ack: ack seq={} had no pending move",
                    ack.sequence
                );
                continue;
            }
            ui_state.selection.last_reject = None;
            ui_state.selection.move_armed = false;
        }
    }
}

#[cfg(feature = "spawn-panel")]
fn handle_world_object_rotate_ack(
    mut receivers: Query<&mut MessageReceiver<WorldObjectRotateAck>>,
    mut ui_state: ResMut<SpawnPanelUi>,
) {
    for mut receiver in &mut receivers {
        for ack in receiver.receive() {
            let previous_len = ui_state.selection.pending_rotations.len();
            ui_state
                .selection
                .pending_rotations
                .retain(|pending| pending.sequence != ack.sequence);
            if ui_state.selection.pending_rotations.len() == previous_len {
                trace!(
                    "handle_world_object_rotate_ack: ack seq={} had no pending rotation",
                    ack.sequence
                );
                continue;
            }
            ui_state.selection.last_reject = None;
        }
    }
}

#[cfg(feature = "spawn-panel")]
fn handle_world_object_edit_reject(
    mut receivers: Query<&mut MessageReceiver<WorldObjectEditReject>>,
    mut ui_state: ResMut<SpawnPanelUi>,
) {
    for mut receiver in &mut receivers {
        for reject in receiver.receive() {
            ui_state
                .selection
                .pending_deletes
                .retain(|pending| pending.sequence != reject.sequence);
            ui_state
                .selection
                .pending_moves
                .retain(|pending| pending.sequence != reject.sequence);
            ui_state
                .selection
                .pending_rotations
                .retain(|pending| pending.sequence != reject.sequence);
            ui_state.selection.last_reject = Some(reject.reason);
        }
    }
}

/// Processes server acknowledgments, clearing confirmed predictions.
fn handle_voxel_edit_ack(
    mut receivers: Query<&mut MessageReceiver<VoxelEditAck>>,
    mut prediction_state: ResMut<VoxelPredictionState>,
    #[cfg(feature = "spawn-panel")] mut history: ResMut<TerrainEditHistory>,
) {
    for mut receiver in &mut receivers {
        for ack in receiver.receive() {
            trace!(
                "handle_voxel_edit_ack: ack seq={}, clearing {} pending",
                ack.sequence,
                prediction_state.pending.len()
            );
            let Some(index) = prediction_state
                .pending
                .iter()
                .position(|p| p.sequence == ack.sequence && p.map_id == ack.map_id)
            else {
                trace!(
                    "handle_voxel_edit_ack: ack seq={} had no pending prediction",
                    ack.sequence
                );
                continue;
            };
            let prediction = prediction_state.pending.remove(index);
            #[cfg(feature = "spawn-panel")]
            record_acknowledged_prediction(&mut history, &prediction);
        }
    }
}

#[cfg(feature = "spawn-panel")]
fn record_acknowledged_prediction(history: &mut TerrainEditHistory, prediction: &VoxelPrediction) {
    let map_history = history.by_map.entry(prediction.map_id.clone()).or_default();
    match prediction.kind {
        TerrainPredictionKind::NewEdit => {
            push_or_merge_acknowledged_record(
                &mut map_history.undo,
                acknowledged_record(prediction),
            );
            map_history.redo.clear();
        }
        TerrainPredictionKind::Undo => map_history
            .redo
            .push(reversed_acknowledged_record(prediction)),
        TerrainPredictionKind::Redo => map_history.undo.push(acknowledged_record(prediction)),
    }
}

#[cfg(feature = "spawn-panel")]
fn push_or_merge_acknowledged_record(
    records: &mut Vec<TerrainEditRecord>,
    record: TerrainEditRecord,
) {
    let Some(gesture_id) = record.gesture_id else {
        records.push(record);
        return;
    };
    let Some(last) = records
        .last_mut()
        .filter(|last| last.map_id == record.map_id && last.gesture_id == Some(gesture_id))
    else {
        records.push(record);
        return;
    };
    merge_acknowledged_changes(last, record.changes);
    if last.changes.is_empty() {
        records.pop();
    }
}

#[cfg(feature = "spawn-panel")]
fn merge_acknowledged_changes(
    record: &mut TerrainEditRecord,
    changes: Vec<AcknowledgedVoxelChange>,
) {
    for change in changes {
        if let Some(existing) = record
            .changes
            .iter_mut()
            .find(|existing| existing.position == change.position)
        {
            existing.new_voxel = change.new_voxel;
            continue;
        }
        record.changes.push(change);
    }
    record
        .changes
        .retain(|change| change.old_voxel != change.new_voxel);
}

#[cfg(feature = "spawn-panel")]
fn acknowledged_record(prediction: &VoxelPrediction) -> TerrainEditRecord {
    TerrainEditRecord {
        map_id: prediction.map_id.clone(),
        gesture_id: prediction.gesture_id,
        changes: prediction
            .changes
            .iter()
            .map(|change| AcknowledgedVoxelChange {
                position: change.position,
                old_voxel: change.old_voxel,
                new_voxel: change.new_voxel,
            })
            .collect(),
    }
}

#[cfg(feature = "spawn-panel")]
fn reversed_acknowledged_record(prediction: &VoxelPrediction) -> TerrainEditRecord {
    TerrainEditRecord {
        map_id: prediction.map_id.clone(),
        gesture_id: prediction.gesture_id,
        changes: prediction
            .changes
            .iter()
            .map(|change| AcknowledgedVoxelChange {
                position: change.position,
                old_voxel: change.new_voxel,
                new_voxel: change.old_voxel,
            })
            .collect(),
    }
}

#[cfg(feature = "spawn-panel")]
fn inverse_changes(record: &TerrainEditRecord) -> Vec<VoxelChange> {
    record
        .changes
        .iter()
        .map(|change| VoxelChange {
            position: change.position,
            voxel: change.old_voxel,
        })
        .collect()
}

#[cfg(feature = "spawn-panel")]
fn reapply_changes(record: &TerrainEditRecord) -> Vec<VoxelChange> {
    record
        .changes
        .iter()
        .map(|change| VoxelChange {
            position: change.position,
            voxel: change.new_voxel,
        })
        .collect()
}

#[cfg(feature = "spawn-panel")]
fn prediction_from_concrete_changes(
    sequence: u32,
    record: &TerrainEditRecord,
    changes: &[VoxelChange],
    kind: TerrainPredictionKind,
) -> VoxelPrediction {
    let predicted = changes
        .iter()
        .zip(record.changes.iter())
        .map(|(change, acknowledged)| PredictedVoxelChange {
            position: change.position,
            old_voxel: match kind {
                TerrainPredictionKind::Undo => acknowledged.new_voxel,
                TerrainPredictionKind::Redo => acknowledged.old_voxel,
                TerrainPredictionKind::NewEdit => acknowledged.old_voxel,
            },
            new_voxel: change.voxel,
        })
        .collect();
    VoxelPrediction {
        sequence,
        map_id: record.map_id.clone(),
        changes: predicted,
        kind,
        gesture_id: record.gesture_id,
    }
}

#[cfg(feature = "spawn-panel")]
fn handle_terrain_undo_redo_input(
    mut history: ResMut<TerrainEditHistory>,
    mut prediction_state: ResMut<VoxelPredictionState>,
    mut voxel_world: VoxelWorld,
    player_query: Query<
        (&ChunkTicket, &MapInstanceId),
        (With<Predicted>, With<Controlled>, With<CharacterMarker>),
    >,
    mut message_sender: Query<&mut MessageSender<VoxelConcreteEditRequest>>,
) {
    let Ok((chunk_ticket, active_map_id)) = player_query.single() else {
        trace!("handle_terrain_undo_redo_input: no predicted player with map context");
        history.undo_requested = false;
        history.redo_requested = false;
        return;
    };
    let operation = terrain_history_operation(&mut history, active_map_id);

    let Some((kind, record, changes)) = operation else {
        trace!("handle_terrain_undo_redo_input: no undo/redo operation requested");
        return;
    };
    debug_assert_eq!(&record.map_id, active_map_id);
    if changes.is_empty() {
        trace!("handle_terrain_undo_redo_input: requested record has no changes");
        return;
    }

    let sequence = prediction_state.next();
    voxel_world.set_voxels(
        chunk_ticket.map_entity,
        changes
            .iter()
            .map(|change| (change.position, WorldVoxel::from(change.voxel))),
    );
    prediction_state
        .pending
        .push(prediction_from_concrete_changes(
            sequence, &record, &changes, kind,
        ));

    for mut sender in message_sender.iter_mut() {
        sender.send::<VoxelChannel>(VoxelConcreteEditRequest {
            sequence,
            map_id: record.map_id.clone(),
            changes: changes.clone(),
        });
    }
}

#[cfg(feature = "spawn-panel")]
fn terrain_history_operation(
    history: &mut TerrainEditHistory,
    active_map_id: &MapInstanceId,
) -> Option<(TerrainPredictionKind, TerrainEditRecord, Vec<VoxelChange>)> {
    if history.undo_requested {
        history.undo_requested = false;
        let map_history = history.by_map.get_mut(active_map_id)?;
        return map_history.undo.pop().map(|record| {
            debug_assert_eq!(&record.map_id, active_map_id);
            let changes = inverse_changes(&record);
            (TerrainPredictionKind::Undo, record, changes)
        });
    }
    if history.redo_requested {
        history.redo_requested = false;
        let map_history = history.by_map.get_mut(active_map_id)?;
        return map_history.redo.pop().map(|record| {
            debug_assert_eq!(&record.map_id, active_map_id);
            let changes = reapply_changes(&record);
            (TerrainPredictionKind::Redo, record, changes)
        });
    }
    None
}

/// Processes server rejections, rolling back predicted voxels to server state.
fn handle_voxel_edit_reject(
    mut receivers: Query<&mut MessageReceiver<VoxelEditReject>>,
    mut prediction_state: ResMut<VoxelPredictionState>,
    mut voxel_world: VoxelWorld,
    registry: Res<MapRegistry>,
) {
    for mut receiver in &mut receivers {
        for reject in receiver.receive() {
            warn!(
                "handle_voxel_edit_reject: rejected seq={} at {:?}, correct={:?}",
                reject.sequence, reject.position, reject.correct_voxel
            );
            let Some(&map_entity) = registry.0.get(&reject.map_id) else {
                trace!(
                    "handle_voxel_edit_reject: map {:?} not registered",
                    reject.map_id
                );
                continue;
            };
            if rollback_prediction(
                reject.sequence,
                &reject.map_id,
                &mut prediction_state,
                &mut voxel_world,
                map_entity,
            ) {
                continue;
            }
            voxel_world.set_voxel(
                map_entity,
                reject.position,
                WorldVoxel::from(reject.correct_voxel),
            );
        }
    }
}

fn rollback_prediction(
    sequence: u32,
    map_id: &MapInstanceId,
    prediction_state: &mut VoxelPredictionState,
    voxel_world: &mut VoxelWorld,
    map_entity: Entity,
) -> bool {
    let Some(index) = prediction_state
        .pending
        .iter()
        .position(|p| p.sequence == sequence && p.map_id == *map_id)
    else {
        trace!("rollback_prediction: no pending prediction for seq={sequence}");
        return false;
    };
    let prediction = prediction_state.pending.remove(index);
    voxel_world.set_voxels(
        map_entity,
        prediction
            .changes
            .iter()
            .map(|change| (change.position, WorldVoxel::from(change.old_voxel))),
    );
    true
}

#[cfg(test)]
fn rollback_prediction_changes(
    sequence: u32,
    map_id: &MapInstanceId,
    prediction_state: &mut VoxelPredictionState,
) -> Option<Vec<(IVec3, VoxelType)>> {
    let index = prediction_state
        .pending
        .iter()
        .position(|p| p.sequence == sequence && p.map_id == *map_id)?;
    let prediction = prediction_state.pending.remove(index);
    Some(
        prediction
            .changes
            .into_iter()
            .map(|change| (change.position, change.old_voxel))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prediction(sequence: u32, positions: impl IntoIterator<Item = IVec3>) -> VoxelPrediction {
        VoxelPrediction {
            sequence,
            map_id: MapInstanceId::Overworld,
            changes: positions
                .into_iter()
                .map(|position| PredictedVoxelChange {
                    position,
                    old_voxel: VoxelType::Air,
                    new_voxel: VoxelType::Solid(1),
                })
                .collect(),
            kind: TerrainPredictionKind::NewEdit,
            gesture_id: None,
        }
    }

    #[test]
    fn prediction_state_sequence_increments() {
        let mut state = VoxelPredictionState::default();
        assert_eq!(state.next(), 0);
        assert_eq!(state.next(), 1);
        assert_eq!(state.next(), 2);
    }

    #[test]
    fn ack_clears_matching_prediction_sequence() {
        let mut state = VoxelPredictionState::default();
        for i in 0..5 {
            state.pending.push(prediction(i, [IVec3::ZERO]));
        }
        state.pending.retain(|p| p.sequence != 2);
        assert_eq!(state.pending.len(), 4);
        assert!(state.pending.iter().all(|p| p.sequence != 2));
        assert!(state.pending.iter().any(|p| p.sequence == 0));
    }

    #[test]
    fn broadcast_skipped_for_any_pending_prediction_position() {
        let mut state = VoxelPredictionState::default();
        state.pending.push(prediction(
            0,
            [IVec3::new(5, 10, 15), IVec3::new(6, 10, 15)],
        ));

        assert!(has_pending_prediction_at(
            &state,
            &MapInstanceId::Overworld,
            IVec3::new(6, 10, 15)
        ));
        assert!(!has_pending_prediction_at(
            &state,
            &MapInstanceId::Overworld,
            IVec3::new(1, 2, 3)
        ));
    }

    #[test]
    fn stroke_state_suppresses_duplicate_discrete_cursor_position() {
        let mut state = TerrainBrushStrokeState {
            active: true,
            last_cursor_position: Some(Vec2::new(12.0, 4.0)),
            ..default()
        };
        let settings = TerrainBrushSettings {
            stroke_mode: TerrainBrushStrokeMode::Discrete,
            ..default()
        };

        assert!(should_suppress_brush_repeat(
            &settings,
            &mut state,
            Vec2::new(12.1, 4.1)
        ));
        assert!(!should_suppress_brush_repeat(
            &settings,
            &mut state,
            Vec2::new(13.0, 4.0)
        ));
    }

    #[test]
    fn reject_rolls_back_all_changes_in_prediction() {
        let mut state = VoxelPredictionState::default();
        state.pending.push(VoxelPrediction {
            sequence: 9,
            map_id: MapInstanceId::Overworld,
            changes: vec![
                PredictedVoxelChange {
                    position: IVec3::new(1, 0, 0),
                    old_voxel: VoxelType::Air,
                    new_voxel: VoxelType::Solid(1),
                },
                PredictedVoxelChange {
                    position: IVec3::new(2, 0, 0),
                    old_voxel: VoxelType::Solid(7),
                    new_voxel: VoxelType::Solid(2),
                },
            ],
            kind: TerrainPredictionKind::NewEdit,
            gesture_id: None,
        });

        assert_eq!(
            rollback_prediction_changes(9, &MapInstanceId::Overworld, &mut state),
            Some(vec![
                (IVec3::new(1, 0, 0), VoxelType::Air),
                (IVec3::new(2, 0, 0), VoxelType::Solid(7)),
            ])
        );
        assert!(state.pending.is_empty());
    }

    #[test]
    fn paint_prediction_recolors_only_existing_solids() {
        assert_eq!(
            predicted_voxel_for_brush_mode(
                WorldVoxel::Solid(1),
                TerrainBrushMode::PaintExisting,
                7
            ),
            Some(VoxelType::Solid(7))
        );
        assert_eq!(
            predicted_voxel_for_brush_mode(WorldVoxel::Air, TerrainBrushMode::PaintExisting, 7),
            None
        );
    }

    #[test]
    fn paint_prediction_replace_all_writes_air_and_solids() {
        assert_eq!(
            predicted_voxel_for_brush_mode(WorldVoxel::Air, TerrainBrushMode::ReplaceAll, 9),
            Some(VoxelType::Solid(9))
        );
        assert_eq!(
            predicted_voxel_for_brush_mode(WorldVoxel::Solid(2), TerrainBrushMode::ReplaceAll, 9),
            Some(VoxelType::Solid(9))
        );
    }

    fn terrain_record() -> TerrainEditRecord {
        TerrainEditRecord {
            map_id: MapInstanceId::Overworld,
            gesture_id: None,
            changes: vec![AcknowledgedVoxelChange {
                position: IVec3::new(1, 2, 3),
                old_voxel: VoxelType::Air,
                new_voxel: VoxelType::Solid(4),
            }],
        }
    }

    fn homebase_map_id() -> MapInstanceId {
        MapInstanceId::Homebase {
            owner: protocol::NostrPublicKey([7; 32]),
        }
    }

    #[test]
    fn terrain_history_operation_is_scoped_to_active_map() {
        let homebase = homebase_map_id();
        let mut history = TerrainEditHistory::default();
        history.undo_requested = true;
        history
            .by_map
            .entry(MapInstanceId::Overworld)
            .or_default()
            .undo
            .push(terrain_record());

        assert!(terrain_history_operation(&mut history, &homebase).is_none());
        assert_eq!(
            history
                .by_map
                .get(&MapInstanceId::Overworld)
                .expect("overworld history should remain present")
                .undo
                .len(),
            1
        );
    }

    #[test]
    fn acknowledged_predictions_record_into_their_own_map_history() {
        let homebase = homebase_map_id();
        let mut history = TerrainEditHistory::default();
        let mut prediction = prediction(10, [IVec3::new(3, 0, 0)]);
        prediction.map_id = homebase.clone();

        record_acknowledged_prediction(&mut history, &prediction);

        assert_eq!(history.active_map, None);
        assert!(history.by_map.get(&MapInstanceId::Overworld).is_none());
        assert_eq!(
            history
                .by_map
                .get(&homebase)
                .expect("homebase history should be created")
                .undo
                .len(),
            1
        );
    }

    #[test]
    fn acknowledged_predictions_do_not_replace_synced_active_map() {
        let homebase = homebase_map_id();
        let mut history = TerrainEditHistory {
            active_map: Some(homebase.clone()),
            ..default()
        };
        let prediction = prediction(10, [IVec3::new(3, 0, 0)]);

        record_acknowledged_prediction(&mut history, &prediction);

        assert_eq!(history.active_map, Some(homebase));
        assert_eq!(
            history
                .by_map
                .get(&MapInstanceId::Overworld)
                .expect("overworld history should be created")
                .undo
                .len(),
            1
        );
    }

    #[test]
    fn acknowledged_predictions_from_one_gesture_share_one_undo_record() {
        let mut history = TerrainEditHistory::default();
        let mut first = prediction(10, [IVec3::new(1, 0, 0)]);
        first.gesture_id = Some(7);
        let mut second = prediction(11, [IVec3::new(2, 0, 0)]);
        second.gesture_id = Some(7);

        record_acknowledged_prediction(&mut history, &first);
        record_acknowledged_prediction(&mut history, &second);

        let undo = &history
            .by_map
            .get(&MapInstanceId::Overworld)
            .expect("overworld history should exist")
            .undo;
        assert_eq!(undo.len(), 1);
        assert_eq!(undo[0].changes.len(), 2);
    }

    #[test]
    fn gesture_history_keeps_original_old_voxel_and_final_new_voxel() {
        let mut history = TerrainEditHistory::default();
        let mut first = prediction(10, [IVec3::new(1, 0, 0)]);
        first.gesture_id = Some(7);
        let mut second = prediction(11, [IVec3::new(1, 0, 0)]);
        second.gesture_id = Some(7);
        second.changes[0].old_voxel = VoxelType::Solid(1);
        second.changes[0].new_voxel = VoxelType::Solid(9);

        record_acknowledged_prediction(&mut history, &first);
        record_acknowledged_prediction(&mut history, &second);

        let undo = &history
            .by_map
            .get(&MapInstanceId::Overworld)
            .expect("overworld history should exist")
            .undo;
        assert_eq!(undo.len(), 1);
        assert_eq!(undo[0].changes.len(), 1);
        assert_eq!(undo[0].changes[0].old_voxel, VoxelType::Air);
        assert_eq!(undo[0].changes[0].new_voxel, VoxelType::Solid(9));
    }

    #[test]
    fn undo_inverse_changes_restore_old_voxels() {
        assert_eq!(
            inverse_changes(&terrain_record()),
            vec![VoxelChange {
                position: IVec3::new(1, 2, 3),
                voxel: VoxelType::Air,
            }]
        );
    }

    #[test]
    fn undo_reapply_changes_restore_new_voxels() {
        assert_eq!(
            reapply_changes(&terrain_record()),
            vec![VoxelChange {
                position: IVec3::new(1, 2, 3),
                voxel: VoxelType::Solid(4),
            }]
        );
    }

    #[test]
    fn undo_ack_preserves_original_record_for_redo() {
        let record = terrain_record();
        let changes = inverse_changes(&record);
        let prediction =
            prediction_from_concrete_changes(7, &record, &changes, TerrainPredictionKind::Undo);

        assert_eq!(reversed_acknowledged_record(&prediction), record);
    }

    #[test]
    fn projected_top_hit_face_preserves_locked_y_anchor() {
        assert_eq!(
            anchor_from_projected_plane_point(
                Vec3::new(4.0, 9.8, 3.2),
                TerrainBrushPlaneLock {
                    point: Vec3::new(0.5, 2.5, 0.5),
                    normal: Vec3::Y,
                    base_anchor: IVec3::new(0, 2, 0),
                },
            ),
            IVec3::new(4, 2, 3)
        );
    }

    #[test]
    fn projected_side_hit_face_preserves_locked_x_anchor() {
        assert_eq!(
            anchor_from_projected_plane_point(
                Vec3::new(9.0, 2.8, 3.2),
                TerrainBrushPlaneLock {
                    point: Vec3::new(4.5, 0.5, 0.5),
                    normal: Vec3::X,
                    base_anchor: IVec3::new(4, 0, 0),
                },
            ),
            IVec3::new(4, 2, 3)
        );
    }

    #[test]
    fn continuous_direction_follows_mode_write_direction() {
        assert_eq!(
            continuous_brush_direction(TerrainBrushMode::FillAir, Vec3::Y),
            Some(IVec3::Y)
        );
        assert_eq!(
            continuous_brush_direction(TerrainBrushMode::ReplaceAll, Vec3::Y),
            Some(IVec3::Y)
        );
        assert_eq!(
            continuous_brush_direction(TerrainBrushMode::Remove, Vec3::Y),
            Some(-IVec3::Y)
        );
        assert_eq!(
            continuous_brush_direction(TerrainBrushMode::PaintExisting, Vec3::Y),
            Some(-IVec3::Y)
        );
    }

    #[test]
    fn reject_removes_specific_prediction() {
        let mut state = VoxelPredictionState::default();
        for i in 0..5 {
            state
                .pending
                .push(prediction(i, [IVec3::new(i as i32, 0, 0)]));
        }

        let rejected_seq = 2u32;
        state.pending.retain(|p| p.sequence != rejected_seq);

        assert_eq!(state.pending.len(), 4);
        assert!(state.pending.iter().all(|p| p.sequence != 2));
        assert!(state.pending.iter().any(|p| p.sequence == 0));
        assert!(state.pending.iter().any(|p| p.sequence == 1));
        assert!(state.pending.iter().any(|p| p.sequence == 3));
        assert!(state.pending.iter().any(|p| p.sequence == 4));
    }
}
