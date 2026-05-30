use nostr_client::{verify_payload_schnorr, NostrKeys};
use nostr_map_persistence::attestation::{
    sign_homebase_attestation, AttestationSigner, AttestationVerifier,
};
use nostr_map_persistence::{
    validate_homebase_manifest_attestation, HomebasePayloadScope, HomebasePublicationAttestation,
    MapPersistenceRejection, NostrMapManifest,
};
use protocol::{MapInstanceId, NostrPublicKey};

/// Signs homebase publication attestations with the server identity.
pub struct ServerAttestationSigner<'a>(pub &'a NostrKeys);

impl AttestationSigner for ServerAttestationSigner<'_> {
    fn public_key(&self) -> NostrPublicKey {
        self.0.protocol_public_key()
    }

    fn sign_attestation_payload(&self, payload: &[u8]) -> Result<Vec<u8>, MapPersistenceRejection> {
        Ok(self.0.sign_payload_schnorr(payload))
    }
}

/// Verifies homebase attestation signatures against the signing server key.
pub struct ServerAttestationVerifier;

impl AttestationVerifier for ServerAttestationVerifier {
    fn verify_attestation_payload(
        &self,
        pubkey: NostrPublicKey,
        payload: &[u8],
        signature: &[u8],
    ) -> Result<(), MapPersistenceRejection> {
        verify_payload_schnorr(pubkey, payload, signature).map_err(|error| {
            MapPersistenceRejection::Invalid(format!("attestation signature invalid: {error}"))
        })
    }
}

/// Authoritative server-side homebase state an attestation request is validated against.
#[derive(Clone, Debug)]
pub struct AuthoritativeHomebaseState {
    pub owner: NostrPublicKey,
    pub map_id: MapInstanceId,
    pub server_revision: u64,
    pub previous_manifest_hash: Option<[u8; 32]>,
    pub descriptor_root: [u8; 32],
    pub payload_scope: HomebasePayloadScope,
}

/// Validates a client attestation request against authoritative homebase state and,
/// on success, returns a server-signed attestation.
///
/// Only homebase maps owned by the requesting player are attested; overworld and
/// foreign-owner requests are rejected.
#[allow(clippy::too_many_arguments)]
pub fn verify_homebase_publication_attestation_request(
    signer: &impl AttestationSigner,
    owner: NostrPublicKey,
    map_id: &MapInstanceId,
    descriptor_root: [u8; 32],
    payload_scope: &HomebasePayloadScope,
    authoritative_state: &AuthoritativeHomebaseState,
    now_unix: u64,
    ttl_seconds: u64,
) -> Result<HomebasePublicationAttestation, MapPersistenceRejection> {
    let MapInstanceId::Homebase { owner: map_owner } = map_id else {
        return Err(MapPersistenceRejection::Invalid(
            "server only attests homebase publication requests".into(),
        ));
    };
    if *map_owner != owner {
        return Err(MapPersistenceRejection::Invalid(
            "homebase attestation owner does not match map owner".into(),
        ));
    }
    if authoritative_state.owner != owner || authoritative_state.map_id != *map_id {
        return Err(MapPersistenceRejection::Invalid(
            "attestation request does not match authoritative homebase state".into(),
        ));
    }
    if authoritative_state.descriptor_root != descriptor_root {
        return Err(MapPersistenceRejection::Invalid(
            "attestation descriptor root does not match authoritative state".into(),
        ));
    }
    if authoritative_state.payload_scope != *payload_scope {
        return Err(MapPersistenceRejection::Incomplete(
            "attestation payload scope does not match authoritative state".into(),
        ));
    }
    let expires_at = now_unix
        .checked_add(ttl_seconds)
        .ok_or_else(|| MapPersistenceRejection::Invalid("attestation expiry overflow".into()))?;

    sign_homebase_attestation(
        signer,
        HomebasePublicationAttestation {
            owner,
            map_id: map_id.clone(),
            server_revision: authoritative_state.server_revision,
            previous_manifest_hash: authoritative_state.previous_manifest_hash,
            descriptor_root,
            payload_scope: payload_scope.clone(),
            expires_at,
            server_pubkey: signer.public_key(),
            server_signature: Vec::new(),
        },
    )
}

/// Server boundary for accepting a client-published homebase manifest: verifies the
/// player signature is already checked upstream, then enforces the server attestation gate.
///
/// Progression-bearing-data rejection and entitlement enforcement (plan 5.7) are a
/// deferred follow-up and are NOT applied here.
pub fn validate_homebase_manifest_import(
    manifest: &NostrMapManifest,
    now_unix: u64,
) -> Result<(), MapPersistenceRejection> {
    validate_homebase_manifest_attestation(&ServerAttestationVerifier, manifest, now_unix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_client::NostrKeys;
    use nostr_map_persistence::attestation::verify_homebase_attestation;

    fn server_keys() -> NostrKeys {
        NostrKeys::generate()
    }

    fn authoritative(owner: NostrPublicKey) -> AuthoritativeHomebaseState {
        AuthoritativeHomebaseState {
            owner,
            map_id: MapInstanceId::Homebase { owner },
            server_revision: 3,
            previous_manifest_hash: Some([4; 32]),
            descriptor_root: [5; 32],
            payload_scope: HomebasePayloadScope::default(),
        }
    }

    #[test]
    fn signs_and_verifies_matching_request() {
        let keys = server_keys();
        let owner = NostrPublicKey([42; 32]);
        let state = authoritative(owner);
        let attestation = verify_homebase_publication_attestation_request(
            &ServerAttestationSigner(&keys),
            owner,
            &MapInstanceId::Homebase { owner },
            state.descriptor_root,
            &state.payload_scope,
            &state,
            1_000,
            600,
        )
        .expect("attestation issued");

        assert_eq!(attestation.expires_at, 1_600);
        assert_eq!(attestation.server_pubkey, keys.protocol_public_key());
        verify_homebase_attestation(&ServerAttestationVerifier, &attestation, 1_200)
            .expect("server-signed attestation verifies");
    }

    #[test]
    fn rejects_overworld_request() {
        let keys = server_keys();
        let owner = NostrPublicKey([42; 32]);
        let state = authoritative(owner);
        assert!(verify_homebase_publication_attestation_request(
            &ServerAttestationSigner(&keys),
            owner,
            &MapInstanceId::Overworld,
            state.descriptor_root,
            &state.payload_scope,
            &state,
            1_000,
            600,
        )
        .is_err());
    }

    #[test]
    fn rejects_foreign_owner_request() {
        let keys = server_keys();
        let owner = NostrPublicKey([42; 32]);
        let foreign = NostrPublicKey([99; 32]);
        let state = authoritative(owner);
        assert!(verify_homebase_publication_attestation_request(
            &ServerAttestationSigner(&keys),
            foreign,
            &MapInstanceId::Homebase { owner },
            state.descriptor_root,
            &state.payload_scope,
            &state,
            1_000,
            600,
        )
        .is_err());
    }

    #[test]
    fn rejects_descriptor_root_divergence() {
        let keys = server_keys();
        let owner = NostrPublicKey([42; 32]);
        let state = authoritative(owner);
        assert!(verify_homebase_publication_attestation_request(
            &ServerAttestationSigner(&keys),
            owner,
            &MapInstanceId::Homebase { owner },
            [0; 32],
            &state.payload_scope,
            &state,
            1_000,
            600,
        )
        .is_err());
    }
}
