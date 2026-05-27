use avian3d::prelude::{ColliderDisabled, LinearVelocity, Position, RigidBodyDisabled};
use bevy::prelude::*;
use lightyear::prelude::*;
use protocol::map::*;
use protocol::transition::*;
use protocol::{MapInstanceId, RespawnPoint};
use voxel_map_engine::prelude::ChunkTicket;

use crate::map::{MapTransitionParams, RoomRegistry};

/// Commits a preflight-approved map transition by relocating/freezing and sending `MapTransitionStart`.
#[allow(clippy::too_many_arguments)]
pub fn commit_map_transition(
    commands: &mut Commands,
    player_entity: Entity,
    client_entity: Entity,
    current_map_id: &MapInstanceId,
    target_map_id: &MapInstanceId,
    prepared_map: Entity,
    params: MapTransitionParams,
    room_registry: &mut RoomRegistry,
    senders: &mut Query<&mut MessageSender<MapTransitionStart>>,
    respawn_query: &Query<(&Position, &MapInstanceId), With<RespawnPoint>>,
) {
    trace!("Committing preflight-approved transition for player {player_entity:?} from {current_map_id:?} to {target_map_id:?}");

    let old_room = room_registry.get_or_create(current_map_id, commands);
    let new_room = room_registry.get_or_create(target_map_id, commands);
    commands.trigger(RoomEvent {
        room: old_room,
        target: RoomTarget::RemoveSender(client_entity),
    });

    let spawn_position = respawn_query
        .iter()
        .find(|(_, mid)| *mid == target_map_id)
        .map(|(pos, _)| pos.0)
        .unwrap_or(crate::gameplay::DEFAULT_SPAWN_POS);

    relocation::relocate_remove(
        commands,
        player_entity,
        old_room,
        target_map_id,
        Some(spawn_position),
    );
    commands.entity(player_entity).insert((
        LinearVelocity(Vec3::ZERO),
        DisableRollback,
        ColliderDisabled,
        RigidBodyDisabled,
        PendingTransition(target_map_id.clone()),
        ChunkTicket::player(prepared_map),
    ));

    let mut sender = senders
        .get_mut(client_entity)
        .expect("transitioning client must have MapTransitionStart sender");
    sender.send::<MapChannel>(MapTransitionStart {
        target: target_map_id.clone(),
        seed: params.seed,
        generation_version: params.generation_version,
        bounds: params.bounds,
        spawn_position,
        chunk_size: params.chunk_size,
        column_y_range: params.column_y_range,
        readiness_radius: TRANSITION_READINESS_RADIUS,
    });
    commands.entity(player_entity).insert(TransitionPending {
        client_entity,
        target_map_id: target_map_id.clone(),
        new_room,
        relocated_entities: vec![player_entity],
    });
}

/// Server Phase 2: On MapTransitionReady from client, add sender to new room,
/// unfreeze + AddEntity for relocated entities, send MapTransitionEnd.
pub fn complete_map_transition(
    mut commands: Commands,
    mut receivers: Query<(Entity, &mut MessageReceiver<MapTransitionReady>)>,
    transition_query: Query<(Entity, &TransitionPending)>,
    mut end_senders: Query<&mut MessageSender<MapTransitionEnd>>,
    mut entity_senders: Query<&mut MessageSender<MapTransitionEntity>>,
) {
    for (client_entity, mut receiver) in &mut receivers {
        for _ready in receiver.receive() {
            trace!("complete_map_transition: received MapTransitionReady from {client_entity:?}");
            let Some((player_entity, pending)) = transition_query
                .iter()
                .find(|(_, p)| p.client_entity == client_entity)
            else {
                warn!("MapTransitionReady from {client_entity:?} but no TransitionPending");
                continue;
            };

            trace!("Phase 2: Completing transition for client {client_entity:?}");

            // Add client sender to new room
            commands.trigger(RoomEvent {
                room: pending.new_room,
                target: RoomTarget::AddSender(client_entity),
            });

            // Unfreeze and add each relocated entity to new room
            for &entity in &pending.relocated_entities {
                commands.entity(entity).remove::<(
                    RigidBodyDisabled,
                    ColliderDisabled,
                    DisableRollback,
                    PendingTransition,
                )>();
                relocation::relocate_add(&mut commands, entity, pending.new_room);

                // Send raw server entity ID to client
                if let Ok(mut sender) = entity_senders.get_mut(client_entity) {
                    sender.send::<MapChannel>(MapTransitionEntity { entity });
                }
            }

            // Send MapTransitionEnd
            if let Ok(mut sender) = end_senders.get_mut(client_entity) {
                sender.send::<MapChannel>(MapTransitionEnd);
            }

            commands.entity(player_entity).remove::<TransitionPending>();
        }
    }
}
