use std::sync::Arc;

use bevy::log::info_span;
use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task};
use ndshape::{RuntimeShape, Shape};

use crate::config::{SurfaceHeightMap, VoxelGenerator, VoxelGeneratorImpl, WorldObjectSpawn};
use crate::meshing::{MeshingMode, mesh_chunk};
use crate::palette::PalettedChunk;
use crate::persistence::fs_chunk::FsChunkStore;
use crate::persistence::fs_chunk_entities::FsChunkEntitiesStore;
use crate::types::{ChunkData, ChunkStatus, FillType, WorldVoxel};

/// Number of chunks to generate per async task.
pub const GEN_BATCH_SIZE: usize = 8;

/// Result of an async chunk generation task.
pub struct ChunkGenResult {
    pub position: IVec3,
    pub mesh: Option<Mesh>,
    /// `None` for stages that update chunk status in-place (Features).
    pub chunk_data: Option<ChunkData>,
    pub entity_spawns: Vec<WorldObjectSpawn>,
    /// Whether this chunk was loaded from disk rather than generated.
    pub from_disk: bool,
}

/// Pending async chunk generation tasks for a map entity.
#[derive(Component, Default)]
pub struct PendingChunks {
    pub tasks: Vec<Task<Vec<ChunkGenResult>>>,
}

/// Queued entity spawns from completed Features stages, awaiting server-side processing.
#[derive(Component, Default)]
pub struct PendingEntitySpawns(pub Vec<(IVec3, Vec<WorldObjectSpawn>)>);

/// Spawn an async task that generates terrain for a batch of chunks.
///
/// Each position is first checked on disk; disk-loaded chunks return at their
/// saved status with mesh if non-empty (fast path). Newly generated chunks
/// produce `ChunkStatus::Terrain` with no mesh.
pub fn spawn_terrain_batch(
    pending: &mut PendingChunks,
    positions: Vec<IVec3>,
    generator: &VoxelGenerator,
    shape: RuntimeShape<u32, 3>,
    chunk_store: Option<FsChunkStore>,
    entity_store: Option<FsChunkEntitiesStore>,
    mode: MeshingMode,
) {
    let generator = Arc::clone(&generator.0);
    let pool = AsyncComputeTaskPool::get();

    let task = pool.spawn(async move {
        let _span = info_span!("terrain_batch", count = positions.len()).entered();
        positions
            .into_iter()
            .map(|pos| {
                if let Some(ref store) = chunk_store {
                    use persistence::Store;
                    match store.load(&pos) {
                        Ok(Some(envelope)) => {
                            let chunk_data = envelope.data;
                            let mesh = if chunk_data.fill_type == FillType::Empty {
                                None
                            } else {
                                let voxels = {
                                    let _span = info_span!("disk_load_expand").entered();
                                    chunk_data.voxels.to_voxels()
                                };
                                let _span = info_span!("mesh_chunk").entered();
                                mesh_chunk(&voxels, &shape, mode)
                            };
                            let entity_spawns = load_chunk_entities_from_store(&entity_store, pos);
                            return ChunkGenResult {
                                position: pos,
                                mesh,
                                chunk_data: Some(chunk_data),
                                entity_spawns,
                                from_disk: true,
                            };
                        }
                        Ok(None) => {}
                        Err(e) => {
                            bevy::log::warn!("Failed to load chunk at {pos}: {e}, regenerating");
                        }
                    }
                }
                generate_terrain(pos, &*generator)
            })
            .collect()
    });

    pending.tasks.push(task);
}

/// Spawn an async task that runs the Features stage for a single chunk.
///
/// Tries loading entity data from disk first (generate-once, save-forever).
/// If no saved entities exist, runs the generator's `place_features`.
/// Returns a result with `chunk_data: None` (status update is handled
/// in-place by the caller) and any entity spawns.
pub fn spawn_features_task(
    pending: &mut PendingChunks,
    position: IVec3,
    height_map: SurfaceHeightMap,
    generator: &VoxelGenerator,
    entity_store: Option<FsChunkEntitiesStore>,
) {
    let generator = Arc::clone(&generator.0);
    let pool = AsyncComputeTaskPool::get();

    let task = pool.spawn(async move {
        let _span = info_span!("features_stage", ?position).entered();
        let saved = load_chunk_entities_from_store(&entity_store, position);
        let entity_spawns = if saved.is_empty() {
            generator.place_features(position, &height_map)
        } else {
            saved
        };
        vec![ChunkGenResult {
            position,
            mesh: None,
            chunk_data: None,
            entity_spawns,
            from_disk: false,
        }]
    });

    pending.tasks.push(task);
}

/// Spawn an async task that meshes a chunk from its voxel data.
///
/// Returns a result with `ChunkData` at `ChunkStatus::Mesh`.
pub fn spawn_mesh_task(
    pending: &mut PendingChunks,
    position: IVec3,
    voxels: Vec<WorldVoxel>,
    shape: RuntimeShape<u32, 3>,
    mode: MeshingMode,
) {
    let pool = AsyncComputeTaskPool::get();

    let task = pool.spawn(async move {
        let _span = info_span!("mesh_stage", ?position).entered();
        let mesh = {
            let _span = info_span!("mesh_chunk").entered();
            mesh_chunk(&voxels, &shape, mode)
        };
        let chunk_data = {
            let _span = info_span!("palettize_chunk").entered();
            ChunkData::from_voxels(&voxels, ChunkStatus::Mesh)
        };
        vec![ChunkGenResult {
            position,
            mesh,
            chunk_data: Some(chunk_data),
            entity_spawns: vec![],
            from_disk: false,
        }]
    });

    pending.tasks.push(task);
}

/// Build a padded surface height map from palettized chunk data.
///
/// Expands the palette to a full voxel array, then scans each XZ column
/// top-down for the highest solid voxel. Iterates the full padded
/// `0..padded_size` XZ footprint so the 1-voxel border is populated for
/// chunk-edge slope checks. Called on the main thread before dispatching
/// the Features async task.
pub fn build_surface_height_map<S: Shape<3, Coord = u32>>(
    chunk_pos: IVec3,
    palette: &PalettedChunk,
    chunk_size: u32,
    padded_size: u32,
    shape: &S,
) -> SurfaceHeightMap {
    let voxels = palette.to_voxels();
    let mut map = SurfaceHeightMap::new(chunk_pos, padded_size);

    for px in 0..padded_size {
        for pz in 0..padded_size {
            for py in (1..=chunk_size).rev() {
                let idx = shape.linearize([px, py, pz]) as usize;
                if matches!(voxels[idx], WorldVoxel::Solid(_)) {
                    // Only treat this as a surface if the voxel directly above is
                    // non-solid. Otherwise terrain continues into the chunk above
                    // and this column's "top" is merely the chunk's ceiling — the
                    // real surface is owned by the chunk stacked on top of us.
                    let above_idx = shape.linearize([px, py + 1, pz]) as usize;
                    if !matches!(voxels[above_idx], WorldVoxel::Solid(_)) {
                        let world_y =
                            chunk_pos.y as f64 * chunk_size as f64 + (py - 1) as f64 + 1.0;
                        map.set(px, pz, Some(world_y));
                    }
                    break;
                }
            }
        }
    }
    map
}

/// Load entity spawns from a store, returning an empty vec on missing or error.
fn load_chunk_entities_from_store(
    store: &Option<FsChunkEntitiesStore>,
    pos: IVec3,
) -> Vec<WorldObjectSpawn> {
    let Some(store) = store else {
        return vec![];
    };
    use persistence::Store;
    match store.load(&pos) {
        Ok(Some(spawns)) => spawns,
        Ok(None) => vec![],
        Err(e) => {
            bevy::log::warn!("Failed to load entities at {pos}: {e}");
            vec![]
        }
    }
}

/// Generate terrain-only for a single chunk position.
fn generate_terrain(position: IVec3, generator: &dyn VoxelGeneratorImpl) -> ChunkGenResult {
    let voxels = {
        let _span = info_span!("terrain_gen").entered();
        generator.generate_terrain(position)
    };
    let chunk_data = {
        let _span = info_span!("palettize_chunk").entered();
        ChunkData::from_voxels(&voxels, ChunkStatus::Terrain)
    };
    ChunkGenResult {
        position,
        mesh: None,
        chunk_data: Some(chunk_data),
        entity_spawns: vec![],
        from_disk: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meshing::flat_terrain_voxels;

    fn padded_shape() -> RuntimeShape<u32, 3> {
        RuntimeShape::<u32, 3>::new([18, 18, 18])
    }

    #[test]
    fn surface_height_map_flat_terrain_at_origin() {
        let shape = padded_shape();
        let chunk_pos = IVec3::ZERO;
        let voxels = flat_terrain_voxels(chunk_pos, 16, &shape);
        let palette = PalettedChunk::from_voxels(&voxels);
        let map = build_surface_height_map(chunk_pos, &palette, 16, 18, &shape);

        assert_eq!(map.chunk_pos, chunk_pos);
        // flat_terrain_voxels places surface at y=0, so all columns should have height
        for x in 0..16u32 {
            for z in 0..16u32 {
                let h = map.at(x + 1, z + 1);
                assert!(h.is_some(), "expected surface at ({x}, {z})");
            }
        }
    }

    #[test]
    fn surface_height_map_all_air_chunk() {
        let shape = padded_shape();
        let chunk_pos = IVec3::new(0, 100, 0);
        let voxels = flat_terrain_voxels(chunk_pos, 16, &shape);
        let palette = PalettedChunk::from_voxels(&voxels);
        let map = build_surface_height_map(chunk_pos, &palette, 16, 18, &shape);

        // chunk_pos.y=100 → world_y ~1600..1616, well above flat terrain surface
        for x in 0..16u32 {
            for z in 0..16u32 {
                let h = map.at(x + 1, z + 1);
                assert!(
                    h.is_none(),
                    "expected no surface at ({x}, {z}) for sky chunk"
                );
            }
        }
    }

    #[test]
    fn surface_height_map_consistent_height_across_columns() {
        let shape = padded_shape();
        let chunk_pos = IVec3::ZERO;
        let voxels = flat_terrain_voxels(chunk_pos, 16, &shape);
        let palette = PalettedChunk::from_voxels(&voxels);
        let map = build_surface_height_map(chunk_pos, &palette, 16, 18, &shape);

        // All columns on flat terrain should have the same height
        let first = map.at(1, 1).unwrap();
        for x in 0..16u32 {
            for z in 0..16u32 {
                let h = map.at(x + 1, z + 1).unwrap();
                assert_eq!(h, first, "height mismatch at ({x}, {z})");
            }
        }
    }

    /// Generate flat voxels with a configurable solid-surface Y for stacked-chunk tests.
    fn flat_voxels_with_surface<S: Shape<3, Coord = u32>>(
        chunk_pos: IVec3,
        chunk_size: u32,
        shape: &S,
        surface_y: i32,
    ) -> Vec<WorldVoxel> {
        let mut voxels = vec![WorldVoxel::Air; shape.usize()];
        for i in 0..shape.size() {
            let [_x, y, _z] = shape.delinearize(i);
            let world_y = chunk_pos.y * chunk_size as i32 + y as i32 - 1;
            if world_y <= surface_y {
                voxels[i as usize] = WorldVoxel::Solid(0);
            }
        }
        voxels
    }

    #[test]
    fn surface_height_map_skips_columns_when_terrain_continues_above() {
        let shape = padded_shape();
        // chunkY=-1 covers world Y -16..-1; terrain surface is at Y=5 (in chunk above).
        // Interior is fully solid and padding at py=17 (world_y=0) is also solid.
        let chunk_pos = IVec3::new(0, -1, 0);
        let voxels = flat_voxels_with_surface(chunk_pos, 16, &shape, 5);
        let palette = PalettedChunk::from_voxels(&voxels);
        let map = build_surface_height_map(chunk_pos, &palette, 16, 18, &shape);

        for x in 0..16u32 {
            for z in 0..16u32 {
                assert!(
                    map.at(x + 1, z + 1).is_none(),
                    "column ({x}, {z}) must not claim surface when terrain continues into chunk above"
                );
            }
        }
    }

    #[test]
    fn surface_height_map_reports_surface_when_terrain_terminates_in_chunk() {
        let shape = padded_shape();
        // chunkY=0 covers world Y 0..15; terrain terminates at Y=5 inside this chunk.
        let chunk_pos = IVec3::ZERO;
        let voxels = flat_voxels_with_surface(chunk_pos, 16, &shape, 5);
        let palette = PalettedChunk::from_voxels(&voxels);
        let map = build_surface_height_map(chunk_pos, &palette, 16, 18, &shape);

        for x in 0..16u32 {
            for z in 0..16u32 {
                assert_eq!(
                    map.at(x + 1, z + 1),
                    Some(6.0),
                    "expected top face of Y=5 (= 6.0) at column ({x}, {z})"
                );
            }
        }
    }
}
