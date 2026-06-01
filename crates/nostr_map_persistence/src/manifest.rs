use nostr_client::events::{NostrEventDraft, NostrEventKind, NostrTag, VerifiedNostrEvent};
use nostr_client::BlobRef;
use protocol::{MapInstanceId, NostrPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Addressable Nostr kind used for map manifests.
pub const NOSTR_KIND_MAP_MANIFEST: u16 = 30079;
/// Current map manifest schema version.
pub const MAP_MANIFEST_SCHEMA_VERSION: u32 = 1;
/// Indexed tag containing the stable map key.
pub const MAP_TAG: &str = "m";
/// Indexed tag containing this manifest hash.
pub const MANIFEST_HASH_TAG: &str = "x";
/// Indexed tag containing the previous manifest hash.
pub const PREVIOUS_MANIFEST_HASH_TAG: &str = "y";
/// Descriptor-root hash domain separator.
pub const DESCRIPTOR_ROOT_DOMAIN: &[u8] = b"untitled-brawler/map-payload-descriptor/v1";
/// Manifest hash domain separator.
pub const MANIFEST_HASH_DOMAIN: &[u8] = b"untitled-brawler/map-manifest/v1";

/// Hash identifying a signed map manifest revision.
pub type ManifestHash = [u8; 32];

/// Identifies a map payload's semantic class.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PayloadClass {
    MapMeta,
    TerrainChunk,
    ChunkEntities,
    MapEntities,
}

/// Identifies a payload within a map manifest.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PayloadKey {
    Singleton,
    Chunk { x: i32, y: i32, z: i32 },
}

/// Signed manifest slot semantics for a payload descriptor.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ManifestPayloadSlot {
    Present { blob: BlobRef },
    Empty,
    Absent,
    Tombstoned,
}

/// Describes one payload slot included in a map manifest descriptor root.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ManifestPayloadDescriptor {
    pub class: PayloadClass,
    pub key: PayloadKey,
    pub slot: ManifestPayloadSlot,
    pub schema_version: u32,
}

/// Production-shaped Nostr map manifest content used by fake and real remote restore.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NostrMapManifest {
    pub map_id: MapInstanceId,
    pub owner: NostrPublicKey,
    pub revision: u64,
    pub previous_hash: Option<ManifestHash>,
    pub payloads: Vec<ManifestPayloadDescriptor>,
    pub schema_version: u32,
    pub descriptor_root: [u8; 32],
    /// Server-signed authorization for client-published homebase manifests.
    /// `None` for server-owned overworld manifests and pre-attestation revisions.
    ///
    /// Skipped when `None` so overworld manifests serialize byte-identically to
    /// manifests published before this field existed, preserving manifest-hash
    /// (and `#d` tag) compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homebase_attestation: Option<protocol::HomebasePublicationAttestation>,
}

/// Stores whether a payload slot is present, intentionally empty, unchanged, or deleted.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum PayloadSlotState<T> {
    Present(T),
    Empty,
    Absent,
    Tombstoned,
}

/// Revision metadata for a map save chain.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MapRevision {
    pub revision: u64,
    pub previous_hash: Option<ManifestHash>,
    pub manifest_hash: ManifestHash,
}

/// Rejection reason for map persistence preflight or validation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum MapPersistenceRejection {
    Filesystem(String),
    Invalid(String),
    Incomplete(String),
    Divergent(String),
    Unavailable(String),
}

/// Raw serialized map metadata payload accepted from remote persistence.
#[derive(Clone, Debug)]
pub struct RawMapMetaPayload {
    pub bytes: Vec<u8>,
}

/// Raw serialized terrain chunk payload accepted from remote persistence.
#[derive(Clone, Debug)]
pub struct RawChunkPayload {
    pub bytes: Vec<u8>,
}

/// Raw serialized chunk-entity payload accepted from remote persistence.
#[derive(Clone, Debug)]
pub struct RawChunkEntitiesPayload {
    pub bytes: Vec<u8>,
}

/// Raw serialized map-level entity payload accepted from remote persistence.
#[derive(Clone, Debug)]
pub struct RawMapEntitiesPayload {
    pub bytes: Vec<u8>,
}

/// Raw present payload collection fetched from a manifest.
#[derive(Clone, Debug)]
pub struct RawMapPayloads {
    pub present_payloads: Vec<(ManifestPayloadDescriptor, Vec<u8>)>,
}

/// Complete raw map save assembled from a validated manifest chain.
#[derive(Clone, Debug)]
pub struct RawValidatedMapSave {
    pub meta: RawMapMetaPayload,
    pub chunks: Vec<(PayloadKey, RawChunkPayload)>,
    pub chunk_entities: Vec<(PayloadKey, RawChunkEntitiesPayload)>,
    pub map_entities: Option<RawMapEntitiesPayload>,
    pub revision: MapRevision,
}

/// Base snapshot for raw remote chain assembly.
#[derive(Clone, Debug)]
pub enum RawSaveBase {
    Empty,
    Snapshot(RawValidatedMapSave),
}

/// Raw delta save assembled from one manifest revision.
#[derive(Clone, Debug)]
pub struct RawValidatedMapDelta {
    pub revision: MapRevision,
    pub meta: PayloadSlotState<RawMapMetaPayload>,
    pub chunks: Vec<(PayloadKey, PayloadSlotState<RawChunkPayload>)>,
    pub chunk_entities: Vec<(PayloadKey, PayloadSlotState<RawChunkEntitiesPayload>)>,
    pub map_entities: PayloadSlotState<RawMapEntitiesPayload>,
}

/// Verified manifest plus its content hash and original event JSON.
#[derive(Clone, Debug)]
pub struct VerifiedManifest {
    pub manifest: NostrMapManifest,
    pub manifest_hash: ManifestHash,
    pub raw_event_json: String,
}

/// Manifest verification failures.
#[derive(Debug, Error)]
pub enum ManifestVerificationError {
    #[error("invalid manifest: {0}")]
    Invalid(String),
    #[error("canonical serialization failed: {0}")]
    CanonicalSerialization(serde_json::Error),
    #[error("event verification failed: {0}")]
    Event(#[from] nostr_client::events::NostrEventError),
}

/// Stable map tag value used for latest-manifest queries.
pub fn map_tag_value(owner: NostrPublicKey, map_id: &MapInstanceId) -> String {
    let owner_key = nostr_client::npub_from_nostr_public_key(owner);
    let map_key = match map_id {
        MapInstanceId::Overworld => "overworld".to_owned(),
        MapInstanceId::Homebase { owner } => {
            format!(
                "homebase-{}",
                nostr_client::npub_from_nostr_public_key(*owner)
            )
        }
    };
    format!("{owner_key}:{map_key}")
}

/// Formats a manifest hash as lowercase hexadecimal.
pub fn manifest_hash_hex(hash: ManifestHash) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Parses a lowercase/uppercase hexadecimal manifest hash.
pub fn parse_manifest_hash_hex(text: &str) -> Result<ManifestHash, ManifestVerificationError> {
    let bytes = hex_decode_32(text).ok_or_else(|| {
        ManifestVerificationError::Invalid(format!("invalid manifest hash hex: {text}"))
    })?;
    Ok(bytes)
}

fn hex_decode_32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0; 32];
    for (index, chunk) in text.as_bytes().chunks_exact(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)? as u8;
        let lo = (chunk[1] as char).to_digit(16)? as u8;
        out[index] = (hi << 4) | lo;
    }
    Some(out)
}

/// Returns canonical bytes for deterministic descriptor ordering.
pub fn manifest_payload_descriptor_order(descriptor: &ManifestPayloadDescriptor) -> Vec<u8> {
    serde_json::to_vec(descriptor)
        .expect("manifest payload descriptors must serialize for deterministic ordering")
}

/// Sorts payload descriptors deterministically, computes the descriptor root, and assembles the
/// (unsigned) manifest. Shared by overworld server publish and homebase publish so both produce
/// byte-identical descriptor roots from the same slots.
#[allow(clippy::too_many_arguments)]
pub fn finalize_manifest(
    mut payloads: Vec<ManifestPayloadDescriptor>,
    map_id: MapInstanceId,
    owner: NostrPublicKey,
    revision: u64,
    previous_hash: Option<ManifestHash>,
    homebase_attestation: Option<protocol::HomebasePublicationAttestation>,
) -> Result<NostrMapManifest, MapPersistenceRejection> {
    payloads.sort_by_key(manifest_payload_descriptor_order);
    let descriptor_root = compute_descriptor_root(&payloads)
        .map_err(|error| MapPersistenceRejection::Invalid(error.to_string()))?;
    Ok(NostrMapManifest {
        map_id,
        owner,
        revision,
        previous_hash,
        payloads,
        schema_version: MAP_MANIFEST_SCHEMA_VERSION,
        descriptor_root,
        homebase_attestation,
    })
}

/// Computes the descriptor root for a payload descriptor list.
pub fn compute_descriptor_root(
    payloads: &[ManifestPayloadDescriptor],
) -> Result<[u8; 32], ManifestVerificationError> {
    let mut payloads = payloads.iter().collect::<Vec<_>>();
    payloads.sort_by_key(|a| manifest_payload_descriptor_order(a));

    let mut hasher = Sha256::new();
    hasher.update(DESCRIPTOR_ROOT_DOMAIN);
    hasher.update((payloads.len() as u64).to_be_bytes());
    for descriptor in payloads {
        hasher.update(
            serde_json::to_vec(&descriptor.class)
                .map_err(ManifestVerificationError::CanonicalSerialization)?,
        );
        hasher.update(
            serde_json::to_vec(&descriptor.key)
                .map_err(ManifestVerificationError::CanonicalSerialization)?,
        );
        hasher.update(descriptor.schema_version.to_be_bytes());
        match &descriptor.slot {
            ManifestPayloadSlot::Present { blob } => {
                hasher.update([0]);
                hasher.update(blob.sha256);
                hasher.update(blob.size.to_be_bytes());
            }
            ManifestPayloadSlot::Empty => hasher.update([1]),
            ManifestPayloadSlot::Absent => hasher.update([2]),
            ManifestPayloadSlot::Tombstoned => hasher.update([3]),
        }
    }
    Ok(hasher.finalize().into())
}

/// Verifies a manifest descriptor root against its signed descriptor slots.
pub fn verify_descriptor_root(
    manifest: &NostrMapManifest,
) -> Result<(), ManifestVerificationError> {
    let actual = compute_descriptor_root(&manifest.payloads)?;
    if actual != manifest.descriptor_root {
        return Err(ManifestVerificationError::Invalid(
            "manifest descriptor root mismatch".to_string(),
        ));
    }
    Ok(())
}

/// Canonical manifest bytes used for manifest hashing.
pub fn canonical_manifest_bytes(
    manifest: &NostrMapManifest,
) -> Result<Vec<u8>, ManifestVerificationError> {
    serde_json::to_vec(manifest).map_err(ManifestVerificationError::CanonicalSerialization)
}

/// Serializes a manifest to canonical JSON for transport (e.g. an unsigned
/// homebase manifest the server hands to the owning client to sign).
pub fn manifest_to_json(manifest: &NostrMapManifest) -> Result<String, ManifestVerificationError> {
    serde_json::to_string(manifest).map_err(ManifestVerificationError::CanonicalSerialization)
}

/// Parses a manifest from the JSON produced by [`manifest_to_json`].
pub fn manifest_from_json(json: &str) -> Result<NostrMapManifest, ManifestVerificationError> {
    serde_json::from_str(json).map_err(ManifestVerificationError::CanonicalSerialization)
}

/// Computes a domain-separated manifest hash.
pub fn compute_manifest_hash(
    manifest: &NostrMapManifest,
) -> Result<ManifestHash, ManifestVerificationError> {
    let mut hasher = Sha256::new();
    hasher.update(MANIFEST_HASH_DOMAIN);
    hasher.update(canonical_manifest_bytes(manifest)?);
    Ok(hasher.finalize().into())
}

/// Builds the tags required for a signed manifest event.
pub fn manifest_event_tags(
    manifest: &NostrMapManifest,
) -> Result<Vec<NostrTag>, ManifestVerificationError> {
    verify_descriptor_root(manifest)?;
    let manifest_hash = compute_manifest_hash(manifest)?;
    let map_key = map_tag_value(manifest.owner, &manifest.map_id);
    let manifest_hash_text = manifest_hash_hex(manifest_hash);
    let mut tags = vec![
        NostrTag::new("d", format!("{map_key}:{manifest_hash_text}")),
        NostrTag::new(MAP_TAG, map_key),
        NostrTag::new(MANIFEST_HASH_TAG, manifest_hash_text),
        NostrTag::new("r", manifest.revision.to_string()),
    ];
    if let Some(previous_hash) = manifest.previous_hash {
        tags.push(NostrTag::new(
            PREVIOUS_MANIFEST_HASH_TAG,
            manifest_hash_hex(previous_hash),
        ));
    }
    Ok(tags)
}

/// Builds an unsigned Nostr event draft for a manifest.
pub fn manifest_event_draft(
    manifest: &NostrMapManifest,
) -> Result<NostrEventDraft, ManifestVerificationError> {
    Ok(NostrEventDraft {
        kind: NostrEventKind::Custom(NOSTR_KIND_MAP_MANIFEST),
        content: serde_json::to_string(manifest)
            .map_err(ManifestVerificationError::CanonicalSerialization)?,
        tags: manifest_event_tags(manifest)?,
    })
}

/// Verifies raw event JSON and returns manifest content.
pub fn verify_manifest_event(
    event_json: &str,
    expected_owner: NostrPublicKey,
    expected_map_id: &MapInstanceId,
) -> Result<NostrMapManifest, ManifestVerificationError> {
    let event = nostr_client::events::verify_event_json(event_json)?;
    if event.kind != NostrEventKind::Custom(NOSTR_KIND_MAP_MANIFEST) {
        return Err(ManifestVerificationError::Invalid(format!(
            "expected kind {NOSTR_KIND_MAP_MANIFEST}, got {:?}",
            event.kind
        )));
    }
    if event.pubkey != expected_owner {
        return Err(ManifestVerificationError::Invalid(
            "manifest signer does not match expected owner".to_string(),
        ));
    }
    let manifest: NostrMapManifest = serde_json::from_str(&event.content)
        .map_err(ManifestVerificationError::CanonicalSerialization)?;
    if manifest.owner != expected_owner || &manifest.map_id != expected_map_id {
        return Err(ManifestVerificationError::Invalid(
            "manifest owner/map id does not match query".to_string(),
        ));
    }
    if manifest.schema_version != MAP_MANIFEST_SCHEMA_VERSION {
        return Err(ManifestVerificationError::Invalid(format!(
            "unsupported manifest schema version {}",
            manifest.schema_version
        )));
    }
    verify_descriptor_root(&manifest)?;
    verify_manifest_event_tags(&event, &manifest)?;
    Ok(manifest)
}

/// Verifies the required indexed tags on a signed manifest event.
pub fn verify_manifest_event_tags(
    event: &VerifiedNostrEvent,
    manifest: &NostrMapManifest,
) -> Result<(), ManifestVerificationError> {
    let manifest_hash = compute_manifest_hash(manifest)?;
    let manifest_hash_text = manifest_hash_hex(manifest_hash);
    let map_key = map_tag_value(manifest.owner, &manifest.map_id);
    let expected_d = format!("{map_key}:{manifest_hash_text}");

    require_one(event, "d", &expected_d)?;
    require_one(event, MAP_TAG, &map_key)?;
    require_one(event, MANIFEST_HASH_TAG, &manifest_hash_text)?;
    match manifest.previous_hash {
        Some(previous_hash) => require_one(
            event,
            PREVIOUS_MANIFEST_HASH_TAG,
            &manifest_hash_hex(previous_hash),
        )?,
        None => {
            if !tag_values(event, PREVIOUS_MANIFEST_HASH_TAG).is_empty() {
                return Err(ManifestVerificationError::Invalid(
                    "genesis manifest must not include previous-hash tag".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn tag_values<'a>(event: &'a VerifiedNostrEvent, name: &str) -> Vec<&'a str> {
    event
        .tags
        .iter()
        .filter(|tag| tag.name == name)
        .map(|tag| tag.value.as_str())
        .collect()
}

fn require_one(
    event: &VerifiedNostrEvent,
    name: &str,
    expected: &str,
) -> Result<(), ManifestVerificationError> {
    let values = tag_values(event, name);
    if values.len() != 1 {
        return Err(ManifestVerificationError::Invalid(format!(
            "manifest event requires exactly one #{name} tag"
        )));
    }
    if values[0] != expected {
        return Err(ManifestVerificationError::Invalid(format!(
            "manifest event #{name} tag mismatch: expected {expected}, got {}",
            values[0]
        )));
    }
    Ok(())
}

/// Verifies raw event JSON and returns the manifest plus its content hash.
pub fn verify_manifest_event_with_hash(
    event_json: &str,
    expected_owner: NostrPublicKey,
    expected_map_id: &MapInstanceId,
) -> Result<VerifiedManifest, ManifestVerificationError> {
    let manifest = verify_manifest_event(event_json, expected_owner, expected_map_id)?;
    let manifest_hash = compute_manifest_hash(&manifest)?;
    Ok(VerifiedManifest {
        manifest,
        manifest_hash,
        raw_event_json: event_json.to_owned(),
    })
}

impl std::fmt::Display for MapPersistenceRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Filesystem(message) => write!(f, "filesystem persistence error: {message}"),
            Self::Invalid(message) => write!(f, "invalid map persistence data: {message}"),
            Self::Incomplete(message) => write!(f, "incomplete map persistence data: {message}"),
            Self::Divergent(message) => write!(f, "divergent map persistence chain: {message}"),
            Self::Unavailable(message) => write!(f, "map persistence unavailable: {message}"),
        }
    }
}

impl std::error::Error for MapPersistenceRejection {}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::{Keys, SecretKey};

    fn owner() -> (SecretKey, NostrPublicKey) {
        let secret = SecretKey::generate();
        let keys = Keys::new(secret.clone());
        (secret, NostrPublicKey(*keys.public_key().as_bytes()))
    }

    fn blob(bytes: &[u8]) -> BlobRef {
        BlobRef {
            sha256: Sha256::digest(bytes).into(),
            size: bytes.len() as u64,
            content_type: "application/octet-stream".to_string(),
            urls: vec!["https://example.com/blob".to_string()],
        }
    }

    fn manifest(owner: NostrPublicKey) -> NostrMapManifest {
        let payloads = vec![ManifestPayloadDescriptor {
            class: PayloadClass::MapMeta,
            key: PayloadKey::Singleton,
            slot: ManifestPayloadSlot::Present {
                blob: blob(b"meta"),
            },
            schema_version: 1,
        }];
        let descriptor_root = compute_descriptor_root(&payloads).unwrap();
        NostrMapManifest {
            map_id: MapInstanceId::Overworld,
            owner,
            revision: 1,
            previous_hash: None,
            payloads,
            schema_version: MAP_MANIFEST_SCHEMA_VERSION,
            descriptor_root,
            homebase_attestation: None,
        }
    }

    #[test]
    fn finalize_manifest_matches_hand_built_manifest() {
        let (_secret, owner) = owner();
        // Two descriptors out of canonical order to exercise the sort.
        let payloads = vec![
            ManifestPayloadDescriptor {
                class: PayloadClass::TerrainChunk,
                key: PayloadKey::Chunk { x: 1, y: 0, z: -2 },
                slot: ManifestPayloadSlot::Tombstoned,
                schema_version: 4,
            },
            ManifestPayloadDescriptor {
                class: PayloadClass::MapMeta,
                key: PayloadKey::Singleton,
                slot: ManifestPayloadSlot::Present { blob: blob(b"meta") },
                schema_version: 1,
            },
        ];

        let finalized =
            finalize_manifest(payloads.clone(), MapInstanceId::Overworld, owner, 7, None, None)
                .unwrap();

        let mut expected_payloads = payloads;
        expected_payloads.sort_by_key(manifest_payload_descriptor_order);
        let expected_root = compute_descriptor_root(&expected_payloads).unwrap();
        let expected = NostrMapManifest {
            map_id: MapInstanceId::Overworld,
            owner,
            revision: 7,
            previous_hash: None,
            payloads: expected_payloads,
            schema_version: MAP_MANIFEST_SCHEMA_VERSION,
            descriptor_root: expected_root,
            homebase_attestation: None,
        };
        assert_eq!(finalized, expected);
    }

    #[test]
    fn overworld_manifest_omits_homebase_attestation_for_hash_back_compat() {
        let (_secret, owner) = owner();
        let overworld = manifest(owner);
        assert!(overworld.homebase_attestation.is_none());

        let json = String::from_utf8(canonical_manifest_bytes(&overworld).unwrap()).unwrap();
        assert!(
            !json.contains("homebase_attestation"),
            "overworld (None) manifest must omit the field so it hashes identically to \
             manifests published before the field existed: {json}"
        );

        // A legacy no-field manifest JSON parses and recomputes to the same hash, so its
        // `#d` tag still verifies.
        let reparsed = manifest_from_json(&json).unwrap();
        assert_eq!(
            compute_manifest_hash(&reparsed).unwrap(),
            compute_manifest_hash(&overworld).unwrap()
        );

        let mut homebase = overworld.clone();
        homebase.map_id = MapInstanceId::Homebase { owner };
        homebase.homebase_attestation = Some(protocol::HomebasePublicationAttestation {
            owner,
            map_id: MapInstanceId::Homebase { owner },
            server_revision: 0,
            previous_manifest_hash: None,
            descriptor_root: homebase.descriptor_root,
            payload_scope: protocol::HomebasePayloadScope::default(),
            expires_at: 0,
            server_pubkey: owner,
            server_signature: vec![1; 64],
        });
        let homebase_json =
            String::from_utf8(canonical_manifest_bytes(&homebase).unwrap()).unwrap();
        assert!(
            homebase_json.contains("homebase_attestation"),
            "homebase (Some) manifest must include the attestation field"
        );
    }

    fn signed_manifest_event(manifest: &NostrMapManifest, secret: SecretKey) -> String {
        manifest_event_draft(manifest)
            .unwrap()
            .sign_with_secret(secret)
            .unwrap()
    }

    #[test]
    fn map_persistence_manifest_event_verifies_signature_tags_and_descriptor_root() {
        let (secret, owner) = owner();
        let manifest = manifest(owner);
        let event_json = signed_manifest_event(&manifest, secret);
        let verified =
            verify_manifest_event_with_hash(&event_json, owner, &MapInstanceId::Overworld)
                .expect("manifest verifies");
        assert_eq!(verified.manifest, manifest);
        assert_eq!(
            verified.manifest_hash,
            compute_manifest_hash(&manifest).unwrap()
        );
    }

    #[test]
    fn map_persistence_manifest_rejects_pubkey_tampering() {
        let (secret, owner) = owner();
        let manifest = manifest(owner);
        let event_json = signed_manifest_event(&manifest, secret);
        let wrong_owner = NostrPublicKey([9; 32]);
        assert!(matches!(
            verify_manifest_event(&event_json, wrong_owner, &MapInstanceId::Overworld),
            Err(ManifestVerificationError::Invalid(_))
        ));
    }

    #[test]
    fn map_persistence_manifest_rejects_map_id_tampering() {
        let (secret, owner) = owner();
        let manifest = manifest(owner);
        let event_json = signed_manifest_event(&manifest, secret);
        assert!(matches!(
            verify_manifest_event(&event_json, owner, &MapInstanceId::Homebase { owner }),
            Err(ManifestVerificationError::Invalid(_))
        ));
    }

    #[test]
    fn map_persistence_manifest_rejects_kind_tampering() {
        let (secret, owner) = owner();
        let manifest = manifest(owner);
        let event_json = NostrEventDraft {
            kind: NostrEventKind::Custom(1),
            content: serde_json::to_string(&manifest).unwrap(),
            tags: manifest_event_tags(&manifest).unwrap(),
        }
        .sign_with_secret(secret)
        .unwrap();
        assert!(matches!(
            verify_manifest_event(&event_json, owner, &MapInstanceId::Overworld),
            Err(ManifestVerificationError::Invalid(_))
        ));
    }

    #[test]
    fn map_persistence_manifest_rejects_tag_tampering() {
        let (secret, owner) = owner();
        let manifest = manifest(owner);
        let mut tags = manifest_event_tags(&manifest).unwrap();
        tags.retain(|tag| tag.name != MAP_TAG);
        let event_json = NostrEventDraft {
            kind: NostrEventKind::Custom(NOSTR_KIND_MAP_MANIFEST),
            content: serde_json::to_string(&manifest).unwrap(),
            tags,
        }
        .sign_with_secret(secret)
        .unwrap();
        assert!(matches!(
            verify_manifest_event(&event_json, owner, &MapInstanceId::Overworld),
            Err(ManifestVerificationError::Invalid(_))
        ));
    }

    #[test]
    fn map_persistence_manifest_rejects_descriptor_slot_tampering() {
        let (_, owner) = owner();
        let mut manifest = manifest(owner);
        manifest.payloads[0].slot = ManifestPayloadSlot::Empty;
        assert!(matches!(
            verify_descriptor_root(&manifest),
            Err(ManifestVerificationError::Invalid(_))
        ));
    }

    #[test]
    fn map_persistence_manifest_descriptor_root_changes_for_each_field() {
        let (_, owner) = owner();
        let manifest = manifest(owner);
        let root = manifest.descriptor_root;

        let mut changed_class = manifest.payloads.clone();
        changed_class[0].class = PayloadClass::MapEntities;
        assert_ne!(root, compute_descriptor_root(&changed_class).unwrap());

        let mut changed_key = manifest.payloads.clone();
        changed_key[0].key = PayloadKey::Chunk { x: 1, y: 0, z: 0 };
        assert_ne!(root, compute_descriptor_root(&changed_key).unwrap());

        let mut changed_blob_hash = manifest.payloads.clone();
        if let ManifestPayloadSlot::Present { blob } = &mut changed_blob_hash[0].slot {
            blob.sha256 = [2; 32];
        }
        assert_ne!(root, compute_descriptor_root(&changed_blob_hash).unwrap());

        let mut changed_blob_size = manifest.payloads.clone();
        if let ManifestPayloadSlot::Present { blob } = &mut changed_blob_size[0].slot {
            blob.size += 1;
        }
        assert_ne!(root, compute_descriptor_root(&changed_blob_size).unwrap());
    }
}
