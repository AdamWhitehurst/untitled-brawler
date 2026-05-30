use protocol::{HomebasePublicationAttestation, NostrPublicKey};
use serde::Serialize;

use crate::manifest::MapPersistenceRejection;

/// Domain separator mixed into the bytes signed for a homebase publication
/// attestation, preventing cross-protocol signature reuse.
pub const HOMEBASE_ATTESTATION_SIGNING_DOMAIN: &[u8] =
    b"untitled-brawler/homebase-publication-attestation/v1";

/// Signs attestation payloads with the server identity without exposing Nostr
/// SDK types to shared persistence code.
pub trait AttestationSigner {
    /// Returns the server public key that signs attestations.
    fn public_key(&self) -> NostrPublicKey;

    /// Signs canonical attestation payload bytes and returns the raw signature.
    fn sign_attestation_payload(&self, payload: &[u8]) -> Result<Vec<u8>, MapPersistenceRejection>;
}

/// Verifies attestation signatures against the signing server public key.
pub trait AttestationVerifier {
    /// Verifies a raw signature over canonical attestation payload bytes.
    fn verify_attestation_payload(
        &self,
        pubkey: NostrPublicKey,
        payload: &[u8],
        signature: &[u8],
    ) -> Result<(), MapPersistenceRejection>;
}

/// Attestation fields with the signature cleared, used to derive signing bytes.
#[derive(Serialize)]
struct UnsignedAttestation<'a> {
    owner: &'a NostrPublicKey,
    map_id: &'a protocol::MapInstanceId,
    server_revision: u64,
    previous_manifest_hash: &'a Option<[u8; 32]>,
    descriptor_root: &'a [u8; 32],
    payload_scope: &'a protocol::HomebasePayloadScope,
    expires_at: u64,
    server_pubkey: &'a NostrPublicKey,
}

/// Returns the domain-separated canonical bytes signed for an attestation.
pub fn attestation_signing_payload(
    attestation: &HomebasePublicationAttestation,
) -> Result<Vec<u8>, MapPersistenceRejection> {
    let unsigned = UnsignedAttestation {
        owner: &attestation.owner,
        map_id: &attestation.map_id,
        server_revision: attestation.server_revision,
        previous_manifest_hash: &attestation.previous_manifest_hash,
        descriptor_root: &attestation.descriptor_root,
        payload_scope: &attestation.payload_scope,
        expires_at: attestation.expires_at,
        server_pubkey: &attestation.server_pubkey,
    };
    let json = serde_json::to_vec(&unsigned).map_err(|error| {
        MapPersistenceRejection::Invalid(format!("serialize attestation payload: {error}"))
    })?;
    let mut payload =
        Vec::with_capacity(HOMEBASE_ATTESTATION_SIGNING_DOMAIN.len() + 1 + json.len());
    payload.extend_from_slice(HOMEBASE_ATTESTATION_SIGNING_DOMAIN);
    payload.push(0);
    payload.extend_from_slice(&json);
    Ok(payload)
}

/// Stamps the server pubkey and signature onto an attestation.
pub fn sign_homebase_attestation(
    signer: &impl AttestationSigner,
    mut attestation: HomebasePublicationAttestation,
) -> Result<HomebasePublicationAttestation, MapPersistenceRejection> {
    attestation.server_pubkey = signer.public_key();
    attestation.server_signature.clear();
    let payload = attestation_signing_payload(&attestation)?;
    attestation.server_signature = signer.sign_attestation_payload(&payload)?;
    if attestation.server_signature.is_empty() {
        return Err(MapPersistenceRejection::Invalid(
            "homebase attestation signature is empty".into(),
        ));
    }
    Ok(attestation)
}

/// Verifies only the server signature over an attestation, ignoring expiry.
///
/// Used by import/read validation, where an attestation issued long ago is still
/// valid evidence that the server authorized a now-historical revision. Expiry
/// bounds the publish window, not how long published data remains loadable.
pub fn verify_homebase_attestation_signature(
    verifier: &impl AttestationVerifier,
    attestation: &HomebasePublicationAttestation,
) -> Result<(), MapPersistenceRejection> {
    if attestation.server_signature.is_empty() {
        return Err(MapPersistenceRejection::Invalid(
            "homebase attestation signature is empty".into(),
        ));
    }
    let payload = attestation_signing_payload(attestation)?;
    verifier.verify_attestation_payload(
        attestation.server_pubkey,
        &payload,
        &attestation.server_signature,
    )
}

/// Verifies an attestation signature and expiry against the current time.
///
/// Used at the publish-window boundary. Import/read validation should use
/// [`verify_homebase_attestation_signature`] instead.
pub fn verify_homebase_attestation(
    verifier: &impl AttestationVerifier,
    attestation: &HomebasePublicationAttestation,
    now_unix: u64,
) -> Result<(), MapPersistenceRejection> {
    if now_unix > attestation.expires_at {
        return Err(MapPersistenceRejection::Invalid(
            "homebase attestation expired".into(),
        ));
    }
    verify_homebase_attestation_signature(verifier, attestation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{HomebasePayloadScope, MapInstanceId};

    const SERVER: NostrPublicKey = NostrPublicKey([7; 32]);
    const OWNER: NostrPublicKey = NostrPublicKey([42; 32]);

    /// Test signer producing a deterministic signature bound to the payload.
    struct StubSigner;

    fn stub_signature(payload: &[u8]) -> Vec<u8> {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"stub-sig");
        hasher.update(SERVER.0);
        hasher.update(payload);
        hasher.finalize().to_vec()
    }

    impl AttestationSigner for StubSigner {
        fn public_key(&self) -> NostrPublicKey {
            SERVER
        }

        fn sign_attestation_payload(
            &self,
            payload: &[u8],
        ) -> Result<Vec<u8>, MapPersistenceRejection> {
            Ok(stub_signature(payload))
        }
    }

    /// Test verifier accepting only signatures the stub signer would produce.
    struct StubVerifier;

    impl AttestationVerifier for StubVerifier {
        fn verify_attestation_payload(
            &self,
            pubkey: NostrPublicKey,
            payload: &[u8],
            signature: &[u8],
        ) -> Result<(), MapPersistenceRejection> {
            if pubkey != SERVER {
                return Err(MapPersistenceRejection::Invalid(
                    "unknown server key".into(),
                ));
            }
            if signature == stub_signature(payload) {
                Ok(())
            } else {
                Err(MapPersistenceRejection::Invalid("bad signature".into()))
            }
        }
    }

    fn attestation() -> HomebasePublicationAttestation {
        HomebasePublicationAttestation {
            owner: OWNER,
            map_id: MapInstanceId::Homebase { owner: OWNER },
            server_revision: 4,
            previous_manifest_hash: Some([9; 32]),
            descriptor_root: [3; 32],
            payload_scope: HomebasePayloadScope::default(),
            expires_at: 1_000,
            server_pubkey: SERVER,
            server_signature: Vec::new(),
        }
    }

    fn signed() -> HomebasePublicationAttestation {
        sign_homebase_attestation(&StubSigner, attestation()).unwrap()
    }

    #[test]
    fn homebase_attestation_roundtrips() {
        let signed = signed();
        assert!(!signed.server_signature.is_empty());
        verify_homebase_attestation(&StubVerifier, &signed, 500).expect("valid attestation");
    }

    #[test]
    fn homebase_attestation_rejects_expired() {
        assert!(matches!(
            verify_homebase_attestation(&StubVerifier, &signed(), 2_000),
            Err(MapPersistenceRejection::Invalid(_))
        ));
    }

    #[test]
    fn homebase_attestation_rejects_owner_tampering() {
        let mut tampered = signed();
        tampered.owner = NostrPublicKey([99; 32]);
        assert!(verify_homebase_attestation(&StubVerifier, &tampered, 500).is_err());
    }

    #[test]
    fn homebase_attestation_rejects_map_id_tampering() {
        let mut tampered = signed();
        tampered.map_id = MapInstanceId::Overworld;
        assert!(verify_homebase_attestation(&StubVerifier, &tampered, 500).is_err());
    }

    #[test]
    fn homebase_attestation_rejects_descriptor_root_tampering() {
        let mut tampered = signed();
        tampered.descriptor_root = [1; 32];
        assert!(verify_homebase_attestation(&StubVerifier, &tampered, 500).is_err());
    }

    #[test]
    fn homebase_attestation_rejects_signature_tampering() {
        let mut tampered = signed();
        tampered.server_signature[0] ^= 0xff;
        assert!(verify_homebase_attestation(&StubVerifier, &tampered, 500).is_err());
    }

    #[test]
    fn homebase_attestation_rejects_empty_signature() {
        let mut tampered = signed();
        tampered.server_signature.clear();
        assert!(matches!(
            verify_homebase_attestation(&StubVerifier, &tampered, 500),
            Err(MapPersistenceRejection::Invalid(_))
        ));
    }
}
