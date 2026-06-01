use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::palette::PalettedChunk;

/// Voxel data stored per position
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Reflect)]
pub enum WorldVoxel {
    Air,
    Unset,
    Solid(u8),
}

impl Default for WorldVoxel {
    fn default() -> Self {
        Self::Unset
    }
}

/// How a chunk is filled (optimization for uniform chunks)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FillType {
    Empty,
    Mixed,
    Uniform(WorldVoxel),
}

/// Generation stage of a chunk in the multi-stage pipeline.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Reflect,
)]
pub enum ChunkStatus {
    Empty = 0,
    Terrain = 1,
    Features = 2,
    Mesh = 3,
    Full = 4,
}

impl ChunkStatus {
    /// Returns the next stage, or `None` if already at `Full`.
    pub fn next(self) -> Option<ChunkStatus> {
        match self {
            Self::Empty => Some(Self::Terrain),
            Self::Terrain => Some(Self::Features),
            Self::Features => Some(Self::Mesh),
            Self::Mesh => Some(Self::Full),
            Self::Full => None,
        }
    }

    /// Maximum achievable status for a chunk at the given effective level.
    /// Levels 0-2 (EntityTicking/BlockTicking/Border) → Full
    /// Level 3 → Mesh, Level 4 → Features, Level 5+ → Terrain
    pub fn max_for_level(effective_level: u32) -> ChunkStatus {
        match effective_level {
            0..=2 => ChunkStatus::Full,
            3 => ChunkStatus::Mesh,
            4 => ChunkStatus::Features,
            _ => ChunkStatus::Terrain,
        }
    }
}

/// Voxel data for one chunk (16^3 with 1-voxel padding = 18^3)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChunkData {
    pub voxels: PalettedChunk,
    pub fill_type: FillType,
    pub hash: u64,
    pub status: ChunkStatus,
}

impl ChunkData {
    /// Create an empty chunk (all air) with the given padded volume.
    ///
    /// Status is `Full` because this creates a fully-resolved all-air chunk
    /// (used by tests and the API layer for explicit voxel edits), not a chunk
    /// entering the generation pipeline.
    pub fn new_empty(padded_volume: usize) -> Self {
        Self {
            voxels: PalettedChunk::SingleValue {
                voxel: WorldVoxel::Air,
                len: padded_volume,
            },
            fill_type: FillType::Empty,
            hash: 0,
            status: ChunkStatus::Full,
        }
    }

    /// Returns whether this chunk's padded voxel buffer is element-wise identical to `voxels`.
    /// A length mismatch returns `false` (cannot be equal) with a `trace!`.
    pub fn matches_voxels(&self, voxels: &[WorldVoxel]) -> bool {
        if self.voxels.len() != voxels.len() {
            trace!(
                stored = self.voxels.len(),
                other = voxels.len(),
                "chunk voxel length mismatch; not equal to generated"
            );
            return false;
        }
        (0..voxels.len()).all(|i| self.voxels.get(i) == voxels[i])
    }

    /// Construct from a flat voxel array (generation output).
    pub fn from_voxels(voxels: &[WorldVoxel], status: ChunkStatus) -> Self {
        let fill_type = classify_fill_type(voxels);
        let hash = compute_chunk_hash(voxels);
        let palettized = PalettedChunk::from_voxels(voxels);
        Self {
            voxels: palettized,
            fill_type,
            hash,
            status,
        }
    }
}

fn classify_fill_type(voxels: &[WorldVoxel]) -> FillType {
    let first = voxels.first().copied().unwrap_or(WorldVoxel::Air);
    if voxels.iter().all(|&v| v == first) {
        if first == WorldVoxel::Air {
            FillType::Empty
        } else {
            FillType::Uniform(first)
        }
    } else {
        FillType::Mixed
    }
}

fn compute_chunk_hash(voxels: &[WorldVoxel]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    voxels.hash(&mut hasher);
    hasher.finish()
}

/// Network-serializable voxel type (mirrors WorldVoxel without Unset)
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Reflect)]
pub enum VoxelType {
    Air,
    Solid(u8),
}

impl From<VoxelType> for WorldVoxel {
    fn from(v: VoxelType) -> Self {
        match v {
            VoxelType::Air => WorldVoxel::Air,
            VoxelType::Solid(m) => WorldVoxel::Solid(m),
        }
    }
}

impl From<WorldVoxel> for VoxelType {
    fn from(v: WorldVoxel) -> Self {
        match v {
            WorldVoxel::Air | WorldVoxel::Unset => VoxelType::Air,
            WorldVoxel::Solid(m) => VoxelType::Solid(m),
        }
    }
}

impl block_mesh::Voxel for WorldVoxel {
    fn get_visibility(&self) -> block_mesh::VoxelVisibility {
        match self {
            WorldVoxel::Air | WorldVoxel::Unset => block_mesh::VoxelVisibility::Empty,
            WorldVoxel::Solid(_) => block_mesh::VoxelVisibility::Opaque,
        }
    }
}

impl block_mesh::MergeVoxel for WorldVoxel {
    type MergeValue = u8;
    type MergeValueFacingNeighbour = u8;

    fn merge_value(&self) -> u8 {
        match self {
            WorldVoxel::Solid(m) => *m,
            _ => 0,
        }
    }

    fn merge_value_facing_neighbour(&self) -> u8 {
        self.merge_value()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Padded chunk volume for the default `chunk_size=16`, used by tests.
    const PADDED_VOLUME_16: usize = 18 * 18 * 18;

    #[test]
    fn chunk_data_new_empty() {
        let chunk = ChunkData::new_empty(PADDED_VOLUME_16);
        assert!(chunk.voxels.is_uniform());
        assert_eq!(chunk.voxels.get(0), WorldVoxel::Air);
        assert_eq!(chunk.fill_type, FillType::Empty);
    }

    #[test]
    fn world_voxel_to_voxel_type_roundtrip() {
        let air: VoxelType = WorldVoxel::Air.into();
        assert_eq!(air, VoxelType::Air);
        let back: WorldVoxel = air.into();
        assert_eq!(back, WorldVoxel::Air);

        let solid: VoxelType = WorldVoxel::Solid(42).into();
        assert_eq!(solid, VoxelType::Solid(42));
        let back: WorldVoxel = solid.into();
        assert_eq!(back, WorldVoxel::Solid(42));
    }

    #[test]
    fn world_voxel_unset_maps_to_air() {
        let vt: VoxelType = WorldVoxel::Unset.into();
        assert_eq!(vt, VoxelType::Air);
    }

    #[test]
    fn classify_fill_type_empty() {
        let voxels = vec![WorldVoxel::Air; 100];
        assert_eq!(classify_fill_type(&voxels), FillType::Empty);
    }

    #[test]
    fn classify_fill_type_uniform_solid() {
        let voxels = vec![WorldVoxel::Solid(5); 100];
        assert_eq!(
            classify_fill_type(&voxels),
            FillType::Uniform(WorldVoxel::Solid(5))
        );
    }

    #[test]
    fn classify_fill_type_mixed() {
        let mut voxels = vec![WorldVoxel::Air; 100];
        voxels[0] = WorldVoxel::Solid(1);
        assert_eq!(classify_fill_type(&voxels), FillType::Mixed);
    }

    #[test]
    fn from_voxels_sets_fill_type_and_hash() {
        let voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
        let chunk = ChunkData::from_voxels(&voxels, ChunkStatus::Full);
        assert_eq!(chunk.fill_type, FillType::Empty);
        assert!(chunk.voxels.is_uniform());

        let mut mixed = vec![WorldVoxel::Air; PADDED_VOLUME_16];
        mixed[0] = WorldVoxel::Solid(1);
        let chunk = ChunkData::from_voxels(&mixed, ChunkStatus::Full);
        assert_eq!(chunk.fill_type, FillType::Mixed);
        assert_ne!(chunk.hash, 0);
    }

    #[test]
    fn chunk_status_next_progresses_through_stages() {
        assert_eq!(ChunkStatus::Empty.next(), Some(ChunkStatus::Terrain));
        assert_eq!(ChunkStatus::Terrain.next(), Some(ChunkStatus::Features));
        assert_eq!(ChunkStatus::Features.next(), Some(ChunkStatus::Mesh));
        assert_eq!(ChunkStatus::Mesh.next(), Some(ChunkStatus::Full));
        assert_eq!(ChunkStatus::Full.next(), None);
    }

    #[test]
    fn chunk_status_ordering() {
        assert!(ChunkStatus::Empty < ChunkStatus::Terrain);
        assert!(ChunkStatus::Terrain < ChunkStatus::Features);
        assert!(ChunkStatus::Features < ChunkStatus::Mesh);
        assert!(ChunkStatus::Mesh < ChunkStatus::Full);
    }

    #[test]
    fn chunk_data_new_empty_has_full_status() {
        let chunk = ChunkData::new_empty(PADDED_VOLUME_16);
        assert_eq!(chunk.status, ChunkStatus::Full);
    }

    #[test]
    fn chunk_data_from_voxels_preserves_status() {
        let voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
        let terrain = ChunkData::from_voxels(&voxels, ChunkStatus::Terrain);
        assert_eq!(terrain.status, ChunkStatus::Terrain);

        let full = ChunkData::from_voxels(&voxels, ChunkStatus::Full);
        assert_eq!(full.status, ChunkStatus::Full);
    }

    #[test]
    fn chunk_status_max_for_level() {
        assert_eq!(ChunkStatus::max_for_level(0), ChunkStatus::Full);
        assert_eq!(ChunkStatus::max_for_level(1), ChunkStatus::Full);
        assert_eq!(ChunkStatus::max_for_level(2), ChunkStatus::Full);
        assert_eq!(ChunkStatus::max_for_level(3), ChunkStatus::Mesh);
        assert_eq!(ChunkStatus::max_for_level(4), ChunkStatus::Features);
        assert_eq!(ChunkStatus::max_for_level(5), ChunkStatus::Terrain);
        assert_eq!(ChunkStatus::max_for_level(20), ChunkStatus::Terrain);
    }
}
