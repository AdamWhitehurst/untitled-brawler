mod chunk;
mod colliders;
mod homebase_publication;
mod persistence;
mod transition;
mod types;
mod voxel;

use serde::{Deserialize, Serialize};

use bevy::prelude::*;

use crate::auth::NostrPublicKey;

pub use voxel_map_engine::prelude::{VoxelChunk, VoxelType};

pub use chunk::{ChunkChannel, ChunkDataSync, UnloadColumn};
pub use colliders::attach_chunk_colliders;
pub use homebase_publication::{
    HomebaseAttestationRequest, HomebaseAttestationResponse, HomebasePayloadScope,
    HomebasePublicationAttestation,
};
pub use persistence::{MapSaveTarget, SavedEntity, SavedEntityKind};
pub use transition::{
    MapChannel, MapTransitionEnd, MapTransitionReady, MapTransitionStart, PendingTransition,
    PlayerMapSwitchRequest, TransitionReadySent,
};
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Reflect)]
#[type_path = "protocol::map"]
pub struct Owner(pub NostrPublicKey);

pub use types::{MapInstanceId, MapRegistry, MapSwitchTarget};
pub use voxel::{
    SectionBlocksUpdate, VoxelBrushEditRequest, VoxelChange, VoxelChannel,
    VoxelConcreteEditRequest, VoxelEditAck, VoxelEditBroadcast, VoxelEditReject, VoxelEditRequest,
};

/// Tags an entity as belonging to a specific chunk on a specific map.
/// Used to save/despawn entities when their chunk is evicted.
#[derive(Component, Clone, Debug)]
pub struct ChunkEntityRef {
    pub chunk_pos: IVec3,
    pub map_entity: Entity,
}
