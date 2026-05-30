use nostr_client::BlobRef;
use persistence::AsyncStore;
use protocol::map::SavedEntity;
use serde::Serialize;
use sha2::{Digest, Sha256};
use voxel_map_engine::config::WorldObjectSpawn;
use voxel_map_engine::persistence::ChunkFileEnvelope;

use crate::manifest::{
    ManifestPayloadDescriptor, ManifestPayloadSlot, MapPersistenceRejection, PayloadClass,
    PayloadKey, PayloadSlotState,
};

/// Schema version for `PayloadClass::MapMeta` payloads.
pub const MAP_META_SCHEMA_VERSION: u32 = 1;
/// Schema version for `PayloadClass::ChunkEntities` payloads.
pub const CHUNK_ENTITIES_SCHEMA_VERSION: u32 = 3;
/// Schema version for `PayloadClass::MapEntities` payloads.
pub const MAP_ENTITIES_SCHEMA_VERSION: u32 = 1;

/// Wire form of map metadata, encoded identically to the server `MapMeta` struct.
///
/// Uses `[f32; 3]` spawn points (not a Bevy `Vec3`) so this crate stays free of a
/// direct Bevy dependency; bincode encodes both identically.
#[derive(Serialize)]
struct MapMetaWire {
    version: u32,
    seed: u64,
    generation_version: u32,
    spawn_points: Vec<[f32; 3]>,
}

/// Encodes map metadata in the same byte format as the filesystem map metadata store.
///
/// Shared so the server save path and the client homebase publish path produce
/// identical bytes. Spawn points are passed as `[f32; 3]` triples.
pub fn encode_map_meta_payload(
    version: u32,
    seed: u64,
    generation_version: u32,
    spawn_points: Vec<[f32; 3]>,
) -> Result<Vec<u8>, MapPersistenceRejection> {
    bincode::serialize(&MapMetaWire {
        version,
        seed,
        generation_version,
        spawn_points,
    })
    .map_err(|error| MapPersistenceRejection::Invalid(format!("encode map meta: {error}")))
}

/// Encodes terrain chunk data in the same format as the filesystem terrain store.
pub fn encode_chunk_payload(value: ChunkFileEnvelope) -> Result<Vec<u8>, MapPersistenceRejection> {
    zstd_bincode_encode(&value, "chunk payload")
}

/// Encodes chunk entity data in the same format as the filesystem chunk entity store.
pub fn encode_chunk_entities_payload(
    value: Vec<WorldObjectSpawn>,
) -> Result<Vec<u8>, MapPersistenceRejection> {
    #[derive(Serialize)]
    struct Envelope {
        version: u32,
        spawns: Vec<WorldObjectSpawn>,
    }
    zstd_bincode_encode(
        &Envelope {
            version: CHUNK_ENTITIES_SCHEMA_VERSION,
            spawns: value,
        },
        "chunk entities payload",
    )
}

/// Encodes map-level entities in the same format as the filesystem entity store.
pub fn encode_map_entities_payload(
    value: Vec<SavedEntity>,
) -> Result<Vec<u8>, MapPersistenceRejection> {
    #[derive(Serialize)]
    struct Envelope {
        version: u32,
        entities: Vec<SavedEntity>,
    }
    bincode::serialize(&Envelope {
        version: MAP_ENTITIES_SCHEMA_VERSION,
        entities: value,
    })
    .map_err(|error| {
        MapPersistenceRejection::Invalid(format!("encode map entities payload: {error}"))
    })
}

fn zstd_bincode_encode<T: Serialize>(
    value: &T,
    label: &str,
) -> Result<Vec<u8>, MapPersistenceRejection> {
    let encoded = bincode::serialize(value)
        .map_err(|error| MapPersistenceRejection::Invalid(format!("encode {label}: {error}")))?;
    zstd::encode_all(encoded.as_slice(), 0)
        .map_err(|error| MapPersistenceRejection::Invalid(format!("compress {label}: {error}")))
}

/// Uploads one payload slot through the blob store and appends the signed manifest descriptor.
///
/// Shared by the server overworld publish path and the client homebase publish path.
#[allow(clippy::too_many_arguments)]
pub async fn upload_publish_slot<T>(
    payloads: &mut Vec<ManifestPayloadDescriptor>,
    blob_store: &impl AsyncStore<BlobRef, Vec<u8>>,
    public_blossom_base_url: &url::Url,
    class: PayloadClass,
    key: PayloadKey,
    schema_version: u32,
    slot: PayloadSlotState<T>,
    encode: impl FnOnce(T) -> Result<Vec<u8>, MapPersistenceRejection>,
) -> Result<(), MapPersistenceRejection> {
    let manifest_slot = match slot {
        PayloadSlotState::Present(value) => {
            let bytes = encode(value)?;
            let sha256: [u8; 32] = Sha256::digest(&bytes).into();
            let mut get_url = public_blossom_base_url.clone();
            get_url.set_path(&hex::encode(sha256));
            let blob = BlobRef {
                sha256,
                size: bytes.len() as u64,
                content_type: "application/octet-stream".to_string(),
                urls: vec![get_url.to_string()],
            };
            blob_store
                .save(&blob, &bytes)
                .await
                .map_err(|error| MapPersistenceRejection::Unavailable(error.to_string()))?;
            ManifestPayloadSlot::Present { blob }
        }
        PayloadSlotState::Empty => ManifestPayloadSlot::Empty,
        PayloadSlotState::Absent => ManifestPayloadSlot::Absent,
        PayloadSlotState::Tombstoned => ManifestPayloadSlot::Tombstoned,
    };
    payloads.push(ManifestPayloadDescriptor {
        class,
        key,
        slot: manifest_slot,
        schema_version,
    });
    Ok(())
}
