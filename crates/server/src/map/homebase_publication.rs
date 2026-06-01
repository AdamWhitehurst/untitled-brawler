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
    compute_descriptor_root, compute_manifest_hash, encode_chunk_payload,
    encode_map_entities_payload, encode_map_meta_payload, finalize_manifest, manifest_to_json,
    validate_homebase_manifest_attestation, BlossomBlobPutStore, HomebasePayloadScope,
    HomebasePublicationAttestation, ManifestPayloadDescriptor, MapPersistenceRejection,
    MapRevision, NostrMapManifest, MAP_ENTITIES_SCHEMA_VERSION, MAP_META_SCHEMA_VERSION,
};
use persistence::{AsyncStore, Store, StoreBackend};
use protocol::map::{
    HomebaseAttestationRequest, HomebaseAttestationResponse, MapChannel, SavedEntity,
};
use protocol::{MapInstanceId, NostrPublicKey, PlayerIdentity};

use super::remote_publish::RemoteMapPublishConfig;
use sha2::{Digest, Sha256};
use voxel_map_engine::config::VoxelGeneratorImpl;
use voxel_map_engine::persistence::fs_chunk::FsChunkStore;
use voxel_map_engine::persistence::{ChunkFileEnvelope, CHUNK_SAVE_VERSION};
use voxel_map_engine::prelude::{Homebase, VoxelGenerator, VoxelMapInstance};

use crate::persistence::fs_map_entities::FsMapEntitiesStore;
use crate::persistence::fs_map_meta::FsMapMetaStore;
use crate::persistence::{
    map_save_dir, FsAcceptedMapHeadStore, FsMapChangeSetStore, MapChangeSet, MapMeta, WorldSavePath,
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

/// Builds a `Tombstoned` manifest descriptor (delete slot; carries no blob).
fn tombstone_descriptor(
    class: PayloadClass,
    key: PayloadKey,
    schema_version: u32,
) -> ManifestPayloadDescriptor {
    ManifestPayloadDescriptor {
        class,
        key,
        slot: ManifestPayloadSlot::Tombstoned,
        schema_version,
    }
}

/// Classified publish slots for one homebase delta: blobs to upload (`Present`) plus
/// `Tombstoned` descriptors (deletes), and the payload scope describing them.
struct HomebasePublishSlots {
    present_payloads: Vec<(ManifestPayloadDescriptor, Vec<u8>)>,
    tombstoned: Vec<ManifestPayloadDescriptor>,
    scope: HomebasePayloadScope,
}

/// Classifies the durable change-set candidates into publish slots using the live map state.
///
/// For each candidate chunk: byte-identical to freshly-generated terrain -> `Tombstoned` (and the
/// on-disk file is deleted so local load regenerates it); otherwise `Present` with the current
/// in-memory chunk bytes. Meta and map-level entities are `Present` on the genesis revision or
/// when their change flag is set, else omitted (restore preserves omitted slots).
///
/// Per-chunk entity slots are not driven by the change-set (it tracks only terrain candidates +
/// meta/map-entity flags), so they are not published here.
fn resolve_homebase_publish_slots(
    instance: &VoxelMapInstance,
    generator: &dyn VoxelGeneratorImpl,
    chunk_store: &FsChunkStore,
    meta_store: &FsMapMetaStore,
    map_entities_store: &FsMapEntitiesStore,
    change_set: &MapChangeSet,
    is_genesis: bool,
) -> Result<HomebasePublishSlots, MapPersistenceRejection> {
    let mut present_payloads: Vec<(ManifestPayloadDescriptor, Vec<u8>)> = Vec::new();
    let mut tombstoned: Vec<ManifestPayloadDescriptor> = Vec::new();
    let mut scope = HomebasePayloadScope::default();

    // Genesis must carry meta so restore can fetch the seed and regenerate folded-out chunks.
    if is_genesis || change_set.meta_changed {
        let meta = meta_store
            .load(&())
            .map_err(|e| MapPersistenceRejection::Filesystem(format!("load homebase meta: {e}")))?
            .ok_or_else(|| {
                MapPersistenceRejection::Incomplete("homebase meta missing for publish".into())
            })?;
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
        present_payloads.push((
            present_descriptor(
                PayloadClass::MapMeta,
                PayloadKey::Singleton,
                MAP_META_SCHEMA_VERSION,
                &bytes,
            ),
            bytes,
        ));
        scope.includes_meta = true;
    }

    let mut candidates: Vec<IVec3> = change_set.chunk_candidates.iter().copied().collect();
    candidates.sort_by_key(|pos| (pos.x, pos.y, pos.z));
    for pos in candidates {
        let key = PayloadKey::Chunk {
            x: pos.x,
            y: pos.y,
            z: pos.z,
        };
        if instance.chunk_matches_generated(pos, generator) {
            tombstoned.push(tombstone_descriptor(
                PayloadClass::TerrainChunk,
                key,
                CHUNK_SAVE_VERSION,
            ));
            scope.tombstoned_chunks.push(pos);
            chunk_store.delete(&pos).map_err(|e| {
                MapPersistenceRejection::Filesystem(format!("delete reverted chunk {pos}: {e}"))
            })?;
            continue;
        }
        let Some(data) = instance.get_chunk_data(pos) else {
            trace!(
                ?pos,
                "publish candidate differs from generated but is not loaded; skipping"
            );
            continue;
        };
        let envelope = ChunkFileEnvelope {
            version: CHUNK_SAVE_VERSION,
            chunk_size: instance.chunk_size,
            data: data.clone(),
        };
        let bytes = encode_chunk_payload(envelope)?;
        present_payloads.push((
            present_descriptor(PayloadClass::TerrainChunk, key, CHUNK_SAVE_VERSION, &bytes),
            bytes,
        ));
        scope.edited_chunks.push(pos);
    }

    if is_genesis || change_set.map_entities_changed {
        if let Some(entities) = map_entities_store
            .load(&())
            .map_err(|e| MapPersistenceRejection::Filesystem(format!("load map entities: {e}")))?
        {
            let bytes = encode_map_entities_payload(entities)?;
            present_payloads.push((
                present_descriptor(
                    PayloadClass::MapEntities,
                    PayloadKey::Singleton,
                    MAP_ENTITIES_SCHEMA_VERSION,
                    &bytes,
                ),
                bytes,
            ));
            scope.includes_map_entities = true;
        }
    }

    Ok(HomebasePublishSlots {
        present_payloads,
        tombstoned,
        scope,
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

/// In-flight homebase publication preparation tasks, keyed by the requesting client entity.
#[derive(Resource, Default)]
pub struct PendingHomebaseAttestations {
    tasks: Vec<(Entity, Task<HomebaseAttestationResponse>)>,
}

/// Per-map components a homebase publish reads: the live voxel state + generator (for the
/// equals-generated classification) and the persistence backends the delta is sourced from.
type HomebasePublishQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static MapInstanceId,
        &'static VoxelMapInstance,
        &'static VoxelGenerator,
        &'static StoreBackend<IVec3, ChunkFileEnvelope, FsChunkStore>,
        &'static StoreBackend<(), MapMeta, FsMapMetaStore>,
        &'static StoreBackend<(), Vec<SavedEntity>, FsMapEntitiesStore>,
        &'static StoreBackend<(), MapChangeSet, FsMapChangeSetStore>,
    ),
    With<Homebase>,
>;

/// Handles client homebase publication requests under the "server encodes, client signs" model.
///
/// The server classifies the durable change-set against the live map (Present edits vs
/// Tombstoned reverts), signs an attestation, and spawns an async task that uploads the Present
/// blobs to Blossom and assembles the unsigned chained-delta manifest the client will sign with
/// the player's Nostr key.
pub fn handle_homebase_attestation_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<HomebaseAttestationRequest>)>,
    player_identities: Query<&PlayerIdentity>,
    homebase_maps: HomebasePublishQuery,
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
                &homebase_maps,
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

/// Validates the request, classifies the change-set against the live homebase map, signs the
/// attestation, and starts the async upload, returning the in-flight task.
fn begin_homebase_publication(
    client_entity: Entity,
    player_identities: &Query<&PlayerIdentity>,
    homebase_maps: &HomebasePublishQuery,
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

    let (_, instance, generator, chunk_backend, meta_backend, map_entities_backend, change_backend) =
        homebase_maps
            .iter()
            .find(|(mid, ..)| **mid == map_id)
            .ok_or_else(|| {
                MapPersistenceRejection::Unavailable(
                    "homebase map is not loaded for publish".to_string(),
                )
            })?;

    // Accepted head lives at the map's top-level dir; read it to chain this delta.
    let canonical_map_dir = map_save_dir(save_root, &map_id);
    let accepted_head = FsAcceptedMapHeadStore {
        map_dir: Arc::new(canonical_map_dir),
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
    let is_genesis = previous_manifest_hash.is_none();

    let change_set = Store::load(&change_backend.0, &())
        .map_err(|e| MapPersistenceRejection::Filesystem(format!("load change set: {e}")))?
        .unwrap_or_default();

    let HomebasePublishSlots {
        present_payloads,
        tombstoned,
        scope,
    } = resolve_homebase_publish_slots(
        instance,
        generator.0.as_ref(),
        &chunk_backend.0,
        &meta_backend.0,
        &map_entities_backend.0,
        &change_set,
        is_genesis,
    )?;

    let mut all_descriptors: Vec<ManifestPayloadDescriptor> =
        present_payloads.iter().map(|(d, _)| d.clone()).collect();
    all_descriptors.extend(tombstoned.iter().cloned());
    let descriptor_root = compute_descriptor_root(&all_descriptors)
        .map_err(|e| MapPersistenceRejection::Invalid(format!("descriptor root: {e}")))?;

    let state = AuthoritativeHomebaseState {
        owner,
        map_id: map_id.clone(),
        server_revision,
        previous_manifest_hash,
        descriptor_root,
        payload_scope: scope.clone(),
    };
    let now_unix = now_unix_seconds()?;
    let attestation = verify_homebase_publication_attestation_request(
        &ServerAttestationSigner(server_identity),
        owner,
        &map_id,
        descriptor_root,
        &scope,
        &state,
        now_unix,
        HOMEBASE_ATTESTATION_TTL_SECONDS,
    )?;

    let blob_store = BlossomBlobPutStore {
        upload_url,
        auth: BlossomAuth::from_keys(server_identity),
    };

    Ok(IoTaskPool::get().spawn(async move {
        match upload_and_build_unsigned_manifest(
            blob_store,
            base_url,
            present_payloads,
            tombstoned,
            owner,
            map_id,
            server_revision,
            previous_manifest_hash,
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

/// Uploads each present payload blob to Blossom, appends the tombstone descriptors, then
/// assembles the unsigned manifest JSON through the shared finalizer.
#[allow(clippy::too_many_arguments)]
async fn upload_and_build_unsigned_manifest(
    blob_store: BlossomBlobPutStore,
    base_url: url::Url,
    present_payloads: Vec<(ManifestPayloadDescriptor, Vec<u8>)>,
    tombstoned: Vec<ManifestPayloadDescriptor>,
    owner: NostrPublicKey,
    map_id: MapInstanceId,
    revision: u64,
    previous_hash: Option<[u8; 32]>,
    descriptor_root: [u8; 32],
    attestation: HomebasePublicationAttestation,
) -> Result<(String, [u8; 32]), MapPersistenceRejection> {
    let mut payloads = Vec::with_capacity(present_payloads.len() + tombstoned.len());
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
    payloads.extend(tombstoned);

    let manifest = finalize_manifest(
        payloads,
        map_id,
        owner,
        revision,
        previous_hash,
        Some(attestation),
    )?;
    debug_assert_eq!(
        manifest.descriptor_root, descriptor_root,
        "uploaded manifest root must equal the attested descriptor root"
    );
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

    use voxel_map_engine::prelude::{ChunkData, ChunkStatus, WorldVoxel};

    const PADDED_VOLUME_16: usize = 18 * 18 * 18;

    /// Test generator producing all-air terrain for every chunk.
    struct AirGenerator;
    impl VoxelGeneratorImpl for AirGenerator {
        fn generate_terrain(&self, _chunk_pos: IVec3) -> Vec<WorldVoxel> {
            vec![WorldVoxel::Air; PADDED_VOLUME_16]
        }
    }

    fn test_chunk_store(dir: &std::path::Path) -> FsChunkStore {
        FsChunkStore {
            map_dir: Arc::new(dir.to_path_buf()),
        }
    }

    fn test_meta_store(dir: &std::path::Path) -> FsMapMetaStore {
        FsMapMetaStore {
            map_dir: Arc::new(dir.to_path_buf()),
        }
    }

    fn test_map_entities_store(dir: &std::path::Path) -> FsMapEntitiesStore {
        FsMapEntitiesStore {
            map_dir: Arc::new(dir.to_path_buf()),
        }
    }

    fn instance_with_air_chunk(pos: IVec3) -> VoxelMapInstance {
        let mut instance = VoxelMapInstance::new(5, 16);
        let voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
        instance.insert_chunk_data(pos, ChunkData::from_voxels(&voxels, ChunkStatus::Full));
        instance
    }

    fn change_set_for(pos: IVec3) -> MapChangeSet {
        let mut change_set = MapChangeSet::default();
        change_set.chunk_candidates.insert(pos);
        change_set
    }

    #[test]
    fn resolve_tombstones_reverted_chunk_and_deletes_file() {
        let dir = tempfile::tempdir().unwrap();
        let pos = IVec3::new(1, 0, -2);
        let instance = instance_with_air_chunk(pos); // equals AirGenerator output
        let chunk_store = test_chunk_store(dir.path());
        chunk_store
            .save(
                &pos,
                &ChunkFileEnvelope {
                    version: CHUNK_SAVE_VERSION,
                    chunk_size: 16,
                    data: ChunkData::from_voxels(
                        &vec![WorldVoxel::Air; PADDED_VOLUME_16],
                        ChunkStatus::Full,
                    ),
                },
            )
            .unwrap();

        let slots = resolve_homebase_publish_slots(
            &instance,
            &AirGenerator,
            &chunk_store,
            &test_meta_store(dir.path()),
            &test_map_entities_store(dir.path()),
            &change_set_for(pos),
            false,
        )
        .expect("resolve");

        assert!(slots.present_payloads.is_empty());
        assert_eq!(slots.tombstoned.len(), 1);
        assert_eq!(slots.tombstoned[0].slot, ManifestPayloadSlot::Tombstoned);
        assert_eq!(slots.scope.tombstoned_chunks, vec![pos]);
        assert!(slots.scope.edited_chunks.is_empty());
        // The reverted chunk's on-disk file is removed so local load regenerates it.
        assert!(chunk_store.load(&pos).unwrap().is_none());
    }

    #[test]
    fn resolve_presents_edited_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let pos = IVec3::ZERO;
        let mut instance = instance_with_air_chunk(pos);
        instance.set_voxel(IVec3::new(5, 5, 5), WorldVoxel::Solid(7)); // now differs from generated

        let slots = resolve_homebase_publish_slots(
            &instance,
            &AirGenerator,
            &test_chunk_store(dir.path()),
            &test_meta_store(dir.path()),
            &test_map_entities_store(dir.path()),
            &change_set_for(pos),
            false,
        )
        .expect("resolve");

        assert!(slots.tombstoned.is_empty());
        assert_eq!(slots.present_payloads.len(), 1);
        assert_eq!(
            slots.present_payloads[0].0.class,
            PayloadClass::TerrainChunk
        );
        assert!(matches!(
            slots.present_payloads[0].0.slot,
            ManifestPayloadSlot::Present { .. }
        ));
        assert_eq!(slots.scope.edited_chunks, vec![pos]);
    }

    #[test]
    fn descriptor_root_covers_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let pos = IVec3::new(2, 0, 0);
        let instance = instance_with_air_chunk(pos);
        let chunk_store = test_chunk_store(dir.path());

        let slots = resolve_homebase_publish_slots(
            &instance,
            &AirGenerator,
            &chunk_store,
            &test_meta_store(dir.path()),
            &test_map_entities_store(dir.path()),
            &change_set_for(pos),
            false,
        )
        .expect("resolve");

        let mut with_tombstone: Vec<ManifestPayloadDescriptor> = slots
            .present_payloads
            .iter()
            .map(|(d, _)| d.clone())
            .collect();
        with_tombstone.extend(slots.tombstoned.iter().cloned());
        let root_with = compute_descriptor_root(&with_tombstone).unwrap();

        let present_only: Vec<ManifestPayloadDescriptor> = slots
            .present_payloads
            .iter()
            .map(|(d, _)| d.clone())
            .collect();
        let root_without = compute_descriptor_root(&present_only).unwrap();

        assert_ne!(
            root_with, root_without,
            "the tombstone slot must contribute to the descriptor root"
        );
    }
}
