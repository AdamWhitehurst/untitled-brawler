use protocol::MapInstanceId;
use serde::{Deserialize, Serialize};

/// Hash identifying a signed map manifest revision.
pub type ManifestHash = [u8; 32];

/// Identifies a map payload's semantic class.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum PayloadClass {
    MapMeta,
    TerrainChunk,
    ChunkEntities,
    MapEntities,
}

/// Identifies a payload within a map manifest.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum PayloadKey {
    Singleton,
    Chunk { x: i32, y: i32, z: i32 },
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

/// Complete raw map save assembled from a validated manifest chain.
#[derive(Clone, Debug)]
pub struct RawValidatedMapSave {
    pub meta: RawMapMetaPayload,
    pub chunks: Vec<(PayloadKey, RawChunkPayload)>,
    pub chunk_entities: Vec<(PayloadKey, RawChunkEntitiesPayload)>,
    pub map_entities: Option<RawMapEntitiesPayload>,
    pub revision: MapRevision,
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

/// Minimal manifest shape reserved for later remote restore phases.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NostrMapManifest {
    pub map_id: MapInstanceId,
    pub revision: u64,
    pub previous_hash: Option<ManifestHash>,
    pub schema_version: u32,
    pub descriptor_root: [u8; 32],
}
