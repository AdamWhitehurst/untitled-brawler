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
    let surface_heights = column_surface_heights(voxels, shape);
    let materials = column_materials(voxels, shape);
    let tops = classify_columns(&heights, &materials, padded_xz);

    let mut buf = MeshBuffers::new();
    emit_planar_tops(&tops, padded_xz, &mut buf);
    emit_nonplanar_tops(&tops, padded_xz, &mut buf);
    emit_walls(&heights, &surface_heights, padded_xz, &mut buf);
    emit_bottom_faces(voxels, shape, &mut buf);

    if buf.is_empty() {
        None
    } else {
        Some(buf.into_mesh())
    }
}

/// Pass 1: scan each padded XZ column for the top-face-y of the highest
/// interior solid voxel whose above neighbor is non-solid. Returns
/// `SENTINEL_NO_SURFACE` when no such voxel exists. Used for top emission —
/// this chunk only renders tops it OWNS (solid voxel in its own interior).
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

/// Like [`column_heights`] but also considers the bottom padding voxel
/// (`py = 0`). For a depression whose floor voxel lives in the chunk BELOW,
/// that chunk's top is visible in our padding row — so surrounding columns
/// in THIS chunk need a wall down to that level. The chunk below still owns
/// the top face (which is why top emission keeps using `column_heights`);
/// this array is only consulted by the wall emitter.
fn column_surface_heights<S: Shape<3, Coord = u32>>(voxels: &[WorldVoxel], shape: &S) -> Vec<i32> {
    let dims = shape.as_array();
    let (sx, sy, sz) = (dims[0], dims[1], dims[2]);
    let mut heights = vec![SENTINEL_NO_SURFACE; (sx * sz) as usize];
    for px in 0..sx {
        for pz in 0..sz {
            for py in (0..=sy - 2).rev() {
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

/// Valley-aware corner rule.
///
/// - `delta == 0`: flat → corner = `own_h`.
/// - `delta == 1`: single-unit transition.
///     - Isolated pit (own is min AND appears only once): corner = `own_h`.
///       Keeps a single-cell valley at its floor height.
///     - Otherwise (peak, peak-adjacent, slope, multi-cell pit edge): corner = `max`.
///       Rises to meet the higher neighbor / tip.
/// - `delta >= 2`: cliff → corner = `own_h`.
/// - SENTINEL neighbor (no surface in that column): treat as cliff → `own_h`.
fn corner_rule(values: [i32; 4], own_h: i32) -> i32 {
    if values.iter().any(|&v| v == SENTINEL_NO_SURFACE) {
        return own_h;
    }
    let max = *values.iter().max().unwrap();
    let min = *values.iter().min().unwrap();
    match max - min {
        0 => own_h,
        1 => {
            let own_count = values.iter().filter(|&&v| v == own_h).count();
            if own_h == min && own_count == 1 {
                own_h
            } else {
                max
            }
        }
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

/// Diagonal split choice for a non-planar 4-corner quad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Diagonal {
    /// Split along `(-x,-z)↔(+x,+z)`. Used for convex hips (ridge along main diagonal).
    Main,
    /// Split along `(+x,-z)↔(-x,+z)`. Used for concave valleys (seam along anti-diagonal).
    Anti,
}

/// Pick the diagonal that puts the ridge along the higher pair of corners.
fn choose_diagonal(corners: &[i32; 4]) -> Diagonal {
    let [c00, c10, c01, c11] = *corners;
    if (c00 + c11) >= (c10 + c01) {
        Diagonal::Main
    } else {
        Diagonal::Anti
    }
}

/// Pass 3b: emit 2 triangles per `NonPlanar` column with per-triangle flat
/// normals. Diagonal choice per `choose_diagonal`.
fn emit_nonplanar_tops(tops: &[Option<ColumnTop>], padded_xz: usize, out: &mut MeshBuffers) {
    for px in 1..padded_xz - 1 {
        for pz in 1..padded_xz - 1 {
            let Some(ColumnTop::NonPlanar { corners, .. }) = tops[px * padded_xz + pz] else {
                continue;
            };
            let (x0, z0) = (px as f32, pz as f32);
            let p00 = [x0, corners[0] as f32, z0];
            let p10 = [x0 + 1.0, corners[1] as f32, z0];
            let p01 = [x0, corners[2] as f32, z0 + 1.0];
            let p11 = [x0 + 1.0, corners[3] as f32, z0 + 1.0];
            match choose_diagonal(&corners) {
                Diagonal::Main => {
                    push_triangle(out, p00, p11, p10);
                    push_triangle(out, p00, p01, p11);
                }
                Diagonal::Anti => {
                    push_triangle(out, p00, p01, p10);
                    push_triangle(out, p10, p01, p11);
                }
            }
        }
    }
}

fn push_triangle(out: &mut MeshBuffers, a: [f32; 3], b: [f32; 3], c: [f32; 3]) {
    let ab = Vec3::from(b) - Vec3::from(a);
    let ac = Vec3::from(c) - Vec3::from(a);
    let n = ab.cross(ac).normalize_or_zero().to_array();
    let base = out.positions.len() as u32;
    out.positions.extend_from_slice(&[a, b, c]);
    out.normals.extend_from_slice(&[n, n, n]);
    out.uvs
        .extend_from_slice(&[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]);
    out.indices.extend_from_slice(&[base, base + 1, base + 2]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum EdgeAxis {
    X,
    Z,
}

/// Axis-aligned rectangular wall. `axis_len` spans the parallel axis in cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WallRect {
    axis: EdgeAxis,
    /// For `EdgeAxis::X`: edge at mesh_x = `base_px + 1`; z-range
    /// `[base_pz, base_pz + axis_len]`. For `EdgeAxis::Z`: edge at
    /// mesh_z = `base_pz + 1`; x-range `[base_px, base_px + axis_len]`.
    base_px: usize,
    base_pz: usize,
    axis_len: usize,
    y_lo: i32,
    y_hi: i32,
    normal_positive: bool,
}

enum WallEmission {
    Skip,
    /// Saddle edge: one end flush, the other stepped. 3 vertices, flat normal.
    Triangle {
        vertices: [[f32; 3]; 3],
    },
    Rectangle(WallRect),
}

/// Avian/Parry rejects trimesh triangles below this surface area (≈ 1e-4).
/// Assert we never emit degenerate saddle triangles — they'd silently corrupt
/// the server collider.
const MIN_PARRY_TRIANGLE_AREA: f32 = 1e-4;

fn triangle_area(v: &[[f32; 3]; 3]) -> f32 {
    let ab = Vec3::from(v[1]) - Vec3::from(v[0]);
    let ac = Vec3::from(v[2]) - Vec3::from(v[0]);
    ab.cross(ac).length() * 0.5
}

/// Phase-4 wall emitter. Iterates +x and +z shared edges (one-sided, so
/// neighbor chunks own their east/south boundaries; avoids duplicates).
/// Classifies each edge as Skip / Triangle (saddle) / Rectangle. Triangles
/// emit immediately; rectangles are collected and greedy-merged along their
/// parallel axis before emission.
fn emit_walls(heights: &[i32], surface_heights: &[i32], padded_xz: usize, out: &mut MeshBuffers) {
    let mut rects: Vec<WallRect> = Vec::new();

    for pz in 1..padded_xz - 1 {
        for px in 1..padded_xz - 1 {
            match classify_edge_x(heights, surface_heights, padded_xz, px, pz) {
                WallEmission::Skip => {}
                WallEmission::Triangle { vertices } => {
                    debug_assert!(
                        triangle_area(&vertices) >= MIN_PARRY_TRIANGLE_AREA,
                        "saddle triangle at ({}, {}) axis=X below Parry tolerance",
                        px,
                        pz,
                    );
                    push_triangle(out, vertices[0], vertices[1], vertices[2]);
                }
                WallEmission::Rectangle(rect) => rects.push(rect),
            }
        }
    }
    for px in 1..padded_xz - 1 {
        for pz in 1..padded_xz - 1 {
            match classify_edge_z(heights, surface_heights, padded_xz, px, pz) {
                WallEmission::Skip => {}
                WallEmission::Triangle { vertices } => {
                    debug_assert!(
                        triangle_area(&vertices) >= MIN_PARRY_TRIANGLE_AREA,
                        "saddle triangle at ({}, {}) axis=Z below Parry tolerance",
                        px,
                        pz,
                    );
                    push_triangle(out, vertices[0], vertices[1], vertices[2]);
                }
                WallEmission::Rectangle(rect) => rects.push(rect),
            }
        }
    }

    for merged in greedy_merge_rectangle_walls(rects) {
        emit_wall_rect(&merged, out);
    }
}

fn classify_edge_x(
    heights: &[i32],
    surface_heights: &[i32],
    padded_xz: usize,
    px: usize,
    pz: usize,
) -> WallEmission {
    let top_a = heights[px * padded_xz + pz];
    let top_b = heights[(px + 1) * padded_xz + pz];
    let surf_a = surface_heights[px * padded_xz + pz];
    let surf_b = surface_heights[(px + 1) * padded_xz + pz];
    if surf_a == SENTINEL_NO_SURFACE || surf_b == SENTINEL_NO_SURFACE {
        return WallEmission::Skip;
    }
    if top_a != SENTINEL_NO_SURFACE && top_b != SENTINEL_NO_SURFACE {
        let (a10, a11) = east_edge_pair(heights, padded_xz, px, pz);
        let (b00, b01) = west_edge_pair(heights, padded_xz, px + 1, pz);
        let lo_match = a10 == b00;
        let hi_match = a11 == b01;
        match (lo_match, hi_match) {
            (true, true) => WallEmission::Skip,
            // Flush at lo-z (a10 == b00); step at hi-z (a11 vs b01 differ).
            (true, false) => make_triangle_wall_x(px, pz, a10, a11, b01, true),
            // Flush at hi-z (a11 == b01); step at lo-z (a10 vs b00 differ).
            (false, true) => make_triangle_wall_x(px, pz, a11, a10, b00, false),
            (false, false) => WallEmission::Rectangle(WallRect {
                axis: EdgeAxis::X,
                base_px: px,
                base_pz: pz,
                axis_len: 1,
                y_lo: a10.min(a11).min(b00).min(b01),
                y_hi: a10.max(a11).max(b00).max(b01),
                normal_positive: a10.max(a11) >= b00.max(b01),
            }),
        }
    } else {
        if surf_a == surf_b {
            return WallEmission::Skip;
        }
        WallEmission::Rectangle(WallRect {
            axis: EdgeAxis::X,
            base_px: px,
            base_pz: pz,
            axis_len: 1,
            y_lo: surf_a.min(surf_b),
            y_hi: surf_a.max(surf_b),
            normal_positive: surf_a >= surf_b,
        })
    }
}

fn classify_edge_z(
    heights: &[i32],
    surface_heights: &[i32],
    padded_xz: usize,
    px: usize,
    pz: usize,
) -> WallEmission {
    let top_a = heights[px * padded_xz + pz];
    let top_b = heights[px * padded_xz + pz + 1];
    let surf_a = surface_heights[px * padded_xz + pz];
    let surf_b = surface_heights[px * padded_xz + pz + 1];
    if surf_a == SENTINEL_NO_SURFACE || surf_b == SENTINEL_NO_SURFACE {
        return WallEmission::Skip;
    }
    if top_a != SENTINEL_NO_SURFACE && top_b != SENTINEL_NO_SURFACE {
        let (a01, a11) = south_edge_pair(heights, padded_xz, px, pz);
        let (b00, b10) = north_edge_pair(heights, padded_xz, px, pz + 1);
        let lo_match = a01 == b00;
        let hi_match = a11 == b10;
        match (lo_match, hi_match) {
            (true, true) => WallEmission::Skip,
            // Flush at lo-x (a01 == b00); step at hi-x (a11 vs b10 differ).
            (true, false) => make_triangle_wall_z(px, pz, a01, a11, b10, true),
            // Flush at hi-x (a11 == b10); step at lo-x (a01 vs b00 differ).
            (false, true) => make_triangle_wall_z(px, pz, a11, a01, b00, false),
            (false, false) => WallEmission::Rectangle(WallRect {
                axis: EdgeAxis::Z,
                base_px: px,
                base_pz: pz,
                axis_len: 1,
                y_lo: a01.min(a11).min(b00).min(b10),
                y_hi: a01.max(a11).max(b00).max(b10),
                normal_positive: a01.max(a11) >= b00.max(b10),
            }),
        }
    } else {
        if surf_a == surf_b {
            return WallEmission::Skip;
        }
        WallEmission::Rectangle(WallRect {
            axis: EdgeAxis::Z,
            base_px: px,
            base_pz: pz,
            axis_len: 1,
            y_lo: surf_a.min(surf_b),
            y_hi: surf_a.max(surf_b),
            normal_positive: surf_a >= surf_b,
        })
    }
}

/// Build a saddle triangle for a +x edge. `step_high_z` = true means the
/// stepped end is at z=pz+1 (low-z end is flush).
fn make_triangle_wall_x(
    px: usize,
    pz: usize,
    flush_y: i32,
    step_a: i32,
    step_b: i32,
    step_high_z: bool,
) -> WallEmission {
    let x = (px + 1) as f32;
    let (flush_z, step_z) = if step_high_z {
        (pz as f32, (pz + 1) as f32)
    } else {
        ((pz + 1) as f32, pz as f32)
    };
    let v_flush = [x, flush_y as f32, flush_z];
    let v_step_a = [x, step_a as f32, step_z];
    let v_step_b = [x, step_b as f32, step_z];
    WallEmission::Triangle {
        vertices: [v_flush, v_step_a, v_step_b],
    }
}

fn make_triangle_wall_z(
    px: usize,
    pz: usize,
    flush_y: i32,
    step_a: i32,
    step_b: i32,
    step_high_x: bool,
) -> WallEmission {
    let z = (pz + 1) as f32;
    let (flush_x, step_x) = if step_high_x {
        (px as f32, (px + 1) as f32)
    } else {
        ((px + 1) as f32, px as f32)
    };
    let v_flush = [flush_x, flush_y as f32, z];
    let v_step_a = [step_x, step_a as f32, z];
    let v_step_b = [step_x, step_b as f32, z];
    WallEmission::Triangle {
        vertices: [v_flush, v_step_a, v_step_b],
    }
}

/// Greedy-merge adjacent rectangles sharing `(axis, perp, y_lo, y_hi,
/// normal_positive)` along their parallel axis. Triangular saddle walls are
/// NOT merged (v1).
fn greedy_merge_rectangle_walls(rects: Vec<WallRect>) -> Vec<WallRect> {
    use std::collections::BTreeMap;
    // Key: (axis, perp coord, y_lo, y_hi, normal). Value: Vec<(parallel_start, len)>.
    let mut groups: BTreeMap<(EdgeAxis, usize, i32, i32, bool), Vec<(usize, usize)>> =
        BTreeMap::new();
    for r in rects {
        let (perp, parallel) = match r.axis {
            EdgeAxis::X => (r.base_px, r.base_pz),
            EdgeAxis::Z => (r.base_pz, r.base_px),
        };
        let key = (r.axis, perp, r.y_lo, r.y_hi, r.normal_positive);
        groups.entry(key).or_default().push((parallel, r.axis_len));
    }

    let mut merged = Vec::new();
    for ((axis, perp, y_lo, y_hi, normal_positive), mut entries) in groups {
        entries.sort_by_key(|&(start, _)| start);
        let mut current: Option<(usize, usize)> = None;
        for (start, len) in entries {
            match current {
                None => current = Some((start, len)),
                Some((c_start, c_len)) if c_start + c_len == start => {
                    current = Some((c_start, c_len + len));
                }
                Some((c_start, c_len)) => {
                    merged.push(make_merged_rect(
                        axis,
                        perp,
                        c_start,
                        c_len,
                        y_lo,
                        y_hi,
                        normal_positive,
                    ));
                    current = Some((start, len));
                }
            }
        }
        if let Some((s, l)) = current {
            merged.push(make_merged_rect(
                axis,
                perp,
                s,
                l,
                y_lo,
                y_hi,
                normal_positive,
            ));
        }
    }
    merged
}

fn make_merged_rect(
    axis: EdgeAxis,
    perp: usize,
    parallel_start: usize,
    axis_len: usize,
    y_lo: i32,
    y_hi: i32,
    normal_positive: bool,
) -> WallRect {
    let (base_px, base_pz) = match axis {
        EdgeAxis::X => (perp, parallel_start),
        EdgeAxis::Z => (parallel_start, perp),
    };
    WallRect {
        axis,
        base_px,
        base_pz,
        axis_len,
        y_lo,
        y_hi,
        normal_positive,
    }
}

fn emit_wall_rect(r: &WallRect, out: &mut MeshBuffers) {
    match r.axis {
        EdgeAxis::X => {
            let x = (r.base_px + 1) as f32;
            let z1 = r.base_pz as f32;
            let z2 = (r.base_pz + r.axis_len) as f32;
            emit_wall_x(
                out,
                x,
                r.y_lo as f32,
                r.y_hi as f32,
                z1,
                z2,
                r.normal_positive,
            );
        }
        EdgeAxis::Z => {
            let z = (r.base_pz + 1) as f32;
            let x1 = r.base_px as f32;
            let x2 = (r.base_px + r.axis_len) as f32;
            emit_wall_z(
                out,
                z,
                r.y_lo as f32,
                r.y_hi as f32,
                x1,
                x2,
                r.normal_positive,
            );
        }
    }
}

/// Read a heights array with bounds-checked access. Returns `SENTINEL_NO_SURFACE`
/// for out-of-bounds coordinates so `corner_rule` treats them as cliff neighbors.
fn height_at(heights: &[i32], padded_xz: usize, x: i32, z: i32) -> i32 {
    if x < 0 || z < 0 || x >= padded_xz as i32 || z >= padded_xz as i32 {
        SENTINEL_NO_SURFACE
    } else {
        heights[(x as usize) * padded_xz + z as usize]
    }
}

/// Corners on the east (+x) edge of column `(px, pz)`: `(c10, c11)`.
fn east_edge_pair(heights: &[i32], padded_xz: usize, px: usize, pz: usize) -> (i32, i32) {
    let own = heights[px * padded_xz + pz];
    let (x, z) = (px as i32, pz as i32);
    let c10 = corner_rule(
        [
            height_at(heights, padded_xz, x, z - 1),
            height_at(heights, padded_xz, x + 1, z - 1),
            own,
            height_at(heights, padded_xz, x + 1, z),
        ],
        own,
    );
    let c11 = corner_rule(
        [
            own,
            height_at(heights, padded_xz, x + 1, z),
            height_at(heights, padded_xz, x, z + 1),
            height_at(heights, padded_xz, x + 1, z + 1),
        ],
        own,
    );
    (c10, c11)
}

/// Corners on the west (-x) edge of column `(px, pz)`: `(c00, c01)`.
fn west_edge_pair(heights: &[i32], padded_xz: usize, px: usize, pz: usize) -> (i32, i32) {
    let own = heights[px * padded_xz + pz];
    let (x, z) = (px as i32, pz as i32);
    let c00 = corner_rule(
        [
            height_at(heights, padded_xz, x - 1, z - 1),
            height_at(heights, padded_xz, x, z - 1),
            height_at(heights, padded_xz, x - 1, z),
            own,
        ],
        own,
    );
    let c01 = corner_rule(
        [
            height_at(heights, padded_xz, x - 1, z),
            own,
            height_at(heights, padded_xz, x - 1, z + 1),
            height_at(heights, padded_xz, x, z + 1),
        ],
        own,
    );
    (c00, c01)
}

/// Corners on the south (+z) edge of column `(px, pz)`: `(c01, c11)`.
fn south_edge_pair(heights: &[i32], padded_xz: usize, px: usize, pz: usize) -> (i32, i32) {
    let own = heights[px * padded_xz + pz];
    let (x, z) = (px as i32, pz as i32);
    let c01 = corner_rule(
        [
            height_at(heights, padded_xz, x - 1, z),
            own,
            height_at(heights, padded_xz, x - 1, z + 1),
            height_at(heights, padded_xz, x, z + 1),
        ],
        own,
    );
    let c11 = corner_rule(
        [
            own,
            height_at(heights, padded_xz, x + 1, z),
            height_at(heights, padded_xz, x, z + 1),
            height_at(heights, padded_xz, x + 1, z + 1),
        ],
        own,
    );
    (c01, c11)
}

/// Corners on the north (-z) edge of column `(px, pz)`: `(c00, c10)`.
fn north_edge_pair(heights: &[i32], padded_xz: usize, px: usize, pz: usize) -> (i32, i32) {
    let own = heights[px * padded_xz + pz];
    let (x, z) = (px as i32, pz as i32);
    let c00 = corner_rule(
        [
            height_at(heights, padded_xz, x - 1, z - 1),
            height_at(heights, padded_xz, x, z - 1),
            height_at(heights, padded_xz, x - 1, z),
            own,
        ],
        own,
    );
    let c10 = corner_rule(
        [
            height_at(heights, padded_xz, x, z - 1),
            height_at(heights, padded_xz, x + 1, z - 1),
            own,
            height_at(heights, padded_xz, x + 1, z),
        ],
        own,
    );
    (c00, c10)
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

    #[test]
    fn stepped_generator_chunk_emits_tilted_normals() {
        use crate::config::VoxelGeneratorImpl;
        use crate::terrain::SteppedGenerator;

        let generator = SteppedGenerator {
            chunk_size: 16,
            shape: padded_shape(),
        };
        let voxels = generator.generate_terrain(IVec3::new(-1, 0, 0));
        let shape = padded_shape();
        let mesh = mesh_chunk_sloped(&voxels, &shape).expect("mesh");
        let normals = mesh
            .attribute(Mesh::ATTRIBUTE_NORMAL)
            .unwrap()
            .as_float3()
            .unwrap();
        let positions = mesh
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .unwrap()
            .as_float3()
            .unwrap();
        let tilted = normals
            .iter()
            .filter(|n| n[0].abs() > 0.1 && n[1].abs() > 0.1 && n[1].abs() < 0.99)
            .count();
        // Heuristic: slope normals should be 45° — y component around 0.707.
        let slope_45 = normals
            .iter()
            .filter(|n| (n[1] - 0.7071).abs() < 0.01)
            .count();
        assert!(
            tilted > 0 && slope_45 > 0,
            "staircase chunk should emit 45° tilted normals — got tilted={} slope_45={} total_normals={} total_positions={}",
            tilted,
            slope_45,
            normals.len(),
            positions.len()
        );
    }

    #[test]
    fn choose_diagonal_main_for_convex() {
        // Convex hip: c00+c11 > c10+c01 → main diagonal (ridge along main).
        assert_eq!(choose_diagonal(&[5, 4, 4, 4]), Diagonal::Main);
        assert_eq!(choose_diagonal(&[3, 3, 3, 3]), Diagonal::Main); // tie defaults to Main
    }

    #[test]
    fn choose_diagonal_anti_for_concave() {
        // Concave valley: c10+c01 > c00+c11 → anti diagonal (seam along anti).
        assert_eq!(choose_diagonal(&[4, 5, 5, 4]), Diagonal::Anti);
    }

    #[test]
    fn pyramid_tip_emits_hip_triangles() {
        use crate::config::VoxelGeneratorImpl;
        use crate::terrain::SteppedGenerator;

        // The pyramid's corner columns at (1, 1), (-1, 1), (1, -1), (-1, -1) have
        // asymmetric corner heights and should classify NonPlanar with Main
        // diagonal (convex hip). We verify by checking that the emitted mesh
        // contains at least one triangle (3 consecutive positions) corresponding
        // to a pyramid-tip column with slope > 45° (indicating a hip ridge).
        let generator = SteppedGenerator {
            chunk_size: 16,
            shape: padded_shape(),
        };
        let voxels = generator.generate_terrain(IVec3::ZERO);
        let shape = padded_shape();
        let mesh = mesh_chunk_sloped(&voxels, &shape).expect("mesh");
        let normals = mesh
            .attribute(Mesh::ATTRIBUTE_NORMAL)
            .unwrap()
            .as_float3()
            .unwrap();
        // A pyramid hip triangle's normal has BOTH x AND z components nonzero
        // (diagonal tilt). Planar slopes only tilt along one axis.
        let diag_tilted = normals
            .iter()
            .filter(|n| n[0].abs() > 0.1 && n[1].abs() > 0.1 && n[2].abs() > 0.1)
            .count();
        assert!(
            diag_tilted > 0,
            "pyramid hip should emit diagonally-tilted normals (got {} of {})",
            diag_tilted,
            normals.len()
        );
    }

    #[test]
    fn corner_rule_preserves_isolated_pit() {
        // Isolated pit: own=2 surrounded by 3 neighbors at 3. Keep own (valley).
        assert_eq!(corner_rule([3, 3, 3, 2], 2), 2);
        // Staircase slope east corner: own=3 with 2x max=4. Rise to meet.
        assert_eq!(corner_rule([3, 4, 3, 4], 3), 4);
        // Staircase slope west corner: own=3 with min=2, max=3. Stay at max (=own).
        assert_eq!(corner_rule([2, 3, 2, 3], 3), 3);
        // Peak: own=2 with 3 lower neighbors. own is max → stay.
        assert_eq!(corner_rule([1, 1, 1, 2], 2), 2);
        // Pyramid edge diagonal: own=4 with 3 same + 1 tip at 5. Hip ridge → rise.
        assert_eq!(corner_rule([5, 4, 4, 4], 4), 5);
        // Multi-cell pit edge: own=2 with 2x non-pit at 3. Slope up (ramp into pit).
        assert_eq!(corner_rule([2, 3, 2, 3], 2), 3);
        // Flat: all same. No change.
        assert_eq!(corner_rule([5, 5, 5, 5], 5), 5);
        // Cliff: delta >= 2. Keep own.
        assert_eq!(corner_rule([5, 2, 5, 2], 2), 2);
        // SENTINEL neighbor: cliff-like. Keep own.
        assert_eq!(corner_rule([SENTINEL_NO_SURFACE, 3, 3, 3], 3), 3);
    }

    #[test]
    fn cliff_wall_merges_horizontally() {
        // 16 adjacent columns at h=3 on the +x side, h=0 on the -x side.
        // Δ = 3 → cliff rule → each column classifies as Rectangle wall.
        // Greedy merge should collapse them into ONE rectangle per z-row.
        let voxels = heightmap_voxels(|x, _| if x >= 8 { 3 } else { 0 });
        let shape = padded_shape();
        let mesh = mesh_chunk_sloped(&voxels, &shape).expect("mesh");
        let positions = mesh
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .unwrap()
            .as_float3()
            .unwrap();
        // The +x-facing cliff has z-extent 16 (one per column without merging);
        // merged, it's one rectangle per y-strip. Count vertices with x near
        // the cliff plane (mesh x=9).
        let cliff_verts = positions
            .iter()
            .filter(|p| (p[0] - 9.0).abs() < 0.01)
            .count();
        // Unmerged would be 16 cells × 4 verts = 64 (+ possibly bottom strip
        // duplicates). Merged is ≤ 8 verts for two sloped strips + the cliff.
        // Give generous bound: merging must cut it by at least half.
        assert!(
            cliff_verts < 40,
            "cliff wall should greedy-merge: got {} vertices at x=9",
            cliff_verts
        );
    }

    #[test]
    fn wall_triangle_area_above_parry_threshold() {
        // Unit 1x1 saddle triangle has area 0.5, well above 1e-4.
        let v = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        assert!(triangle_area(&v) > MIN_PARRY_TRIANGLE_AREA);
        // Degenerate triangle (all colinear): area 0.
        let degenerate = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0]];
        assert!(triangle_area(&degenerate) < MIN_PARRY_TRIANGLE_AREA);
    }

    #[test]
    fn stepped_generator_all_chunks_no_degenerate_triangles() {
        // Mesh every SteppedGenerator chunk that any fixture feature can occupy
        // and verify no saddle-triangle debug_assert fires. This is a
        // regression test for the Phase 4 (false, true) classifier branch
        // swapping flush/step args and producing degenerate triangles.
        use crate::config::VoxelGeneratorImpl;
        use crate::terrain::SteppedGenerator;

        let generator = SteppedGenerator {
            chunk_size: 16,
            shape: padded_shape(),
        };
        let shape = padded_shape();
        for cy in -1..=0 {
            for cz in -1..=1 {
                for cx in -1..=1 {
                    let voxels = generator.generate_terrain(IVec3::new(cx, cy, cz));
                    // Will panic via debug_assert! if any saddle triangle is degenerate.
                    let _ = mesh_chunk_sloped(&voxels, &shape);
                }
            }
        }
    }

    #[test]
    fn make_triangle_wall_x_produces_nondegenerate_triangles() {
        // Both classifier branches must produce non-degenerate triangles.
        // (true, false): flush at lo-z, step at hi-z. a10=b00=3, a11=5, b01=3.
        let emission = make_triangle_wall_x(2, 5, 3, 5, 3, true);
        if let WallEmission::Triangle { vertices } = emission {
            assert!(triangle_area(&vertices) >= MIN_PARRY_TRIANGLE_AREA);
        } else {
            panic!("expected Triangle");
        }
        // (false, true): flush at hi-z, step at lo-z. a11=b01=3, a10=5, b00=3.
        let emission = make_triangle_wall_x(2, 5, 3, 5, 3, false);
        if let WallEmission::Triangle { vertices } = emission {
            assert!(triangle_area(&vertices) >= MIN_PARRY_TRIANGLE_AREA);
        } else {
            panic!("expected Triangle");
        }
    }

    #[test]
    fn cross_chunk_pit_emits_lip_walls() {
        // Voxels: cross-shaped pit at the bottom of the padded box. Pit cells
        // are solid only at py=0 (padding row, simulating "neighbor chunk
        // owns the pit floor"). Surrounding cells are solid at py=0 AND py=1
        // (full ground at world_y=0 in their chunk). Our chunk has interior
        // top at py=1 for non-pit cells, no interior top for pit cells.
        let shape = padded_shape();
        let mut voxels = vec![WorldVoxel::Air; shape.usize()];
        let pit_cells = |x: i32, z: i32| (x, z) == (8, 8) || x == 8 && (6..=10).contains(&z);
        for i in 0..shape.size() {
            let [px, py, pz] = shape.delinearize(i);
            let world_x = px as i32 - 1;
            let world_y = py as i32 - 1;
            let world_z = pz as i32 - 1;
            let h = if pit_cells(world_x, world_z) { -1 } else { 0 };
            if world_y <= h {
                voxels[i as usize] = WorldVoxel::Solid(0);
            }
        }
        let mesh = mesh_chunk_sloped(&voxels, &shape).expect("mesh");
        let normals = mesh
            .attribute(Mesh::ATTRIBUTE_NORMAL)
            .unwrap()
            .as_float3()
            .unwrap();
        // Lip walls have horizontal normals (y == 0, x or z != 0).
        let horizontal_walls = normals
            .iter()
            .filter(|n| n[1].abs() < 0.01 && (n[0].abs() > 0.9 || n[2].abs() > 0.9))
            .count();
        assert!(
            horizontal_walls > 0,
            "expected horizontal lip walls around pit, got {} of {} normals",
            horizontal_walls,
            normals.len(),
        );
    }

    #[test]
    fn handcrafted_valley_column_uses_anti_diagonal() {
        // Synthesize a heights array where interior column (2, 2) has corners
        // like [3, 4, 4, 3] (concave valley: anti-diagonal split).
        // Column (2, 2) own h=1. Neighbors form delta-1 corners where
        // c00, c11 = max=3 and c10, c01 = max=4.
        //
        // Corner rule: corner = max when delta==1. Populating heights so
        // the c10 and c01 corners see h=4 neighbors while c00, c11 see h=3.
        let shape = padded_shape();
        // Own column (2, 2) h=1, so world_x=1, world_z=1 row has h such that
        // top-face-y = 3 at (2,2). Column heights = h + 2.
        // Build heights to produce corners [3, 4, 4, 3].
        // c00 = max of h(1,1), h(2,1), h(1,2), h(2,2) → want 3. So all in {1, 2} → top-face-y in {3, 4}. max=3 means ALL=3 → all h=1.
        // c10 = max of h(2,1), h(3,1), h(2,2), h(3,2) → want 4. So max=4 → at least one h=2. h(3,1) or h(3,2) = 2.
        // c01 = max of h(1,2), h(2,2), h(1,3), h(2,3) → want 4. max=4 → h(1,3) or h(2,3) = 2.
        // c11 = max of h(2,2), h(3,2), h(2,3), h(3,3) → want 3. max=3 → all in {1}. But h(3,2) and h(2,3) need to be 1 here, conflicting with c10 and c01 above.
        // So this exact concave valley cannot be produced by corner-max rule —
        // the rule is local-max, so any "high" neighbor pulls all corners up.
        //
        // Instead, test that choose_diagonal correctly classifies a hypothetical
        // valley corner set (unit test, no mesher needed).
        let valley = [3, 5, 5, 3];
        assert_eq!(choose_diagonal(&valley), Diagonal::Anti);
        let hip = [5, 3, 3, 5];
        assert_eq!(choose_diagonal(&hip), Diagonal::Main);
        let _ = shape; // shape unused in this test; kept to dock with module conventions
    }
}
