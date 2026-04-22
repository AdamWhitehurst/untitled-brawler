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
    emit_walls(&surface_heights, &tops, padded_xz, &mut buf);
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
/// Return the 4 raw heights feeding `corner_rule` for mesh corner `corner_idx`
/// of column `(px, pz)`. `corner_idx` ∈ [0, 4) in order `[c00, c10, c01, c11]`.
/// Out-of-range (x, z) sample as `SENTINEL_NO_SURFACE` so SENTINEL propagation
/// matches `compute_corners`.
fn corner_neighborhood_heights(
    heights: &[i32],
    padded_xz: usize,
    px: usize,
    pz: usize,
    corner_idx: usize,
) -> [i32; 4] {
    let (dx, dz) = match corner_idx {
        0 => (-1i32, -1),
        1 => (1, -1),
        2 => (-1, 1),
        3 => (1, 1),
        _ => unreachable!("corner_idx must be 0..4"),
    };
    let sample = |x: i32, z: i32| -> i32 {
        if x < 0 || z < 0 || x as usize >= padded_xz || z as usize >= padded_xz {
            SENTINEL_NO_SURFACE
        } else {
            heights[x as usize * padded_xz + z as usize]
        }
    };
    let px_i = px as i32;
    let pz_i = pz as i32;
    [
        sample(px_i, pz_i),
        sample(px_i + dx, pz_i),
        sample(px_i, pz_i + dz),
        sample(px_i + dx, pz_i + dz),
    ]
}

/// A corner was pulled to `own_h` by the cliff branch of `corner_rule` iff its
/// 2×2 neighborhood has `max - min ≥ 2` OR contains a SENTINEL. These corners
/// are safe to lift onto a slope plane during extension; `max`-pulled hip
/// ridges (delta = 1 with `own_h == min`, count ≠ 1) and isolated-pit floors
/// are not.
fn corner_is_cliff_pulled(values: [i32; 4]) -> bool {
    if values.iter().any(|&v| v == SENTINEL_NO_SURFACE) {
        return true;
    }
    let max = *values.iter().max().unwrap();
    let min = *values.iter().min().unwrap();
    max - min >= 2
}

fn extend_slopes_over_nonplanar(
    tops: &mut Vec<Option<ColumnTop>>,
    heights: &[i32],
    padded_xz: usize,
) {
    loop {
        let mut changed = false;
        for px in 1..padded_xz - 1 {
            for pz in 1..padded_xz - 1 {
                let (np_corners, material) = match tops[px * padded_xz + pz] {
                    Some(ColumnTop::NonPlanar { corners, material }) => (corners, material),
                    _ => continue,
                };
                // For each of 4 cardinal neighbors with a Planar slope plane,
                // count how many of this NonPlanar col's corners match the
                // plane's y at the corresponding mesh position. Adopt the
                // plane only if the mismatched corners were cliff-pulled to
                // `own_h` by `corner_rule` (2×2 delta ≥ 2). Other mismatches
                // are hip ridges (`max`-pulled) or isolated-pit floors, which
                // must not be overwritten or the slope extension erases
                // pyramid/saddle hip wedges.
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
                    if gx == 0 && gz == 0 {
                        // Flat planes don't extend into NonPlanar cols
                        // (would erase pyramid hip wedges).
                        continue;
                    }
                    let px_i = px as i32;
                    let pz_i = pz as i32;
                    let plane_corners = [
                        gx * px_i + gz * pz_i + intercept,
                        gx * (px_i + 1) + gz * pz_i + intercept,
                        gx * px_i + gz * (pz_i + 1) + intercept,
                        gx * (px_i + 1) + gz * (pz_i + 1) + intercept,
                    ];
                    let matches = (0..4)
                        .filter(|&i| np_corners[i] == plane_corners[i])
                        .count();
                    let mismatches_are_cliff_pulled = (0..4).all(|i| {
                        np_corners[i] == plane_corners[i]
                            || corner_is_cliff_pulled(corner_neighborhood_heights(
                                heights, padded_xz, px, pz, i,
                            ))
                    });
                    if matches >= 3 && mismatches_are_cliff_pulled {
                        adopt = Some((gx, gz, intercept));
                        break;
                    }
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
            let is_pit = is_true_pit(heights, padded_xz, px, pz, h);
            let corners = compute_corners(heights, padded_xz, px, pz, h, is_pit);
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

/// A cell is a "true pit" iff all 8 cardinal+diagonal neighbors have height
/// strictly greater than `own_h`. The per-corner ISOLATED-PIT branch of
/// `corner_rule` preserves `own_h` at corners whose 2×2 signature matches
/// `[H, H, H, own]`, which is valid for 1-cell pits but ALSO fires at inside
/// concave corners of plateaus (where the cell is not a pit). This cell-level
/// check lets `corner_rule` distinguish the two cases: at non-pit cells the
/// rule falls through to `max`, producing `[H, H, H, H]`-style lifts that let
/// Phase 4.6's diagonal rule render a beveled flat+tilted top.
///
/// Limitation: multi-cell pits (2+ adjacent cells at the same floor height)
/// do NOT classify as true pits because the neighboring pit cells are equal
/// to `own_h`, not strictly greater. Their tops get lifted to the surrounding
/// plateau level and the pit floor becomes invisible on the mesh. This is an
/// accepted v1 trade-off.
fn is_true_pit(heights: &[i32], padded_xz: usize, px: usize, pz: usize, own_h: i32) -> bool {
    for dx in -1..=1i32 {
        for dz in -1..=1i32 {
            if dx == 0 && dz == 0 {
                continue;
            }
            let nx = (px as i32 + dx) as usize;
            let nz = (pz as i32 + dz) as usize;
            let nh = heights[nx * padded_xz + nz];
            if nh == SENTINEL_NO_SURFACE || nh <= own_h {
                return false;
            }
        }
    }
    true
}

/// Compute 4 corner top-face-y values for interior column at `(px, pz)` via the
/// corner-max rule. SENTINEL neighbors mark absent terrain → treat as cliff.
/// `is_pit` gates the ISOLATED-PIT branch of `corner_rule` (see
/// [`is_true_pit`]).
fn compute_corners(
    heights: &[i32],
    padded_xz: usize,
    px: usize,
    pz: usize,
    own_h: i32,
    is_pit: bool,
) -> [i32; 4] {
    let h = |x: usize, z: usize| -> i32 { heights[x * padded_xz + z] };
    let c00 = corner_rule(
        [h(px - 1, pz - 1), h(px, pz - 1), h(px - 1, pz), own_h],
        own_h,
        is_pit,
    );
    let c10 = corner_rule(
        [h(px, pz - 1), h(px + 1, pz - 1), own_h, h(px + 1, pz)],
        own_h,
        is_pit,
    );
    let c01 = corner_rule(
        [h(px - 1, pz), own_h, h(px - 1, pz + 1), h(px, pz + 1)],
        own_h,
        is_pit,
    );
    let c11 = corner_rule(
        [own_h, h(px + 1, pz), h(px, pz + 1), h(px + 1, pz + 1)],
        own_h,
        is_pit,
    );
    [c00, c10, c01, c11]
}

/// Valley-aware corner rule.
///
/// - `delta == 0`: flat → corner = `own_h`.
/// - `delta == 1`: single-unit transition.
///     - Isolated pit branch fires only when `is_pit` is true AND `own_h` is
///       the unique minimum of the 2×2: corner = `own_h`. Keeps a 1-cell
///       pit's floor at its own height. At non-pit cells (e.g. the inside
///       concave corner of a plateau, which has the same 2×2 signature) this
///       branch is skipped so the corner lifts to `max`.
///     - Otherwise (peak, peak-adjacent, slope, multi-cell pit edge): corner = `max`.
///       Rises to meet the higher neighbor / tip.
/// - `delta >= 2`: cliff → corner = `own_h`.
/// - SENTINEL neighbor (no surface in that column): treat as cliff → `own_h`.
fn corner_rule(values: [i32; 4], own_h: i32, is_pit: bool) -> i32 {
    if values.iter().any(|&v| v == SENTINEL_NO_SURFACE) {
        return own_h;
    }
    let max = *values.iter().max().unwrap();
    let min = *values.iter().min().unwrap();
    match max - min {
        0 => own_h,
        1 => {
            let own_count = values.iter().filter(|&&v| v == own_h).count();
            if is_pit && own_h == min && own_count == 1 {
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

/// Pick the diagonal so a single-odd-corner cell renders as a flat triangle
/// (three equal corners) plus one tilted triangle (containing the odd corner).
/// Splitting THROUGH the odd corner would emit two tilted triangles meeting
/// at a ridge line, visible as a spike at the cell. Two-distinct-pair saddles
/// fall through to the historical "split along the higher pair" rule.
fn choose_diagonal(corners: &[i32; 4]) -> Diagonal {
    let [c00, c10, c01, c11] = *corners;
    let vals = [c00, c10, c01, c11];
    for i in 0..4 {
        let others = [vals[(i + 1) % 4], vals[(i + 2) % 4], vals[(i + 3) % 4]];
        if others[0] == others[1] && others[1] == others[2] && vals[i] != others[0] {
            return match i {
                0 | 3 => Diagonal::Anti,
                1 | 2 => Diagonal::Main,
                _ => unreachable!(),
            };
        }
    }
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

/// Trapezoidal wall segment between two adjacent columns, sized from the
/// columns' classified corner heights at the two endpoints of the shared
/// mesh edge (not raw column heights). `axis_len` spans the parallel axis in
/// cells. When `y_lo_west == y_hi_west` or `y_lo_east == y_hi_east`, one
/// endpoint is a single point — the wall degenerates to a triangle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WallRect {
    axis: EdgeAxis,
    /// For `EdgeAxis::X`: edge at mesh_x = `base_px + 1`; z-range
    /// `[base_pz, base_pz + axis_len]`. For `EdgeAxis::Z`: edge at
    /// mesh_z = `base_pz + 1`; x-range `[base_px, base_px + axis_len]`.
    base_px: usize,
    base_pz: usize,
    axis_len: usize,
    y_lo_west: i32,
    y_hi_west: i32,
    y_lo_east: i32,
    y_hi_east: i32,
    normal_positive: bool,
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
            y_lo_west: r.y_lo_west,
            y_lo_east: r.y_lo_east,
            y_hi_west: r.y_hi_west,
            y_hi_east: r.y_hi_east,
            normal_positive: r.normal_positive,
        }
    }
}

enum WallEmission {
    Skip,
    Rectangle(WallRect),
}

/// Phase-4 wall emitter. Iterates +x and +z shared edges (one-sided, so
/// neighbor chunks own their east/south boundaries; avoids duplicates).
/// Classifies each edge as Skip / Triangle (saddle) / Rectangle. Triangles
/// emit immediately; rectangles are collected and greedy-merged along their
/// parallel axis before emission.
fn emit_walls(
    surface_heights: &[i32],
    tops: &[Option<ColumnTop>],
    padded_xz: usize,
    out: &mut MeshBuffers,
) {
    let mut rects: Vec<WallRect> = Vec::new();

    for pz in 1..padded_xz - 1 {
        for px in 1..padded_xz - 1 {
            match classify_edge_x(surface_heights, tops, padded_xz, px, pz) {
                WallEmission::Skip => {}
                WallEmission::Rectangle(rect) => rects.push(rect),
            }
        }
    }
    for px in 1..padded_xz - 1 {
        for pz in 1..padded_xz - 1 {
            match classify_edge_z(surface_heights, tops, padded_xz, px, pz) {
                WallEmission::Skip => {}
                WallEmission::Rectangle(rect) => rects.push(rect),
            }
        }
    }

    for quad in merge_walls_with_tilt(rects) {
        emit_wall_quad(&quad, out);
    }
}

fn classify_edge_x(
    surface_heights: &[i32],
    tops: &[Option<ColumnTop>],
    padded_xz: usize,
    px: usize,
    pz: usize,
) -> WallEmission {
    let surf_a = surface_heights[px * padded_xz + pz];
    let surf_b = surface_heights[(px + 1) * padded_xz + pz];
    if surf_a == SENTINEL_NO_SURFACE || surf_b == SENTINEL_NO_SURFACE {
        return WallEmission::Skip;
    }
    let ca = column_corners_or_flat(&tops[px * padded_xz + pz], surf_a);
    let cb = column_corners_or_flat(&tops[(px + 1) * padded_xz + pz], surf_b);
    // +x shared edge: mesh x = px+1, running from (px+1, pz) to (px+1, pz+1).
    // a.c10 at (px+1, pz), a.c11 at (px+1, pz+1).
    // b.c00 at (px+1, pz), b.c01 at (px+1, pz+1).
    classify_edge_from_corners(EdgeAxis::X, px, pz, ca[1], ca[3], cb[0], cb[2])
}

fn classify_edge_z(
    surface_heights: &[i32],
    tops: &[Option<ColumnTop>],
    padded_xz: usize,
    px: usize,
    pz: usize,
) -> WallEmission {
    let surf_a = surface_heights[px * padded_xz + pz];
    let surf_b = surface_heights[px * padded_xz + pz + 1];
    if surf_a == SENTINEL_NO_SURFACE || surf_b == SENTINEL_NO_SURFACE {
        return WallEmission::Skip;
    }
    let ca = column_corners_or_flat(&tops[px * padded_xz + pz], surf_a);
    let cb = column_corners_or_flat(&tops[px * padded_xz + pz + 1], surf_b);
    // +z shared edge: mesh z = pz+1, running from (px, pz+1) to (px+1, pz+1).
    // a.c01 at (px, pz+1), a.c11 at (px+1, pz+1).
    // b.c00 at (px, pz+1), b.c10 at (px+1, pz+1).
    classify_edge_from_corners(EdgeAxis::Z, px, pz, ca[2], ca[3], cb[0], cb[1])
}

/// Corner-aware edge classification. `a_w`/`a_e` are `a`'s corner heights at
/// the west/east endpoints of the shared edge; `b_w`/`b_e` are `b`'s at the
/// same endpoints. Emits `Skip` when the two columns' top surfaces mate along
/// the full edge (no cliff), otherwise a `WallRect` whose four y values are
/// `min`/`max` of each endpoint. A truly-triangular cliff (one endpoint
/// matches, the other has a drop) is represented by equal y_lo/y_hi on the
/// matching side — emission handles the degenerate case.
fn classify_edge_from_corners(
    axis: EdgeAxis,
    px: usize,
    pz: usize,
    a_w: i32,
    a_e: i32,
    b_w: i32,
    b_e: i32,
) -> WallEmission {
    if a_w == b_w && a_e == b_e {
        return WallEmission::Skip;
    }
    let normal_positive = if a_w != b_w { a_w > b_w } else { a_e > b_e };
    WallEmission::Rectangle(WallRect {
        axis,
        base_px: px,
        base_pz: pz,
        axis_len: 1,
        y_lo_west: a_w.min(b_w),
        y_hi_west: a_w.max(b_w),
        y_lo_east: a_e.min(b_e),
        y_hi_east: a_e.max(b_e),
        normal_positive,
    })
}

/// Return a column's 4 corner heights, or `[surf; 4]` when the column has no
/// classified top (SENTINEL top but a below-padding surface is visible).
fn column_corners_or_flat(top: &Option<ColumnTop>, surf: i32) -> [i32; 4] {
    match top {
        Some(ColumnTop::Planar { corners, .. }) | Some(ColumnTop::NonPlanar { corners, .. }) => {
            *corners
        }
        None => [surf, surf, surf, surf],
    }
}

/// Merge adjacent `WallRect`s into `WallQuad`s. Consecutive 1-unit rects along
/// the parallel axis merge when (a) they're continuous at the shared seam
/// (east endpoint of one equals west endpoint of the next), and (b) their
/// west→east gradient in both `y_lo` and `y_hi` matches the run's existing
/// gradient — so the combined polyline is a straight edge renderable as one
/// linear quad. Each rect's four corners already track the slope-plane
/// endpoints exactly (since corners come from the classified `tops[]`),
/// so no post-hoc plane correction is needed.
fn merge_walls_with_tilt(rects: Vec<WallRect>) -> Vec<WallQuad> {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<(EdgeAxis, usize, bool), Vec<WallRect>> = BTreeMap::new();
    for r in rects {
        let perp = match r.axis {
            EdgeAxis::X => r.base_px,
            EdgeAxis::Z => r.base_pz,
        };
        groups
            .entry((r.axis, perp, r.normal_positive))
            .or_default()
            .push(r);
    }

    let mut out = Vec::new();
    for ((_axis, _perp, _normal), mut group) in groups {
        group.sort_by_key(|r| match r.axis {
            EdgeAxis::X => r.base_pz,
            EdgeAxis::Z => r.base_px,
        });

        let mut run: Option<WallQuad> = None;
        let mut gradient: Option<(i32, i32)> = None;

        for rect in group {
            let parallel_start = match rect.axis {
                EdgeAxis::X => rect.base_pz,
                EdgeAxis::Z => rect.base_px,
            };
            let rect_quad = WallQuad::from(rect);
            debug_assert_eq!(
                rect.axis_len, 1,
                "merge_walls_with_tilt expects 1-unit input rects"
            );
            let rect_delta_lo = rect.y_lo_east - rect.y_lo_west;
            let rect_delta_hi = rect.y_hi_east - rect.y_hi_west;

            match &mut run {
                None => {
                    run = Some(rect_quad);
                    gradient = Some((rect_delta_lo, rect_delta_hi));
                }
                Some(r) => {
                    let r_end = match r.axis {
                        EdgeAxis::X => r.base_pz + r.axis_len,
                        EdgeAxis::Z => r.base_px + r.axis_len,
                    };
                    let continuous = r_end == parallel_start
                        && r.y_lo_east == rect.y_lo_west
                        && r.y_hi_east == rect.y_hi_west;
                    let (g_lo, g_hi) = gradient.expect("gradient set alongside run");
                    if continuous && rect_delta_lo == g_lo && rect_delta_hi == g_hi {
                        r.axis_len += rect.axis_len;
                        r.y_lo_east = rect.y_lo_east;
                        r.y_hi_east = rect.y_hi_east;
                    } else {
                        out.push(*r);
                        run = Some(rect_quad);
                        gradient = Some((rect_delta_lo, rect_delta_hi));
                    }
                }
            }
        }
        if let Some(r) = run {
            out.push(r);
        }
    }
    out
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

/// Emit a wall on the `+x`/`-x` plane at `x`, with potentially-different
/// y ranges at each z endpoint (`z1` = west/low-z, `z2` = east/high-z). When
/// one endpoint's `y_lo == y_hi`, that end collapses to a point and the wall
/// is emitted as a single triangle (avoids degenerate triangles in the
/// trimesh collider). Otherwise two triangles form the trapezoid.
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
    let west_deg = y_lo_west >= y_hi_west;
    let east_deg = y_lo_east >= y_hi_east;
    if west_deg && east_deg {
        return;
    }
    let axis_normal = if normal_positive {
        [1.0, 0.0, 0.0]
    } else {
        [-1.0, 0.0, 0.0]
    };
    if west_deg {
        // Triangle: west vertex alone at (y_lo_west==y_hi_west), east edge full.
        let p_w = [x, y_lo_west, z1];
        let p_lo_e = [x, y_lo_east, z2];
        let p_hi_e = [x, y_hi_east, z2];
        // For normal +x, order (p_w, p_hi_e, p_lo_e) is CCW.
        let (v0, v1, v2) = if normal_positive {
            (p_w, p_hi_e, p_lo_e)
        } else {
            (p_w, p_lo_e, p_hi_e)
        };
        push_wall_triangle(out, v0, v1, v2, axis_normal);
        return;
    }
    if east_deg {
        let p_lo_w = [x, y_lo_west, z1];
        let p_hi_w = [x, y_hi_west, z1];
        let p_e = [x, y_lo_east, z2];
        let (v0, v1, v2) = if normal_positive {
            (p_lo_w, p_hi_w, p_e)
        } else {
            (p_hi_w, p_lo_w, p_e)
        };
        push_wall_triangle(out, v0, v1, v2, axis_normal);
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
    let west_deg = y_lo_west >= y_hi_west;
    let east_deg = y_lo_east >= y_hi_east;
    if west_deg && east_deg {
        return;
    }
    let axis_normal = if normal_positive {
        [0.0, 0.0, 1.0]
    } else {
        [0.0, 0.0, -1.0]
    };
    if west_deg {
        let p_w = [x1, y_lo_west, z];
        let p_lo_e = [x2, y_lo_east, z];
        let p_hi_e = [x2, y_hi_east, z];
        // For normal +z, order (p_w, p_lo_e, p_hi_e) is CCW.
        let (v0, v1, v2) = if normal_positive {
            (p_w, p_lo_e, p_hi_e)
        } else {
            (p_w, p_hi_e, p_lo_e)
        };
        push_wall_triangle(out, v0, v1, v2, axis_normal);
        return;
    }
    if east_deg {
        let p_lo_w = [x1, y_lo_west, z];
        let p_hi_w = [x1, y_hi_west, z];
        let p_e = [x2, y_lo_east, z];
        let (v0, v1, v2) = if normal_positive {
            (p_hi_w, p_lo_w, p_e)
        } else {
            (p_lo_w, p_hi_w, p_e)
        };
        push_wall_triangle(out, v0, v1, v2, axis_normal);
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

fn push_wall_triangle(
    out: &mut MeshBuffers,
    v0: [f32; 3],
    v1: [f32; 3],
    v2: [f32; 3],
    normal: [f32; 3],
) {
    let base = out.positions.len() as u32;
    out.positions.extend_from_slice(&[v0, v1, v2]);
    out.normals.extend_from_slice(&[normal; 3]);
    out.uvs
        .extend_from_slice(&[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]);
    out.indices.extend_from_slice(&[base, base + 1, base + 2]);
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
    fn choose_diagonal_beveled_tip_for_single_odd_corner() {
        // Single raised corner on main diagonal → split anti-diagonal.
        assert_eq!(choose_diagonal(&[5, 4, 4, 4]), Diagonal::Anti); // c00 raised
        assert_eq!(choose_diagonal(&[4, 4, 4, 5]), Diagonal::Anti); // c11 raised
        // Single raised corner on anti diagonal → split main.
        assert_eq!(choose_diagonal(&[4, 5, 4, 4]), Diagonal::Main); // c10 raised
        assert_eq!(choose_diagonal(&[4, 4, 5, 4]), Diagonal::Main); // c01 raised
        // Single sunk corner follows the same rule.
        assert_eq!(choose_diagonal(&[0, 1, 1, 1]), Diagonal::Anti); // c00 sunk
        assert_eq!(choose_diagonal(&[1, 0, 1, 1]), Diagonal::Main); // c10 sunk
    }

    #[test]
    fn choose_diagonal_tie_defaults_to_main() {
        // No odd corner → fallthrough. Equal-sum tie keeps Main.
        assert_eq!(choose_diagonal(&[3, 3, 3, 3]), Diagonal::Main);
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

        // Pyramid single-raised-corner hip cells render under Phase 4.6 as
        // beveled tips: one flat triangle (normal = (0, 1, 0)) at the lower
        // level plus one tilted triangle with normal along the `(1, 1, 1)/√3`
        // family (all three components ≈ 0.577, one of `n[0]`/`n[2]` possibly
        // negative depending on which corner is raised). We assert that at
        // least one such tilted normal exists — the flat component blends
        // in with the pyramid's planar rings, so we don't assert on it.
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
        const S: f32 = 0.5773;
        let hip_count = normals
            .iter()
            .filter(|n| {
                (n[0].abs() - S).abs() < 0.02
                    && (n[1] - S).abs() < 0.02
                    && (n[2].abs() - S).abs() < 0.02
            })
            .count();
        assert!(
            hip_count > 0,
            "pyramid should emit beveled-tip tilted normals (x≈±0.577, y≈0.577, z≈±0.577); got {} of {}",
            hip_count,
            normals.len()
        );
    }

    #[test]
    fn corner_rule_preserves_isolated_pit() {
        // All cases below gate pit preservation on `is_pit=true` (the cell
        // passed `is_true_pit`). `corner_rule_lifts_isolated_at_non_pit_cell`
        // covers the `is_pit=false` inside-concave-corner case.
        // Isolated pit: own=2 surrounded by 3 neighbors at 3. Keep own (valley).
        assert_eq!(corner_rule([3, 3, 3, 2], 2, true), 2);
        // Staircase slope east corner: own=3 with 2x max=4. Rise to meet.
        assert_eq!(corner_rule([3, 4, 3, 4], 3, true), 4);
        // Staircase slope west corner: own=3 with min=2, max=3. Stay at max (=own).
        assert_eq!(corner_rule([2, 3, 2, 3], 3, true), 3);
        // Peak: own=2 with 3 lower neighbors. own is max → stay.
        assert_eq!(corner_rule([1, 1, 1, 2], 2, true), 2);
        // Pyramid edge diagonal: own=4 with 3 same + 1 tip at 5. Hip ridge → rise.
        assert_eq!(corner_rule([5, 4, 4, 4], 4, true), 5);
        // Multi-cell pit edge: own=2 with 2x non-pit at 3. Slope up (ramp into pit).
        assert_eq!(corner_rule([2, 3, 2, 3], 2, true), 3);
        // Flat: all same. No change.
        assert_eq!(corner_rule([5, 5, 5, 5], 5, true), 5);
        // Cliff: delta >= 2. Keep own.
        assert_eq!(corner_rule([5, 2, 5, 2], 2, true), 2);
        // SENTINEL neighbor: cliff-like. Keep own.
        assert_eq!(corner_rule([SENTINEL_NO_SURFACE, 3, 3, 3], 3, true), 3);
    }

    #[test]
    fn corner_rule_lifts_isolated_at_non_pit_cell() {
        // Same `[H, H, H, own]` signature as an isolated pit, but the cell
        // was detected as non-pit (e.g. an inside concave corner of a
        // plateau): the ISOLATED-PIT branch is skipped, corner lifts to max.
        assert_eq!(corner_rule([3, 3, 3, 2], 2, false), 3);
        // Cliffs, flats, and non-isolated deltas are unaffected by `is_pit`.
        assert_eq!(corner_rule([5, 2, 5, 2], 2, false), 2);
        assert_eq!(corner_rule([5, 5, 5, 5], 5, false), 5);
        assert_eq!(corner_rule([3, 4, 3, 4], 3, false), 4);
        assert_eq!(corner_rule([5, 4, 4, 4], 4, false), 5);
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
    fn merge_walls_with_tilt_combines_arithmetic_progression() {
        // 6 consecutive 1-unit +z-axis wall rects, each tilted by 1 unit in
        // y_hi (west→east), continuous at their shared seams, merge into ONE
        // WallQuad spanning the full run with tilted y_hi 4 (west) → 10 (east).
        let mut rects = Vec::new();
        for i in 0..6 {
            rects.push(WallRect {
                axis: EdgeAxis::Z,
                base_px: 11 + i,
                base_pz: 5,
                axis_len: 1,
                y_lo_west: 2,
                y_lo_east: 2,
                y_hi_west: 4 + i as i32,
                y_hi_east: 5 + i as i32,
                normal_positive: true,
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
        assert_eq!(q.y_hi_east, 10);
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

    /// Run the classification + extension passes against a heightmap-derived
    /// voxel grid and return the resulting `tops` array. Used by the
    /// cliff-pulled-extension tests below.
    fn tops_for_heightmap<F: Fn(i32, i32) -> i32>(f: F) -> (Vec<Option<ColumnTop>>, usize) {
        let voxels = heightmap_voxels(f);
        let shape = padded_shape();
        let padded_xz = shape.as_array()[0] as usize;
        let heights = column_heights(&voxels, &shape);
        let materials = column_materials(&voxels, &shape);
        let mut tops = classify_columns(&heights, &materials, padded_xz);
        extend_slopes_over_nonplanar(&mut tops, &heights, padded_xz);
        (tops, padded_xz)
    }

    /// Pyramid hip column `(1, 1)` has corners `[3,2,2,2]` (pulled up at c00 by
    /// the tip). Its W neighbor `(0, 1)` classifies Planar with `[3,3,2,2]`
    /// (gx=0, gz=-1). Pre-fix, the ≥3-match rule adopted the W plane and
    /// erased the hip; post-fix, c10's 2×2 neighborhood has delta=1 (not
    /// cliff), so adoption is rejected and the hip survives.
    #[test]
    fn extension_rejects_pyramid_hip_corner() {
        let (tops, padded_xz) = tops_for_heightmap(|wx, wz| match wx.abs().max(wz.abs()) {
            0 => 3,
            1 => 2,
            2 => 1,
            _ => 0,
        });
        // world (1, 1) → padded (2, 2). heights[] stores top-face-y = h + 2.
        let top = tops[2 * padded_xz + 2];
        assert!(
            matches!(
                top,
                Some(ColumnTop::NonPlanar {
                    corners: [5, 4, 4, 4],
                    ..
                })
            ),
            "pyramid hip at padded (2, 2) should remain NonPlanar [5, 4, 4, 4] (top-face), got {:?}",
            top
        );
    }

    /// Saddle SE hip column `(1, 8)` has corners `[1,0,1,1]`. Its N neighbor
    /// `(1, 7)` classifies Planar with `[1,0,1,0]` (gx=-1, gz=0). Pre-fix,
    /// adoption flipped c11 down from 1 to 0, collapsing the two-direction
    /// hip into a one-direction slope. Post-fix, c11's 2×2 delta is 1 (not
    /// cliff), so adoption is rejected.
    #[test]
    fn extension_rejects_saddle_se_hip_corner() {
        // Replica of original saddle shifted to center (3, 3) for padding
        // comfort. Corresponds to original column (1, 8) → here (4, 3).
        let saddle = |wx: i32, wz: i32| -> i32 {
            if (1..=5).contains(&wx) && (1..=5).contains(&wz) {
                ((wx > 3) == (wz > 3)) as i32
            } else {
                0
            }
        };
        let (tops, padded_xz) = tops_for_heightmap(saddle);
        // world (4, 3) → padded (5, 4). Heights stored with +2 offset.
        let top = tops[5 * padded_xz + 4];
        assert!(
            matches!(
                top,
                Some(ColumnTop::NonPlanar {
                    corners: [3, 2, 3, 3],
                    ..
                })
            ),
            "saddle SE hip at padded (5, 4) should remain NonPlanar [3, 2, 3, 3] (top-face), got {:?}",
            top
        );
    }

    /// Staircase north-edge column with corners `[h, h+1, h, h]` extends to
    /// `[h, h+1, h, h+1]` because c11's 2×2 neighborhood spans the cliff
    /// (delta ≥ 2), making it cliff-pulled.
    #[test]
    fn extension_accepts_staircase_edge_corner() {
        // Staircase inside wx ∈ [0, 7], wz ∈ [0, 4]: h = wx + 1. Else 0.
        let staircase = |wx: i32, wz: i32| -> i32 {
            if (0..=7).contains(&wx) && (0..=4).contains(&wz) {
                wx + 1
            } else {
                0
            }
        };
        let (tops, padded_xz) = tops_for_heightmap(staircase);
        // world (3, 4) → padded (4, 5). own_h = 4 + 2 = 6 (top-face).
        let top = tops[4 * padded_xz + 5];
        let expected = [6, 7, 6, 7];
        assert!(
            matches!(
                top,
                Some(ColumnTop::Planar { corners, .. }) if corners == expected
            ),
            "staircase north-edge at padded (4, 5) should extend to Planar {:?}, got {:?}",
            expected,
            top
        );
    }

    /// Staircase *west* end where the far-side height drop is only 1 unit.
    /// The naïve "far-side drop ≥ 2" heuristic rejects this, but the c11
    /// corner IS cliff-pulled because the 2×2 neighborhood spans the drop
    /// on the east-plus-south diagonal (delta = 2). Fix must accept it.
    #[test]
    fn extension_accepts_small_drop_edge_corner() {
        let staircase = |wx: i32, wz: i32| -> i32 {
            if (0..=7).contains(&wx) && (0..=4).contains(&wz) {
                wx + 1
            } else {
                0
            }
        };
        let (tops, padded_xz) = tops_for_heightmap(staircase);
        // world (0, 4) → padded (1, 5). own_h = 0 + 1 + 2 = 3 (top-face).
        // Corners before extension: [3, 4, 3, 3] (c11 cliff-pulled).
        // Corners after extension: [3, 4, 3, 4] (Planar).
        let top = tops[1 * padded_xz + 5];
        let expected = [3, 4, 3, 4];
        assert!(
            matches!(
                top,
                Some(ColumnTop::Planar { corners, .. }) if corners == expected
            ),
            "staircase west-end north edge at padded (1, 5) should extend to Planar {:?}, got {:?}",
            expected,
            top
        );
    }

    /// L-shape in SteppedGenerator places an inside concave corner cell at
    /// world (-20, -8). Pre-4.7b it classified as saddle [0, 1, 1, 0] (stored
    /// heights [2, 3, 3, 2]) and rendered two tilted triangles with no flat
    /// component. Post-4.7b: `is_true_pit` returns false (cardinal S and E are
    /// equal to own), the ISOLATED-PIT branch is skipped at c00, and the
    /// corner lifts to max=3 — giving [3, 3, 3, 2] (single-sunk c11), which
    /// Phase 4.6's diagonal rule emits as a flat triangle at y=3 plus one
    /// tilted triangle dropping to c11.
    #[test]
    fn inside_concave_corner_emits_beveled_top() {
        use crate::config::VoxelGeneratorImpl;
        use crate::terrain::SteppedGenerator;

        let generator = SteppedGenerator {
            chunk_size: 16,
            shape: padded_shape(),
        };
        // world (-20, -8) → chunk (-2, 0, -1), padded (13, 9).
        let voxels = generator.generate_terrain(IVec3::new(-2, 0, -1));
        let shape = padded_shape();
        let padded_xz = shape.as_array()[0] as usize;
        let heights = column_heights(&voxels, &shape);
        let materials = column_materials(&voxels, &shape);
        let tops = classify_columns(&heights, &materials, padded_xz);
        let top = tops[13 * padded_xz + 9];
        assert!(
            matches!(
                top,
                Some(ColumnTop::NonPlanar {
                    corners: [3, 3, 3, 2],
                    ..
                })
            ),
            "inside concave corner at world (-20, -8) should classify NonPlanar [3, 3, 3, 2] (top-face), got {:?}",
            top
        );
    }

    /// 1-cell pit in SteppedGenerator (cell at world (-20, -2) carved out of
    /// a surrounding h=1 plateau). All 8 neighbors strictly higher →
    /// `is_true_pit` returns true → ISOLATED-PIT preservation fires at every
    /// corner. Cell stays flat at own_h.
    #[test]
    fn single_cell_pit_stays_flat() {
        use crate::config::VoxelGeneratorImpl;
        use crate::terrain::SteppedGenerator;

        let generator = SteppedGenerator {
            chunk_size: 16,
            shape: padded_shape(),
        };
        // world (-20, -2) → chunk (-2, 0, -1), padded (13, 15).
        let voxels = generator.generate_terrain(IVec3::new(-2, 0, -1));
        let shape = padded_shape();
        let padded_xz = shape.as_array()[0] as usize;
        let heights = column_heights(&voxels, &shape);
        let materials = column_materials(&voxels, &shape);
        let tops = classify_columns(&heights, &materials, padded_xz);
        let top = tops[13 * padded_xz + 15];
        assert!(
            matches!(
                top,
                Some(ColumnTop::Planar {
                    corners: [2, 2, 2, 2],
                    gradient_x: 0,
                    gradient_z: 0,
                    ..
                })
            ),
            "1-cell pit at world (-20, -2) should classify Planar flat [2, 2, 2, 2], got {:?}",
            top
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
