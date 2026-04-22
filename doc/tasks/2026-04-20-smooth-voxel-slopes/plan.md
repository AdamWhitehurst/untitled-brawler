# Implementation Plan

## Overview

Introduce `mesh_chunk_sloped` alongside `mesh_chunk_greedy`, toggled per-map via a new `MeshingMode` field on `VoxelMapConfig`. Implement in four algorithmic passes (heights → corner classification → top emission → wall emission), rolling in each surface-shape class incrementally. Server collider and client mesh stay automatically consistent because both dispatch through the same toggle.

---

## Phase 1: Toggle Infrastructure + Stub Dispatch

### Changes

#### 1. `MeshingMode` enum + `VoxelMapConfig` field
**File**: `crates/voxel_map_engine/src/config.rs`
**Action**: modify

```rust
/// Selects the chunk meshing algorithm for a map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize, Reflect)]
pub enum MeshingMode {
    /// Axis-aligned greedy quads via `block-mesh-rs`. Stepped surfaces.
    #[default]
    Cubic,
    /// Corner-max sloped meshing. Single-unit elevation changes become 45° faces.
    Sloped,
}

#[derive(Component)]
pub struct VoxelMapConfig {
    pub seed: u64,
    pub generation_version: u32,
    pub spawning_distance: u32,
    pub save_dir: Option<PathBuf>,
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
        debug_assert!(spawning_distance > 0, "VoxelMapConfig: spawning_distance must be > 0");
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
```

All existing `VoxelMapConfig::new(...)` call sites (enumerated in research: `server/src/map.rs:225,1214`; `client/src/transition.rs:138`; all examples; all tests) continue to work because the field defaults to `Cubic`.

#### 2. Dispatcher + stub
**File**: `crates/voxel_map_engine/src/meshing.rs`
**Action**: modify

```rust
// Re-export so prelude picks it up (prelude already re-exports `meshing::*`).
pub use crate::config::MeshingMode;

/// Mesh a padded voxel array using the selected algorithm.
pub fn mesh_chunk<S: Shape<3, Coord = u32>>(
    voxels: &[WorldVoxel],
    shape: &S,
    mode: MeshingMode,
) -> Option<Mesh> {
    match mode {
        MeshingMode::Cubic => mesh_chunk_greedy(voxels, shape),
        MeshingMode::Sloped => mesh_chunk_sloped(voxels, shape),
    }
}

/// Slope-aware mesher. Phase 1: delegates to greedy (stub for plumbing).
pub fn mesh_chunk_sloped<S: Shape<3, Coord = u32>>(
    voxels: &[WorldVoxel],
    shape: &S,
) -> Option<Mesh> {
    mesh_chunk_greedy(voxels, shape)
}
```

#### 3. Thread `mode` through generation tasks
**File**: `crates/voxel_map_engine/src/generation.rs`
**Action**: modify

Replace `use crate::meshing::mesh_chunk_greedy;` with `use crate::meshing::{mesh_chunk, MeshingMode};`.

Add `mode: MeshingMode` parameter to both `spawn_terrain_batch` (`generation.rs:44`) and `spawn_mesh_task` (`generation.rs:137`). Move `mode` into the async closures. Replace both `mesh_chunk_greedy(&voxels, &shape)` call sites (`generation.rs:73, 149`) with `mesh_chunk(&voxels, &shape, mode)`.

```rust
pub fn spawn_terrain_batch(
    pending: &mut PendingChunks,
    positions: Vec<IVec3>,
    generator: &VoxelGenerator,
    shape: RuntimeShape<u32, 3>,
    chunk_store: Option<FsChunkStore>,
    entity_store: Option<FsChunkEntitiesStore>,
    mode: MeshingMode,
) { /* capture `mode` in the spawned async task, pass to mesh_chunk */ }

pub fn spawn_mesh_task(
    pending: &mut PendingChunks,
    position: IVec3,
    voxels: Vec<WorldVoxel>,
    shape: RuntimeShape<u32, 3>,
    mode: MeshingMode,
) { /* same */ }
```

#### 4. Thread `mode` through lifecycle
**File**: `crates/voxel_map_engine/src/lifecycle.rs`
**Action**: modify

Replace `use crate::meshing::mesh_chunk_greedy;` (line 17) with `use crate::meshing::{mesh_chunk, MeshingMode};`.

`drain_gen_queue` (line 615) — add `meshing_mode: MeshingMode` parameter. Thread into `spawn_terrain_batch` (lines 690, 758) and `spawn_mesh_task` (line 743) calls.

`update_chunks` (line 310) already queries `&VoxelMapConfig` (line 314); extract `config.meshing_mode` and pass to `drain_gen_queue` at the call site (line 386):

```rust
drain_gen_queue(
    &mut instance, &mut pending, &mut gen_queue, &mut tracker,
    dimensions, generator, &budget, entity_store, chunk_store,
    config.meshing_mode,
);
```

`spawn_remesh_tasks` (line 1019) — add `&VoxelMapConfig` to the map_query tuple, extract `meshing_mode`, and replace the closure body (line 1085):

```rust
pub fn spawn_remesh_tasks(
    mut map_query: Query<(
        &mut VoxelMapInstance,
        &mut PendingRemeshes,
        &ChunkWorkBudget,
        &TicketLevelPropagator,
        &mut ChunkWorkTracker,
        &VoxelMapConfig,
    )>,
) {
    // ... iterate tuple destructured with `config` in scope
    let mode = config.meshing_mode;
    let task = pool.spawn(async move { mesh_chunk(&voxels, &shape, mode) });
    trace!("spawn_remesh_tasks: chunk {:?} dispatched with meshing_mode={:?}",
           work.position, mode);
}
```

At the top of the `drain_gen_queue` function body, add:
```rust
trace!("drain_gen_queue: meshing_mode={:?}", meshing_mode);
```

#### 5. `SteppedGenerator`
**File**: `crates/voxel_map_engine/src/terrain.rs`
**Action**: modify (append)

```rust
/// Deterministic fixture for verifying sloped meshing.
///
/// Emits a surface with:
/// - staircase strip (1D slope test) in x=-8..=-1
/// - 3x3 pyramid (hip corners) centered at (0, 0) in world coords
/// - cross-shaped depression (valley corners) at (8, 0)
/// - saddle (two diagonal peaks and two low corners) at (0, 8)
/// - multi-unit cliff (Δ≥2 stays cubic) at (-8, 8)
pub struct SteppedGenerator {
    pub chunk_size: u32,
    pub shape: RuntimeShape<u32, 3>,
}

impl VoxelGeneratorImpl for SteppedGenerator {
    fn generate_terrain(&self, chunk_pos: IVec3) -> Vec<WorldVoxel> {
        let mut voxels = vec![WorldVoxel::Air; self.shape.usize()];
        for i in 0..self.shape.size() {
            let [px, py, pz] = self.shape.delinearize(i);
            let world_x = chunk_pos.x * self.chunk_size as i32 + px as i32 - 1;
            let world_y = chunk_pos.y * self.chunk_size as i32 + py as i32 - 1;
            let world_z = chunk_pos.z * self.chunk_size as i32 + pz as i32 - 1;
            let h = stepped_surface_height(world_x, world_z);
            if world_y <= h {
                voxels[i as usize] = WorldVoxel::Solid(0);
            }
        }
        voxels
    }
}

/// Height function for [`SteppedGenerator`]. Kept free-standing so tests
/// can exercise expected heights without materializing a full chunk.
fn stepped_surface_height(x: i32, z: i32) -> i32 {
    // Staircase: rises 1 per unit in `x` for x ∈ -8..=-1 at z ∈ -4..=4
    if (-8..=-1).contains(&x) && (-4..=4).contains(&z) {
        return x + 8; // 0..=7
    }
    // Pyramid centered at (0, 0): concentric diamond rings
    if (-3..=3).contains(&x) && (-3..=3).contains(&z) {
        return 3 - x.abs().max(z.abs());
    }
    // Cross depression centered at (8, 0): dig down in cardinal arms
    if (x, z) == (8, 0) || (5..=11).contains(&x) && z == 0
        || x == 8 && (-3..=3).contains(&z)
    {
        return -1;
    }
    // Saddle centered at (0, 8): two high corners, two low corners
    if (-2..=2).contains(&x) && (6..=10).contains(&z) {
        return if (x > 0) == (z > 8) { 1 } else { 0 };
    }
    // Multi-unit cliff at x ∈ [-11, -9], z ∈ [6, 10] (Δ=3, should stay cubic)
    if (-11..=-9).contains(&x) && (6..=10).contains(&z) {
        return 3;
    }
    0
}
```

#### 6. `stepped_terrain` example
**File**: `crates/voxel_map_engine/examples/stepped_terrain.rs`
**Action**: create (model on `examples/terrain.rs`)

```rust
use std::sync::Arc;
use bevy::prelude::*;
use voxel_map_engine::prelude::*;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(VoxelPlugin)
        .insert_resource(ChunkGenerationEnabled)
        .add_systems(Startup, setup)
        .add_systems(Update, move_camera)
        .run();
}

fn setup(mut commands: Commands) {
    let mut instance = VoxelMapInstance::new(5, 16);
    instance.debug_colors = true;

    let map_entity = commands
        .spawn((
            instance,
            VoxelMapConfig::new(0, 0, 5, true).with_meshing_mode(MeshingMode::Sloped),
            MapDimensions {
                chunk_size: 16,
                column_y_range: (-8, 8),
                tree_height: 5,
                bounds: None,
            },
            VoxelGenerator(Arc::new(SteppedGenerator {
                chunk_size: 16,
                shape: RuntimeShape::<u32, 3>::new([18, 18, 18]),
            })),
            PendingChunks::default(),
            Transform::default(),
        ))
        .id();

    commands.spawn((
        Camera3d::default(),
        Transform::from_translation(Vec3::new(30.0, 20.0, 30.0)).looking_at(Vec3::ZERO, Vec3::Y),
        ChunkTicket::new(map_entity, TicketType::Player, 5),
    ));

    commands.spawn((
        DirectionalLight {
            illuminance: 10000.0,
            shadows_enabled: true,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.5, 0.5, 0.0)),
    ));
}

// Reuse the `move_camera` system body verbatim from `examples/terrain.rs`.
fn move_camera(/* identical to examples/terrain.rs:55-86 */) { /* ... */ }
```

### Verification

#### Automated
- [x] `cargo check-all` passes
- [x] `cargo test-native` passes (all existing tests still green)
- [x] `cargo run --example terrain` renders identically to master (cubic default preserved)

#### Manual
- [x] `cargo run --example stepped_terrain` launches and renders the fixture **cubic** (stub phase — slopes come in Phase 2)
- [x] Tracy/log shows `drain_gen_queue: meshing_mode=Sloped` and `spawn_remesh_tasks: ... meshing_mode=Sloped` when running `stepped_terrain`
- [x] Same logs read `Cubic` when running `cargo run --example terrain`

---

## Phase 2: Planar Top Surfaces (1D Slopes + Flats)

### Changes

#### 1. Split out submodule
**File**: `crates/voxel_map_engine/src/meshing.rs`
**Action**: modify

Add at top of `meshing.rs`:
```rust
mod sloped;
pub use sloped::mesh_chunk_sloped;
```

Remove the Phase 1 stub `pub fn mesh_chunk_sloped` body from `meshing.rs`.

#### 2. Sloped mesher: passes 1-3a + simple walls + bottoms
**File**: `crates/voxel_map_engine/src/meshing/sloped.rs`
**Action**: create

```rust
use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use ndshape::Shape;

use crate::types::WorldVoxel;

/// Internal mesh accumulator.
struct MeshBuffers {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    indices: Vec<u32>,
}

impl MeshBuffers {
    fn new() -> Self {
        Self { positions: vec![], normals: vec![], uvs: vec![], indices: vec![] }
    }

    fn is_empty(&self) -> bool { self.indices.is_empty() }

    fn into_mesh(self) -> Mesh {
        let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
        mesh.try_insert_attribute(Mesh::ATTRIBUTE_POSITION, self.positions).unwrap();
        mesh.try_insert_attribute(Mesh::ATTRIBUTE_NORMAL, self.normals).unwrap();
        mesh.try_insert_attribute(Mesh::ATTRIBUTE_UV_0, self.uvs).unwrap();
        mesh.try_insert_indices(Indices::U32(self.indices)).unwrap();
        mesh
    }
}

/// Top-surface classification per column.
enum ColumnTop {
    /// Corners lie on the plane `y = gx*x + gz*z + intercept`.
    Planar { gradient_x: i32, gradient_z: i32, intercept: i32, material: u8 },
    /// Corners are not coplanar. Phase 2 renders these as cubic tops
    /// (fallback); Phase 3 emits hip/valley triangles here.
    NonPlanar { corners: [i32; 4], material: u8 },
}

const SENTINEL_NO_SURFACE: i32 = i32::MIN;

/// Entry point. Produces a Mesh in the same vertex-attribute layout as
/// `mesh_chunk_greedy` so `Collider::trimesh_from_mesh` accepts it.
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
    emit_nonplanar_tops_cubic_fallback(&tops, padded_xz, &mut buf); // replaced in Phase 3
    emit_cubic_walls_simple(&heights, &materials, padded_xz, &mut buf);
    emit_bottom_faces(voxels, shape, &mut buf);

    if buf.is_empty() { None } else { Some(buf.into_mesh()) }
}

/// Pass 1: for each padded XZ column, the world-Y of the highest solid voxel,
/// or `SENTINEL_NO_SURFACE` if the column is all air.
fn column_heights<S: Shape<3, Coord = u32>>(voxels: &[WorldVoxel], shape: &S) -> Vec<i32> {
    let dims = shape.as_array();
    let (sx, sy, sz) = (dims[0], dims[1], dims[2]);
    let mut heights = vec![SENTINEL_NO_SURFACE; (sx * sz) as usize];
    for px in 0..sx {
        for pz in 0..sz {
            for py in (0..sy).rev() {
                let idx = shape.linearize([px, py, pz]) as usize;
                if matches!(voxels[idx], WorldVoxel::Solid(_)) {
                    heights[(px * sz + pz) as usize] = py as i32;
                    break;
                }
            }
        }
    }
    heights
}

/// Pass 1b: material byte of each column's top voxel (undefined when no surface).
fn column_materials<S: Shape<3, Coord = u32>>(voxels: &[WorldVoxel], shape: &S) -> Vec<u8> {
    // walk the same loops as `column_heights`; store 0 for no-surface columns
    // (the caller must gate on heights != SENTINEL_NO_SURFACE before reading)
    /* ... */
    todo!()
}

/// Pass 2: classify each interior column (x ∈ 1..padded-1, z ∈ 1..padded-1).
/// Corner heights follow the corner-max rule (design decision #6):
///   max(H)-min(H) == 0  → corner = h         (flat)
///   max(H)-min(H) == 1  → corner = max(H)    (slope — adjacent rises to meet)
///   max(H)-min(H) >= 2  → corner = own h     (cliff — each column owns its corner)
///
/// Returns `None` for columns with no surface voxel.
fn classify_columns(
    heights: &[i32],
    materials: &[u8],
    padded_xz: usize,
) -> Vec<Option<ColumnTop>> { /* ... */ todo!() }

/// Pass 3a: 2D greedy rectangle sweep over `Planar` columns. Two cells merge
/// iff same `(material, gradient_x, gradient_z, intercept)`. Extends +x then
/// +z. For each merged rect, emit 4 verts + 6 indices with the plane's
/// analytic normal.
fn emit_planar_tops(tops: &[Option<ColumnTop>], padded_xz: usize, out: &mut MeshBuffers) {
    /* ... */ todo!()
}

/// Phase-2 fallback: `NonPlanar` columns emit as a flat cubic top at their
/// max corner. Phase 3 replaces this with `emit_nonplanar_tops`.
fn emit_nonplanar_tops_cubic_fallback(
    tops: &[Option<ColumnTop>],
    padded_xz: usize,
    out: &mut MeshBuffers,
) { /* ... */ todo!() }

/// Phase-2 placeholder wall emitter: for each interior shared edge between
/// columns A and B, if heights differ, emit an axis-aligned rectangular wall
/// filling the height delta (no greedy merging, no saddle handling).
/// Phase 4 replaces this with `emit_walls`.
fn emit_cubic_walls_simple(
    heights: &[i32],
    materials: &[u8],
    padded_xz: usize,
    out: &mut MeshBuffers,
) { /* ... */ todo!() }

/// Emit per-voxel cubic faces for bottoms and any below-surface uncovered
/// faces (design decision #9). Keep naive — greedy merging of sub-surface
/// faces is deferred until profiling justifies it.
fn emit_bottom_faces<S: Shape<3, Coord = u32>>(
    voxels: &[WorldVoxel],
    shape: &S,
    out: &mut MeshBuffers,
) { /* ... */ todo!() }

#[cfg(test)]
mod tests {
    use super::*;
    use ndshape::RuntimeShape;

    fn padded_shape() -> RuntimeShape<u32, 3> { RuntimeShape::<u32, 3>::new([18, 18, 18]) }

    /// Fill voxels such that column (wx, wz) has `heights[wx, wz]` solid rows.
    fn heightmap_voxels<F: Fn(i32, i32) -> i32>(f: F) -> Vec<WorldVoxel> {
        /* ... */ todo!()
    }

    #[test]
    fn staircase_emits_planar_slopes() {
        // single-axis staircase: h(x, z) = clamp(x, 0, 7)
        let voxels = heightmap_voxels(|x, _| x.clamp(0, 7));
        let shape = padded_shape();
        let mesh = mesh_chunk_sloped(&voxels, &shape).expect("mesh");
        let normals = mesh.attribute(Mesh::ATTRIBUTE_NORMAL).unwrap().as_float3().unwrap();
        let tilted = normals.iter().filter(|n| n[0].abs() > 0.1 && n[1].abs() > 0.1).count();
        assert!(tilted > 0, "staircase should produce tilted top normals");
    }

    #[test]
    fn flat_region_merges_to_single_quad() {
        let voxels = heightmap_voxels(|_, _| 5);
        let shape = padded_shape();
        let mesh = mesh_chunk_sloped(&voxels, &shape).expect("mesh");
        // A single 16x16 flat-top chunk should produce at most a few greedy-merged top quads.
        let positions = mesh.attribute(Mesh::ATTRIBUTE_POSITION).unwrap().as_float3().unwrap();
        // 4 verts per top quad; expect << 4*16*16 = 1024
        assert!(positions.len() < 1024, "flat top should greedy-merge");
    }
}
```

### Verification

#### Automated
- [x] `cargo check-all` passes
- [x] `cargo test-native` passes, including new `staircase_emits_planar_slopes` and `flat_region_merges_to_single_quad`

#### Manual
- [x] `cargo run --example stepped_terrain` renders the staircase strip as 45° ramps
- [x] Pyramid, saddle, depression sections may show visible artifacts (expected — deferred to Phase 3/4)
- [x] No `trimesh_from_mesh` warnings in log output from the server-side collider attach
- [x] Tracy `greedy_quads`-equivalent span present under the sloped mesher

---

## Phase 3: Hip & Valley Corner Triangles

### Changes

#### 1. Extend `classify_columns`
**File**: `crates/voxel_map_engine/src/meshing/sloped.rs`
**Action**: modify

Inside `classify_columns`, after computing the 4 corner heights, test coplanarity:

```rust
fn plane_fit(corners: [i32; 4]) -> Option<(i32, i32, i32)> {
    // Corners in order: (-x,-z), (+x,-z), (-x,+z), (+x,+z)
    let [c00, c10, c01, c11] = corners;
    let gx = c10 - c00;
    let gz = c01 - c00;
    // Fourth corner must match: c11 == c00 + gx + gz
    if c11 - c00 == gx + gz {
        Some((gx, gz, c00))
    } else {
        None
    }
}
```

Return `ColumnTop::Planar` when `plane_fit` returns `Some`, else `ColumnTop::NonPlanar`.

#### 2. Diagonal choice
**File**: `crates/voxel_map_engine/src/meshing/sloped.rs`
**Action**: modify

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Diagonal {
    /// Split (-x,-z)↔(+x,+z). Use when c00+c11 > c10+c01 (convex hip: ridge along main diag).
    Main,
    /// Split (+x,-z)↔(-x,+z). Use when c00+c11 < c10+c01 (concave valley: seam along anti-diag).
    Anti,
}

fn choose_diagonal(corners: &[i32; 4]) -> Diagonal {
    let [c00, c10, c01, c11] = *corners;
    if (c00 + c11) >= (c10 + c01) { Diagonal::Main } else { Diagonal::Anti }
}
```

#### 3. Emit hip/valley triangles
**File**: `crates/voxel_map_engine/src/meshing/sloped.rs`
**Action**: modify

Replace `emit_nonplanar_tops_cubic_fallback` with:

```rust
fn emit_nonplanar_tops(tops: &[Option<ColumnTop>], padded_xz: usize, out: &mut MeshBuffers) {
    for (idx, top) in tops.iter().enumerate() {
        let Some(ColumnTop::NonPlanar { corners, material: _ }) = top else { continue };
        let (px, pz) = (idx / padded_xz, idx % padded_xz);
        let (x0, z0) = ((px - 1) as f32, (pz - 1) as f32); // interior coords
        let diag = choose_diagonal(corners);

        let p00 = [x0,     corners[0] as f32, z0    ];
        let p10 = [x0+1.0, corners[1] as f32, z0    ];
        let p01 = [x0,     corners[2] as f32, z0+1.0];
        let p11 = [x0+1.0, corners[3] as f32, z0+1.0];

        // Emit two triangles with per-triangle flat normals. Winding: CCW viewed from +y.
        match diag {
            Diagonal::Main => {
                push_triangle(out, p00, p10, p11);
                push_triangle(out, p00, p11, p01);
            }
            Diagonal::Anti => {
                push_triangle(out, p00, p10, p01);
                push_triangle(out, p10, p11, p01);
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
    out.uvs.extend_from_slice(&[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]);
    out.indices.extend_from_slice(&[base, base + 1, base + 2]);
}
```

Update `mesh_chunk_sloped` entry to call `emit_nonplanar_tops` instead of the cubic fallback.

### Verification

#### Automated
- [x] `cargo check-all` passes
- [x] `cargo test-native` passes with new tests:
  - [x] `pyramid_tip_emits_hip_triangles` — pyramid column emits diagonally-tilted normals (hip)
  - [x] `choose_diagonal_main_for_convex` / `choose_diagonal_anti_for_concave` (pure unit tests)
  - [x] `handcrafted_valley_column_uses_anti_diagonal` — replaces `cross_depression_emits_valley_triangles`; the cross-fixture can't produce NonPlanar corners under corner-max (local max pulls all corners up), so the diagonal-choice logic is tested directly via constructed corners instead

#### Manual
- [ ] `cargo run --example stepped_terrain` shows the 3x3 pyramid with a clean 4-sided hip apex (no T-junction cracks between planar ramps and the hip column)
- [ ] Cross-shaped depression renders — note: under corner-max rule the depression's h=-1 arms get pulled up to flat y=2 (surrounding h=0), so no visible valley seam. This is a known consequence of the rule, not a Phase 3 regression
- [ ] Rotate camera around pyramid — normals shade correctly from all sides

---

## Phase 4: Generalized Wall Rule (Saddles, Cliffs, Merged Walls)

### Changes

#### 1. Replace `emit_cubic_walls_simple` with `emit_walls`
**File**: `crates/voxel_map_engine/src/meshing/sloped.rs`
**Action**: modify

```rust
/// One of the 2 shared edges per column (along +x or +z from the owning column).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EdgeAxis { X, Z }

#[derive(Clone, Copy, Debug, PartialEq)]
struct WallCornerPair { low_corner: f32, high_corner: f32 }

enum WallEmission {
    Skip,
    Triangle { vertices: [[f32; 3]; 3], material: u8 },
    Rectangle {
        base_xz: Vec2,
        axis: EdgeAxis,
        axis_len: f32,
        y_lo: f32,
        y_hi: f32,
        material: u8,
    },
}

/// For a shared edge between columns A (owning the edge) and B (neighbor along
/// `axis`), extract the 2 corner-Y-pairs along the shared edge and choose
/// between skip / triangle (saddle) / rectangle.
fn classify_wall_edge(
    a: &ColumnTop,
    b: &ColumnTop,
    axis: EdgeAxis,
) -> WallEmission { /* ... */ todo!() }

fn emit_walls(tops: &[Option<ColumnTop>], padded_xz: usize, out: &mut MeshBuffers) {
    let mut rects: Vec<WallEmission> = Vec::new();
    for z in 1..(padded_xz - 1) {
        for x in 1..(padded_xz - 1) {
            let here = tops[x * padded_xz + z].as_ref();
            for (axis, nx, nz) in [(EdgeAxis::X, x + 1, z), (EdgeAxis::Z, x, z + 1)] {
                let n = tops[nx * padded_xz + nz].as_ref();
                match (here, n) {
                    (Some(a), Some(b)) => match classify_wall_edge(a, b, axis) {
                        WallEmission::Skip => {}
                        WallEmission::Triangle { vertices, material: _ } => {
                            debug_assert!(
                                triangle_area(&vertices) > MIN_PARRY_TRIANGLE_AREA,
                                "wall triangle at ({x}, {z}) axis={axis:?} below Parry tolerance",
                            );
                            push_triangle(out, vertices[0], vertices[1], vertices[2]);
                        }
                        r @ WallEmission::Rectangle { .. } => rects.push(r),
                    },
                    _ => { /* edge of padded band, skip */ }
                }
            }
        }
    }
    for merged in greedy_merge_rectangle_walls(rects) {
        emit_wall_rect(&merged, out);
    }
}

const MIN_PARRY_TRIANGLE_AREA: f32 = 1e-4;

fn triangle_area(v: &[[f32; 3]; 3]) -> f32 {
    let ab = Vec3::from(v[1]) - Vec3::from(v[0]);
    let ac = Vec3::from(v[2]) - Vec3::from(v[0]);
    ab.cross(ac).length() * 0.5
}

/// Collapse coplanar adjacent rectangular walls with matching `(material, axis,
/// base_xz.<perp>, y_lo, y_hi)` into fewer quads. Sweep along the edge's
/// parallel axis first. Triangular saddle walls are NOT merged (v1).
fn greedy_merge_rectangle_walls(walls: Vec<WallEmission>) -> Vec<WallEmission> {
    /* ... */ todo!()
}

fn emit_wall_rect(rect: &WallEmission, out: &mut MeshBuffers) { /* 2 triangles */ todo!() }
```

Delete `emit_cubic_walls_simple` — `mesh_chunk_sloped` now calls `emit_walls` instead.

### Verification

#### Automated
- [x] `cargo check-all` passes
- [x] `cargo test-native` passes with new tests:
  - [x] `cliff_wall_merges_horizontally` — Δ=3 cliff wall merges across columns
  - [x] `wall_triangle_area_above_parry_threshold` — area check guards against degenerate triangles
  - [x] `make_triangle_wall_x_produces_nondegenerate_triangles` — direct classifier-branch coverage
  - [x] `stepped_generator_all_chunks_no_degenerate_triangles` — regression test; meshes every chunk the SteppedGenerator fixture can occupy and verifies the area assertion doesn't fire
  - Deviation: `saddle_emits_triangular_walls` replaced with the two tests above. Saddle triangles DO fire in the SteppedGenerator fixture (e.g., chunk (0, 0, 0) edges near the cross-depression z-arm). An early implementation had swapped args in the `(false, true)` classifier branch that produced degenerate triangles; the regression test catches that.

#### Manual
- [ ] `cargo run --example stepped_terrain` saddle section shows the two low corners filled with triangular walls; no cracks
- [ ] Multi-unit cliff at (-11..=-9, 6..=10) renders fully cubic with walls correctly greedy-merged (visible as single-sided large faces, not per-voxel strips)
- [ ] **Multiplayer manual**: `cargo server` in one shell, `cargo client` in another; walk the player capsule across the staircase, pyramid, saddle, and cliff — no snagging on collider edges, no falling through geometry, no visible desync between rendered mesh and capsule contact surface
- [ ] Server log shows zero `Collider::trimesh_from_mesh` rejection warnings for chunks from the stepped_terrain map

---

## Phase 4.5: Cliff-Pulled-Only Slope Extension

### Problem

`extend_slopes_over_nonplanar` (introduced by the Phase 4 extension commit `ed466820`) adopts a neighbor's Planar slope plane into a `NonPlanar` column whenever ≥3 of 4 corners match the plane. This criterion cannot distinguish two structurally different reasons a corner mismatches:

1. **Cliff artifact** — `corner_rule` pulled the corner down to `own_h` because the 2×2 neighborhood has delta ≥ 2. The column genuinely sits on the slope plane; adoption restores the correct corner.
2. **Hip/valley ridge** — `corner_rule` pulled the corner up to `max` because the 2×2 neighborhood has delta = 1 with `own_h == min` but `own_count != 1`. The column is a genuine hip or valley; adoption would erase that shape and produce thin triangular wall fins.

Screenshot bug: pyramid hip columns (e.g. `(1, 1)` corners `[3,2,2,2]`) and saddle hip/valley columns (e.g. `(1, 8)` corners `[1,0,1,1]`, `(0, 9)` corners `[1,1,0,1]`) all get over-adopted, rendering one-directional slopes instead of two-sloped-edge hips.

### Fix

Only lift corners that were *cliff-pulled* by `corner_rule`. For each NP corner that differs from the adopted plane's value, re-derive the 4 raw heights feeding `corner_rule` at that mesh corner and check whether `max - min ≥ 2`. If not, the mismatch is a hip ridge or pit floor and must not be overwritten.

### Changes

#### 1. Add cliff-pulled helper
**File**: `crates/voxel_map_engine/src/meshing/sloped.rs`
**Action**: modify

```rust
/// Returns the 4 raw heights feeding `corner_rule` for mesh corner `corner_idx`
/// of column `(px, pz)`. `corner_idx` ∈ [0, 4) in order `[c00, c10, c01, c11]`.
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
    // The 2×2 mesh corner is shared by 4 columns; build the same set
    // `corner_rule` sees from the classifier.
    let h = |x: i32, z: i32| -> i32 {
        if x < 0 || z < 0 || x as usize >= padded_xz || z as usize >= padded_xz {
            SENTINEL_NO_SURFACE
        } else {
            heights[x as usize * padded_xz + z as usize]
        }
    };
    let px_i = px as i32;
    let pz_i = pz as i32;
    [
        h(px_i, pz_i),
        h(px_i + dx, pz_i),
        h(px_i, pz_i + dz),
        h(px_i + dx, pz_i + dz),
    ]
}

/// A corner was pulled to `own_h` by the cliff branch of `corner_rule`
/// iff its 2×2 neighborhood has max-min ≥ 2 OR contains a SENTINEL.
/// These corners are safe to lift onto a slope plane; `max`-pulled hip
/// corners and isolated-pit floors are not.
fn corner_is_cliff_pulled(values: [i32; 4]) -> bool {
    if values.iter().any(|&v| v == SENTINEL_NO_SURFACE) {
        return true;
    }
    let max = *values.iter().max().unwrap();
    let min = *values.iter().min().unwrap();
    max - min >= 2
}
```

#### 2. Filter the adoption check
**File**: `crates/voxel_map_engine/src/meshing/sloped.rs`
**Action**: modify `extend_slopes_over_nonplanar`

Change the match-count test to additionally require that every mismatched corner is cliff-pulled. The heights slice (currently `_heights`) is re-threaded in:

```rust
fn extend_slopes_over_nonplanar(
    tops: &mut Vec<Option<ColumnTop>>,
    heights: &[i32],
    padded_xz: usize,
) {
    // ... existing loop structure ...
    let all_mismatches_cliff_pulled = (0..4).all(|i| {
        np_corners[i] == plane_corners[i]
            || corner_is_cliff_pulled(
                corner_neighborhood_heights(heights, padded_xz, px, pz, i),
            )
    });
    if matches >= 3 && all_mismatches_cliff_pulled {
        adopt = Some((gx, gz, intercept));
        break;
    }
    // ...
}
```

Call site at `mesh_chunk_sloped` already passes `&heights`; the `_heights` parameter reverts to `heights` (rename removed).

### Verification

#### Automated
- [x] `cargo check-all` passes
- [x] `cargo test -p voxel_map_engine --lib` passes (131/131)
- [x] New test `extension_rejects_pyramid_hip_corner` — pyramid `(1, 1)` in padded coords stays NonPlanar with corners `[5, 4, 4, 4]` (stored top-face heights; equivalent to `[3, 2, 2, 2]` in terrain-h terms)
- [x] New test `extension_rejects_saddle_se_hip_corner` — saddle SE hip stays NonPlanar with corners `[3, 2, 3, 3]` (equivalent to `[1, 0, 1, 1]`)
- [x] New test `extension_accepts_staircase_edge_corner` — staircase north-edge column extends to Planar `[6, 7, 6, 7]` (equivalent to `[3, 4, 3, 4]`)
- [x] New test `extension_accepts_small_drop_edge_corner` — west-end staircase edge extends to Planar `[3, 4, 3, 4]`; c11's 2×2 neighborhood has diagonal delta=2 so the cliff check accepts it
- [x] Existing `stepped_generator_staircase_chunk_has_merged_slope` and `staircase_south_face_emits_tilted_wall_not_stepped` still pass
- [x] Existing `pyramid_tip_emits_hip_triangles` still passes

#### Manual
- [ ] `cargo run --example stepped_terrain` — pyramid's four corner columns render as hip wedges with two sloped edges meeting at the raised corner
- [ ] Saddle's interior hip/valley columns at `(1, 8)` and `(0, 9)` render with two sloped edges each, not single-direction slopes
- [ ] No thin triangular wall fins at `x=2` or `z=9` where adopted corners previously mismatched planar neighbors
- [ ] Staircase, reversed staircase, and cliff fixtures look unchanged from pre-fix

---

## Phase 4.6: Beveled Hip Diagonal

### Problem

`choose_diagonal` currently splits a NonPlanar cell along the diagonal whose two corner sum is highest (Main if `c00+c11 >= c10+c01`, else Anti). Applied to a single-odd-corner hip like `[H, h, h, h]` (c00 raised) the split passes *through* the raised corner, producing two tilted triangles that meet at a ridge line — a sail/wedge shape.

The desired shape at a single-raised-corner hip is **1 flat triangle at `y = h` + 1 tilted triangle containing the raised corner** — a beveled tip. This is what the current rule produces *by accident* when the raised corner is `c10` or `c01` (the Anti diagonal), because then the Main split excludes that corner from one triangle. For `c00` or `c11` raised, the shape is wrong.

Screenshot bug: natural heightmap terrain shows ridge wedges ("spikes") at every convex corner where the raised diagonal is main. The user's earlier screenshot circled a `c10`/`c01`-raised corner that renders correctly under current code; everywhere else produces the ridge artifact.

### Fix

Special-case the single-odd-corner configuration in `choose_diagonal`: split *away from* the odd corner so one triangle is the three equal corners (flat at `h`) and the other is the triangle containing the odd corner (tilted).

- Odd at `c00` or `c11` (corners on main diagonal) → return `Anti`.
- Odd at `c10` or `c01` (corners on anti diagonal) → return `Main`.
- Two or more corners differ (saddle/valley) → keep the existing comparison.

### Changes

#### 1. Rule update
**File**: `crates/voxel_map_engine/src/meshing/sloped.rs`
**Action**: modify `choose_diagonal`

```rust
fn choose_diagonal(corners: &[i32; 4]) -> Diagonal {
    let [c00, c10, c01, c11] = *corners;
    // Single-odd-corner hip/valley: put the split AWAY from the odd corner
    // so the resulting triangles are one flat (three equal corners) + one
    // tilted (containing the odd corner). A ridge/wedge shape (split THROUGH
    // the odd corner) looks like a spike at the cell and does not match the
    // expected beveled-tip visual.
    let vals = [c00, c10, c01, c11];
    for i in 0..4 {
        let others: [i32; 3] = [vals[(i + 1) % 4], vals[(i + 2) % 4], vals[(i + 3) % 4]];
        if others[0] == others[1] && others[1] == others[2] && vals[i] != others[0] {
            return match i {
                0 | 3 => Diagonal::Anti,
                1 | 2 => Diagonal::Main,
                _ => unreachable!(),
            };
        }
    }
    // Saddle / valley with two distinct pairs: split along the higher pair so
    // the seam runs along the raised diagonal (keeps the existing behavior
    // for `handcrafted_valley_column_uses_anti_diagonal`).
    if (c00 + c11) >= (c10 + c01) {
        Diagonal::Main
    } else {
        Diagonal::Anti
    }
}
```

#### 2. Flip `choose_diagonal_main_for_convex`
**File**: `crates/voxel_map_engine/src/meshing/sloped.rs`
**Action**: modify test

Rename to `choose_diagonal_anti_for_single_raised_main_corner` (or similar) and flip its assertion. The `[3,3,3,3]` tie case stays Main via the fallthrough.

#### 3. Rewrite `pyramid_tip_emits_hip_triangles`
**File**: `crates/voxel_map_engine/src/meshing/sloped.rs`
**Action**: modify test

The old test asserts a `(±0.707, 0.707, 0)` or `(0, 0.707, ±0.707)` hip normal — that's Main-diagonal 45° tilt on a single axis. With Anti on a c00-raised hip the tilted normal becomes `(1, 1, 1)/√3 ≈ (0.577, 0.577, 0.577)` and the flat triangle contributes `(0, 1, 0)`. Replace with an assertion that both patterns appear.

### Verification

#### Automated
- [x] `cargo check-all` passes
- [x] `cargo test -p voxel_map_engine --lib` passes (132/132)
- [x] New `choose_diagonal_beveled_tip_for_single_odd_corner` covers all 4 raised and 2 sunk single-odd cases; `choose_diagonal_tie_defaults_to_main` covers the all-equal tie
- [x] `choose_diagonal_anti_for_concave` and `handcrafted_valley_column_uses_anti_diagonal` still pass
- [x] `pyramid_tip_emits_hip_triangles` rewritten to assert the `(±0.577, 0.577, ±0.577)` beveled-tip tilted normal

#### Manual
- [ ] `cargo run --example terrain` with heightmap noise: convex corners render as beveled tips (flat triangle at lower level + single tilted slope) rather than pointy spikes
- [ ] `cargo run --example stepped_terrain`: pyramid's outer-ring corner columns render as beveled tips. Silhouette of pyramid loses the four sharp edge-ridges but still has the tiered stack shape
- [ ] Staircase, reversed staircase, saddle, cliff fixtures visually unchanged from their Phase 4.5 state (except where saddle cells contain single-odd configurations — those switch to beveled)

---

## Phase 4.7: Inside-Concave-Corner Fixture + True-Pit Detection

### Problem

At the inside concave corner of an L-shaped plateau (cell adjacent to plateau on `N` and `W` cardinals, ground on `S`/`E`, with `NW` diagonal also plateau), corner classification produces `[0, 1, 1, 0]` — a saddle. The 2-triangle Anti-diagonal split gives two tilted triangles meeting at the cell center with no flat component, so the cell's top has no flat triangular region at the plateau level. User's wireframe shows this as two diagonal faces with an apparent vertical seam between them and no flat cap.

Root cause: `c00` gets pulled down to `own_h` by `corner_rule`'s ISOLATED-PIT branch (2×2 = `[H, H, H, own]`, own_count==1). The same 2×2 signature appears for a genuine 1-cell pit (which *should* stay at own_h) and for an inside concave corner (which should lift to max). Per-corner logic can't distinguish them.

### Phase 4.7a: Fixture

Add two features to `SteppedGenerator` that reliably produce the bug and a reference-correct case:

1. **L-shaped plateau** — horizontal leg `x ∈ [-24, -18], z ∈ [-12, -9]`; vertical leg `x ∈ [-24, -21], z ∈ [-9, -7]`. Inside concave corner cell at world `(-20, -8)`.
2. **1-cell pit** — carve `(-20, -2)` out of a surrounding `h=1` plateau at `x ∈ [-22, -18], z ∈ [-4, 0]`. All 8 neighbors of the pit cell are higher, so it should stay flat at own_h=0.

No classifier changes in this phase. Visual confirmation in the stepped_terrain example before implementing the fix.

### Phase 4.7b: True-Pit Detection (next phase)

Detect true pits at the cell level before calling `corner_rule`:

```rust
/// A cell is a "true pit" when all 8 cardinal+diagonal neighbors have height
/// strictly greater than own_h. Out-of-range (SENTINEL) neighbors are ignored.
fn is_true_pit(heights: &[i32], padded_xz: usize, px: usize, pz: usize, own_h: i32) -> bool;
```

Pass a flag into `corner_rule` (or branch in `compute_corners`) so the ISOLATED-PIT preservation fires only for true pits. Non-pit cells fall through to `max` — inside-corner cells become `[1, 1, 1, 0]` (single-sunk c11) which Phase 4.6's rule already renders as 1 flat + 1 tilted.

Expected post-fix classifications:
- Inside corner cell `(-20, -8)` → `[1, 1, 1, 0]`.
- 1-cell pit cell `(-20, -2)` → `[0, 0, 0, 0]` (unchanged).

### Verification (Phase 4.7a fixture)

#### Automated
- [x] `cargo check-all` passes
- [x] `cargo test -p voxel_map_engine --lib` passes (132/132) with existing tests unchanged

#### Manual
- [x] User confirmed the saddle artifact at `(-20, -8)` reproduces in stepped_terrain
- (1-cell pit at `(-20, -2)` reference check folded into 4.7b regression test)

### Verification (Phase 4.7b fix)

#### Automated
- [x] `cargo check-all` passes
- [x] `cargo test -p voxel_map_engine --lib` passes (135/135)
- [x] New `corner_rule_lifts_isolated_at_non_pit_cell` covers the `is_pit=false` branch
- [x] New `inside_concave_corner_emits_beveled_top` — cell `(-20, -8)` classifies NonPlanar `[3, 3, 3, 2]` (single-sunk c11)
- [x] New `single_cell_pit_stays_flat` — cell `(-20, -2)` classifies Planar flat `[2, 2, 2, 2]`
- [x] Existing `corner_rule_preserves_isolated_pit`, `extension_rejects_*`, pyramid/staircase tests all pass

#### Manual
- [ ] `cargo run --example stepped_terrain` — inside concave corner at `(-20, -8)` now renders as a flat triangular top at y=1 plus one tilted triangle descending to the deep corner
- [ ] 1-cell pit at `(-20, -2)` still renders as a recessed flat cell (no regression)
- [ ] Natural heightmap terrain (arena / overworld map): inside concave corners along elevation steps now show beveled flat-plus-tilted tops instead of the saddle spikes

---

## Phase 5: Cross-Chunk Continuity + Integration Validation

### Changes

#### 1. Extend `SteppedGenerator` with cross-chunk pattern
**File**: `crates/voxel_map_engine/src/terrain.rs`
**Action**: modify

Add a long diagonal ramp that spans multiple chunks — `stepped_surface_height` for `(x, z)` in `-24..=24` along `x == z` direction rises by 1 per step, crossing chunk boundaries at `x == 0, 16, -16`:

```rust
// Append to stepped_surface_height before the final `0` return:
// Cross-chunk staircase along x=z diagonal, for verification of padding sync
if x == z && (-24..=24).contains(&x) { return x / 2; }
```

#### 2. Padding-symmetry `debug_assert!`
**File**: `crates/voxel_map_engine/src/meshing/sloped.rs`
**Action**: modify (at `mesh_chunk_sloped` entry)

```rust
debug_assert_padding_symmetry(voxels, shape);
```

```rust
/// Check that the 1-voxel padding band is consistent: for each face of the
/// padded cube, a padding voxel is assumed to equal the corresponding interior
/// voxel of the logical neighbor chunk. We can only verify intra-chunk: the
/// padding must not contain `WorldVoxel::Unset` (unset padding indicates a
/// neighbor sync bypass). Catches the `update_neighbor_padding` desync risk.
#[cfg(debug_assertions)]
fn debug_assert_padding_symmetry<S: Shape<3, Coord = u32>>(
    voxels: &[WorldVoxel],
    shape: &S,
) {
    let dims = shape.as_array();
    for px in 0..dims[0] {
        for py in 0..dims[1] {
            for pz in 0..dims[2] {
                let is_padding = px == 0 || px == dims[0] - 1
                    || py == 0 || py == dims[1] - 1
                    || pz == 0 || pz == dims[2] - 1;
                if !is_padding { continue; }
                let idx = shape.linearize([px, py, pz]) as usize;
                debug_assert!(
                    !matches!(voxels[idx], WorldVoxel::Unset),
                    "padding voxel at ({px},{py},{pz}) is Unset — neighbor sync bypass",
                );
            }
        }
    }
}
#[cfg(not(debug_assertions))]
fn debug_assert_padding_symmetry<S: Shape<3, Coord = u32>>(_: &[WorldVoxel], _: &S) {}
```

#### 3. README update (if user-facing)
**File**: `README.md`
**Action**: modify iff the README documents map spawning or terrain options

Per the CLAUDE.md rule: after making code changes, review `README.md` and update if changes affect documented features, commands, architecture, or usage. Add a short note about `VoxelMapConfig::with_meshing_mode(MeshingMode::Sloped)` and the `stepped_terrain` example under whatever "examples" or "features" section exists.

### Verification

#### Automated
- [ ] `cargo check-all` passes
- [ ] `cargo test-native` passes with new test `cross_chunk_slope_continuity` — meshes two adjacent chunks with a ramp that crosses the boundary; compares corner heights of the east edge of chunk A to the west edge of chunk B; asserts identical world-Y

#### Manual
- [ ] `cargo run --example stepped_terrain` renders the diagonal cross-chunk ramp with no seam or T-junction at chunk boundaries; rotate camera to confirm lighting is continuous
- [ ] `cargo server` + `cargo client` with `stepped_terrain`-equivalent map: walk the capsule up the cross-chunk ramp — smooth traversal, no snag at the chunk boundary, no falling through the collider
- [ ] `cargo run --example terrain` still renders cubic (regression check — default unchanged)
- [ ] If README was updated, re-read it to confirm accuracy

---

## Deviations from structure.md

**One minor deviation** — structure.md mentions threading `mode` only through `spawn_mesh_task` and the remesh closure. Phase 1 also threads it through `spawn_terrain_batch`, because its disk-load fast path calls `mesh_chunk_greedy` at `generation.rs:73`. Without this, disk-loaded chunks would always be cubic regardless of `meshing_mode`, breaking mode consistency across the gen pipeline. Call this out for user approval before implementation begins.

All other code follows structure.md verbatim.
