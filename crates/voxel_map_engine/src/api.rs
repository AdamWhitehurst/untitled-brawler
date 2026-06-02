use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use ndshape::Shape;

use crate::config::VoxelGenerator;
use crate::instance::VoxelMapInstance;
use crate::raycast::{VoxelRaycastResult, voxel_line_traversal};
use crate::types::WorldVoxel;

/// SystemParam for reading/writing voxels on any map instance.
///
/// Every operation takes a `map: Entity` parameter to select which map instance to operate on.
#[derive(SystemParam)]
pub struct VoxelWorld<'w, 's> {
    maps: Query<'w, 's, (&'static mut VoxelMapInstance, &'static VoxelGenerator)>,
}

impl VoxelWorld<'_, '_> {
    /// Get the voxel at a world-space integer position on a specific map instance.
    ///
    /// Checks the octree first, then evaluates the voxel generator as fallback.
    pub fn get_voxel(&self, map: Entity, pos: IVec3) -> WorldVoxel {
        let Ok((instance, generator)) = self.maps.get(map) else {
            warn!("get_voxel: entity {map:?} has no VoxelMapInstance");
            return WorldVoxel::Unset;
        };

        let chunk_size = instance.chunk_size;
        let chunk_pos = voxel_to_chunk_pos(pos, chunk_size);
        if let Some(chunk_data) = instance.get_chunk_data(chunk_pos) {
            let local = pos - chunk_pos * chunk_size as i32;
            let padded = [
                (local.x + 1) as u32,
                (local.y + 1) as u32,
                (local.z + 1) as u32,
            ];
            let index = instance.shape.linearize(padded) as usize;
            return chunk_data.voxels.get(index);
        }

        evaluate_voxel_at(pos, generator, chunk_size, &instance.shape)
    }

    /// Returns the chunk_size for the given map, if it exists.
    pub fn chunk_size(&self, map: Entity) -> Option<u32> {
        self.maps
            .get(map)
            .ok()
            .map(|(instance, _)| instance.chunk_size)
    }

    /// Mutate multiple world-space voxels on one map. Returns the number of loaded voxels written.
    pub fn set_voxels(
        &mut self,
        map: Entity,
        edits: impl IntoIterator<Item = (IVec3, WorldVoxel)>,
    ) -> usize {
        let Ok((mut instance, _)) = self.maps.get_mut(map) else {
            warn!("set_voxels: entity {map:?} has no VoxelMapInstance");
            return 0;
        };
        instance.set_voxels(edits)
    }

    /// Mutate a voxel directly in the octree. Marks the chunk dirty and queues remesh.
    pub fn set_voxel(&mut self, map: Entity, pos: IVec3, voxel: WorldVoxel) {
        debug_assert!(
            voxel != WorldVoxel::Unset,
            "set_voxel: cannot write Unset (internal sentinel)"
        );

        let Ok((mut instance, _)) = self.maps.get_mut(map) else {
            warn!("set_voxel: entity {map:?} has no VoxelMapInstance");
            return;
        };

        instance.set_voxel(pos, voxel);
    }

    /// Raycast against a specific map instance.
    ///
    /// Casts a ray from `ray.origin` in `ray.direction` up to `max_distance`.
    /// Returns the first voxel matching `filter`.
    pub fn raycast(
        &self,
        map: Entity,
        ray: Ray3d,
        max_distance: f32,
        filter: impl Fn(WorldVoxel) -> bool,
    ) -> Option<VoxelRaycastResult> {
        let Ok((instance, generator)) = self.maps.get(map) else {
            warn!("raycast: entity {map:?} has no VoxelMapInstance");
            return None;
        };

        let start = ray.origin;
        let end = ray.origin + *ray.direction * max_distance;

        let chunk_size = instance.chunk_size;
        let shape = instance.shape.clone();

        let mut cached_chunk: Option<(IVec3, Vec<WorldVoxel>)> = None;

        let mut result = None;

        voxel_line_traversal(start, end, |voxel_pos, t, face| {
            let voxel = lookup_voxel(
                voxel_pos,
                instance,
                generator,
                &mut cached_chunk,
                chunk_size,
                &shape,
            );

            if filter(voxel) {
                result = Some(VoxelRaycastResult {
                    position: voxel_pos,
                    normal: face.normal(),
                    voxel,
                    t,
                });
                return false;
            }
            true
        });

        result
    }
}

/// Look up a voxel at a world position, checking octree first then generator cache.
fn lookup_voxel<S: Shape<3, Coord = u32>>(
    voxel_pos: IVec3,
    instance: &VoxelMapInstance,
    generator: &VoxelGenerator,
    cached_chunk: &mut Option<(IVec3, Vec<WorldVoxel>)>,
    chunk_size: u32,
    shape: &S,
) -> WorldVoxel {
    let chunk_pos = voxel_to_chunk_pos(voxel_pos, chunk_size);

    if let Some(chunk_data) = instance.get_chunk_data(chunk_pos) {
        let local = voxel_pos - chunk_pos * chunk_size as i32;
        let padded = [
            (local.x + 1) as u32,
            (local.y + 1) as u32,
            (local.z + 1) as u32,
        ];
        let index = shape.linearize(padded) as usize;
        return chunk_data.voxels.get(index);
    }

    let needs_generate = match cached_chunk.as_ref() {
        Some((cached_pos, _)) if *cached_pos == chunk_pos => false,
        _ => true,
    };
    if needs_generate {
        *cached_chunk = Some((chunk_pos, generator.0.generate_terrain(chunk_pos)));
    }

    let (_, voxels) = cached_chunk.as_ref().unwrap();
    lookup_voxel_in_chunk(voxels, voxel_pos, chunk_pos, chunk_size, shape)
}

/// Evaluate the voxel generator at a single world-space position.
fn evaluate_voxel_at<S: Shape<3, Coord = u32>>(
    pos: IVec3,
    generator: &VoxelGenerator,
    chunk_size: u32,
    shape: &S,
) -> WorldVoxel {
    let chunk_pos = voxel_to_chunk_pos(pos, chunk_size);
    let voxels = generator.0.generate_terrain(chunk_pos);
    lookup_voxel_in_chunk(&voxels, pos, chunk_pos, chunk_size, shape)
}

/// Index into a flat voxel array to get the voxel at a world position within the given chunk.
fn lookup_voxel_in_chunk<S: Shape<3, Coord = u32>>(
    voxels: &[WorldVoxel],
    voxel_pos: IVec3,
    chunk_pos: IVec3,
    chunk_size: u32,
    shape: &S,
) -> WorldVoxel {
    let local = voxel_pos - chunk_pos * chunk_size as i32;
    let padded = [
        (local.x + 1) as u32,
        (local.y + 1) as u32,
        (local.z + 1) as u32,
    ];
    let index = shape.linearize(padded) as usize;

    if index < voxels.len() {
        voxels[index]
    } else {
        WorldVoxel::Unset
    }
}

/// Converts a world-space voxel position to the chunk coordinate containing it.
pub fn voxel_to_chunk_pos(voxel_pos: IVec3, chunk_size: u32) -> IVec3 {
    let cs = chunk_size as i32;
    IVec3::new(
        voxel_pos.x.div_euclid(cs),
        voxel_pos.y.div_euclid(cs),
        voxel_pos.z.div_euclid(cs),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voxel_to_chunk_pos_basic() {
        assert_eq!(voxel_to_chunk_pos(IVec3::new(0, 0, 0), 16), IVec3::ZERO);
        assert_eq!(voxel_to_chunk_pos(IVec3::new(16, 0, 0), 16), IVec3::X);
        assert_eq!(voxel_to_chunk_pos(IVec3::new(-1, 0, 0), 16), -IVec3::X);
        assert_eq!(voxel_to_chunk_pos(IVec3::new(15, 0, 0), 16), IVec3::ZERO);
    }

    #[test]
    fn evaluate_voxel_flat_terrain() {
        use crate::terrain::FlatGenerator;
        use ndshape::RuntimeShape;
        use std::sync::Arc;
        let shape = RuntimeShape::<u32, 3>::new([18, 18, 18]);
        let generator = VoxelGenerator(Arc::new(FlatGenerator {
            chunk_size: 16,
            shape: shape.clone(),
        }));

        let voxel = evaluate_voxel_at(IVec3::new(0, -1, 0), &generator, 16, &shape);
        assert_eq!(voxel, WorldVoxel::Solid(0));

        let voxel = evaluate_voxel_at(IVec3::new(0, 1, 0), &generator, 16, &shape);
        assert_eq!(voxel, WorldVoxel::Air);
    }

    #[test]
    fn lookup_voxel_in_chunk_roundtrip() {
        use crate::meshing::flat_terrain_voxels;
        use ndshape::RuntimeShape;
        let chunk_pos = IVec3::ZERO;
        let shape = RuntimeShape::<u32, 3>::new([18, 18, 18]);
        let voxels = flat_terrain_voxels(chunk_pos, 16, &shape);

        let voxel = lookup_voxel_in_chunk(&voxels, IVec3::new(0, -1, 0), chunk_pos, 16, &shape);
        assert_eq!(voxel, WorldVoxel::Solid(0));

        let voxel = lookup_voxel_in_chunk(&voxels, IVec3::new(0, 5, 0), chunk_pos, 16, &shape);
        assert_eq!(voxel, WorldVoxel::Air);
    }
}
