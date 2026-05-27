use bevy::prelude::*;
use lightyear::prelude::{ControlledBy, MessageReceiver};
use protocol::map::{MapSwitchTarget, PlayerMapSwitchRequest};
use protocol::{CharacterMarker, MapInstanceId, NostrPublicKey, PendingTransition, PlayerIdentity};

use super::{
    MapPreflightKind, PendingMapPreflight, PendingMapPreflights, PendingMapSwitchPreflight,
};

/// Captures map switch requests as persistence preflight intents without relocating the player.
pub fn handle_map_switch_requests(
    mut commands: Commands,
    mut receivers: Query<(Entity, &mut MessageReceiver<PlayerMapSwitchRequest>)>,
    controlled_query: Query<(Entity, &ControlledBy, &MapInstanceId), With<CharacterMarker>>,
    pending_transition: Query<(), With<PendingTransition>>,
    pending_preflight: Query<&PendingMapSwitchPreflight>,
    player_identities: Query<&PlayerIdentity>,
    mut queue: ResMut<PendingMapPreflights>,
    time: Res<Time>,
) {
    for (client_entity, mut receiver) in &mut receivers {
        for request in receiver.receive() {
            trace!(
                ?request,
                ?client_entity,
                "received map switch preflight request"
            );
            let (player_entity, _controlled_by, current_map_id) = controlled_query
                .iter()
                .find(|(_, ctrl, _)| ctrl.owner == client_entity)
                .unwrap_or_else(|| {
                    panic!(
                        "No character entity found for client {client_entity:?} during map switch"
                    )
                });

            if pending_transition.get(player_entity).is_ok() {
                trace!(
                    ?player_entity,
                    "player already transitioning; ignoring map switch request"
                );
                continue;
            }
            if pending_preflight.get(player_entity).is_ok() {
                trace!(
                    ?player_entity,
                    "player already has pending map preflight; ignoring duplicate request"
                );
                continue;
            }

            let identity = player_identities
                .get(client_entity)
                .expect("Authenticated client must have PlayerIdentity before map switch");
            let target_map_id = resolve_switch_target(&request.target, identity.0);

            if *current_map_id == target_map_id {
                trace!(
                    ?player_entity,
                    ?target_map_id,
                    "player already on target map"
                );
                continue;
            }

            let requested_at = time.elapsed_secs_f64();
            commands
                .entity(player_entity)
                .insert(PendingMapSwitchPreflight {
                    target_map_id: target_map_id.clone(),
                    requested_at,
                });
            queue.0.push_back(PendingMapPreflight {
                target_map_id,
                kind: MapPreflightKind::MapSwitch {
                    client_entity,
                    player_entity,
                    current_map_id: current_map_id.clone(),
                    requested_at,
                },
            });
        }
    }
}

/// Resolves a `MapSwitchTarget` to a `MapInstanceId` using the authenticated player's public key.
pub fn resolve_switch_target(target: &MapSwitchTarget, owner: NostrPublicKey) -> MapInstanceId {
    match target {
        MapSwitchTarget::Overworld => MapInstanceId::Overworld,
        MapSwitchTarget::Homebase => MapInstanceId::Homebase { owner },
    }
}
