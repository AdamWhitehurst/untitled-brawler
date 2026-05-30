use nostr_client::events::NostrEventDraft;
use protocol::{HomebasePublicationAttestation, MapInstanceId, NostrPublicKey};

use crate::manifest::{
    compute_descriptor_root, compute_manifest_hash, manifest_event_draft,
    verify_manifest_event_with_hash, ManifestHash, ManifestPayloadDescriptor, MapRevision,
    NostrMapManifest, MAP_MANIFEST_SCHEMA_VERSION,
};
use crate::read::RemotePersistenceError;

/// Signs map manifest drafts without exposing Nostr SDK types to this crate.
pub trait MapManifestSigner {
    /// Returns the Nostr public key that will sign manifest events.
    fn public_key(&self) -> NostrPublicKey;

    /// Signs a prepared Nostr event draft and returns raw event JSON.
    fn sign_map_manifest_event(
        &self,
        draft: NostrEventDraft,
    ) -> Result<String, RemotePersistenceError>;
}

/// Computes the map manifest hash embedded in a signed event JSON string.
pub fn manifest_hash_from_signed_event_json(
    event_json: &str,
) -> Result<ManifestHash, RemotePersistenceError> {
    let event = nostr_client::events::verify_event_json(event_json)?;
    let manifest: NostrMapManifest = serde_json::from_str(&event.content)
        .map_err(|error| RemotePersistenceError::Invalid(error.to_string()))?;
    compute_descriptor_root(&manifest.payloads)?;
    compute_manifest_hash(&manifest).map_err(RemotePersistenceError::from)
}

/// Client-owned homebase manifest update authorized by a server attestation.
pub struct ClientHomebaseUpdate {
    pub owner: NostrPublicKey,
    pub map_id: MapInstanceId,
    pub payloads: Vec<ManifestPayloadDescriptor>,
    pub previous_revision: Option<MapRevision>,
    pub attestation: HomebasePublicationAttestation,
}

/// Builds and signs a homebase manifest event, binding it to a server attestation.
///
/// Rejects any update where the signer is not the owner, the map is not the
/// owner's homebase, or the attestation does not match the update's owner, map,
/// previous hash, and descriptor root.
pub fn build_homebase_manifest_event(
    signer: &impl MapManifestSigner,
    update: ClientHomebaseUpdate,
) -> Result<(ManifestHash, String), RemotePersistenceError> {
    if signer.public_key() != update.owner {
        return Err(RemotePersistenceError::Invalid(
            "homebase manifest signer must equal owner".into(),
        ));
    }
    if !matches!(update.map_id, MapInstanceId::Homebase { owner } if owner == update.owner) {
        return Err(RemotePersistenceError::Invalid(
            "client may only publish own homebase manifest".into(),
        ));
    }
    let expected_previous = update
        .previous_revision
        .as_ref()
        .map(|revision| revision.manifest_hash);
    if update.attestation.owner != update.owner
        || update.attestation.map_id != update.map_id
        || update.attestation.previous_manifest_hash != expected_previous
    {
        return Err(RemotePersistenceError::Invalid(
            "homebase attestation does not match update".into(),
        ));
    }

    let descriptor_root = compute_descriptor_root(&update.payloads)?;
    if descriptor_root != update.attestation.descriptor_root {
        return Err(RemotePersistenceError::Invalid(
            "homebase descriptor root does not match attestation".into(),
        ));
    }
    let manifest = NostrMapManifest {
        map_id: update.map_id,
        owner: update.owner,
        revision: update.attestation.server_revision,
        previous_hash: update.attestation.previous_manifest_hash,
        payloads: update.payloads,
        schema_version: MAP_MANIFEST_SCHEMA_VERSION,
        descriptor_root,
        homebase_attestation: Some(update.attestation),
    };
    build_signed_map_manifest_event(signer, manifest)
}

/// Builds, signs, and verifies a deterministic Nostr map manifest event.
pub fn build_signed_map_manifest_event(
    identity: &impl MapManifestSigner,
    mut manifest: NostrMapManifest,
) -> Result<(ManifestHash, String), RemotePersistenceError> {
    let signer = identity.public_key();
    if manifest.owner != signer {
        return Err(RemotePersistenceError::Invalid(format!(
            "manifest owner {:?} does not match signer {:?}",
            manifest.owner, signer
        )));
    }

    manifest.descriptor_root = compute_descriptor_root(&manifest.payloads)?;
    let manifest_hash = compute_manifest_hash(&manifest)?;
    let draft = manifest_event_draft(&manifest)?;
    let signed_event_json = identity.sign_map_manifest_event(draft)?;

    let verified =
        verify_manifest_event_with_hash(&signed_event_json, manifest.owner, &manifest.map_id)?;
    if verified.manifest_hash != manifest_hash {
        return Err(RemotePersistenceError::Invalid(
            "signed manifest event hash does not match prepared manifest hash".to_string(),
        ));
    }
    Ok((manifest_hash, signed_event_json))
}
