use bevy::asset::RenderAssetUsages;
use bevy::log::info_span;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use block_mesh::{GreedyQuadsBuffer, RIGHT_HANDED_Y_UP_CONFIG, greedy_quads};
use ndshape::Shape;

use crate::types::WorldVoxel;

/// Mesh a padded voxel array into a Bevy Mesh using greedy quads.
/// `shape` describes the padded chunk dimensions; `voxels.len()` must equal `shape.usize()`.
pub fn mesh_chunk_greedy<S: Shape<3, Coord = u32>>(
    voxels: &[WorldVoxel],
    shape: &S,
) -> Option<Mesh> {
    debug_assert_eq!(voxels.len(), shape.usize());

    let mut buffer = GreedyQuadsBuffer::new(voxels.len());
    let faces = RIGHT_HANDED_Y_UP_CONFIG.faces;
    let dims = shape.as_array();
    let max = [dims[0] - 1, dims[1] - 1, dims[2] - 1];
    {
        let _span = info_span!("greedy_quads").entered();
        greedy_quads(voxels, shape, [0; 3], max, &faces, &mut buffer);
    }

    if buffer.quads.num_quads() == 0 {
        return None;
    }

    let _span = info_span!("assemble_vertices").entered();
    let num_vertices = buffer.quads.num_quads() * 4;
    let num_indices = buffer.quads.num_quads() * 6;

    let mut positions = Vec::with_capacity(num_vertices);
    let mut normals = Vec::with_capacity(num_vertices);
    let mut indices = Vec::with_capacity(num_indices);
    let mut tex_coords = Vec::with_capacity(num_vertices);
    // TODO(perf): Float32x4 vertex colors cost 16 bytes per vertex. If terrain mesh memory or
    // vertex bandwidth becomes a bottleneck, switch this to packed Uint8x4 colors or material indices.
    let mut colors = Vec::with_capacity(num_vertices);

    for (group, face) in buffer.quads.groups.iter().zip(faces.iter()) {
        for quad in group.iter() {
            let color = voxel_color_for_quad(quad, voxels, shape);
            indices.extend_from_slice(&face.quad_mesh_indices(positions.len() as u32));
            positions.extend_from_slice(&face.quad_mesh_positions(quad, 1.0));
            normals.extend_from_slice(&face.quad_mesh_normals());
            tex_coords.extend_from_slice(&face.tex_coords(
                RIGHT_HANDED_Y_UP_CONFIG.u_flip_face,
                true,
                quad,
            ));
            colors.extend_from_slice(&[color; 4]);
        }
    }

    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    );
    mesh.try_insert_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .expect("valid position attribute");
    mesh.try_insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .expect("valid normal attribute");
    mesh.try_insert_attribute(Mesh::ATTRIBUTE_UV_0, tex_coords)
        .expect("valid uv attribute");
    mesh.try_insert_attribute(Mesh::ATTRIBUTE_COLOR, colors)
        .expect("valid color attribute");
    mesh.try_insert_indices(Indices::U32(indices))
        .expect("valid indices");
    Some(mesh)
}

/// Look up the material color for a greedy quad by reading the voxel at `quad.minimum`.
fn voxel_color_for_quad<S: Shape<3, Coord = u32>>(
    quad: &block_mesh::UnorientedQuad,
    voxels: &[WorldVoxel],
    shape: &S,
) -> [f32; 4] {
    let linear = shape.linearize(quad.minimum) as usize;
    let material = match voxels[linear] {
        WorldVoxel::Solid(material) => material,
        WorldVoxel::Air | WorldVoxel::Unset => {
            debug_assert!(false, "quad minimum voxel must be solid");
            0
        }
    };
    terrain_material_color(material)
}

/// Return the terrain material color as linear RGBA.
fn terrain_material_color(material: u8) -> [f32; 4] {
    match material {
        1 => srgb_color_to_linear(139, 90, 43, 255),
        2 => srgb_color_to_linear(128, 128, 128, 255),
        _ => srgb_color_to_linear(128, 179, 77, 255),
    }
}

/// Convert an sRGB u8 channel value to linear f32.
fn srgb_channel_to_linear(value: u8) -> f32 {
    let s = value as f32 / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

/// Convert sRGB u8 RGBA to linear f32 RGBA.
fn srgb_color_to_linear(r: u8, g: u8, b: u8, a: u8) -> [f32; 4] {
    [
        srgb_channel_to_linear(r),
        srgb_channel_to_linear(g),
        srgb_channel_to_linear(b),
        a as f32 / 255.0,
    ]
}

/// Generate voxels for flat terrain at y=0.
/// world_y <= 0 → Solid(0), world_y > 0 → Air.
pub fn flat_terrain_voxels<S: Shape<3, Coord = u32>>(
    chunk_pos: IVec3,
    chunk_size: u32,
    shape: &S,
) -> Vec<WorldVoxel> {
    let mut voxels = vec![WorldVoxel::Air; shape.usize()];
    for i in 0..shape.size() {
        let [_x, y, _z] = shape.delinearize(i);
        let world_y = chunk_pos.y * chunk_size as i32 + y as i32 - 1;
        if world_y <= 0 {
            voxels[i as usize] = WorldVoxel::Solid(0);
        }
    }
    voxels
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::mesh::VertexAttributeValues;
    use ndshape::RuntimeShape;

    fn padded_shape() -> RuntimeShape<u32, 3> {
        RuntimeShape::<u32, 3>::new([18, 18, 18])
    }

    #[test]
    fn flat_terrain_voxels_produces_mesh_at_surface() {
        let shape = padded_shape();
        let voxels = flat_terrain_voxels(IVec3::new(0, 0, 0), 16, &shape);
        let mesh = mesh_chunk_greedy(&voxels, &shape);
        assert!(
            mesh.is_some(),
            "y=0 chunk should contain a surface crossing"
        );
    }

    #[test]
    fn flat_terrain_voxels_no_mesh_for_underground() {
        let shape = padded_shape();
        let voxels = flat_terrain_voxels(IVec3::new(0, -2, 0), 16, &shape);
        let mesh = mesh_chunk_greedy(&voxels, &shape);
        assert!(
            mesh.is_none(),
            "fully underground chunk should produce no mesh"
        );
    }

    #[test]
    fn flat_terrain_voxels_no_mesh_for_sky() {
        let shape = padded_shape();
        let voxels = flat_terrain_voxels(IVec3::new(0, 2, 0), 16, &shape);
        let mesh = mesh_chunk_greedy(&voxels, &shape);
        assert!(
            mesh.is_none(),
            "fully above-surface chunk should produce no mesh"
        );
    }

    #[test]
    fn mesh_has_valid_attributes() {
        let shape = padded_shape();
        let voxels = flat_terrain_voxels(IVec3::new(0, 0, 0), 16, &shape);
        let mesh = mesh_chunk_greedy(&voxels, &shape).expect("should produce mesh");
        assert!(mesh.attribute(Mesh::ATTRIBUTE_POSITION).is_some());
        assert!(mesh.attribute(Mesh::ATTRIBUTE_NORMAL).is_some());
        assert!(mesh.attribute(Mesh::ATTRIBUTE_UV_0).is_some());
        assert!(mesh.attribute(Mesh::ATTRIBUTE_COLOR).is_some());
        assert!(mesh.indices().is_some());
    }

    #[test]
    fn terrain_material_indices_have_distinct_colors() {
        assert_eq!(
            terrain_material_color(1),
            srgb_color_to_linear(139, 90, 43, 255)
        );
        assert_eq!(
            terrain_material_color(2),
            srgb_color_to_linear(128, 128, 128, 255)
        );
        assert_ne!(terrain_material_color(1), terrain_material_color(2));
    }

    #[test]
    fn mesh_uses_vertex_color_for_solid_material() {
        let shape = RuntimeShape::<u32, 3>::new([4, 4, 4]);
        let mut voxels = vec![WorldVoxel::Air; shape.usize()];
        let solid_index = shape.linearize([1, 1, 1]) as usize;
        voxels[solid_index] = WorldVoxel::Solid(1);

        let mesh = mesh_chunk_greedy(&voxels, &shape).expect("solid voxel should produce mesh");
        let colors = mesh
            .attribute(Mesh::ATTRIBUTE_COLOR)
            .expect("mesh should have vertex colors");
        let VertexAttributeValues::Float32x4(colors) = colors else {
            panic!("terrain mesh colors should be Float32x4");
        };

        assert!(
            colors
                .iter()
                .all(|color| *color == terrain_material_color(1))
        );
    }
}
