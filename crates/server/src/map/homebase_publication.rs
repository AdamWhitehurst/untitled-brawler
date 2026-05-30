use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bevy::prelude::*;
use lightyear::prelude::{MessageReceiver, MessageSender};
use nostr_client::{verify_payload_schnorr, BlobRef, NostrKeys};
use nostr_map_persistence::attestation::{
    sign_homebase_attestation, AttestationSigner, AttestationVerifier,
};
use nostr_map_persistence::manifest::{ManifestPayloadSlot, PayloadClass, PayloadKey};
use nostr_map_persistence::{
    compute_descriptor_root, encode_chunk_entities_payload, encode_chunk_payload,
    encode_map_entities_payload, encode_map_meta_payload, validate_homebase_manifest_attestation,
    HomebasePayloadScope, HomebasePublicationAttestation, ManifestPayloadDescriptor,
    MapPersistenceRejection, MapRevision, NostrMapManifest, CHUNK_ENTITIES_SCHEMA_VERSION,
    MAP_ENTITIES_SCHEMA_VERSION, MAP_META_SCHEMA_VERSION,
};
use persistence::Store;
use protocol::map::{HomebaseAttestationRequest, HomebaseAttestationResponse, MapChannel};
use protocol::{MapInstanceId, NostrPublicKey, PlayerIdentity};
use sha2::{Digest, Sha256};
use voxel_map_engine::persistence::fs_chunk::FsChunkStore;
use voxel_map_engine::persistence::fs_chunk_entities::FsChunkEntitiesStore;
use voxel_map_engine::persistence::{ChunkFileEnvelope, CHUNK_SAVE_VERSION};

use crate::persistence::fs_map_entities::FsMapEntitiesStore;
use crate::persistence::fs_map_meta::FsMapMetaStore;
use crate::persistence::{
    map_save_dir, store_map_dir_for_loading, FsAcceptedMapHeadStore, WorldSavePath,
};

/// Validity window for issued homebase publication attestations.
pub const HOMEBASE_ATTESTATION_TTL_SECONDS: u64 = 600;

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

/// Server boundary for accepting a client-published homebase manifest: the player
/// signature over the manifest event is verified upstream, this enforces the server
/// attestation gate. Expiry is not checked so historical revisions stay loadable.
///
/// Progression-bearing-data rejection and entitlement enforcement (plan 5.7) are a
/// deferred follow-up and are NOT applied here.
pub fn validate_homebase_manifest_import(
    manifest: &NostrMapManifest,
) -> Result<(), MapPersistenceRejection> {
    validate_homebase_manifest_attestation(&ServerAttestationVerifier, manifest)
}

/// Builds a `Present` manifest descriptor from already-encoded payload bytes.
///
/// The descriptor root only depends on class/key/schema_version/sha256/size, so the
/// server can reproduce a client's descriptor root from read-back filesystem bytes
/// without uploading blobs or knowing the client's Blossom URL.
fn present_descriptor(
    class: PayloadClass,
    key: PayloadKey,
    schema_version: u32,
    bytes: &[u8],
) -> ManifestPayloadDescriptor {
    let sha256: [u8; 32] = Sha256::digest(bytes).into();
    ManifestPayloadDescriptor {
        class,
        key,
        slot: ManifestPayloadSlot::Present {
            blob: BlobRef {
                sha256,
                size: bytes.len() as u64,
                content_type: "application/octet-stream".to_string(),
                urls: Vec::new(),
            },
        },
        schema_version,
    }
}

/// Parses an `IVec3` chunk position from a `chunk_{x}_{y}_{z}` file stem.
fn parse_chunk_pos(stem: &str) -> Option<IVec3> {
    let rest = stem.strip_prefix("chunk_")?;
    let mut parts = rest.split('_');
    let x = parts.next()?.parse().ok()?;
    let y = parts.next()?.parse().ok()?;
    let z = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(IVec3::new(x, y, z))
}

/// Lists saved chunk positions in `<map_dir>/<dir>` whose file names end with `suffix`.
fn list_saved_chunk_positions(
    map_dir: &Path,
    dir: &str,
    suffix: &str,
) -> Result<Vec<IVec3>, MapPersistenceRejection> {
    let path = map_dir.join(dir);
    if !path.exists() {
        trace!(
            ?path,
            "homebase {dir} directory absent during attestation read-back"
        );
        return Ok(Vec::new());
    }
    let mut positions = Vec::new();
    for entry in std::fs::read_dir(&path)
        .map_err(|e| MapPersistenceRejection::Filesystem(format!("read {dir} dir: {e}")))?
    {
        let entry = entry
            .map_err(|e| MapPersistenceRejection::Filesystem(format!("read {dir} entry: {e}")))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(suffix) else {
            trace!(
                ?name,
                "skipping non-payload file during attestation read-back"
            );
            continue;
        };
        let Some(pos) = parse_chunk_pos(stem) else {
            return Err(MapPersistenceRejection::Invalid(format!(
                "unparseable chunk file name: {name}"
            )));
        };
        positions.push(pos);
    }
    positions.sort_by_key(|pos| (pos.x, pos.y, pos.z));
    Ok(positions)
}

/// Reads the materialized homebase save from disk and recomputes the authoritative
/// descriptor root, payload scope, and revision the player's publish must match.
///
/// This is the server's source of truth for attestation: it re-encodes every saved
/// slot with the shared payload encoders so the root matches a faithful client publish.
pub fn read_authoritative_homebase_state(
    save_root: &Path,
    owner: NostrPublicKey,
) -> Result<AuthoritativeHomebaseState, MapPersistenceRejection> {
    let map_id = MapInstanceId::Homebase { owner };
    let canonical_map_dir = map_save_dir(save_root, &map_id);
    let map_dir = store_map_dir_for_loading(&canonical_map_dir)
        .map_err(|e| MapPersistenceRejection::Filesystem(format!("resolve homebase dir: {e}")))?;
    let map_dir_arc = Arc::new(map_dir.clone());

    let mut payloads = Vec::new();
    let mut scope = HomebasePayloadScope::default();

    let meta = FsMapMetaStore {
        map_dir: map_dir_arc.clone(),
    }
    .load(&())
    .map_err(|e| MapPersistenceRejection::Filesystem(format!("load homebase meta: {e}")))?;
    if let Some(meta) = meta {
        let spawn_points = meta
            .spawn_points
            .iter()
            .map(|point| [point.x, point.y, point.z])
            .collect();
        let bytes = encode_map_meta_payload(
            meta.version,
            meta.seed,
            meta.generation_version,
            spawn_points,
        )?;
        payloads.push(present_descriptor(
            PayloadClass::MapMeta,
            PayloadKey::Singleton,
            MAP_META_SCHEMA_VERSION,
            &bytes,
        ));
        scope.includes_meta = true;
    }

    let chunk_store = FsChunkStore {
        map_dir: map_dir_arc.clone(),
    };
    for chunk_pos in list_saved_chunk_positions(&map_dir, "terrain", ".bin")? {
        let envelope: ChunkFileEnvelope = chunk_store
            .load(&chunk_pos)
            .map_err(|e| MapPersistenceRejection::Filesystem(format!("load terrain chunk: {e}")))?
            .ok_or_else(|| {
                MapPersistenceRejection::Incomplete(format!(
                    "listed terrain chunk {chunk_pos} missing on read-back"
                ))
            })?;
        let bytes = encode_chunk_payload(envelope)?;
        payloads.push(present_descriptor(
            PayloadClass::TerrainChunk,
            PayloadKey::Chunk {
                x: chunk_pos.x,
                y: chunk_pos.y,
                z: chunk_pos.z,
            },
            CHUNK_SAVE_VERSION,
            &bytes,
        ));
        scope.terrain_chunks.push(chunk_pos);
    }

    let chunk_entities_store = FsChunkEntitiesStore {
        map_dir: map_dir_arc.clone(),
    };
    for chunk_pos in list_saved_chunk_positions(&map_dir, "entities", ".entities.bin")? {
        let spawns = chunk_entities_store
            .load(&chunk_pos)
            .map_err(|e| MapPersistenceRejection::Filesystem(format!("load chunk entities: {e}")))?
            .ok_or_else(|| {
                MapPersistenceRejection::Incomplete(format!(
                    "listed chunk entities {chunk_pos} missing on read-back"
                ))
            })?;
        let bytes = encode_chunk_entities_payload(spawns)?;
        payloads.push(present_descriptor(
            PayloadClass::ChunkEntities,
            PayloadKey::Chunk {
                x: chunk_pos.x,
                y: chunk_pos.y,
                z: chunk_pos.z,
            },
            CHUNK_ENTITIES_SCHEMA_VERSION,
            &bytes,
        ));
        scope.chunk_entities.push(chunk_pos);
    }

    let map_entities = FsMapEntitiesStore {
        map_dir: map_dir_arc.clone(),
    }
    .load(&())
    .map_err(|e| MapPersistenceRejection::Filesystem(format!("load map entities: {e}")))?;
    if let Some(entities) = map_entities {
        let bytes = encode_map_entities_payload(entities)?;
        payloads.push(present_descriptor(
            PayloadClass::MapEntities,
            PayloadKey::Singleton,
            MAP_ENTITIES_SCHEMA_VERSION,
            &bytes,
        ));
        scope.includes_map_entities = true;
    }

    let descriptor_root = compute_descriptor_root(&payloads)
        .map_err(|e| MapPersistenceRejection::Invalid(format!("descriptor root: {e}")))?;

    let accepted_head = FsAcceptedMapHeadStore {
        map_dir: map_dir_arc,
    }
    .load(&())
    .map_err(|e| MapPersistenceRejection::Filesystem(format!("load accepted head: {e}")))?;
    let (server_revision, previous_manifest_hash) = match accepted_head {
        Some(MapRevision {
            revision,
            manifest_hash,
            ..
        }) => (revision + 1, Some(manifest_hash)),
        None => (0, None),
    };

    Ok(AuthoritativeHomebaseState {
        owner,
        map_id,
        server_revision,
        previous_manifest_hash,
        descriptor_root,
        payload_scope: scope,
    })
}

/// Current unix time in seconds for attestation issuance/expiry.
fn now_unix_seconds() -> Result<u64, MapPersistenceRejection> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|e| {
            MapPersistenceRejection::Invalid(format!("system clock before unix epoch: {e}"))
        })
}

/// Issues server-signed attestations for client homebase publication requests.
///
/// Authoritative homebase state is read back from the materialized filesystem save and
/// rehashed, so an attestation reflects the last server-side save. The request must match
/// the read-back descriptor root and payload scope; otherwise it is rejected.
pub fn handle_homebase_attestation_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<HomebaseAttestationRequest>)>,
    player_identities: Query<&PlayerIdentity>,
    mut responders: Query<&mut MessageSender<HomebaseAttestationResponse>>,
    server_identity: Res<NostrKeys>,
    save_path: Res<WorldSavePath>,
) {
    for (client_entity, mut receiver) in &mut receivers {
        for request in receiver.receive() {
            let response = issue_homebase_attestation(
                client_entity,
                &request,
                &player_identities,
                &server_identity,
                &save_path.0,
            );
            match responders.get_mut(client_entity) {
                Ok(mut sender) => sender.send::<MapChannel>(response),
                Err(_) => warn!(
                    ?client_entity,
                    "client requesting homebase attestation has no response sender"
                ),
            }
        }
    }
}

/// Resolves the request to an attestation response, rejecting unauthenticated or mismatched requests.
fn issue_homebase_attestation(
    client_entity: Entity,
    request: &HomebaseAttestationRequest,
    player_identities: &Query<&PlayerIdentity>,
    server_identity: &NostrKeys,
    save_root: &Path,
) -> HomebaseAttestationResponse {
    let Ok(identity) = player_identities.get(client_entity) else {
        return HomebaseAttestationResponse::Rejected("client is not authenticated".to_string());
    };
    let owner = identity.0;
    let map_id = MapInstanceId::Homebase { owner };

    let result = read_authoritative_homebase_state(save_root, owner).and_then(|state| {
        let now_unix = now_unix_seconds()?;
        verify_homebase_publication_attestation_request(
            &ServerAttestationSigner(server_identity),
            owner,
            &map_id,
            request.descriptor_root,
            &request.payload_scope,
            &state,
            now_unix,
            HOMEBASE_ATTESTATION_TTL_SECONDS,
        )
    });
    match result {
        Ok(attestation) => HomebaseAttestationResponse::Granted(attestation),
        Err(rejection) => {
            warn!(
                ?client_entity,
                ?rejection,
                "rejected homebase attestation request"
            );
            HomebaseAttestationResponse::Rejected(format!("{rejection:?}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let result = verify_homebase_publication_attestation_request(
            &ServerAttestationSigner(&keys),
            owner,
            &MapInstanceId::Overworld,
            state.descriptor_root,
            &state.payload_scope,
            &state,
            1_000,
            600,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_foreign_owner_request() {
        let keys = server_keys();
        let owner = NostrPublicKey([42; 32]);
        let foreign = NostrPublicKey([99; 32]);
        let state = authoritative(owner);
        let result = verify_homebase_publication_attestation_request(
            &ServerAttestationSigner(&keys),
            foreign,
            &MapInstanceId::Homebase { owner },
            state.descriptor_root,
            &state.payload_scope,
            &state,
            1_000,
            600,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_descriptor_root_divergence() {
        let keys = server_keys();
        let owner = NostrPublicKey([42; 32]);
        let state = authoritative(owner);
        let result = verify_homebase_publication_attestation_request(
            &ServerAttestationSigner(&keys),
            owner,
            &MapInstanceId::Homebase { owner },
            [0; 32],
            &state.payload_scope,
            &state,
            1_000,
            600,
        );
        assert!(result.is_err());
    }

    #[test]
    fn parses_chunk_positions_including_negatives() {
        assert_eq!(parse_chunk_pos("chunk_1_-2_3"), Some(IVec3::new(1, -2, 3)));
        assert_eq!(parse_chunk_pos("chunk_0_0_0"), Some(IVec3::ZERO));
        assert_eq!(parse_chunk_pos("chunk_1_2"), None);
        assert_eq!(parse_chunk_pos("chunk_1_2_3_4"), None);
        assert_eq!(parse_chunk_pos("other_1_2_3"), None);
    }

    #[test]
    fn read_back_empty_homebase_is_genesis() {
        let dir = tempfile::tempdir().unwrap();
        let owner = NostrPublicKey([7; 32]);
        let state = read_authoritative_homebase_state(dir.path(), owner).expect("read empty state");

        assert_eq!(state.server_revision, 0);
        assert_eq!(state.previous_manifest_hash, None);
        assert_eq!(state.payload_scope, HomebasePayloadScope::default());
        assert_eq!(state.descriptor_root, compute_descriptor_root(&[]).unwrap());
    }

    #[test]
    fn read_back_with_meta_grants_matching_request_and_rejects_tampered_root() {
        use crate::persistence::fs_map_meta::FsMapMetaStore;
        use crate::persistence::{map_save_dir, MapMeta};

        let dir = tempfile::tempdir().unwrap();
        let owner = NostrPublicKey([7; 32]);
        let map_dir = map_save_dir(dir.path(), &MapInstanceId::Homebase { owner });
        FsMapMetaStore {
            map_dir: Arc::new(map_dir),
        }
        .save(
            &(),
            &MapMeta {
                version: 1,
                seed: 99,
                generation_version: 2,
                spawn_points: vec![Vec3::new(1.0, 2.0, 3.0)],
            },
        )
        .expect("save meta");

        let state = read_authoritative_homebase_state(dir.path(), owner).expect("read state");
        assert!(state.payload_scope.includes_meta);
        assert!(state.payload_scope.terrain_chunks.is_empty());
        assert_eq!(state.server_revision, 0);

        let keys = server_keys();
        verify_homebase_publication_attestation_request(
            &ServerAttestationSigner(&keys),
            owner,
            &MapInstanceId::Homebase { owner },
            state.descriptor_root,
            &state.payload_scope,
            &state,
            1_000,
            HOMEBASE_ATTESTATION_TTL_SECONDS,
        )
        .expect("matching request granted");

        let tampered = verify_homebase_publication_attestation_request(
            &ServerAttestationSigner(&keys),
            owner,
            &MapInstanceId::Homebase { owner },
            [0; 32],
            &state.payload_scope,
            &state,
            1_000,
            HOMEBASE_ATTESTATION_TTL_SECONDS,
        );
        assert!(tampered.is_err());
    }
}
