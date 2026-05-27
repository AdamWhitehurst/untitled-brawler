use nostr_client::BlobRef;
use protocol::{MapInstanceId, NostrPublicKey};

use crate::ManifestHash;

/// Query for the latest visible manifest descendant of an accepted local head.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ManifestHeadQuery {
    pub owner: NostrPublicKey,
    pub map_id: MapInstanceId,
    pub accepted_head: Option<ManifestHash>,
}

/// Request to fetch and verify a content-addressed blob.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlobFetchRequest {
    pub blob: BlobRef,
    pub max_bytes: u64,
}
