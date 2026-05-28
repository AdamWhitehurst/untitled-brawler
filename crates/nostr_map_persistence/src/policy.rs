use std::collections::BTreeSet;
use std::time::Duration;

use crate::PayloadClass;

/// Tie-break strategy for same-revision manifests visible from relays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestTieBreak {
    LowestHash,
    HighestHash,
}

/// Map payload and Blossom download policy.
#[derive(Clone, Debug)]
pub struct MapPersistencePolicy {
    pub max_blob_bytes: u64,
    pub max_manifest_bytes: usize,
    pub max_payloads: usize,
    pub allowed_payload_classes: BTreeSet<PayloadClass>,
    pub allowed_blossom_hosts: BTreeSet<String>,
}

impl Default for MapPersistencePolicy {
    fn default() -> Self {
        Self {
            max_blob_bytes: 16 * 1024 * 1024,
            max_manifest_bytes: 64 * 1024,
            max_payloads: 4096,
            allowed_payload_classes: BTreeSet::from([
                PayloadClass::MapMeta,
                PayloadClass::TerrainChunk,
                PayloadClass::ChunkEntities,
                PayloadClass::MapEntities,
            ]),
            allowed_blossom_hosts: BTreeSet::new(),
        }
    }
}

/// Relay query policy for map manifest reads.
#[derive(Clone, Debug)]
pub struct NostrMapQueryPolicy {
    pub relays: Vec<String>,
    pub timeout: Duration,
    pub limit: usize,
    pub tie_break: ManifestTieBreak,
}

impl Default for NostrMapQueryPolicy {
    fn default() -> Self {
        Self {
            relays: Vec::new(),
            timeout: Duration::from_secs(5),
            limit: 32,
            tie_break: ManifestTieBreak::HighestHash,
        }
    }
}
