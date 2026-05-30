use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::auth::NostrPublicKey;
use crate::map::MapInstanceId;

/// Server-signed authorization permitting a client to publish a specific
/// homebase manifest revision. The server only issues one for a homebase it
/// owns authority over, after validating the descriptor root and payload scope
/// against authoritative state.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct HomebasePublicationAttestation {
    pub owner: NostrPublicKey,
    pub map_id: MapInstanceId,
    pub server_revision: u64,
    pub previous_manifest_hash: Option<[u8; 32]>,
    pub descriptor_root: [u8; 32],
    pub payload_scope: HomebasePayloadScope,
    pub expires_at: u64,
    pub server_pubkey: NostrPublicKey,
    pub server_signature: Vec<u8>,
}

/// Exact set of payload slots an attestation authorizes, so a client cannot
/// publish data the server never validated.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct HomebasePayloadScope {
    pub terrain_chunks: Vec<IVec3>,
    pub chunk_entities: Vec<IVec3>,
    pub includes_meta: bool,
    pub includes_map_entities: bool,
}
