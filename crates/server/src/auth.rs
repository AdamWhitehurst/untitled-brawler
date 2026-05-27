use bevy::prelude::*;
use lightyear::connection::client::Disconnected;
use lightyear::prelude::server::ClientOf;
use lightyear::prelude::*;
use protocol::{IdentityProof, PlayerIdentity};
use std::time::Instant;

#[derive(Component, Debug)]
pub struct PendingAuth {
    pub nonce: [u8; 32],
    pub issued_at: Instant,
}

pub fn cleanup_pending_auth_on_disconnect(
    trigger: On<Add, Disconnected>,
    pending: Query<(), With<PendingAuth>>,
) {
    if pending.get(trigger.entity).is_ok() {
        info!(
            client = ?trigger.entity,
            "client disconnected during identity challenge"
        );
    }
}

pub fn handle_identity_proof(
    mut commands: Commands,
    mut proof_receivers: Query<
        (
            Entity,
            &RemoteId,
            &PendingAuth,
            &mut MessageReceiver<IdentityProof>,
        ),
        With<ClientOf>,
    >,
    time: Res<Time>,
) {
    let mut outcomes = Vec::new();

    for (client_entity, remote_id, pending, mut receiver) in &mut proof_receivers {
        let Some(proof) = receiver.receive().next() else {
            continue;
        };

        let result = validate_identity_proof(&proof, pending, *remote_id);
        outcomes.push((client_entity, *remote_id, result));
    }

    for (client_entity, remote_id, result) in outcomes {
        match result {
            Ok(player_identity) => {
                info!(
                    ?client_entity,
                    ?player_identity,
                    "verified client identity proof"
                );
                commands.entity(client_entity).remove::<PendingAuth>();
                crate::gameplay::queue_authenticated_initial_spawn(
                    &mut commands,
                    client_entity,
                    remote_id,
                    player_identity,
                    time.elapsed_secs_f64(),
                );
            }
            Err(error) => {
                warn!(?client_entity, %error, "identity proof validation failed");
                commands.entity(client_entity).remove::<PendingAuth>();
                commands.trigger(Disconnect {
                    entity: client_entity,
                });
            }
        }
    }
}

fn validate_identity_proof(
    proof: &IdentityProof,
    pending: &PendingAuth,
    remote_id: RemoteId,
) -> Result<PlayerIdentity, String> {
    nostr_client::verify_identity_proof(proof, pending.nonce, remote_id.0.to_bits())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_client::{build_identity_proof, generate_encrypted_identity, ClientIdentity};

    fn identity_and_proof(nonce: [u8; 32]) -> (ClientIdentity, IdentityProof) {
        let (identity, _encrypted) = generate_encrypted_identity("test passphrase").unwrap();
        let proof = build_identity_proof(&identity, nonce).unwrap();
        (identity, proof)
    }

    fn remote_id_for(proof: &IdentityProof) -> RemoteId {
        RemoteId(PeerId::Netcode(proof.pubkey.client_id_prefix()))
    }

    fn pending(nonce: [u8; 32]) -> PendingAuth {
        PendingAuth {
            nonce,
            issued_at: Instant::now(),
        }
    }

    fn replace_json_string_field(json: &str, field: &str, value: &str) -> String {
        let needle = format!("\"{field}\":\"");
        let value_start = json.find(&needle).expect("field should exist") + needle.len();
        let value_end = value_start + json[value_start..].find('"').expect("field should end");
        format!("{}{}{}", &json[..value_start], value, &json[value_end..])
    }

    #[test]
    fn valid_proof_returns_player_identity() {
        let (_identity, proof) = identity_and_proof([1; 32]);

        let player_identity =
            validate_identity_proof(&proof, &pending([1; 32]), remote_id_for(&proof)).unwrap();

        assert_eq!(player_identity, PlayerIdentity(proof.pubkey));
    }

    #[test]
    fn wrong_nonce_is_rejected() {
        let (_identity, proof) = identity_and_proof([1; 32]);

        let error =
            validate_identity_proof(&proof, &pending([2; 32]), remote_id_for(&proof)).unwrap_err();

        assert!(error.contains("nonce"));
    }

    #[test]
    fn wrong_pubkey_is_rejected() {
        let (_identity, mut proof) = identity_and_proof([1; 32]);
        let remote_id = remote_id_for(&proof);
        proof.pubkey = protocol::NostrPublicKey([9; 32]);

        let error = validate_identity_proof(&proof, &pending([1; 32]), remote_id).unwrap_err();

        assert!(error.contains("pubkey"));
    }

    #[test]
    fn invalid_signature_is_rejected() {
        let (_identity, mut proof) = identity_and_proof([1; 32]);
        proof.signed_event_json =
            replace_json_string_field(&proof.signed_event_json, "sig", &"00".repeat(64));

        let error =
            validate_identity_proof(&proof, &pending([1; 32]), remote_id_for(&proof)).unwrap_err();

        assert!(error.contains("signature") || error.contains("invalid Nostr event JSON"));
    }

    #[test]
    fn pubkey_client_id_mismatch_is_rejected() {
        let (_identity, proof) = identity_and_proof([1; 32]);
        let mismatched_remote_id = RemoteId(PeerId::Netcode(
            proof.pubkey.client_id_prefix().wrapping_add(1),
        ));

        let error =
            validate_identity_proof(&proof, &pending([1; 32]), mismatched_remote_id).unwrap_err();

        assert!(error.contains("client_id mismatch"));
    }
}
