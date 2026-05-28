use bevy::prelude::*;
use lightyear::prelude::*;
use nostr_client::{build_identity_proof, NostrKeys};
use protocol::{AuthChannel, IdentityChallenge, IdentityProof};

pub struct ClientAuthPlugin;

impl Plugin for ClientAuthPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            handle_identity_challenge.run_if(resource_exists::<NostrKeys>),
        );
    }
}

fn handle_identity_challenge(
    identity: Res<NostrKeys>,
    mut receivers: Query<&mut MessageReceiver<IdentityChallenge>>,
    mut senders: Query<&mut MessageSender<IdentityProof>>,
) {
    for mut receiver in &mut receivers {
        for challenge in receiver.receive() {
            let proof = build_identity_proof(&identity, challenge.nonce)
                .expect("NostrKeys should sign identity proof");
            for mut sender in &mut senders {
                sender.send::<AuthChannel>(proof.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_client::generate_encrypted_identity;

    #[test]
    fn client_proof_json_includes_auth_kind_and_challenge_tag() {
        let (identity, _encrypted) = generate_encrypted_identity("test passphrase").unwrap();
        let nonce = [0x2a; 32];
        let proof = build_identity_proof(&identity, nonce).unwrap();

        let expected_nonce = "2a".repeat(32);

        assert!(proof.signed_event_json.contains("\"kind\":22242"));
        assert!(proof
            .signed_event_json
            .contains(&format!("[\"challenge\",\"{expected_nonce}\"]")));
    }
}
