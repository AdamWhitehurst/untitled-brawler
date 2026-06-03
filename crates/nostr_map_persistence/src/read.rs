use std::collections::{BTreeMap, BTreeSet};

use nostr_client::events::{NostrEventClient, NostrEventKind, NostrEventQuery};
use protocol::{MapInstanceId, NostrPublicKey};
use thiserror::Error;

use crate::manifest::{
    compute_manifest_hash, manifest_hash_hex, map_tag_value, verify_descriptor_root,
    verify_manifest_event_with_hash, ManifestHash, ManifestPayloadDescriptor, ManifestPayloadSlot,
    MapPersistenceRejection, MapRevision, NostrMapManifest, PayloadClass, PayloadKey,
    PayloadSlotState, RawChunkEntitiesPayload, RawChunkPayload, RawMapEntitiesPayload,
    RawMapMetaPayload, RawMapPayloads, RawSaveBase, RawValidatedMapDelta, RawValidatedMapSave,
    MANIFEST_HASH_TAG, MAP_TAG, NOSTR_KIND_MAP_MANIFEST,
};
use crate::policy::{ManifestTieBreak, MapPersistencePolicy, NostrMapQueryPolicy};

/// Result of comparing a remote manifest chain to a local accepted head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevisionDecision {
    AtAcceptedHead,
    Descendant(Vec<NostrMapManifest>),
}

/// Remote read-path failures.
#[derive(Debug, Error)]
pub enum RemotePersistenceError {
    #[error("invalid remote data: {0}")]
    Invalid(String),
    #[error("remote chain is divergent: {0}")]
    Divergent(String),
    #[error("missing ancestor manifest {0}")]
    MissingAncestor(String),
    #[error("ambiguous ancestor manifest {0}")]
    AmbiguousAncestor(String),
    #[error("nostr event error: {0}")]
    Event(#[from] nostr_client::events::NostrEventError),
    #[error("manifest verification error: {0}")]
    Manifest(#[from] crate::manifest::ManifestVerificationError),
    #[error("blob read error: {0}")]
    Blob(#[from] nostr_client::blobs::BlobReadError),
}

impl From<RemotePersistenceError> for MapPersistenceRejection {
    fn from(value: RemotePersistenceError) -> Self {
        use nostr_client::blobs::BlobReadError;
        use nostr_client::events::NostrEventError;
        match value {
            RemotePersistenceError::Invalid(message) => Self::Invalid(message),
            RemotePersistenceError::Divergent(message) => Self::Divergent(message),
            RemotePersistenceError::MissingAncestor(hash) => {
                Self::Incomplete(format!("missing ancestor manifest {hash}"))
            }
            RemotePersistenceError::AmbiguousAncestor(hash) => {
                Self::Divergent(format!("ambiguous ancestor manifest {hash}"))
            }
            // Relay query/transport failures are environmental: fall back, do not block.
            RemotePersistenceError::Event(NostrEventError::Query(message)) => {
                Self::Unavailable(format!("nostr relay query: {message}"))
            }
            RemotePersistenceError::Event(error @ NostrEventError::Invalid(_)) => {
                Self::Invalid(format!("nostr event: {error}"))
            }
            RemotePersistenceError::Manifest(error) => {
                Self::Invalid(format!("manifest verification: {error}"))
            }
            // Blob transport failure is environmental; everything else is data we must reject.
            RemotePersistenceError::Blob(BlobReadError::Http(message)) => {
                Self::Unavailable(format!("blossom blob fetch: {message}"))
            }
            RemotePersistenceError::Blob(
                error @ (BlobReadError::InvalidUrl(_)
                | BlobReadError::MissingHost
                | BlobReadError::ForbiddenHost(_)
                | BlobReadError::NoAllowedUrls),
            ) => Self::Invalid(format!("blossom url policy: {error}")),
            RemotePersistenceError::Blob(
                error @ (BlobReadError::TooLarge { .. }
                | BlobReadError::SizeMismatch { .. }
                | BlobReadError::HashMismatch { .. }),
            ) => Self::Invalid(format!("blob verification: {error}")),
        }
    }
}

/// Verifies a remote manifest chain descends from the accepted local head.
pub fn verify_revision_chain(
    manifest_chain: &[NostrMapManifest],
    accepted_head: Option<MapRevision>,
) -> Result<RevisionDecision, MapPersistenceRejection> {
    if manifest_chain.is_empty() {
        return Err(MapPersistenceRejection::Incomplete(
            "manifest chain is empty".to_string(),
        ));
    }

    let head_hash = compute_manifest_hash(&manifest_chain[0])
        .map_err(|error| MapPersistenceRejection::Invalid(error.to_string()))?;
    if let Some(accepted) = &accepted_head {
        if head_hash == accepted.manifest_hash {
            if manifest_chain[0].revision != accepted.revision {
                return Err(MapPersistenceRejection::Invalid(
                    "accepted manifest hash has mismatched revision number".to_string(),
                ));
            }
            return Ok(RevisionDecision::AtAcceptedHead);
        }
        if manifest_chain[0].revision <= accepted.revision {
            return Err(MapPersistenceRejection::Divergent(
                "remote candidate is not newer than accepted head".to_string(),
            ));
        }
    }

    for pair in manifest_chain.windows(2) {
        let child = &pair[0];
        let parent = &pair[1];
        let parent_hash = compute_manifest_hash(parent)
            .map_err(|error| MapPersistenceRejection::Invalid(error.to_string()))?;
        if child.previous_hash != Some(parent_hash) {
            return Err(MapPersistenceRejection::Divergent(
                "manifest chain previous_hash does not match parent hash".to_string(),
            ));
        }
        if parent.revision >= child.revision {
            return Err(MapPersistenceRejection::Invalid(
                "manifest revision numbers must strictly increase".to_string(),
            ));
        }
    }

    let tail = manifest_chain
        .last()
        .expect("manifest_chain was checked non-empty");
    match accepted_head {
        Some(accepted) if tail.previous_hash != Some(accepted.manifest_hash) => {
            Err(MapPersistenceRejection::Divergent(
                "manifest chain does not descend from accepted head".to_string(),
            ))
        }
        None if tail.previous_hash.is_some() => Err(MapPersistenceRejection::Incomplete(
            "manifest chain without accepted head must include genesis".to_string(),
        )),
        _ => Ok(RevisionDecision::Descendant(manifest_chain.to_vec())),
    }
}

/// Fetches one manifest by manifest hash using indexed `#x` tags.
pub async fn fetch_manifest_by_hash(
    client: &NostrEventClient,
    owner: NostrPublicKey,
    map_id: &MapInstanceId,
    manifest_hash: ManifestHash,
    policy: NostrMapQueryPolicy,
) -> Result<Option<NostrMapManifest>, RemotePersistenceError> {
    let events = client
        .query(
            NostrEventQuery::new()
                .author(owner)
                .kind(NostrEventKind::Custom(NOSTR_KIND_MAP_MANIFEST))
                .tag(MANIFEST_HASH_TAG, manifest_hash_hex(manifest_hash))
                .limit(policy.limit)
                .timeout(policy.timeout),
        )
        .await?;
    let mut matches = Vec::new();
    for event_json in events {
        let verified = verify_manifest_event_with_hash(&event_json, owner, map_id)?;
        if verified.manifest_hash == manifest_hash {
            matches.push(verified.manifest);
        }
    }
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        _ => Err(RemotePersistenceError::AmbiguousAncestor(
            manifest_hash_hex(manifest_hash),
        )),
    }
}

/// Fetches manifest ancestors until the accepted head or genesis is reached.
pub async fn fetch_manifest_ancestors(
    client: &NostrEventClient,
    head: &NostrMapManifest,
    accepted_head: Option<MapRevision>,
    policy: NostrMapQueryPolicy,
) -> Result<Vec<NostrMapManifest>, RemotePersistenceError> {
    let mut chain = vec![head.clone()];
    let mut current = head.clone();
    let mut seen = BTreeSet::new();
    seen.insert(compute_manifest_hash(&current)?);

    loop {
        let current_hash = compute_manifest_hash(&current)?;
        if accepted_head
            .as_ref()
            .is_some_and(|accepted| accepted.manifest_hash == current_hash)
        {
            return Ok(chain);
        }

        let Some(previous_hash) = current.previous_hash else {
            if accepted_head.is_some() {
                return Err(RemotePersistenceError::Divergent(
                    "remote chain reached genesis before accepted head".to_string(),
                ));
            }
            return Ok(chain);
        };
        if accepted_head
            .as_ref()
            .is_some_and(|accepted| accepted.manifest_hash == previous_hash)
        {
            return Ok(chain);
        }

        let Some(ancestor) = fetch_manifest_by_hash(
            client,
            current.owner,
            &current.map_id,
            previous_hash,
            policy.clone(),
        )
        .await?
        else {
            return Err(RemotePersistenceError::MissingAncestor(manifest_hash_hex(
                previous_hash,
            )));
        };
        if ancestor.revision >= current.revision {
            return Err(RemotePersistenceError::Invalid(
                "ancestor revision must be lower than child revision".to_string(),
            ));
        }
        if !seen.insert(previous_hash) {
            return Err(RemotePersistenceError::Invalid(
                "manifest chain contains a hash cycle".to_string(),
            ));
        }
        current = ancestor.clone();
        chain.push(ancestor);
    }
}

/// Returns the latest visible manifest according to the configured query policy.
pub async fn latest_visible_manifest(
    client: &NostrEventClient,
    owner: NostrPublicKey,
    map_id: &MapInstanceId,
    policy: NostrMapQueryPolicy,
) -> Result<Option<NostrMapManifest>, RemotePersistenceError> {
    let map_key = map_tag_value(owner, map_id);
    let events = client
        .query(
            NostrEventQuery::new()
                .author(owner)
                .kind(NostrEventKind::Custom(NOSTR_KIND_MAP_MANIFEST))
                .tag(MAP_TAG, map_key)
                .limit(policy.limit)
                .timeout(policy.timeout),
        )
        .await?;

    let mut manifests = Vec::new();
    for event_json in events {
        manifests.push(verify_manifest_event_with_hash(&event_json, owner, map_id)?);
    }
    manifests.sort_by(|left, right| {
        let ordering = left
            .manifest
            .revision
            .cmp(&right.manifest.revision)
            .then_with(|| left.manifest_hash.cmp(&right.manifest_hash));
        match policy.tie_break {
            ManifestTieBreak::HighestHash => ordering,
            ManifestTieBreak::LowestHash => left
                .manifest
                .revision
                .cmp(&right.manifest.revision)
                .then_with(|| right.manifest_hash.cmp(&left.manifest_hash)),
        }
    });
    Ok(manifests.pop().map(|verified| verified.manifest))
}

/// Downloads and verifies all present payload blobs in a manifest chain.
pub async fn download_payloads(
    manifest_chain: &[NostrMapManifest],
    policy: MapPersistencePolicy,
) -> Result<RawMapPayloads, RemotePersistenceError> {
    let descriptor_count = manifest_chain
        .iter()
        .map(|manifest| manifest.payloads.len())
        .sum::<usize>();
    if descriptor_count > policy.max_payloads {
        return Err(RemotePersistenceError::Invalid(format!(
            "manifest chain references {descriptor_count} payloads; limit is {}",
            policy.max_payloads
        )));
    }

    let blob_policy = nostr_client::blobs::BlobFetchPolicy {
        max_bytes: policy.max_blob_bytes,
        allowed_hosts: policy.allowed_blossom_hosts.clone(),
    };
    let mut present_payloads = Vec::with_capacity(descriptor_count);
    for manifest in manifest_chain {
        verify_descriptor_root(manifest)?;
        for descriptor in &manifest.payloads {
            if !policy.allowed_payload_classes.contains(&descriptor.class) {
                return Err(RemotePersistenceError::Invalid(format!(
                    "payload class {:?} is not allowed",
                    descriptor.class
                )));
            }
            match &descriptor.slot {
                ManifestPayloadSlot::Present { blob } => {
                    let verified =
                        nostr_client::blobs::fetch_and_verify_blob(blob, &blob_policy).await?;
                    present_payloads.push((descriptor.clone(), verified.bytes));
                }
                ManifestPayloadSlot::Empty
                | ManifestPayloadSlot::Absent
                | ManifestPayloadSlot::Tombstoned => {}
            }
        }
    }
    Ok(RawMapPayloads { present_payloads })
}

/// Validates raw payloads against a manifest chain and assembles a raw save.
pub fn validate_remote_map_save(
    manifest_chain: Vec<NostrMapManifest>,
    payloads: RawMapPayloads,
    policy: MapPersistencePolicy,
    base: RawSaveBase,
) -> Result<RawValidatedMapSave, MapPersistenceRejection> {
    if manifest_chain.is_empty() {
        return Err(MapPersistenceRejection::Incomplete(
            "remote manifest chain is empty".to_string(),
        ));
    }

    let mut bytes_by_descriptor = BTreeMap::new();
    for (descriptor, bytes) in payloads.present_payloads {
        let ManifestPayloadSlot::Present { blob } = &descriptor.slot else {
            return Err(MapPersistenceRejection::Invalid(
                "raw payload bytes supplied for non-present descriptor".to_string(),
            ));
        };
        let verified = nostr_client::blobs::verify_blob_bytes(blob.sha256, Some(blob.size), bytes)
            .map_err(|error| MapPersistenceRejection::Invalid(error.to_string()))?;
        bytes_by_descriptor.insert(
            (
                descriptor.class.clone(),
                descriptor.key.clone(),
                blob.sha256,
            ),
            verified.bytes,
        );
    }

    let mut deltas = Vec::new();
    for manifest in manifest_chain.into_iter().rev() {
        verify_descriptor_root(&manifest)
            .map_err(|error| MapPersistenceRejection::Invalid(error.to_string()))?;
        if manifest.payloads.len() > policy.max_payloads {
            return Err(MapPersistenceRejection::Invalid(
                "manifest payload count exceeds policy".to_string(),
            ));
        }
        deltas.push(raw_delta_from_manifest(
            manifest,
            &policy,
            &bytes_by_descriptor,
        )?);
    }
    assemble_raw_validated_map_save(base, deltas)
}

fn raw_delta_from_manifest(
    manifest: NostrMapManifest,
    policy: &MapPersistencePolicy,
    bytes_by_descriptor: &BTreeMap<(PayloadClass, PayloadKey, [u8; 32]), Vec<u8>>,
) -> Result<RawValidatedMapDelta, MapPersistenceRejection> {
    let manifest_hash = compute_manifest_hash(&manifest)
        .map_err(|error| MapPersistenceRejection::Invalid(error.to_string()))?;
    let revision = MapRevision {
        revision: manifest.revision,
        previous_hash: manifest.previous_hash,
        manifest_hash,
    };
    let mut delta = RawValidatedMapDelta {
        revision,
        meta: PayloadSlotState::Absent,
        chunks: Vec::new(),
        chunk_entities: Vec::new(),
        map_entities: PayloadSlotState::Absent,
    };

    for descriptor in manifest.payloads {
        if !policy.allowed_payload_classes.contains(&descriptor.class) {
            return Err(MapPersistenceRejection::Invalid(format!(
                "payload class {:?} is not allowed",
                descriptor.class
            )));
        }
        match descriptor.class {
            PayloadClass::MapMeta => {
                require_singleton(&descriptor)?;
                delta.meta = descriptor_slot_to_payload(&descriptor, bytes_by_descriptor)?
                    .map_slot(|bytes| RawMapMetaPayload { bytes });
            }
            PayloadClass::TerrainChunk => {
                let key = require_chunk_key(&descriptor)?;
                let slot = descriptor_slot_to_payload(&descriptor, bytes_by_descriptor)?
                    .map_slot(|bytes| RawChunkPayload { bytes });
                delta.chunks.push((key, slot));
            }
            PayloadClass::ChunkEntities => {
                let key = require_chunk_key(&descriptor)?;
                let slot = descriptor_slot_to_payload(&descriptor, bytes_by_descriptor)?
                    .map_slot(|bytes| RawChunkEntitiesPayload { bytes });
                delta.chunk_entities.push((key, slot));
            }
            PayloadClass::MapEntities => {
                require_singleton(&descriptor)?;
                delta.map_entities = descriptor_slot_to_payload(&descriptor, bytes_by_descriptor)?
                    .map_slot(|bytes| RawMapEntitiesPayload { bytes });
            }
        }
    }
    Ok(delta)
}

trait MapSlot<T> {
    fn map_slot<U>(self, f: impl FnOnce(T) -> U) -> PayloadSlotState<U>;
}

impl<T> MapSlot<T> for PayloadSlotState<T> {
    fn map_slot<U>(self, f: impl FnOnce(T) -> U) -> PayloadSlotState<U> {
        match self {
            PayloadSlotState::Present(value) => PayloadSlotState::Present(f(value)),
            PayloadSlotState::Empty => PayloadSlotState::Empty,
            PayloadSlotState::Absent => PayloadSlotState::Absent,
            PayloadSlotState::Tombstoned => PayloadSlotState::Tombstoned,
        }
    }
}

fn descriptor_slot_to_payload(
    descriptor: &ManifestPayloadDescriptor,
    bytes_by_descriptor: &BTreeMap<(PayloadClass, PayloadKey, [u8; 32]), Vec<u8>>,
) -> Result<PayloadSlotState<Vec<u8>>, MapPersistenceRejection> {
    match &descriptor.slot {
        ManifestPayloadSlot::Present { blob } => bytes_by_descriptor
            .get(&(
                descriptor.class.clone(),
                descriptor.key.clone(),
                blob.sha256,
            ))
            .cloned()
            .map(PayloadSlotState::Present)
            .ok_or_else(|| {
                MapPersistenceRejection::Incomplete(format!(
                    "missing payload bytes for {:?} {:?}",
                    descriptor.class, descriptor.key
                ))
            }),
        ManifestPayloadSlot::Empty => Ok(PayloadSlotState::Empty),
        ManifestPayloadSlot::Absent => Ok(PayloadSlotState::Absent),
        ManifestPayloadSlot::Tombstoned => Ok(PayloadSlotState::Tombstoned),
    }
}

fn require_singleton(
    descriptor: &ManifestPayloadDescriptor,
) -> Result<(), MapPersistenceRejection> {
    if descriptor.key != PayloadKey::Singleton {
        return Err(MapPersistenceRejection::Invalid(format!(
            "payload class {:?} requires singleton key",
            descriptor.class
        )));
    }
    Ok(())
}

fn require_chunk_key(
    descriptor: &ManifestPayloadDescriptor,
) -> Result<PayloadKey, MapPersistenceRejection> {
    match descriptor.key {
        PayloadKey::Chunk { .. } => Ok(descriptor.key.clone()),
        PayloadKey::Singleton => Err(MapPersistenceRejection::Invalid(format!(
            "payload class {:?} requires chunk key",
            descriptor.class
        ))),
    }
}

/// Replays raw deltas over a base save.
pub fn assemble_raw_validated_map_save(
    base: RawSaveBase,
    deltas: Vec<RawValidatedMapDelta>,
) -> Result<RawValidatedMapSave, MapPersistenceRejection> {
    let mut save = match base {
        RawSaveBase::Snapshot(save) => Some(save),
        RawSaveBase::Empty => None,
    };

    for delta in deltas {
        let mut current = save.take().unwrap_or(RawValidatedMapSave {
            meta: RawMapMetaPayload { bytes: Vec::new() },
            chunks: Vec::new(),
            chunk_entities: Vec::new(),
            map_entities: None,
            revision: delta.revision.clone(),
        });
        apply_required_meta_slot(&mut current, delta.meta)?;
        apply_keyed_slots(&mut current.chunks, delta.chunks);
        apply_keyed_slots(&mut current.chunk_entities, delta.chunk_entities);
        match delta.map_entities {
            PayloadSlotState::Present(payload) => current.map_entities = Some(payload),
            PayloadSlotState::Empty => {
                current.map_entities = Some(RawMapEntitiesPayload { bytes: Vec::new() });
            }
            PayloadSlotState::Absent => {}
            PayloadSlotState::Tombstoned => current.map_entities = None,
        }
        current.revision = delta.revision;
        save = Some(current);
    }

    let save = save.ok_or_else(|| {
        MapPersistenceRejection::Incomplete("remote save has no revisions".to_string())
    })?;
    if save.meta.bytes.is_empty() {
        return Err(MapPersistenceRejection::Incomplete(
            "remote save is missing map metadata".to_string(),
        ));
    }
    Ok(save)
}

fn apply_required_meta_slot(
    save: &mut RawValidatedMapSave,
    slot: PayloadSlotState<RawMapMetaPayload>,
) -> Result<(), MapPersistenceRejection> {
    match slot {
        PayloadSlotState::Present(payload) => save.meta = payload,
        PayloadSlotState::Absent => {}
        PayloadSlotState::Empty | PayloadSlotState::Tombstoned => {
            return Err(MapPersistenceRejection::Invalid(
                "map metadata cannot be empty or tombstoned".to_string(),
            ));
        }
    }
    Ok(())
}

fn apply_keyed_slots<T>(
    target: &mut Vec<(PayloadKey, T)>,
    slots: Vec<(PayloadKey, PayloadSlotState<T>)>,
) {
    for (key, slot) in slots {
        match slot {
            PayloadSlotState::Present(payload) => upsert_keyed_payload(target, key, payload),
            PayloadSlotState::Empty | PayloadSlotState::Tombstoned => {
                target.retain(|(existing_key, _)| existing_key != &key);
            }
            PayloadSlotState::Absent => {}
        }
    }
}

fn upsert_keyed_payload<T>(target: &mut Vec<(PayloadKey, T)>, key: PayloadKey, payload: T) {
    if let Some((_, existing)) = target
        .iter_mut()
        .find(|(existing_key, _)| existing_key == &key)
    {
        *existing = payload;
    } else {
        target.push((key, payload));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        compute_descriptor_root, manifest_event_draft, ManifestPayloadSlot,
        MAP_MANIFEST_SCHEMA_VERSION,
    };
    use nostr_sdk::{Keys, SecretKey};
    use sha2::{Digest, Sha256};

    fn owner() -> (SecretKey, NostrPublicKey) {
        let secret = SecretKey::generate();
        let keys = Keys::new(secret.clone());
        (secret, NostrPublicKey(*keys.public_key().as_bytes()))
    }

    fn blob(bytes: &[u8]) -> nostr_client::BlobRef {
        nostr_client::BlobRef {
            sha256: Sha256::digest(bytes).into(),
            size: bytes.len() as u64,
            content_type: "application/octet-stream".to_string(),
            urls: vec!["https://example.com/blob".to_string()],
        }
    }

    fn manifest_with_payloads(
        owner: NostrPublicKey,
        revision: u64,
        previous_hash: Option<ManifestHash>,
        payloads: Vec<ManifestPayloadDescriptor>,
    ) -> NostrMapManifest {
        let descriptor_root = compute_descriptor_root(&payloads).unwrap();
        NostrMapManifest {
            map_id: MapInstanceId::Overworld,
            owner,
            revision,
            previous_hash,
            payloads,
            schema_version: MAP_MANIFEST_SCHEMA_VERSION,
            descriptor_root,
            homebase_attestation: None,
        }
    }

    fn meta_descriptor(bytes: &[u8]) -> ManifestPayloadDescriptor {
        ManifestPayloadDescriptor {
            class: PayloadClass::MapMeta,
            key: PayloadKey::Singleton,
            slot: ManifestPayloadSlot::Present { blob: blob(bytes) },
            schema_version: 1,
        }
    }

    #[test]
    fn map_persistence_revision_chain_accepts_descendant() {
        let (_, owner) = owner();
        let genesis = manifest_with_payloads(owner, 1, None, vec![meta_descriptor(b"one")]);
        let genesis_hash = compute_manifest_hash(&genesis).unwrap();
        let head =
            manifest_with_payloads(owner, 2, Some(genesis_hash), vec![meta_descriptor(b"two")]);
        assert!(matches!(
            verify_revision_chain(&[head, genesis], None),
            Ok(RevisionDecision::Descendant(_))
        ));
    }

    #[test]
    fn map_persistence_revision_chain_rejects_divergent_parent() {
        let (_, owner) = owner();
        let genesis = manifest_with_payloads(owner, 1, None, vec![meta_descriptor(b"one")]);
        let head = manifest_with_payloads(owner, 2, Some([9; 32]), vec![meta_descriptor(b"two")]);
        assert!(matches!(
            verify_revision_chain(&[head, genesis], None),
            Err(MapPersistenceRejection::Divergent(_))
        ));
    }

    #[test]
    fn map_persistence_revision_chain_rejects_revision_tampering() {
        let (_, owner) = owner();
        let genesis = manifest_with_payloads(owner, 2, None, vec![meta_descriptor(b"one")]);
        let genesis_hash = compute_manifest_hash(&genesis).unwrap();
        let head =
            manifest_with_payloads(owner, 2, Some(genesis_hash), vec![meta_descriptor(b"two")]);
        assert!(matches!(
            verify_revision_chain(&[head, genesis], None),
            Err(MapPersistenceRejection::Invalid(_))
        ));
    }

    #[test]
    fn map_persistence_fetches_ancestor_by_manifest_hash_tag() {
        let (secret, owner) = owner();
        let genesis = manifest_with_payloads(owner, 1, None, vec![meta_descriptor(b"one")]);
        let genesis_hash = compute_manifest_hash(&genesis).unwrap();
        let head =
            manifest_with_payloads(owner, 2, Some(genesis_hash), vec![meta_descriptor(b"two")]);
        let genesis_event = manifest_event_draft(&genesis)
            .unwrap()
            .sign_with_secret(secret.clone())
            .unwrap();
        let head_event = manifest_event_draft(&head)
            .unwrap()
            .sign_with_secret(secret)
            .unwrap();
        let client = NostrEventClient::from_events(vec![head_event, genesis_event]);
        let chain = futures_lite::future::block_on(fetch_manifest_ancestors(
            &client,
            &head,
            None,
            NostrMapQueryPolicy::default(),
        ))
        .expect("ancestor fetch");
        assert_eq!(chain, vec![head, genesis]);
    }

    #[test]
    fn map_persistence_validate_remote_map_save_rejects_missing_payload_bytes() {
        let (_, owner) = owner();
        let manifest = manifest_with_payloads(owner, 1, None, vec![meta_descriptor(b"meta")]);
        let rejection = validate_remote_map_save(
            vec![manifest],
            RawMapPayloads {
                present_payloads: Vec::new(),
            },
            MapPersistencePolicy::default(),
            RawSaveBase::Empty,
        )
        .expect_err("missing bytes rejected");
        assert!(matches!(rejection, MapPersistenceRejection::Incomplete(_)));
    }

    #[test]
    fn map_persistence_rejection_classifies_relay_query_failure_as_unavailable() {
        let rejection: MapPersistenceRejection = RemotePersistenceError::Event(
            nostr_client::events::NostrEventError::Query("connection refused".into()),
        )
        .into();
        assert!(matches!(rejection, MapPersistenceRejection::Unavailable(_)));
    }

    #[test]
    fn map_persistence_rejection_classifies_blob_http_failure_as_unavailable() {
        let rejection: MapPersistenceRejection = RemotePersistenceError::Blob(
            nostr_client::blobs::BlobReadError::Http("timed out".into()),
        )
        .into();
        assert!(matches!(rejection, MapPersistenceRejection::Unavailable(_)));
    }

    #[test]
    fn map_persistence_rejection_classifies_blob_policy_and_verification_as_invalid() {
        let policy: MapPersistenceRejection = RemotePersistenceError::Blob(
            nostr_client::blobs::BlobReadError::ForbiddenHost("evil.example".into()),
        )
        .into();
        let MapPersistenceRejection::Invalid(policy_message) = policy else {
            panic!("blob URL policy rejection must classify as Invalid");
        };
        assert!(policy_message.contains("blossom url policy"));

        let verification: MapPersistenceRejection =
            RemotePersistenceError::Blob(nostr_client::blobs::BlobReadError::HashMismatch {
                expected: [0; 32],
                actual: [1; 32],
            })
            .into();
        let MapPersistenceRejection::Invalid(verification_message) = verification else {
            panic!("blob hash mismatch must classify as Invalid");
        };
        assert!(verification_message.contains("blob verification"));
    }

    #[test]
    fn map_persistence_validate_remote_map_save_assembles_present_payload() {
        let (_, owner) = owner();
        let bytes = b"meta".to_vec();
        let descriptor = meta_descriptor(&bytes);
        let manifest = manifest_with_payloads(owner, 1, None, vec![descriptor.clone()]);
        let save = validate_remote_map_save(
            vec![manifest],
            RawMapPayloads {
                present_payloads: vec![(descriptor, bytes.clone())],
            },
            MapPersistencePolicy::default(),
            RawSaveBase::Empty,
        )
        .expect("raw save assembled");
        assert_eq!(save.meta.bytes, bytes);
    }
}
