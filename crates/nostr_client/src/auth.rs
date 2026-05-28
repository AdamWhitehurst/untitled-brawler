use nostr_sdk::{Event, EventBuilder, JsonUtil, Kind, Tag, TagKind};
use protocol::{IdentityProof, NostrPublicKey, PlayerIdentity};

use crate::NostrKeys;

pub const NOSTR_KIND_AUTH: u16 = 22242;

pub fn build_identity_proof(
    identity: &NostrKeys,
    nonce: [u8; 32],
) -> Result<IdentityProof, String> {
    let event = EventBuilder::new(Kind::Custom(NOSTR_KIND_AUTH), "")
        .tag(Tag::custom(
            TagKind::custom("challenge"),
            [hex::encode(nonce)],
        ))
        .sign_with_keys(identity.keys())
        .map_err(|error| format!("sign identity proof: {error}"))?;

    Ok(IdentityProof {
        pubkey: identity.protocol_public_key(),
        signed_event_json: event.as_json(),
    })
}

pub fn verify_identity_proof(
    proof: &IdentityProof,
    expected_nonce: [u8; 32],
    expected_client_id: u64,
) -> Result<PlayerIdentity, String> {
    let event = Event::from_json(&proof.signed_event_json)
        .map_err(|error| format!("invalid Nostr event JSON: {error}"))?;
    if event.kind != Kind::Custom(NOSTR_KIND_AUTH) {
        return Err(format!(
            "identity proof event kind must be {NOSTR_KIND_AUTH}, got {}",
            event.kind
        ));
    }
    if !event.verify_signature() {
        return Err("identity proof signature verification failed".to_string());
    }

    let event_pubkey = NostrPublicKey(*event.pubkey.as_bytes());
    if event_pubkey != proof.pubkey {
        return Err("identity proof pubkey does not match signed event pubkey".to_string());
    }
    if !event_has_nonce(&event, expected_nonce) {
        return Err("identity proof nonce tag mismatch".to_string());
    }
    if proof.pubkey.client_id_prefix() != expected_client_id {
        return Err(format!(
            "identity proof pubkey/client_id mismatch: proof={} remote={}",
            proof.pubkey.client_id_prefix(),
            expected_client_id
        ));
    }

    Ok(PlayerIdentity(proof.pubkey))
}

fn event_has_nonce(event: &Event, nonce: [u8; 32]) -> bool {
    let expected = hex::encode(nonce);
    event.tags.iter().any(|tag| {
        tag.as_slice().first().map(String::as_str) == Some("challenge")
            && tag.as_slice().get(1).map(String::as_str) == Some(expected.as_str())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::SecretKey;

    fn identity() -> NostrKeys {
        NostrKeys::from_secret(SecretKey::generate())
    }

    #[test]
    fn identity_proof_roundtrips() {
        let identity = identity();
        let nonce = [7; 32];
        let proof = build_identity_proof(&identity, nonce).unwrap();

        let player_identity =
            verify_identity_proof(&proof, nonce, proof.pubkey.client_id_prefix()).unwrap();

        assert_eq!(player_identity.0, proof.pubkey);
    }

    #[test]
    fn identity_proof_rejects_wrong_nonce() {
        let identity = identity();
        let proof = build_identity_proof(&identity, [7; 32]).unwrap();

        let error =
            verify_identity_proof(&proof, [8; 32], proof.pubkey.client_id_prefix()).unwrap_err();

        assert!(error.contains("nonce"));
    }
}
