use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bevy::prelude::*;
use bevy::tasks::{IoTaskPool, Task};
use lightyear::prelude::{MessageReceiver, MessageSender};
use nostr_client::{verify_payload_schnorr, BlobRef, BlossomAuth, NostrKeys};
use nostr_map_persistence::attestation::{
    sign_homebase_attestation, AttestationSigner, AttestationVerifier,
};
use nostr_map_persistence::manifest::{ManifestPayloadSlot, PayloadClass, PayloadKey};
use nostr_map_persistence::{
    compute_descriptor_root, compute_manifest_hash, encode_chunk_entities_payload,
    encode_chunk_payload, encode_map_entities_payload, encode_map_meta_payload, manifest_to_json,
    validate_homebase_manifest_attestation, BlossomBlobPutStore, HomebasePayloadScope,
    HomebasePublicationAttestation, ManifestPayloadDescriptor, MapPersistenceRejection,
    MapRevision, NostrMapManifest, CHUNK_ENTITIES_SCHEMA_VERSION, MAP_ENTITIES_SCHEMA_VERSION,
    MAP_MANIFEST_SCHEMA_VERSION, MAP_META_SCHEMA_VERSION,
};
use persistence::{AsyncStore, Store};
use protocol::map::{HomebaseAttestationRequest, HomebaseAttestationResponse, MapChannel};
use protocol::{MapInstanceId, NostrPublicKey, PlayerIdentity};

use super::remote_publish::RemoteMapPublishConfig;
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

/// Reads the materialized homebase save from disk, re-encoding each present slot with the
/// shared payload encoders. Returns the authoritative state (descriptor root, payload scope,
/// revision) plus the encoded bytes for every present slot so the caller can upload them.
///
/// This is the server's source of truth: under the "server encodes, client signs" model the
/// server, not the client, produces canonical payload bytes, so the descriptor root always
/// matches the manifest the client signs.
fn read_authoritative_homebase_publish(
    save_root: &Path,
    owner: NostrPublicKey,
) -> Result<
    (
        AuthoritativeHomebaseState,
        Vec<(ManifestPayloadDescriptor, Vec<u8>)>,
    ),
    MapPersistenceRejection,
> {
    let map_id = MapInstanceId::Homebase { owner };
    let canonical_map_dir = map_save_dir(save_root, &map_id);
    let map_dir = store_map_dir_for_loading(&canonical_map_dir)
        .map_err(|e| MapPersistenceRejection::Filesystem(format!("resolve homebase dir: {e}")))?;
    let map_dir_arc = Arc::new(map_dir.clone());

    let mut present_payloads: Vec<(ManifestPayloadDescriptor, Vec<u8>)> = Vec::new();
    let mut scope = HomebasePayloadScope::default();
    let mut push_present = |class, key, schema_version, bytes: Vec<u8>| {
        present_payloads.push((
            present_descriptor(class, key, schema_version, &bytes),
            bytes,
        ));
    };

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
        push_present(
            PayloadClass::MapMeta,
            PayloadKey::Singleton,
            MAP_META_SCHEMA_VERSION,
            bytes,
        );
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
        push_present(
            PayloadClass::TerrainChunk,
            PayloadKey::Chunk {
                x: chunk_pos.x,
                y: chunk_pos.y,
                z: chunk_pos.z,
            },
            CHUNK_SAVE_VERSION,
            bytes,
        );
        scope.edited_chunks.push(chunk_pos);
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
        push_present(
            PayloadClass::ChunkEntities,
            PayloadKey::Chunk {
                x: chunk_pos.x,
                y: chunk_pos.y,
                z: chunk_pos.z,
            },
            CHUNK_ENTITIES_SCHEMA_VERSION,
            bytes,
        );
        scope.chunk_entities.push(chunk_pos);
    }

    let map_entities = FsMapEntitiesStore {
        map_dir: map_dir_arc.clone(),
    }
    .load(&())
    .map_err(|e| MapPersistenceRejection::Filesystem(format!("load map entities: {e}")))?;
    if let Some(entities) = map_entities {
        let bytes = encode_map_entities_payload(entities)?;
        push_present(
            PayloadClass::MapEntities,
            PayloadKey::Singleton,
            MAP_ENTITIES_SCHEMA_VERSION,
            bytes,
        );
        scope.includes_map_entities = true;
    }

    let descriptors: Vec<ManifestPayloadDescriptor> =
        present_payloads.iter().map(|(d, _)| d.clone()).collect();
    let descriptor_root = compute_descriptor_root(&descriptors)
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

    let state = AuthoritativeHomebaseState {
        owner,
        map_id,
        server_revision,
        previous_manifest_hash,
        descriptor_root,
        payload_scope: scope,
    };
    Ok((state, present_payloads))
}

/// Reads authoritative homebase state (descriptor root, payload scope, revision) from disk.
pub fn read_authoritative_homebase_state(
    save_root: &Path,
    owner: NostrPublicKey,
) -> Result<AuthoritativeHomebaseState, MapPersistenceRejection> {
    read_authoritative_homebase_publish(save_root, owner).map(|(state, _)| state)
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

/// In-flight homebase publication preparation tasks, keyed by the requesting client entity.
#[derive(Resource, Default)]
pub struct PendingHomebaseAttestations {
    tasks: Vec<(Entity, Task<HomebaseAttestationResponse>)>,
}

/// Handles client homebase publication requests under the "server encodes, client signs" model.
///
/// The server reads back its authoritative homebase save, signs an attestation, and spawns an
/// async task that uploads the payload blobs to Blossom and assembles the unsigned manifest the
/// client will sign with the player's Nostr key.
pub fn handle_homebase_attestation_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<HomebaseAttestationRequest>)>,
    player_identities: Query<&PlayerIdentity>,
    mut responders: Query<&mut MessageSender<HomebaseAttestationResponse>>,
    server_identity: Res<NostrKeys>,
    save_path: Res<WorldSavePath>,
    publish_config: Res<RemoteMapPublishConfig>,
    mut pending: ResMut<PendingHomebaseAttestations>,
) {
    for (client_entity, mut receiver) in &mut receivers {
        for HomebaseAttestationRequest in receiver.receive() {
            match begin_homebase_publication(
                client_entity,
                &player_identities,
                &server_identity,
                &save_path.0,
                &publish_config,
            ) {
                Ok(task) => pending.tasks.push((client_entity, task)),
                Err(rejection) => {
                    warn!(
                        ?client_entity,
                        ?rejection,
                        "rejected homebase publication request"
                    );
                    reply_attestation(
                        &mut responders,
                        client_entity,
                        HomebaseAttestationResponse::Rejected(format!("{rejection:?}")),
                    );
                }
            }
        }
    }
}

/// Drains completed homebase publication tasks and replies to each requesting client.
pub fn poll_homebase_attestation_uploads(
    mut pending: ResMut<PendingHomebaseAttestations>,
    mut responders: Query<&mut MessageSender<HomebaseAttestationResponse>>,
) {
    let mut index = 0;
    while index < pending.tasks.len() {
        let Some(response) = bevy::tasks::futures::check_ready(&mut pending.tasks[index].1) else {
            index += 1;
            continue;
        };
        let (client_entity, _) = pending.tasks.swap_remove(index);
        reply_attestation(&mut responders, client_entity, response);
    }
}

fn reply_attestation(
    responders: &mut Query<&mut MessageSender<HomebaseAttestationResponse>>,
    client_entity: Entity,
    response: HomebaseAttestationResponse,
) {
    match responders.get_mut(client_entity) {
        Ok(mut sender) => sender.send::<MapChannel>(response),
        Err(_) => warn!(
            ?client_entity,
            "homebase publication requester has no response sender"
        ),
    }
}

/// Validates the request and starts the async upload, returning the in-flight task.
fn begin_homebase_publication(
    client_entity: Entity,
    player_identities: &Query<&PlayerIdentity>,
    server_identity: &NostrKeys,
    save_root: &Path,
    publish_config: &RemoteMapPublishConfig,
) -> Result<Task<HomebaseAttestationResponse>, MapPersistenceRejection> {
    let identity = player_identities
        .get(client_entity)
        .map_err(|_| MapPersistenceRejection::Invalid("client is not authenticated".to_string()))?;
    let owner = identity.0;
    let map_id = MapInstanceId::Homebase { owner };

    if !publish_config.enabled {
        return Err(MapPersistenceRejection::Unavailable(
            "server remote map publishing is disabled".to_string(),
        ));
    }
    let upload_url = publish_config.blossom_upload_url.clone().ok_or_else(|| {
        MapPersistenceRejection::Unavailable("server Blossom upload URL not configured".to_string())
    })?;
    let base_url = publish_config
        .blossom_public_base_url
        .clone()
        .ok_or_else(|| {
            MapPersistenceRejection::Unavailable(
                "server Blossom public base URL not configured".to_string(),
            )
        })?;

    let (state, present_payloads) = read_authoritative_homebase_publish(save_root, owner)?;
    let now_unix = now_unix_seconds()?;
    let attestation = verify_homebase_publication_attestation_request(
        &ServerAttestationSigner(server_identity),
        owner,
        &map_id,
        state.descriptor_root,
        &state.payload_scope,
        &state,
        now_unix,
        HOMEBASE_ATTESTATION_TTL_SECONDS,
    )?;

    let blob_store = BlossomBlobPutStore {
        upload_url,
        auth: BlossomAuth::from_keys(server_identity),
    };
    let revision = state.server_revision;
    let previous_hash = state.previous_manifest_hash;
    let descriptor_root = state.descriptor_root;

    Ok(IoTaskPool::get().spawn(async move {
        match upload_and_build_unsigned_manifest(
            blob_store,
            base_url,
            present_payloads,
            owner,
            map_id,
            revision,
            previous_hash,
            descriptor_root,
            attestation,
        )
        .await
        {
            Ok((json, manifest_hash)) => HomebaseAttestationResponse::Granted {
                unsigned_manifest_json: json,
                manifest_hash,
            },
            Err(rejection) => HomebaseAttestationResponse::Rejected(format!("{rejection:?}")),
        }
    }))
}

/// Uploads each present payload blob to Blossom, then assembles the unsigned manifest JSON.
#[allow(clippy::too_many_arguments)]
async fn upload_and_build_unsigned_manifest(
    blob_store: BlossomBlobPutStore,
    base_url: url::Url,
    present_payloads: Vec<(ManifestPayloadDescriptor, Vec<u8>)>,
    owner: NostrPublicKey,
    map_id: MapInstanceId,
    revision: u64,
    previous_hash: Option<[u8; 32]>,
    descriptor_root: [u8; 32],
    attestation: HomebasePublicationAttestation,
) -> Result<(String, [u8; 32]), MapPersistenceRejection> {
    let mut payloads = Vec::with_capacity(present_payloads.len());
    for (mut descriptor, bytes) in present_payloads {
        if let ManifestPayloadSlot::Present { blob } = &mut descriptor.slot {
            let mut get_url = base_url.clone();
            get_url.set_path(&hex::encode(blob.sha256));
            blob.urls = vec![get_url.to_string()];
            blob_store.save(blob, &bytes).await.map_err(|e| {
                MapPersistenceRejection::Unavailable(format!("upload homebase blob: {e}"))
            })?;
        }
        payloads.push(descriptor);
    }

    let manifest = NostrMapManifest {
        map_id,
        owner,
        revision,
        previous_hash,
        payloads,
        schema_version: MAP_MANIFEST_SCHEMA_VERSION,
        descriptor_root,
        homebase_attestation: Some(attestation),
    };
    let manifest_hash = compute_manifest_hash(&manifest)
        .map_err(|e| MapPersistenceRejection::Invalid(format!("hash unsigned manifest: {e}")))?;
    let json = manifest_to_json(&manifest).map_err(|e| {
        MapPersistenceRejection::Invalid(format!("serialize unsigned manifest: {e}"))
    })?;
    Ok((json, manifest_hash))
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
        assert!(state.payload_scope.edited_chunks.is_empty());
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
