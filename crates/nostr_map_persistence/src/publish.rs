use nostr_client::events::NostrEventDraft;
use protocol::NostrPublicKey;

use crate::manifest::{
    compute_descriptor_root, compute_manifest_hash, manifest_event_draft,
    verify_manifest_event_with_hash, ManifestHash, NostrMapManifest,
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
