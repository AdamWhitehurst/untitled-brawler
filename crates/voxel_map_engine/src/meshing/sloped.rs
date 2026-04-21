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
    let mut tops = classify_columns(&heights, &materials, padded_xz);
    extend_slopes_over_nonplanar(&mut tops, &heights, padded_xz);

    let mut buf = MeshBuffers::new();
    emit_planar_tops(&tops, padded_xz, &mut buf);
    emit_nonplanar_tops(&tops, padded_xz, &mut buf);
    emit_walls(&heights, &surface_heights, &tops, padded_xz, &mut buf);
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

/// Extend a Planar slope into neighboring NonPlanar columns whose own
/// top-face-y lies on the slope's plane. The corner-max rule classifies a
/// column as NonPlanar whenever any corner lies on a cliff (delta ≥ 2). At
/// the z-edge of a staircase, for example, columns inside the staircase see
/// a cliff south (to non-staircase) and get NonPlanar corners like
/// `[3, 4, 3, 3]` — the `c11` corner is pulled down to own_h by the cliff
/// rule. But the column's own top lies ON the slope's plane, so rendering
/// it as part of the slope (rather than as an individual hip wedge)
/// produces a continuous ramp. Walls at the cliff side still emit via
/// `classify_edge_*`, filling the drop to the flat.
fn extend_slopes_over_nonplanar(
    tops: &mut Vec<Option<ColumnTop>>,
    heights: &[i32],
    padded_xz: usize,
) {
    loop {
        let mut changed = false;
        for px in 1..padded_xz - 1 {
            for pz in 1..padded_xz - 1 {
                let is_nonplanar =
                    matches!(tops[px * padded_xz + pz], Some(ColumnTop::NonPlanar { .. }));
                if !is_nonplanar {
                    continue;
                }
                let own_h = heights[px * padded_xz + pz];
                let material = match tops[px * padded_xz + pz] {
                    Some(ColumnTop::NonPlanar { material, .. }) => material,
                    _ => continue,
                };
                // Check 4 cardinal neighbors for a Planar slope whose plane
                // passes through own_h at (px, pz).
                let mut adopt: Option<(i32, i32, i32)> = None;
                for (dx, dz) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)] {
                    let nx = px as i32 + dx;
                    let nz = pz as i32 + dz;
                    if nx < 1 || nx >= padded_xz as i32 - 1 || nz < 1 || nz >= padded_xz as i32 - 1
                    {
                        continue;
                    }
                    let Some(ColumnTop::Planar {
                        gradient_x: gx,
                        gradient_z: gz,
                        intercept,
                        material: n_mat,
                        ..
                    }) = tops[nx as usize * padded_xz + nz as usize]
                    else {
                        continue;
                    };
                    if n_mat != material {
                        continue;
                    }
                    // Only extend SLOPES, not flat planes — extending a flat
                    // plane into a NonPlanar column would erase the hip
                    // wedge that visually represents the corner-max
                    // raised-corner shape (pyramid edges, etc.).
                    if gx == 0 && gz == 0 {
                        continue;
                    }
                    let plane_y_here = gx * px as i32 + gz * pz as i32 + intercept;
                    if plane_y_here != own_h {
                        continue;
                    }
                    adopt = Some((gx, gz, intercept));
                    break;
                }
                if let Some((gx, gz, intercept)) = adopt {
                    let px_i = px as i32;
                    let pz_i = pz as i32;
                    let corners = [
                        gx * px_i + gz * pz_i + intercept,
                        gx * (px_i + 1) + gz * pz_i + intercept,
                        gx * px_i + gz * (pz_i + 1) + intercept,
                        gx * (px_i + 1) + gz * (pz_i + 1) + intercept,
                    ];
                    tops[px * padded_xz + pz] = Some(ColumnTop::Planar {
                        corners,
                        gradient_x: gx,
                        gradient_z: gz,
                        intercept,
                        material,
                    });
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
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
    /// The taller column's Planar plane `(gx, gz, intercept)`, if the taller
    /// col is on a Planar slope. Used by the tilt merger so that merged
    /// tilted quads follow the plane's exact gradient (not a linear
    /// interpolation between rect endpoints that doesn't match the slope).
    taller_plane: Option<(i32, i32, i32)>,
}

/// A 4-corner wall polygon. When `y_*_west` equals `y_*_east`, it's a
/// rectangle; when they differ along the parallel axis, it's a tilted quad.
/// Used to smooth out stepped wall profiles that trace a slope plane — for
/// example, the south face of a staircase where each column has a different
/// exposed height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WallQuad {
    axis: EdgeAxis,
    base_px: usize,
    base_pz: usize,
    axis_len: usize,
    y_lo_west: i32,
    y_lo_east: i32,
    y_hi_west: i32,
    y_hi_east: i32,
    normal_positive: bool,
}

impl From<WallRect> for WallQuad {
    fn from(r: WallRect) -> Self {
        WallQuad {
            axis: r.axis,
            base_px: r.base_px,
            base_pz: r.base_pz,
            axis_len: r.axis_len,
            y_lo_west: r.y_lo,
            y_lo_east: r.y_lo,
            y_hi_west: r.y_hi,
            y_hi_east: r.y_hi,
            normal_positive: r.normal_positive,
        }
    }
}

#[allow(dead_code)] // `Triangle` retained for future saddle-wall support
enum WallEmission {
    Skip,
    /// Saddle edge (no longer emitted; kept for future re-introduction if a
    /// corner rule emerges that produces true 1-mismatch saddles).
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
fn emit_walls(
    heights: &[i32],
    surface_heights: &[i32],
    tops: &[Option<ColumnTop>],
    padded_xz: usize,
    out: &mut MeshBuffers,
) {
    let mut rects: Vec<WallRect> = Vec::new();

    for pz in 1..padded_xz - 1 {
        for px in 1..padded_xz - 1 {
            match classify_edge_x(heights, surface_heights, tops, padded_xz, px, pz) {
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
            match classify_edge_z(heights, surface_heights, tops, padded_xz, px, pz) {
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

    for quad in merge_walls_with_tilt(rects) {
        emit_wall_quad(&quad, out);
    }
}

fn classify_edge_x(
    heights: &[i32],
    surface_heights: &[i32],
    tops: &[Option<ColumnTop>],
    padded_xz: usize,
    px: usize,
    pz: usize,
) -> WallEmission {
    classify_edge_by_own_heights(
        heights[px * padded_xz + pz],
        heights[(px + 1) * padded_xz + pz],
        surface_heights[px * padded_xz + pz],
        surface_heights[(px + 1) * padded_xz + pz],
        &tops[px * padded_xz + pz],
        &tops[(px + 1) * padded_xz + pz],
        EdgeAxis::X,
        px,
        pz,
    )
}

fn classify_edge_z(
    heights: &[i32],
    surface_heights: &[i32],
    tops: &[Option<ColumnTop>],
    padded_xz: usize,
    px: usize,
    pz: usize,
) -> WallEmission {
    classify_edge_by_own_heights(
        heights[px * padded_xz + pz],
        heights[px * padded_xz + pz + 1],
        surface_heights[px * padded_xz + pz],
        surface_heights[px * padded_xz + pz + 1],
        &tops[px * padded_xz + pz],
        &tops[px * padded_xz + pz + 1],
        EdgeAxis::Z,
        px,
        pz,
    )
}

/// Emit a wall rectangle sized from the two columns' OWN top-face-y values.
/// Also captures the taller column's Planar-slope plane so the tilt merger
/// can later use the plane's exact gradient for merged quad endpoints —
/// otherwise a linear interpolation between first/last rect y_hi values
/// would produce a slope-mismatched gradient and visible gaps at the
/// slope's south edge.
fn classify_edge_by_own_heights(
    top_a: i32,
    top_b: i32,
    surf_a: i32,
    surf_b: i32,
    top_cls_a: &Option<ColumnTop>,
    top_cls_b: &Option<ColumnTop>,
    axis: EdgeAxis,
    px: usize,
    pz: usize,
) -> WallEmission {
    if surf_a == SENTINEL_NO_SURFACE || surf_b == SENTINEL_NO_SURFACE {
        return WallEmission::Skip;
    }
    let h_a = if top_a != SENTINEL_NO_SURFACE {
        top_a
    } else {
        surf_a
    };
    let h_b = if top_b != SENTINEL_NO_SURFACE {
        top_b
    } else {
        surf_b
    };
    if h_a == h_b {
        return WallEmission::Skip;
    }
    let taller_plane = if h_a >= h_b {
        extract_slope_plane(top_cls_a)
    } else {
        extract_slope_plane(top_cls_b)
    };
    WallEmission::Rectangle(WallRect {
        axis,
        base_px: px,
        base_pz: pz,
        axis_len: 1,
        y_lo: h_a.min(h_b),
        y_hi: h_a.max(h_b),
        normal_positive: h_a >= h_b,
        taller_plane,
    })
}

/// Extract a non-flat Planar plane from a `ColumnTop`, or None for flat/NonPlanar.
fn extract_slope_plane(top: &Option<ColumnTop>) -> Option<(i32, i32, i32)> {
    match top {
        Some(ColumnTop::Planar {
            gradient_x,
            gradient_z,
            intercept,
            ..
        }) if *gradient_x != 0 || *gradient_z != 0 => Some((*gradient_x, *gradient_z, *intercept)),
        _ => None,
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
/// Merge adjacent wall rectangles into `WallQuad`s. When consecutive rects
/// along the parallel axis form a linear gradient in `y_lo` or `y_hi`, emit a
/// tilted quad that connects the run's west and east endpoints. Constant runs
/// emit as rectangles (all 4 corners at matching y). This produces smooth
/// diagonal wall faces along staircase-like terrain.
fn merge_walls_with_tilt(rects: Vec<WallRect>) -> Vec<WallQuad> {
    use std::collections::BTreeMap;
    // Group by taller_plane too — rects on the same slope plane can merge
    // with plane-derived endpoints; rects with different planes (or no plane)
    // emit separately so the merged quad's top edge follows the slope exactly
    // at the plane boundary.
    let mut groups: BTreeMap<(EdgeAxis, usize, bool, Option<(i32, i32, i32)>), Vec<WallRect>> =
        BTreeMap::new();
    for r in rects {
        let perp = match r.axis {
            EdgeAxis::X => r.base_px,
            EdgeAxis::Z => r.base_pz,
        };
        groups
            .entry((r.axis, perp, r.normal_positive, r.taller_plane))
            .or_default()
            .push(r);
    }

    let mut out = Vec::new();
    for ((_axis, _perp, _normal, taller_plane), mut group) in groups {
        group.sort_by_key(|r| match r.axis {
            EdgeAxis::X => r.base_pz,
            EdgeAxis::Z => r.base_px,
        });

        let mut run: Option<WallQuad> = None;
        let mut gradient: Option<(i32, i32)> = None; // (delta_y_lo, delta_y_hi) per unit

        let emit = |out: &mut Vec<WallQuad>, q: WallQuad| {
            let final_quad = apply_slope_plane_to_quad(q, taller_plane);
            out.push(final_quad);
        };

        for rect in group {
            let parallel_start = match rect.axis {
                EdgeAxis::X => rect.base_pz,
                EdgeAxis::Z => rect.base_px,
            };
            let rect_quad = WallQuad::from(rect);

            match &mut run {
                None => {
                    run = Some(rect_quad);
                    gradient = None;
                }
                Some(r) => {
                    let r_end = match r.axis {
                        EdgeAxis::X => r.base_pz + r.axis_len,
                        EdgeAxis::Z => r.base_px + r.axis_len,
                    };
                    let adjacent = r_end == parallel_start;
                    if !adjacent {
                        emit(&mut out, *r);
                        run = Some(rect_quad);
                        gradient = None;
                        continue;
                    }
                    let delta_lo = rect.y_lo - r.y_lo_east;
                    let delta_hi = rect.y_hi - r.y_hi_east;
                    let rect_len = rect.axis_len as i32;
                    debug_assert_eq!(
                        rect_len, 1,
                        "merge_walls_with_tilt expects 1-unit input rects"
                    );
                    match gradient {
                        None => {
                            gradient = Some((delta_lo, delta_hi));
                            r.axis_len += rect.axis_len;
                            r.y_lo_east = rect.y_lo;
                            r.y_hi_east = rect.y_hi;
                        }
                        Some((g_lo, g_hi)) => {
                            if delta_lo == g_lo && delta_hi == g_hi {
                                r.axis_len += rect.axis_len;
                                r.y_lo_east = rect.y_lo;
                                r.y_hi_east = rect.y_hi;
                            } else {
                                emit(&mut out, *r);
                                run = Some(rect_quad);
                                gradient = None;
                            }
                        }
                    }
                }
            }
        }
        if let Some(r) = run {
            out.push(apply_slope_plane_to_quad(r, taller_plane));
        }
    }
    out
}

/// If the quad's taller side is a Planar slope, replace its y_hi values with
/// the plane's exact y at the quad's west and east endpoints — so the wall's
/// top edge matches the slope surface pixel-for-pixel and leaves no gap.
fn apply_slope_plane_to_quad(q: WallQuad, plane: Option<(i32, i32, i32)>) -> WallQuad {
    let Some((gx, gz, intercept)) = plane else {
        return q;
    };
    let (west_x, west_z, east_x, east_z) = match q.axis {
        EdgeAxis::X => {
            // Wall at mesh_x = base_px + 1, z varying.
            let mx = (q.base_px + 1) as i32;
            (mx, q.base_pz as i32, mx, (q.base_pz + q.axis_len) as i32)
        }
        EdgeAxis::Z => {
            // Wall at mesh_z = base_pz + 1, x varying.
            let mz = (q.base_pz + 1) as i32;
            (q.base_px as i32, mz, (q.base_px + q.axis_len) as i32, mz)
        }
    };
    WallQuad {
        y_hi_west: gx * west_x + gz * west_z + intercept,
        y_hi_east: gx * east_x + gz * east_z + intercept,
        ..q
    }
}

fn emit_wall_quad(q: &WallQuad, out: &mut MeshBuffers) {
    match q.axis {
        EdgeAxis::X => {
            let x = (q.base_px + 1) as f32;
            let z1 = q.base_pz as f32;
            let z2 = (q.base_pz + q.axis_len) as f32;
            // For +x edge: parallel axis is Z. "West" = low z, "east" = high z.
            emit_wall_x_quad(
                out,
                x,
                q.y_lo_west as f32,
                q.y_hi_west as f32,
                q.y_lo_east as f32,
                q.y_hi_east as f32,
                z1,
                z2,
                q.normal_positive,
            );
        }
        EdgeAxis::Z => {
            let z = (q.base_pz + 1) as f32;
            let x1 = q.base_px as f32;
            let x2 = (q.base_px + q.axis_len) as f32;
            // For +z edge: parallel axis is X. "West" = low x, "east" = high x.
            emit_wall_z_quad(
                out,
                z,
                q.y_lo_west as f32,
                q.y_hi_west as f32,
                q.y_lo_east as f32,
                q.y_hi_east as f32,
                x1,
                x2,
                q.normal_positive,
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

/// Emit a wall on the `+x`/`-x` plane at `x`, with potentially-different y
/// values at each z endpoint (`z1` = west/low-z, `z2` = east/high-z). When
/// `y_*_west == y_*_east`, this degenerates to a rectangle. Otherwise it's a
/// tilted quad that smooths stepped profiles. Normals are computed per-
/// triangle from positions so tilt is reflected correctly.
fn emit_wall_x_quad(
    out: &mut MeshBuffers,
    x: f32,
    y_lo_west: f32,
    y_hi_west: f32,
    y_lo_east: f32,
    y_hi_east: f32,
    z1: f32,
    z2: f32,
    normal_positive: bool,
) {
    if y_hi_west <= y_lo_west && y_hi_east <= y_lo_east {
        return;
    }
    let p0 = [x, y_lo_west, z1];
    let p1 = [x, y_hi_west, z1];
    let p2 = [x, y_lo_east, z2];
    let p3 = [x, y_hi_east, z2];
    let base = out.positions.len() as u32;
    out.positions.extend_from_slice(&[p0, p1, p2, p3]);
    let n0 = triangle_normal_from_indices(&[p0, p1, p2, p3], 0, 1, 2, normal_positive);
    let n1 = triangle_normal_from_indices(&[p0, p1, p2, p3], 1, 3, 2, normal_positive);
    // Shared vertices get averaged normals for smooth shading on the tilt.
    out.normals
        .extend_from_slice(&[n0, average_normal(n0, n1), average_normal(n0, n1), n1]);
    out.uvs.extend_from_slice(&[
        [z1, y_lo_west],
        [z1, y_hi_west],
        [z2, y_lo_east],
        [z2, y_hi_east],
    ]);
    if normal_positive {
        out.indices
            .extend_from_slice(&[base, base + 1, base + 2, base + 1, base + 3, base + 2]);
    } else {
        out.indices
            .extend_from_slice(&[base, base + 2, base + 1, base + 1, base + 2, base + 3]);
    }
}

/// Mirror of [`emit_wall_x_quad`] for walls on the `+z`/`-z` plane at `z`,
/// where y values may vary along x (`x1` = west, `x2` = east).
fn emit_wall_z_quad(
    out: &mut MeshBuffers,
    z: f32,
    y_lo_west: f32,
    y_hi_west: f32,
    y_lo_east: f32,
    y_hi_east: f32,
    x1: f32,
    x2: f32,
    normal_positive: bool,
) {
    if y_hi_west <= y_lo_west && y_hi_east <= y_lo_east {
        return;
    }
    let p0 = [x1, y_lo_west, z];
    let p1 = [x1, y_hi_west, z];
    let p2 = [x2, y_lo_east, z];
    let p3 = [x2, y_hi_east, z];
    let base = out.positions.len() as u32;
    out.positions.extend_from_slice(&[p0, p1, p2, p3]);
    let n0 = triangle_normal_from_indices(&[p0, p1, p2, p3], 0, 2, 1, normal_positive);
    let n1 = triangle_normal_from_indices(&[p0, p1, p2, p3], 2, 3, 1, normal_positive);
    out.normals
        .extend_from_slice(&[n0, average_normal(n0, n1), average_normal(n0, n1), n1]);
    out.uvs.extend_from_slice(&[
        [x1, y_lo_west],
        [x1, y_hi_west],
        [x2, y_lo_east],
        [x2, y_hi_east],
    ]);
    if normal_positive {
        out.indices
            .extend_from_slice(&[base, base + 2, base + 1, base + 2, base + 3, base + 1]);
    } else {
        out.indices
            .extend_from_slice(&[base, base + 1, base + 2, base + 1, base + 3, base + 2]);
    }
}

fn triangle_normal_from_indices(
    positions: &[[f32; 3]; 4],
    i0: usize,
    i1: usize,
    i2: usize,
    normal_positive: bool,
) -> [f32; 3] {
    let v0 = Vec3::from(positions[i0]);
    let v1 = Vec3::from(positions[i1]);
    let v2 = Vec3::from(positions[i2]);
    let mut n = (v1 - v0).cross(v2 - v0).normalize_or_zero();
    if !normal_positive {
        n = -n;
    }
    // The ordering passed from the caller already encodes normal direction;
    // negate above only reverses when the caller passed a "-x/-z" wall.
    // Re-assert: if n is near-zero (degenerate), fall back to axis-aligned.
    if n.length_squared() < 1e-6 {
        return [0.0, 1.0, 0.0];
    }
    n.to_array()
}

fn average_normal(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    Vec3::from(a)
        .midpoint(Vec3::from(b))
        .normalize_or_zero()
        .to_array()
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
        // Pyramid hip triangles on NonPlanar cols emit normals of the form
        // (±0.707, 0.707, 0) or (0, 0.707, ±0.707) — tilted in y plus one
        // horizontal axis. If these are absent, hip classification broke.
        let hip_count = normals
            .iter()
            .filter(|n| {
                let y_half = (n[1] - 0.7071).abs() < 0.02;
                let x_tilt = (n[0].abs() - 0.7071).abs() < 0.02 && n[2].abs() < 0.02;
                let z_tilt = n[0].abs() < 0.02 && (n[2].abs() - 0.7071).abs() < 0.02;
                y_half && (x_tilt || z_tilt)
            })
            .count();
        assert!(
            hip_count > 0,
            "pyramid should emit hip-triangle normals (got {} of {})",
            hip_count,
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
    fn merge_walls_with_tilt_combines_arithmetic_progression() {
        // 6 consecutive 1-unit +z-axis walls with y_hi stepping 4..9 should
        // merge into ONE WallQuad with tilted y_hi from 4 (west) to 9 (east).
        let mut rects = Vec::new();
        for i in 0..6 {
            rects.push(WallRect {
                axis: EdgeAxis::Z,
                base_px: 11 + i,
                base_pz: 5,
                axis_len: 1,
                y_lo: 2,
                y_hi: 4 + i as i32,
                normal_positive: true,
                taller_plane: None,
            });
        }
        let merged = merge_walls_with_tilt(rects);
        assert_eq!(
            merged.len(),
            1,
            "expected 1 merged quad, got {}",
            merged.len()
        );
        let q = merged[0];
        assert_eq!(q.axis, EdgeAxis::Z);
        assert_eq!(q.base_px, 11);
        assert_eq!(q.base_pz, 5);
        assert_eq!(q.axis_len, 6);
        assert_eq!(q.y_lo_west, 2);
        assert_eq!(q.y_lo_east, 2);
        assert_eq!(q.y_hi_west, 4);
        assert_eq!(q.y_hi_east, 9);
    }

    #[test]
    fn staircase_south_face_emits_tilted_wall_not_stepped() {
        // A staircase of height h(x) = x.clamp(0, 7) terminating at z=4 has a
        // south face whose top edge forms an arithmetic progression. The
        // tilted-wall merger should consolidate the per-column rectangles
        // into a single quad with varying y along the parallel axis — i.e.,
        // the south face's top edge is NOT horizontal at any discrete y.
        use crate::config::VoxelGeneratorImpl;
        use crate::terrain::SteppedGenerator;

        let generator = SteppedGenerator {
            chunk_size: 16,
            shape: padded_shape(),
        };
        let voxels = generator.generate_terrain(IVec3::new(-1, 0, 0));
        let shape = padded_shape();
        let mesh = mesh_chunk_sloped(&voxels, &shape).expect("mesh");
        let positions = mesh
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .unwrap()
            .as_float3()
            .unwrap();
        // A smooth tilted wall at the south face (mesh z=6) has endpoint
        // vertices at (first_x, y_first) and (last_x, y_last) with different
        // y values. Before Option B, every step had its own rect with
        // constant y, producing stepped cubic walls. After Option B, adjacent
        // rects with gradient=1 merge — we should see at most 1 top-edge
        // vertex per gradient run, not 6.
        // A vertex at exactly the west-top and east-top of the tilted wall
        // quad: (11, 4, 6) and (17, 9, 6).
        let has_pos = |target: [f32; 3]| {
            positions.iter().any(|p| {
                (p[0] - target[0]).abs() < 0.01
                    && (p[1] - target[1]).abs() < 0.01
                    && (p[2] - target[2]).abs() < 0.01
            })
        };
        assert!(
            has_pos([11.0, 4.0, 6.0]),
            "tilted south wall should have west-top at (11, 4, 6)"
        );
        assert!(
            has_pos([17.0, 9.0, 6.0]),
            "tilted south wall should have east-top at (17, 9, 6)"
        );
    }

    #[test]
    fn stepped_generator_staircase_has_continuous_ramp_positions() {
        // The main slope rect should have 4 corners at the extremes of the
        // staircase x-range. Intermediate integer y-values (3, 4, 5, 6, 7, 8)
        // should NOT appear as vertex positions — if they do, the slope is
        // being emitted per-column (step mesh), not as one merged ramp.
        use crate::config::VoxelGeneratorImpl;
        use crate::terrain::SteppedGenerator;

        let generator = SteppedGenerator {
            chunk_size: 16,
            shape: padded_shape(),
        };
        let voxels = generator.generate_terrain(IVec3::new(-1, 0, 0));
        let shape = padded_shape();
        let mesh = mesh_chunk_sloped(&voxels, &shape).expect("mesh");
        let positions = mesh
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .unwrap()
            .as_float3()
            .unwrap();
        // Expect the main slope rect's 4 corners.
        let has_pos = |target: [f32; 3]| {
            positions.iter().any(|p| {
                (p[0] - target[0]).abs() < 0.01
                    && (p[1] - target[1]).abs() < 0.01
                    && (p[2] - target[2]).abs() < 0.01
            })
        };
        // After fix-2 slope extension over NonPlanar cols at pz=5, the main
        // rect's south edge extends to mesh z=6. Merged rect: px_min=9,
        // px_max=15, pz_min=1, pz_max=5. x1=9, x2=16, z1=1, z2=6.
        assert!(has_pos([9.0, 2.0, 1.0]), "main rect p00 missing");
        assert!(has_pos([16.0, 9.0, 1.0]), "main rect p10 missing");
        assert!(has_pos([16.0, 9.0, 6.0]), "main rect p11 missing");
        assert!(has_pos([9.0, 2.0, 6.0]), "main rect p01 missing");
    }

    #[test]
    fn stepped_generator_staircase_chunk_has_merged_slope() {
        // Chunk (-1, 0, 0) contains the east half of the staircase (world_x
        // -8..=-1, z 0..=4). The Planar slope region should greedy-merge.
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
        // Sloped top normals are ~(-0.707, 0.707, 0) for +x gradient.
        // Planar region merges into 2 rects (main 28-cell + isolated (9, 5))
        // = 8 verts. Hip triangles at the z-edge (NonPlanar pz=5 columns)
        // emit one slope-direction triangle per column = 3 × 6 = 18 more
        // verts. Plus (9, 6) NonPlanar anti-diagonal → 3 verts. Total ~29-39
        // expected; loose upper bound catches real regressions.
        let sloped_verts = normals
            .iter()
            .filter(|n| {
                (n[0] - (-0.7071)).abs() < 0.01 && (n[1] - 0.7071).abs() < 0.01 && n[2].abs() < 0.01
            })
            .count();
        assert!(
            sloped_verts >= 8,
            "expected main slope rect (4) + (9, 5) standalone rect (4) at minimum; got {}",
            sloped_verts
        );
        assert!(
            sloped_verts < 80,
            "slopes not merging: got {} sloped vertices (unmerged per-column would be ~112)",
            sloped_verts
        );
    }

    #[test]
    fn staircase_emits_single_merged_ramp() {
        // Staircase h(x, z) = x in 0..=7, flat outside. Should produce ONE
        // merged sloped quad for the slope region and a separate flat-top
        // quad for the plateau. If greedy merging fails (slopes classified as
        // NonPlanar or signatures diverging), we'd see many small quads.
        let voxels = heightmap_voxels(|x, _| x.clamp(0, 7));
        let shape = padded_shape();
        let mesh = mesh_chunk_sloped(&voxels, &shape).expect("mesh");
        let normals = mesh
            .attribute(Mesh::ATTRIBUTE_NORMAL)
            .unwrap()
            .as_float3()
            .unwrap();
        // Count tilted normals (slope surface).
        let sloped_verts = normals
            .iter()
            .filter(|n| (n[1] - 0.7071).abs() < 0.01)
            .count();
        // A single merged sloped quad has 4 vertices. More means slopes
        // aren't merging (emitted per-column).
        assert!(
            sloped_verts <= 8,
            "expected staircase slope to merge into few quads; got {} sloped vertices",
            sloped_verts
        );
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
