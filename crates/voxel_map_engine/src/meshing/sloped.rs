use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use ndshape::Shape;

use crate::types::WorldVoxel;

const SENTINEL_NO_SURFACE: i32 = i32::MIN;

/// Internal mesh accumulator matching `mesh_chunk_greedy`'s vertex layout.
struct MeshBuffers {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    indices: Vec<u32>,
}

impl MeshBuffers {
    fn new() -> Self {
        Self {
            positions: vec![],
            normals: vec![],
            uvs: vec![],
            indices: vec![],
        }
    }

    fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    fn into_mesh(self) -> Mesh {
        let mut mesh = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::default(),
        );
        mesh.try_insert_attribute(Mesh::ATTRIBUTE_POSITION, self.positions)
            .expect("valid position attribute");
        mesh.try_insert_attribute(Mesh::ATTRIBUTE_NORMAL, self.normals)
            .expect("valid normal attribute");
        mesh.try_insert_attribute(Mesh::ATTRIBUTE_UV_0, self.uvs)
            .expect("valid uv attribute");
        mesh.try_insert_indices(Indices::U32(self.indices))
            .expect("valid indices");
        mesh
    }
}

/// Top-surface classification per column. Corners are top-face-y values
/// in order `[c00, c10, c01, c11]` for `(-x,-z), (+x,-z), (-x,+z), (+x,+z)`.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // `material` consumed in later phases
enum ColumnTop {
    /// Corners lie on the plane `y = gradient_x * x + gradient_z * z + intercept`.
    Planar {
        corners: [i32; 4],
        gradient_x: i32,
        gradient_z: i32,
        /// Plane y-intercept at mesh origin `(0, 0)`. Same value for every
        /// column sharing this plane — used as the greedy-merge signature.
        intercept: i32,
        material: u8,
    },
    /// Corners not coplanar. Phase 2 renders as cubic fallback; Phase 3 emits
    /// hip/valley triangles.
    NonPlanar { corners: [i32; 4], material: u8 },
}

fn column_corners(top: &ColumnTop) -> [i32; 4] {
    match *top {
        ColumnTop::Planar { corners, .. } => corners,
        ColumnTop::NonPlanar { corners, .. } => corners,
    }
}

/// Mesh a padded voxel array using corner-max sloped meshing.
pub fn mesh_chunk_sloped<S: Shape<3, Coord = u32>>(
    voxels: &[WorldVoxel],
    shape: &S,
) -> Option<Mesh> {
    debug_assert_eq!(voxels.len(), shape.usize());
    let dims = shape.as_array();
    let padded_xz = dims[0] as usize;
    debug_assert_eq!(dims[0], dims[2], "padded XZ must be square");

    let heights = column_heights(voxels, shape);
    let materials = column_materials(voxels, shape);
    let tops = classify_columns(&heights, &materials, padded_xz);

    let mut buf = MeshBuffers::new();
    emit_planar_tops(&tops, padded_xz, &mut buf);
    emit_nonplanar_tops_cubic_fallback(&tops, padded_xz, &mut buf);
    emit_cubic_walls_simple(&tops, padded_xz, &mut buf);
    emit_bottom_faces(voxels, shape, &mut buf);

    if buf.is_empty() {
        None
    } else {
        Some(buf.into_mesh())
    }
}

/// Pass 1: scan each padded XZ column for the top-face-y of the highest
/// interior solid voxel whose above neighbor is non-solid. Returns
/// `SENTINEL_NO_SURFACE` when no such voxel exists.
fn column_heights<S: Shape<3, Coord = u32>>(voxels: &[WorldVoxel], shape: &S) -> Vec<i32> {
    let dims = shape.as_array();
    let (sx, sy, sz) = (dims[0], dims[1], dims[2]);
    let mut heights = vec![SENTINEL_NO_SURFACE; (sx * sz) as usize];
    for px in 0..sx {
        for pz in 0..sz {
            for py in (1..=sy - 2).rev() {
                let idx = shape.linearize([px, py, pz]) as usize;
                if !matches!(voxels[idx], WorldVoxel::Solid(_)) {
                    continue;
                }
                let above_idx = shape.linearize([px, py + 1, pz]) as usize;
                if matches!(voxels[above_idx], WorldVoxel::Solid(_)) {
                    break;
                }
                heights[(px * sz + pz) as usize] = (py + 1) as i32;
                break;
            }
        }
    }
    heights
}

/// Pass 1b: material byte of each column's top voxel. Zero when no top surface.
fn column_materials<S: Shape<3, Coord = u32>>(voxels: &[WorldVoxel], shape: &S) -> Vec<u8> {
    let dims = shape.as_array();
    let (sx, sy, sz) = (dims[0], dims[1], dims[2]);
    let mut materials = vec![0u8; (sx * sz) as usize];
    for px in 0..sx {
        for pz in 0..sz {
            for py in (1..=sy - 2).rev() {
                let idx = shape.linearize([px, py, pz]) as usize;
                let WorldVoxel::Solid(m) = voxels[idx] else {
                    continue;
                };
                let above_idx = shape.linearize([px, py + 1, pz]) as usize;
                if matches!(voxels[above_idx], WorldVoxel::Solid(_)) {
                    break;
                }
                materials[(px * sz + pz) as usize] = m;
                break;
            }
        }
    }
    materials
}

/// Pass 2: classify each interior column by corner coplanarity.
fn classify_columns(heights: &[i32], materials: &[u8], padded_xz: usize) -> Vec<Option<ColumnTop>> {
    let mut out = vec![None; padded_xz * padded_xz];
    for px in 1..padded_xz - 1 {
        for pz in 1..padded_xz - 1 {
            let h = heights[px * padded_xz + pz];
            if h == SENTINEL_NO_SURFACE {
                continue;
            }
            let corners = compute_corners(heights, padded_xz, px, pz, h);
            let material = materials[px * padded_xz + pz];
            out[px * padded_xz + pz] = Some(match plane_fit(corners, px as i32, pz as i32) {
                Some((gradient_x, gradient_z, intercept)) => ColumnTop::Planar {
                    corners,
                    gradient_x,
                    gradient_z,
                    intercept,
                    material,
                },
                None => ColumnTop::NonPlanar { corners, material },
            });
        }
    }
    out
}

/// Compute 4 corner top-face-y values for interior column at `(px, pz)` via the
/// corner-max rule. SENTINEL neighbors mark absent terrain → treat as cliff.
fn compute_corners(
    heights: &[i32],
    padded_xz: usize,
    px: usize,
    pz: usize,
    own_h: i32,
) -> [i32; 4] {
    let h = |x: usize, z: usize| -> i32 { heights[x * padded_xz + z] };
    let c00 = corner_rule(
        [h(px - 1, pz - 1), h(px, pz - 1), h(px - 1, pz), own_h],
        own_h,
    );
    let c10 = corner_rule(
        [h(px, pz - 1), h(px + 1, pz - 1), own_h, h(px + 1, pz)],
        own_h,
    );
    let c01 = corner_rule(
        [h(px - 1, pz), own_h, h(px - 1, pz + 1), h(px, pz + 1)],
        own_h,
    );
    let c11 = corner_rule(
        [own_h, h(px + 1, pz), h(px, pz + 1), h(px + 1, pz + 1)],
        own_h,
    );
    [c00, c10, c01, c11]
}

fn corner_rule(values: [i32; 4], own_h: i32) -> i32 {
    if values.iter().any(|&v| v == SENTINEL_NO_SURFACE) {
        return own_h;
    }
    let max = *values.iter().max().unwrap();
    let min = *values.iter().min().unwrap();
    match max - min {
        0 => own_h,
        1 => max,
        _ => own_h,
    }
}

/// Returns `Some((gx, gz, intercept_at_origin))` if the 4 corners are coplanar.
/// Intercept is the plane's y-value at mesh origin `(0, 0)` — shared by every
/// column on the same plane, so it can key greedy merging.
fn plane_fit(corners: [i32; 4], px: i32, pz: i32) -> Option<(i32, i32, i32)> {
    let [c00, c10, c01, c11] = corners;
    let gx = c10 - c00;
    let gz = c01 - c00;
    if c11 - c00 != gx + gz {
        return None;
    }
    Some((gx, gz, c00 - gx * px - gz * pz))
}

/// Pass 3a: 2D greedy rectangle sweep over `Planar` columns keyed on
/// `(material, gradient_x, gradient_z, intercept)`. Extends `+x` then `+z`.
fn emit_planar_tops(tops: &[Option<ColumnTop>], padded_xz: usize, out: &mut MeshBuffers) {
    let mut visited = vec![false; padded_xz * padded_xz];
    for px in 1..padded_xz - 1 {
        for pz in 1..padded_xz - 1 {
            if visited[px * padded_xz + pz] {
                continue;
            }
            let Some(ColumnTop::Planar {
                gradient_x,
                gradient_z,
                intercept,
                material,
                ..
            }) = tops[px * padded_xz + pz]
            else {
                continue;
            };
            let sig = (material, gradient_x, gradient_z, intercept);

            let mut px_end = px;
            while px_end + 1 < padded_xz - 1 {
                let next = px_end + 1;
                if visited[next * padded_xz + pz] {
                    break;
                }
                if !planar_sig_matches(&tops[next * padded_xz + pz], sig) {
                    break;
                }
                px_end = next;
            }

            let mut pz_end = pz;
            'z_ext: while pz_end + 1 < padded_xz - 1 {
                let next_z = pz_end + 1;
                for xi in px..=px_end {
                    if visited[xi * padded_xz + next_z] {
                        break 'z_ext;
                    }
                    if !planar_sig_matches(&tops[xi * padded_xz + next_z], sig) {
                        break 'z_ext;
                    }
                }
                pz_end = next_z;
            }

            for xi in px..=px_end {
                for zi in pz..=pz_end {
                    visited[xi * padded_xz + zi] = true;
                }
            }

            emit_planar_rect(
                out, px, pz, px_end, pz_end, gradient_x, gradient_z, intercept,
            );
        }
    }
}

fn planar_sig_matches(top: &Option<ColumnTop>, sig: (u8, i32, i32, i32)) -> bool {
    matches!(
        top,
        Some(ColumnTop::Planar {
            gradient_x,
            gradient_z,
            intercept,
            material,
            ..
        }) if (*material, *gradient_x, *gradient_z, *intercept) == sig
    )
}

fn emit_planar_rect(
    out: &mut MeshBuffers,
    px_min: usize,
    pz_min: usize,
    px_max: usize,
    pz_max: usize,
    gx: i32,
    gz: i32,
    intercept: i32,
) {
    let (x1, z1) = (px_min as i32, pz_min as i32);
    let (x2, z2) = ((px_max + 1) as i32, (pz_max + 1) as i32);
    let y00 = gx * x1 + gz * z1 + intercept;
    let y10 = gx * x2 + gz * z1 + intercept;
    let y01 = gx * x1 + gz * z2 + intercept;
    let y11 = gx * x2 + gz * z2 + intercept;
    let p00 = [x1 as f32, y00 as f32, z1 as f32];
    let p10 = [x2 as f32, y10 as f32, z1 as f32];
    let p11 = [x2 as f32, y11 as f32, z2 as f32];
    let p01 = [x1 as f32, y01 as f32, z2 as f32];
    let normal = Vec3::new(-(gx as f32), 1.0, -(gz as f32))
        .normalize_or_zero()
        .to_array();
    let base = out.positions.len() as u32;
    out.positions.extend_from_slice(&[p00, p10, p11, p01]);
    out.normals.extend_from_slice(&[normal; 4]);
    out.uvs.extend_from_slice(&[
        [x1 as f32, z1 as f32],
        [x2 as f32, z1 as f32],
        [x2 as f32, z2 as f32],
        [x1 as f32, z2 as f32],
    ]);
    out.indices
        .extend_from_slice(&[base, base + 2, base + 1, base, base + 3, base + 2]);
}

/// Phase-2 fallback for `NonPlanar` columns: emit a flat cubic top at the
/// max corner height. Replaced by hip/valley triangles in Phase 3.
fn emit_nonplanar_tops_cubic_fallback(
    tops: &[Option<ColumnTop>],
    padded_xz: usize,
    out: &mut MeshBuffers,
) {
    for px in 1..padded_xz - 1 {
        for pz in 1..padded_xz - 1 {
            let Some(ColumnTop::NonPlanar { corners, .. }) = tops[px * padded_xz + pz] else {
                continue;
            };
            let y = *corners.iter().max().unwrap() as f32;
            let (x1, z1) = (px as f32, pz as f32);
            let (x2, z2) = (x1 + 1.0, z1 + 1.0);
            let p00 = [x1, y, z1];
            let p10 = [x2, y, z1];
            let p11 = [x2, y, z2];
            let p01 = [x1, y, z2];
            let normal = [0.0, 1.0, 0.0];
            let base = out.positions.len() as u32;
            out.positions.extend_from_slice(&[p00, p10, p11, p01]);
            out.normals.extend_from_slice(&[normal; 4]);
            out.uvs
                .extend_from_slice(&[[x1, z1], [x2, z1], [x2, z2], [x1, z2]]);
            out.indices
                .extend_from_slice(&[base, base + 2, base + 1, base, base + 3, base + 2]);
        }
    }
}

/// Phase-2 placeholder wall emitter. For each shared edge between interior
/// columns, if the 4 corner y-values along the edge disagree, emit an
/// axis-aligned wall rectangle spanning `[min, max]` of those corners.
/// Saddle edges (mixed direction) are approximated by a single rect —
/// Phase 4 replaces with a proper split.
fn emit_cubic_walls_simple(tops: &[Option<ColumnTop>], padded_xz: usize, out: &mut MeshBuffers) {
    for pz in 1..padded_xz - 1 {
        for px in 1..padded_xz - 2 {
            let Some(a) = &tops[px * padded_xz + pz] else {
                continue;
            };
            let Some(b) = &tops[(px + 1) * padded_xz + pz] else {
                continue;
            };
            let ac = column_corners(a);
            let bc = column_corners(b);
            let a_edge = (ac[1], ac[3]);
            let b_edge = (bc[0], bc[2]);
            if a_edge == b_edge {
                continue;
            }
            let vals = [a_edge.0, a_edge.1, b_edge.0, b_edge.1];
            let y_lo = *vals.iter().min().unwrap() as f32;
            let y_hi = *vals.iter().max().unwrap() as f32;
            let a_max = a_edge.0.max(a_edge.1);
            let b_max = b_edge.0.max(b_edge.1);
            let x = (px + 1) as f32;
            let (z1, z2) = (pz as f32, (pz + 1) as f32);
            emit_wall_x(out, x, y_lo, y_hi, z1, z2, a_max >= b_max);
        }
    }
    for px in 1..padded_xz - 1 {
        for pz in 1..padded_xz - 2 {
            let Some(a) = &tops[px * padded_xz + pz] else {
                continue;
            };
            let Some(b) = &tops[px * padded_xz + pz + 1] else {
                continue;
            };
            let ac = column_corners(a);
            let bc = column_corners(b);
            let a_edge = (ac[2], ac[3]);
            let b_edge = (bc[0], bc[1]);
            if a_edge == b_edge {
                continue;
            }
            let vals = [a_edge.0, a_edge.1, b_edge.0, b_edge.1];
            let y_lo = *vals.iter().min().unwrap() as f32;
            let y_hi = *vals.iter().max().unwrap() as f32;
            let a_max = a_edge.0.max(a_edge.1);
            let b_max = b_edge.0.max(b_edge.1);
            let z = (pz + 1) as f32;
            let (x1, x2) = (px as f32, (px + 1) as f32);
            emit_wall_z(out, z, y_lo, y_hi, x1, x2, a_max >= b_max);
        }
    }
}

fn emit_wall_x(
    out: &mut MeshBuffers,
    x: f32,
    y_lo: f32,
    y_hi: f32,
    z1: f32,
    z2: f32,
    normal_positive: bool,
) {
    if y_hi <= y_lo {
        return;
    }
    let p0 = [x, y_lo, z1];
    let p1 = [x, y_hi, z1];
    let p2 = [x, y_lo, z2];
    let p3 = [x, y_hi, z2];
    let normal = if normal_positive {
        [1.0, 0.0, 0.0]
    } else {
        [-1.0, 0.0, 0.0]
    };
    let base = out.positions.len() as u32;
    out.positions.extend_from_slice(&[p0, p1, p2, p3]);
    out.normals.extend_from_slice(&[normal; 4]);
    out.uvs
        .extend_from_slice(&[[z1, y_lo], [z1, y_hi], [z2, y_lo], [z2, y_hi]]);
    if normal_positive {
        out.indices
            .extend_from_slice(&[base, base + 1, base + 2, base + 1, base + 3, base + 2]);
    } else {
        out.indices
            .extend_from_slice(&[base, base + 2, base + 1, base + 1, base + 2, base + 3]);
    }
}

fn emit_wall_z(
    out: &mut MeshBuffers,
    z: f32,
    y_lo: f32,
    y_hi: f32,
    x1: f32,
    x2: f32,
    normal_positive: bool,
) {
    if y_hi <= y_lo {
        return;
    }
    let p0 = [x1, y_lo, z];
    let p1 = [x1, y_hi, z];
    let p2 = [x2, y_lo, z];
    let p3 = [x2, y_hi, z];
    let normal = if normal_positive {
        [0.0, 0.0, 1.0]
    } else {
        [0.0, 0.0, -1.0]
    };
    let base = out.positions.len() as u32;
    out.positions.extend_from_slice(&[p0, p1, p2, p3]);
    out.normals.extend_from_slice(&[normal; 4]);
    out.uvs
        .extend_from_slice(&[[x1, y_lo], [x1, y_hi], [x2, y_lo], [x2, y_hi]]);
    if normal_positive {
        out.indices
            .extend_from_slice(&[base, base + 2, base + 1, base + 2, base + 3, base + 1]);
    } else {
        out.indices
            .extend_from_slice(&[base, base + 1, base + 2, base + 1, base + 3, base + 2]);
    }
}

/// Emit per-voxel cubic bottom faces where a solid voxel sits above an
/// air/unset voxel. Naive — greedy merging deferred.
fn emit_bottom_faces<S: Shape<3, Coord = u32>>(
    voxels: &[WorldVoxel],
    shape: &S,
    out: &mut MeshBuffers,
) {
    let dims = shape.as_array();
    let (sx, sy, sz) = (dims[0], dims[1], dims[2]);
    for px in 1..sx - 1 {
        for py in 1..sy - 1 {
            for pz in 1..sz - 1 {
                let idx = shape.linearize([px, py, pz]) as usize;
                if !matches!(voxels[idx], WorldVoxel::Solid(_)) {
                    continue;
                }
                let below_idx = shape.linearize([px, py - 1, pz]) as usize;
                if matches!(voxels[below_idx], WorldVoxel::Solid(_)) {
                    continue;
                }
                let y = py as f32;
                let (x1, z1) = (px as f32, pz as f32);
                let (x2, z2) = (x1 + 1.0, z1 + 1.0);
                let p0 = [x1, y, z1];
                let p1 = [x2, y, z1];
                let p2 = [x2, y, z2];
                let p3 = [x1, y, z2];
                let normal = [0.0, -1.0, 0.0];
                let base = out.positions.len() as u32;
                out.positions.extend_from_slice(&[p0, p1, p2, p3]);
                out.normals.extend_from_slice(&[normal; 4]);
                out.uvs
                    .extend_from_slice(&[[x1, z1], [x2, z1], [x2, z2], [x1, z2]]);
                out.indices.extend_from_slice(&[
                    base,
                    base + 1,
                    base + 2,
                    base + 2,
                    base + 3,
                    base,
                ]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndshape::RuntimeShape;

    fn padded_shape() -> RuntimeShape<u32, 3> {
        RuntimeShape::<u32, 3>::new([18, 18, 18])
    }

    /// Build a padded voxel array for a single chunk at `(0, 0, 0)` where
    /// column `(world_x, world_z)` is solid for `world_y <= f(world_x, world_z)`.
    fn heightmap_voxels<F: Fn(i32, i32) -> i32>(f: F) -> Vec<WorldVoxel> {
        let shape = padded_shape();
        let mut voxels = vec![WorldVoxel::Air; shape.usize()];
        for i in 0..shape.size() {
            let [px, py, pz] = shape.delinearize(i);
            let world_x = px as i32 - 1;
            let world_y = py as i32 - 1;
            let world_z = pz as i32 - 1;
            if world_y <= f(world_x, world_z) {
                voxels[i as usize] = WorldVoxel::Solid(0);
            }
        }
        voxels
    }

    #[test]
    fn staircase_emits_planar_slopes() {
        let voxels = heightmap_voxels(|x, _| x.clamp(0, 7));
        let shape = padded_shape();
        let mesh = mesh_chunk_sloped(&voxels, &shape).expect("mesh");
        let normals = mesh
            .attribute(Mesh::ATTRIBUTE_NORMAL)
            .unwrap()
            .as_float3()
            .unwrap();
        let tilted = normals
            .iter()
            .filter(|n| n[0].abs() > 0.1 && n[1].abs() > 0.1)
            .count();
        assert!(tilted > 0, "staircase should produce tilted top normals");
    }

    #[test]
    fn flat_region_merges_to_single_quad() {
        let voxels = heightmap_voxels(|_, _| 5);
        let shape = padded_shape();
        let mesh = mesh_chunk_sloped(&voxels, &shape).expect("mesh");
        let positions = mesh
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .unwrap()
            .as_float3()
            .unwrap();
        assert!(
            positions.len() < 1024,
            "flat top should greedy-merge (got {} positions)",
            positions.len()
        );
    }
}
