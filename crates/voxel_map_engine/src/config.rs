use std::path::PathBuf;
use std::sync::Arc;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::types::WorldVoxel;

/// Trait for multi-stage chunk generation.
///
/// Implementors produce terrain voxels and optionally place entity-based features.
/// Each method corresponds to a pipeline stage.
pub trait VoxelGeneratorImpl: Send + Sync {
    /// Stage 1: Base terrain shape. Returns a padded voxel array sized `padded_size³`.
    fn generate_terrain(&self, chunk_pos: IVec3) -> Vec<WorldVoxel>;

    /// Stage 2: Entity placement on terrain surface.
    /// Receives a padded surface height map (not raw voxels). Default: no features.
    fn place_features(
        &self,
        _chunk_pos: IVec3,
        _heights: &SurfaceHeightMap,
    ) -> Vec<WorldObjectSpawn> {
        Vec::new()
    }
}

/// Spawn data for a world object placed during the Features stage.
///
/// Uses bare `String` for `object_id` (not `WorldObjectId`) because `WorldObjectId`
/// lives in the `protocol` crate, and `voxel_map_engine` must not depend on it.
/// The server spawn system converts to `WorldObjectId` at the boundary.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorldObjectSpawn {
    pub object_id: String,
    pub position: Vec3,
    /// RON-serialized persisted components. Empty for fresh spawns.
    #[serde(default)]
    pub persisted_components: Vec<PersistedComponent>,
}

/// A single persisted component: type path + RON data.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedComponent {
    pub type_path: String,
    pub ron_data: String,
}

/// Padded surface height map built from `PalettedChunk` on the main thread.
///
/// Indexed in padded space `[0, padded_size)²` — the 1-voxel border is populated
/// so slope checks at chunk edges can read neighbor heights without clipping.
pub struct SurfaceHeightMap {
    pub chunk_pos: IVec3,
    pub padded_size: u32,
    pub heights: Box<[Option<f64>]>,
}

impl SurfaceHeightMap {
    pub fn new(chunk_pos: IVec3, padded_size: u32) -> Self {
        let len = (padded_size as usize) * (padded_size as usize);
        Self {
            chunk_pos,
            padded_size,
            heights: vec![None; len].into_boxed_slice(),
        }
    }

    /// Look up height by padded XZ coordinate. `px`/`pz` must be in `0..padded_size`.
    pub fn at(&self, px: u32, pz: u32) -> Option<f64> {
        debug_assert!(px < self.padded_size && pz < self.padded_size);
        self.heights[(px * self.padded_size + pz) as usize]
    }

    pub fn set(&mut self, px: u32, pz: u32, h: Option<f64>) {
        debug_assert!(px < self.padded_size && pz < self.padded_size);
        self.heights[(px * self.padded_size + pz) as usize] = h;
    }
}

/// The chunk generation implementation for a map instance.
///
/// Separate component from `VoxelMapConfig` so maps can exist without a
/// generator while terrain components are being applied (deferred commands).
#[derive(Component, Clone)]
pub struct VoxelGenerator(pub Arc<dyn VoxelGeneratorImpl>);

/// Static per-map-type dimensional config, loaded from `.terrain.ron`.
///
/// Inserted onto map entities via the terrain def pipeline. Separate from
/// `VoxelMapConfig` so systems that only need dimensional data can query
/// this component without contending with runtime state.
#[derive(Component, Reflect, Clone, Debug)]
#[reflect(Component)]
pub struct MapDimensions {
    /// Edge length of a chunk in voxels. Power of two, >= 8.
    pub chunk_size: u32,
    /// Inclusive-exclusive Y chunk range for column expansion: `(y_min, y_max)`.
    pub column_y_range: (i32, i32),
    /// Octree tree_height for this map.
    pub tree_height: u32,
    /// Fixed map dimensions. `None` = infinite generation.
    pub bounds: Option<IVec3>,
}

impl MapDimensions {
    /// `chunk_size + 2`.
    pub fn padded_size(&self) -> u32 {
        self.chunk_size + 2
    }
}

/// Selects the chunk meshing algorithm for a map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize, Reflect)]
pub enum MeshingMode {
    /// Axis-aligned greedy quads via `block-mesh-rs`. Stepped surfaces.
    #[default]
    Cubic,
    /// Corner-max sloped meshing. Single-unit elevation changes become 45° faces.
    Sloped,
}

/// Runtime configuration for a map instance.
///
/// Holds state that varies per-run (seed, save directory) or per-deployment
/// (chunk generation locality). Static dimensional config lives separately
/// in `MapDimensions`, which is loaded from `.terrain.ron`.
#[derive(Component)]
pub struct VoxelMapConfig {
    pub seed: u64,
    /// Tracks the version of the generation algorithm for save compatibility.
    pub generation_version: u32,
    pub spawning_distance: u32,
    /// Directory for persisting chunk data. `None` means no persistence.
    pub save_dir: Option<PathBuf>,
    /// Whether this map generates chunks locally. Server sets `true`, client sets `false`
    /// when chunks are streamed from the server.
    pub generates_chunks: bool,
    /// Meshing algorithm. Defaults to `Cubic` via `MeshingMode::default`.
    pub meshing_mode: MeshingMode,
}

impl VoxelMapConfig {
    pub fn new(
        seed: u64,
        generation_version: u32,
        spawning_distance: u32,
        generates_chunks: bool,
    ) -> Self {
        debug_assert!(
            spawning_distance > 0,
            "VoxelMapConfig: spawning_distance must be > 0"
        );
        Self {
            seed,
            generation_version,
            spawning_distance,
            save_dir: None,
            generates_chunks,
            meshing_mode: MeshingMode::default(),
        }
    }

    /// Builder: set meshing mode (default `Cubic`).
    pub fn with_meshing_mode(mut self, mode: MeshingMode) -> Self {
        self.meshing_mode = mode;
        self
    }
}
