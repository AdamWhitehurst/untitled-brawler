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
    /// Terrain chunks published as `Present` (genuine edits differing from generated terrain).
    pub edited_chunks: Vec<IVec3>,
    /// Terrain chunks published as `Tombstoned` (reverted to generated; folds out on restore).
    pub tombstoned_chunks: Vec<IVec3>,
    pub chunk_entities: Vec<IVec3>,
    /// Chunk-entity slots published as `Tombstoned`.
    pub tombstoned_chunk_entities: Vec<IVec3>,
    pub includes_meta: bool,
    pub includes_map_entities: bool,
}

/// Client request asking the server to prepare and authorize a publication of
/// the player's own homebase.
///
/// The owner is the authenticated player on the connection; the server derives
/// it rather than trusting a client-supplied identity. The client carries no
/// payload descriptors because it cannot faithfully reproduce the server's
/// authoritative save bytes; the server reads, encodes, and uploads them itself
/// (see the Phase 5 "server encodes, client signs" decision).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Message)]
pub struct HomebaseAttestationRequest;

/// Server reply to a [`HomebaseAttestationRequest`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Message)]
pub enum HomebaseAttestationResponse {
    /// The server uploaded the homebase payloads and signed an attestation,
    /// returning the canonical JSON of the unsigned `NostrMapManifest` (which
    /// embeds the attestation). The client signs this manifest event with the
    /// player's Nostr key and publishes it.
    ///
    /// `manifest_hash` identifies the in-flight revision so the client can echo
    /// it back in [`HomebasePublished`] once the event is on relays.
    Granted {
        unsigned_manifest_json: String,
        manifest_hash: [u8; 32],
    },
    /// The server refused; the string explains why (diagnostics only).
    Rejected(String),
}

/// Client -> server: the player published the granted homebase manifest event to relays.
/// The server uses `manifest_hash` to advance the accepted head and clear the durable
/// change-set for exactly that in-flight revision.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Message)]
pub struct HomebasePublished {
    pub manifest_hash: [u8; 32],
}
