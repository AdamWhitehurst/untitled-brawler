use protocol::MapInstanceId;

use crate::attestation::{verify_homebase_attestation, AttestationVerifier};
use crate::manifest::{MapPersistenceRejection, NostrMapManifest};

/// Verifies that a client-published homebase manifest is authorized by a valid
/// server attestation bound to exactly this manifest revision.
///
/// The player signature over the manifest event is verified separately by
/// [`crate::verify_manifest_event_with_hash`]; this function adds the
/// server-authority gate: the manifest must carry an attestation whose
/// signature is valid and whose fields match the manifest one-for-one.
///
/// Progression-bearing-data rejection and entitlement checks (plan 5.7) are not
/// performed here and remain a server-side follow-up.
pub fn validate_homebase_manifest_attestation(
    verifier: &impl AttestationVerifier,
    manifest: &NostrMapManifest,
    now_unix: u64,
) -> Result<(), MapPersistenceRejection> {
    let MapInstanceId::Homebase { owner: map_owner } = manifest.map_id else {
        return Err(MapPersistenceRejection::Invalid(
            "attested import accepts only homebase manifests".into(),
        ));
    };
    if map_owner != manifest.owner {
        return Err(MapPersistenceRejection::Invalid(
            "homebase manifest owner does not match map owner".into(),
        ));
    }
    let Some(attestation) = manifest.homebase_attestation.as_ref() else {
        return Err(MapPersistenceRejection::Invalid(
            "homebase manifest is missing a server attestation".into(),
        ));
    };

    verify_homebase_attestation(verifier, attestation, now_unix)?;

    if attestation.owner != manifest.owner {
        return Err(MapPersistenceRejection::Invalid(
            "attestation owner does not match manifest owner".into(),
        ));
    }
    if attestation.map_id != manifest.map_id {
        return Err(MapPersistenceRejection::Invalid(
            "attestation map id does not match manifest map id".into(),
        ));
    }
    if attestation.server_revision != manifest.revision {
        return Err(MapPersistenceRejection::Invalid(
            "attestation revision does not match manifest revision".into(),
        ));
    }
    if attestation.previous_manifest_hash != manifest.previous_hash {
        return Err(MapPersistenceRejection::Invalid(
            "attestation previous hash does not match manifest previous hash".into(),
        ));
    }
    if attestation.descriptor_root != manifest.descriptor_root {
        return Err(MapPersistenceRejection::Invalid(
            "attestation descriptor root does not match manifest descriptor root".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attestation::{sign_homebase_attestation, AttestationSigner};
    use crate::manifest::MAP_MANIFEST_SCHEMA_VERSION;
    use protocol::{HomebasePayloadScope, HomebasePublicationAttestation, NostrPublicKey};
    use sha2::{Digest, Sha256};

    const SERVER: NostrPublicKey = NostrPublicKey([7; 32]);
    const OWNER: NostrPublicKey = NostrPublicKey([42; 32]);

    struct StubSigner;
    struct StubVerifier;

    fn stub_signature(payload: &[u8]) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(b"stub-sig");
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

    impl AttestationVerifier for StubVerifier {
        fn verify_attestation_payload(
            &self,
            pubkey: NostrPublicKey,
            payload: &[u8],
            signature: &[u8],
        ) -> Result<(), MapPersistenceRejection> {
            if pubkey == SERVER && signature == stub_signature(payload) {
                Ok(())
            } else {
                Err(MapPersistenceRejection::Invalid("bad signature".into()))
            }
        }
    }

    fn attested_manifest() -> NostrMapManifest {
        let descriptor_root = [3; 32];
        let attestation = sign_homebase_attestation(
            &StubSigner,
            HomebasePublicationAttestation {
                owner: OWNER,
                map_id: MapInstanceId::Homebase { owner: OWNER },
                server_revision: 5,
                previous_manifest_hash: Some([9; 32]),
                descriptor_root,
                payload_scope: HomebasePayloadScope::default(),
                expires_at: 1_000,
                server_pubkey: SERVER,
                server_signature: Vec::new(),
            },
        )
        .unwrap();
        NostrMapManifest {
            map_id: MapInstanceId::Homebase { owner: OWNER },
            owner: OWNER,
            revision: 5,
            previous_hash: Some([9; 32]),
            payloads: Vec::new(),
            schema_version: MAP_MANIFEST_SCHEMA_VERSION,
            descriptor_root,
            homebase_attestation: Some(attestation),
        }
    }

    #[test]
    fn accepts_matching_attested_manifest() {
        validate_homebase_manifest_attestation(&StubVerifier, &attested_manifest(), 500)
            .expect("valid attested manifest");
    }

    #[test]
    fn rejects_missing_attestation() {
        let mut manifest = attested_manifest();
        manifest.homebase_attestation = None;
        assert!(matches!(
            validate_homebase_manifest_attestation(&StubVerifier, &manifest, 500),
            Err(MapPersistenceRejection::Invalid(_))
        ));
    }

    #[test]
    fn rejects_overworld_manifest() {
        let mut manifest = attested_manifest();
        manifest.map_id = MapInstanceId::Overworld;
        assert!(validate_homebase_manifest_attestation(&StubVerifier, &manifest, 500).is_err());
    }

    #[test]
    fn rejects_revision_mismatch() {
        let mut manifest = attested_manifest();
        manifest.revision = 6;
        assert!(validate_homebase_manifest_attestation(&StubVerifier, &manifest, 500).is_err());
    }

    #[test]
    fn rejects_descriptor_root_mismatch() {
        let mut manifest = attested_manifest();
        manifest.descriptor_root = [1; 32];
        assert!(validate_homebase_manifest_attestation(&StubVerifier, &manifest, 500).is_err());
    }

    #[test]
    fn rejects_expired_attestation() {
        assert!(
            validate_homebase_manifest_attestation(&StubVerifier, &attested_manifest(), 2_000)
                .is_err()
        );
    }
}
