use bevy::prelude::*;
use grid_tree::{NodeKey, OctreeI32, VisitCommand};
use ndshape::{RuntimeShape, Shape};
use std::collections::{HashMap, HashSet};

use crate::api::voxel_to_chunk_pos;
use crate::config::VoxelGeneratorImpl;
use crate::types::{ChunkData, WorldVoxel};

/// Marker: this map is the shared overworld.
#[derive(Component)]
pub struct Overworld;

/// Marker: this map is a player's homebase.
#[derive(Component)]
pub struct Homebase;

/// Marker: this map is a competition arena.
#[derive(Component)]
pub struct Arena {
    pub id: u64,
}

/// Core component on every map entity. Owns the spatial index and per-instance state.
#[derive(Component)]
pub struct VoxelMapInstance {
    pub tree: OctreeI32<Option<ChunkData>>,
    /// Effective level per loaded column (level <= LOAD_LEVEL_THRESHOLD only).
    pub chunk_levels: HashMap<IVec2, u32>,
    /// Chunks with unsaved voxel modifications (generated or edited); drives local saves.
    pub dirty_chunks: HashSet<IVec3>,
    /// Chunks changed by genuine content edits (not procedural generation); drives remote publish.
    pub content_dirty_chunks: HashSet<IVec3>,
    /// Chunks that need async remeshing after in-place mutation.
    pub chunks_needing_remesh: HashSet<IVec3>,
    pub debug_colors: bool,
    pub chunk_size: u32,
    pub padded_size: u32,
    /// Padded chunk shape `[padded_size; 3]`. Cloned into async tasks.
    pub shape: RuntimeShape<u32, 3>,
}

impl VoxelMapInstance {
    pub fn new(tree_height: u32, chunk_size: u32) -> Self {
        debug_assert!(chunk_size.is_power_of_two() && chunk_size >= 8);
        let padded_size = chunk_size + 2;
        Self {
            tree: OctreeI32::new(tree_height as u8),
            chunk_levels: HashMap::new(),
            dirty_chunks: HashSet::new(),
            content_dirty_chunks: HashSet::new(),
            chunks_needing_remesh: HashSet::new(),
            debug_colors: false,
            chunk_size,
            padded_size,
            shape: RuntimeShape::<u32, 3>::new([padded_size, padded_size, padded_size]),
        }
    }
}

/// Octree chunk data operations.
impl VoxelMapInstance {
    /// Insert chunk data into the octree at the given chunk position.
    pub fn insert_chunk_data(&mut self, chunk_pos: IVec3, data: ChunkData) {
        let key = NodeKey::new(0, chunk_pos);
        self.tree.fill_path_to_node_from_root(key, |_key, entry| {
            entry.or_insert_with(|| None);
            VisitCommand::Continue
        });
        let relation = self.tree.find_node(key).expect("just created path");
        *self
            .tree
            .get_value_mut(relation.child)
            .expect("just created node") = Some(data);
    }

    /// Remove chunk data from the octree. Returns the data if it existed.
    pub fn remove_chunk_data(&mut self, chunk_pos: IVec3) -> Option<ChunkData> {
        let key = NodeKey::new(0, chunk_pos);
        let relation = self.tree.find_node(key)?;
        let value = self.tree.get_value_mut(relation.child)?;
        value.take()
    }

    /// Get a reference to chunk data in the octree.
    pub fn get_chunk_data(&self, chunk_pos: IVec3) -> Option<&ChunkData> {
        let key = NodeKey::new(0, chunk_pos);
        let relation = self.tree.find_node(key)?;
        self.tree.get_value(relation.child)?.as_ref()
    }

    /// Get a mutable reference to chunk data in the octree.
    pub fn get_chunk_data_mut(&mut self, chunk_pos: IVec3) -> Option<&mut ChunkData> {
        let key = NodeKey::new(0, chunk_pos);
        let relation = self.tree.find_node(key)?;
        self.tree.get_value_mut(relation.child)?.as_mut()
    }

    /// Returns whether the loaded chunk at `chunk_pos` is byte-identical to freshly-generated
    /// terrain (i.e. holds no genuine edits). Used to classify a publish candidate as a delete
    /// (equals generated -> Tombstone) vs an edit (differs -> Present).
    ///
    /// Returns `false` when the chunk is not loaded (cannot prove it equals generated).
    pub fn chunk_matches_generated(
        &self,
        chunk_pos: IVec3,
        generator: &dyn VoxelGeneratorImpl,
    ) -> bool {
        let Some(chunk_data) = self.get_chunk_data(chunk_pos) else {
            trace!(?chunk_pos, "chunk not loaded; cannot compare to generated");
            return false;
        };
        let generated = generator.generate_terrain(chunk_pos);
        chunk_data.matches_voxels(&generated)
    }

    /// Mutate multiple world-space voxels by reusing single-voxel mutation behavior.
    pub fn set_voxels(&mut self, edits: impl IntoIterator<Item = (IVec3, WorldVoxel)>) -> usize {
        let mut written = 0;
        for (world_pos, voxel) in edits {
            debug_assert!(
                voxel != WorldVoxel::Unset,
                "set_voxels: cannot write Unset (internal sentinel)"
            );
            let chunk_pos = voxel_to_chunk_pos(world_pos, self.chunk_size);
            if self.get_chunk_data(chunk_pos).is_none() {
                trace!("set_voxels: chunk {chunk_pos} not loaded, edit at {world_pos} dropped");
                continue;
            }
            self.set_voxel(world_pos, voxel);
            written += 1;
        }
        written
    }

    /// Mutate a voxel directly in the octree. Marks the chunk dirty and queues
    /// it for async remesh. Also updates neighbor chunk padding for boundary voxels.
    /// If the chunk is not loaded, the edit is silently dropped.
    pub fn set_voxel(&mut self, world_pos: IVec3, voxel: WorldVoxel) {
        let chunk_pos = voxel_to_chunk_pos(world_pos, self.chunk_size);
        let local = world_pos - chunk_pos * self.chunk_size as i32;
        let padded = [
            (local.x + 1) as u32,
            (local.y + 1) as u32,
            (local.z + 1) as u32,
        ];
        let index = self.shape.linearize(padded) as usize;

        let Some(chunk_data) = self.get_chunk_data_mut(chunk_pos) else {
            trace!("set_voxel: chunk {chunk_pos} not loaded, edit at {world_pos} dropped");
            return;
        };
        chunk_data.voxels.set(index, voxel);

        self.dirty_chunks.insert(chunk_pos);
        self.content_dirty_chunks.insert(chunk_pos);
        self.chunks_needing_remesh.insert(chunk_pos);

        self.update_neighbor_padding(chunk_pos, local, voxel);
    }

    /// Update padding voxels in neighboring chunks when a boundary voxel is modified.
    fn update_neighbor_padding(&mut self, chunk_pos: IVec3, local: IVec3, voxel: WorldVoxel) {
        let chunk_size = self.chunk_size as i32;
        for axis in 0..3 {
            let l = local[axis];
            if l == 0 {
                let mut neighbor = chunk_pos;
                neighbor[axis] -= 1;
                let mut pl = local;
                pl[axis] = chunk_size;
                let padded = [(pl.x + 1) as u32, (pl.y + 1) as u32, (pl.z + 1) as u32];
                let idx = self.shape.linearize(padded) as usize;
                if let Some(nd) = self.get_chunk_data_mut(neighbor) {
                    nd.voxels.set(idx, voxel);
                }
                self.chunks_needing_remesh.insert(neighbor);
            }
            if l == chunk_size - 1 {
                let mut neighbor = chunk_pos;
                neighbor[axis] += 1;
                let mut pl = local;
                pl[axis] = -1;
                let padded = [(pl.x + 1) as u32, (pl.y + 1) as u32, (pl.z + 1) as u32];
                let idx = self.shape.linearize(padded) as usize;
                if let Some(nd) = self.get_chunk_data_mut(neighbor) {
                    nd.voxels.set(idx, voxel);
                }
                self.chunks_needing_remesh.insert(neighbor);
            }
        }
    }
}

pub fn bounds_to_spawning_distance(bounds: IVec3) -> u32 {
    bounds.max_element().max(1) as u32
}

/// Derives a deterministic seed from a player/entity ID.
pub fn seed_from_id(id: u64) -> u64 {
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terrain::AirGenerator;
    use crate::types::{ChunkStatus, FillType};

    /// Padded chunk volume for the default `chunk_size=16`, used by tests.
    const PADDED_VOLUME_16: usize = 18 * 18 * 18;

    #[test]
    fn chunk_matches_generated_true_for_unedited_chunk() {
        let mut instance = VoxelMapInstance::new(5, 16);
        let pos = IVec3::ZERO;
        let voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
        instance.insert_chunk_data(pos, ChunkData::from_voxels(&voxels, ChunkStatus::Full));
        assert!(instance.chunk_matches_generated(pos, &AirGenerator::new(16)));
    }

    #[test]
    fn chunk_matches_generated_false_after_edit() {
        let mut instance = VoxelMapInstance::new(5, 16);
        let pos = IVec3::ZERO;
        let voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
        instance.insert_chunk_data(pos, ChunkData::from_voxels(&voxels, ChunkStatus::Full));
        instance.set_voxel(IVec3::new(5, 5, 5), WorldVoxel::Solid(1));
        assert!(!instance.chunk_matches_generated(pos, &AirGenerator::new(16)));
    }

    #[test]
    fn chunk_matches_generated_false_when_unloaded() {
        let instance = VoxelMapInstance::new(5, 16);
        assert!(!instance.chunk_matches_generated(IVec3::ZERO, &AirGenerator::new(16)));
    }

    #[test]
    fn new_creates_empty_instance() {
        let instance = VoxelMapInstance::new(3, 16);
        assert!(instance.chunk_levels.is_empty());
        assert!(instance.dirty_chunks.is_empty());
        assert!(instance.chunks_needing_remesh.is_empty());
    }

    #[test]
    fn insert_and_find_chunk() {
        let mut instance = VoxelMapInstance::new(3, 16);
        let key = NodeKey::new(0, IVec3::new(1, 0, 2));

        instance
            .tree
            .fill_path_to_node_from_root(key, |_node_key, entry| {
                entry.or_insert_with(|| None);
                VisitCommand::Continue
            });

        let relation = instance.tree.find_node(key).unwrap();
        *instance.tree.get_value_mut(relation.child).unwrap() =
            Some(ChunkData::new_empty(PADDED_VOLUME_16));

        let found = instance.tree.find_node(key);
        assert!(found.is_some());
        let data = instance.tree.get_value(found.unwrap().child).unwrap();
        assert!(data.is_some());
    }

    #[test]
    fn bounds_to_spawning_distance_uses_max_axis() {
        assert_eq!(bounds_to_spawning_distance(IVec3::new(1, 2, 3)), 3);
        assert_eq!(bounds_to_spawning_distance(IVec3::new(10, 1, 1)), 10);
        assert_eq!(bounds_to_spawning_distance(IVec3::new(1, 1, 1)), 1);
    }

    #[test]
    fn insert_and_retrieve_chunk_data() {
        let mut instance = VoxelMapInstance::new(5, 16);
        let pos = IVec3::new(1, 0, 2);
        let chunk = ChunkData::new_empty(PADDED_VOLUME_16);
        instance.insert_chunk_data(pos, chunk);
        assert!(instance.get_chunk_data(pos).is_some());
        assert_eq!(
            instance.get_chunk_data(pos).unwrap().fill_type,
            FillType::Empty
        );
    }

    #[test]
    fn remove_chunk_data_returns_data() {
        let mut instance = VoxelMapInstance::new(5, 16);
        let pos = IVec3::ZERO;
        instance.insert_chunk_data(pos, ChunkData::new_empty(PADDED_VOLUME_16));
        let removed = instance.remove_chunk_data(pos);
        assert!(removed.is_some());
        assert!(instance.get_chunk_data(pos).is_none());
    }

    #[test]
    fn remove_nonexistent_chunk_returns_none() {
        let mut instance = VoxelMapInstance::new(5, 16);
        assert!(instance.remove_chunk_data(IVec3::ZERO).is_none());
    }

    #[test]
    fn get_nonexistent_chunk_returns_none() {
        let instance = VoxelMapInstance::new(5, 16);
        assert!(instance.get_chunk_data(IVec3::new(99, 99, 99)).is_none());
    }

    #[test]
    fn overwrite_chunk_data() {
        let mut instance = VoxelMapInstance::new(5, 16);
        let pos = IVec3::ZERO;
        instance.insert_chunk_data(pos, ChunkData::new_empty(PADDED_VOLUME_16));

        let mut voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
        voxels[0] = WorldVoxel::Solid(1);
        let solid_chunk = ChunkData::from_voxels(&voxels, ChunkStatus::Full);
        instance.insert_chunk_data(pos, solid_chunk);

        let data = instance.get_chunk_data(pos).unwrap();
        assert_eq!(data.fill_type, FillType::Mixed);
        assert_eq!(data.voxels.get(0), WorldVoxel::Solid(1));
    }

    #[test]
    fn set_voxel_mutates_octree_in_place() {
        let mut instance = VoxelMapInstance::new(5, 16);
        let chunk_pos = IVec3::ZERO;
        let voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
        instance.insert_chunk_data(
            chunk_pos,
            ChunkData::from_voxels(&voxels, ChunkStatus::Full),
        );

        let world_pos = IVec3::new(5, 5, 5);
        instance.set_voxel(world_pos, WorldVoxel::Solid(42));

        let local = world_pos - chunk_pos * instance.chunk_size as i32;
        let padded = [
            (local.x + 1) as u32,
            (local.y + 1) as u32,
            (local.z + 1) as u32,
        ];
        let index = instance.shape.linearize(padded) as usize;
        let data = instance.get_chunk_data(chunk_pos).unwrap();
        assert_eq!(data.voxels.get(index), WorldVoxel::Solid(42));
        assert!(instance.dirty_chunks.contains(&chunk_pos));
        assert!(instance.chunks_needing_remesh.contains(&chunk_pos));
    }

    #[test]
    fn content_dirty_tracks_edits_not_generated_inserts() {
        let mut instance = VoxelMapInstance::new(5, 16);
        let chunk_pos = IVec3::ZERO;
        let voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];

        // Generated/disk-loaded chunk insertion must not mark content dirty.
        instance.insert_chunk_data(
            chunk_pos,
            ChunkData::from_voxels(&voxels, ChunkStatus::Full),
        );
        assert!(instance.content_dirty_chunks.is_empty());

        // A genuine voxel edit marks the chunk content dirty (and locally dirty).
        instance.set_voxel(IVec3::new(5, 5, 5), WorldVoxel::Solid(42));
        assert!(instance.content_dirty_chunks.contains(&chunk_pos));
        assert!(instance.dirty_chunks.contains(&chunk_pos));
    }

    #[test]
    fn set_voxel_on_unloaded_chunk_is_dropped() {
        let mut instance = VoxelMapInstance::new(5, 16);
        instance.set_voxel(IVec3::new(5, 5, 5), WorldVoxel::Solid(1));
        assert!(instance.dirty_chunks.is_empty());
        assert!(instance.chunks_needing_remesh.is_empty());
    }

    #[test]
    fn multiple_edits_same_chunk_single_remesh() {
        let mut instance = VoxelMapInstance::new(5, 16);
        let chunk_pos = IVec3::ZERO;
        let voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
        instance.insert_chunk_data(
            chunk_pos,
            ChunkData::from_voxels(&voxels, ChunkStatus::Full),
        );

        instance.set_voxel(IVec3::new(1, 1, 1), WorldVoxel::Solid(1));
        instance.set_voxel(IVec3::new(2, 2, 2), WorldVoxel::Solid(2));
        instance.set_voxel(IVec3::new(3, 3, 3), WorldVoxel::Solid(3));

        assert_eq!(instance.chunks_needing_remesh.len(), 1);
        assert!(instance.chunks_needing_remesh.contains(&chunk_pos));
    }

    #[test]
    fn boundary_edit_updates_neighbor_padding() {
        let mut instance = VoxelMapInstance::new(5, 16);
        let chunk_a = IVec3::ZERO;
        let chunk_b = IVec3::X; // neighbor in +x direction

        let voxels_a = vec![WorldVoxel::Air; PADDED_VOLUME_16];
        let voxels_b = vec![WorldVoxel::Air; PADDED_VOLUME_16];
        instance.insert_chunk_data(
            chunk_a,
            ChunkData::from_voxels(&voxels_a, ChunkStatus::Full),
        );
        instance.insert_chunk_data(
            chunk_b,
            ChunkData::from_voxels(&voxels_b, ChunkStatus::Full),
        );

        // Edit at x=15 (last voxel in chunk_a along x) — boundary with chunk_b
        let world_pos = IVec3::new(15, 5, 5);
        instance.set_voxel(world_pos, WorldVoxel::Solid(7));

        // Owning chunk should have the voxel at padded [16, 6, 6]
        let idx_a = instance.shape.linearize([16, 6, 6]) as usize;
        let idx_b = instance.shape.linearize([0, 6, 6]) as usize;
        let data_a = instance.get_chunk_data(chunk_a).unwrap();
        assert_eq!(data_a.voxels.get(idx_a), WorldVoxel::Solid(7));

        // Neighbor chunk_b should have padding updated at padded [0, 6, 6]
        let data_b = instance.get_chunk_data(chunk_b).unwrap();
        assert_eq!(
            data_b.voxels.get(idx_b),
            WorldVoxel::Solid(7),
            "neighbor chunk padding should mirror boundary edit"
        );

        // Both chunks should need remesh
        assert!(instance.chunks_needing_remesh.contains(&chunk_a));
        assert!(instance.chunks_needing_remesh.contains(&chunk_b));
    }

    #[test]
    fn boundary_edit_low_edge_updates_neighbor_padding() {
        let mut instance = VoxelMapInstance::new(5, 16);
        let chunk_a = IVec3::ZERO;
        let chunk_neg = -IVec3::X; // neighbor in -x direction

        let voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
        instance.insert_chunk_data(chunk_a, ChunkData::from_voxels(&voxels, ChunkStatus::Full));
        instance.insert_chunk_data(
            chunk_neg,
            ChunkData::from_voxels(&voxels, ChunkStatus::Full),
        );

        // Edit at x=0 (first voxel in chunk_a along x) — boundary with chunk_neg
        let world_pos = IVec3::new(0, 3, 3);
        instance.set_voxel(world_pos, WorldVoxel::Solid(11));

        // Owning chunk should have the voxel at padded [1, 4, 4]
        let idx_a = instance.shape.linearize([1, 4, 4]) as usize;
        let idx_neg = instance.shape.linearize([17, 4, 4]) as usize;
        let data_a = instance.get_chunk_data(chunk_a).unwrap();
        assert_eq!(data_a.voxels.get(idx_a), WorldVoxel::Solid(11));

        // Neighbor chunk_neg should have padding updated at padded [17, 4, 4]
        let data_neg = instance.get_chunk_data(chunk_neg).unwrap();
        assert_eq!(
            data_neg.voxels.get(idx_neg),
            WorldVoxel::Solid(11),
            "negative neighbor padding should mirror boundary edit"
        );

        assert!(instance.chunks_needing_remesh.contains(&chunk_a));
        assert!(instance.chunks_needing_remesh.contains(&chunk_neg));
    }
}
