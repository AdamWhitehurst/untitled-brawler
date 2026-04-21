# Structure Outline

## Approach

Introduce `mesh_chunk_sloped` alongside the existing `mesh_chunk_greedy`, selected per-map via a new `MeshingMode` field on `VoxelMapConfig`. Build the sloped mesher in four algorithmic passes (heights → corner classification → top emission → wall emission), rolling in each class of surface shape incrementally. Each phase lands a complete, testable render path for a subset of terrain patterns, validated against a purpose-built `stepped_terrain` example fixture.

## Phase 1: Toggle Infrastructure + Stub Dispatch

Add the `MeshingMode` enum, wire it into `VoxelMapConfig`, and dispatch to a stub `mesh_chunk_sloped` that delegates to `mesh_chunk_greedy`. Adds the `stepped_terrain` example scaffold. Proves the plumbing end-to-end with zero algorithmic risk.

**Files**: `crates/voxel_map_engine/src/config.rs`, `crates/voxel_map_engine/src/meshing.rs`, `crates/voxel_map_engine/src/generation.rs`, `crates/voxel_map_engine/src/lifecycle.rs`, `crates/voxel_map_engine/examples/stepped_terrain.rs` (new), `crates/voxel_map_engine/src/terrain.rs` (add `SteppedGenerator`).

**Key changes**:
- `enum MeshingMode { Cubic, Sloped }` — new, `#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]`, `Cubic` is `#[default]`
- `VoxelMapConfig { ..., meshing_mode: MeshingMode }` — new field, default `Cubic`
- `pub fn mesh_chunk<S: Shape<3, Coord=u32>>(voxels: &[WorldVoxel], shape: &S, mode: MeshingMode) -> Option<Mesh>` — new dispatcher in `meshing.rs`
- `pub fn mesh_chunk_sloped<S: Shape<3, Coord=u32>>(voxels: &[WorldVoxel], shape: &S) -> Option<Mesh>` — stub: returns `mesh_chunk_greedy(voxels, shape)`
- `spawn_mesh_task(…, mode: MeshingMode)` and remesh closure at `lifecycle.rs:1085` — threaded `mode` param, read from `VoxelMapConfig` on the map entity
- `SteppedGenerator { chunk_size: u32 }` — new `VoxelGeneratorImpl`: produces a deterministic staircase-pyramid-saddle fixture

**Verify**: `cargo check-all` + `cargo test-all` pass. `cargo run --example terrain` renders identical output to previous master (cubic default). `cargo run --example stepped_terrain` runs and renders the fixture as cubic (stub still delegates). `trace!("meshing mode: {:?}", mode)` fires at both call sites.

---

## Phase 2: Planar Top Surfaces (1D Slopes + Flats)

Implement passes 1, 2 (planar-only), and 3 for `Planar` column tops with plane-signature greedy merging. Emit cubic bottom faces and simple cubic walls at height mismatches (no saddles yet, no hip/valley corners). Delivers working 45° ramps for staircase-style terrain.

**Files**: `crates/voxel_map_engine/src/meshing.rs` (implementation replaces Phase 1 stub), `crates/voxel_map_engine/src/meshing/sloped.rs` (new submodule — split to keep files atomic per CLAUDE.md).

**Key changes**:
- `mod sloped;` in `meshing.rs`; `mesh_chunk_sloped` routes there
- `fn column_heights(voxels: &[WorldVoxel], shape: &S) -> Vec<i32>` — pass 1: padded `(padded_size)²` heightmap, `i32` top-row per column
- `enum ColumnTop { Planar { gradient_x: i32, gradient_z: i32, intercept: i32, material: u8 }, NonPlanar { corners: [i32;4], material: u8 } }` — pass 2 output
- `fn classify_column(heights: &[i32], xz_shape: &S2, x: u32, z: u32, material: u8) -> Option<ColumnTop>` — corner-max with 1-unit threshold per design decision #6
- `fn emit_planar_tops(tops: &[Option<ColumnTop>], out: &mut MeshBuffers)` — 2D greedy rectangle sweep over `Planar` cells keyed on `(gradient_x, gradient_z, intercept, material)`
- `fn emit_cubic_walls_simple(heights: &[i32], out: &mut MeshBuffers)` — stand-in: emits axis-aligned walls for each height mismatch along shared edges (no merging; saddles / hip walls deferred)
- `fn emit_bottom_faces(voxels: &[WorldVoxel], shape: &S, out: &mut MeshBuffers)` — cubic per-voxel bottoms per decision #9
- `struct MeshBuffers { positions, normals, uvs, indices }` — internal accumulator

**Verify**: `cargo test-all` passes with new unit tests: `staircase_emits_planar_slopes` (asserts quad count, normal vectors roughly `(1,1,0)/√2` scaled), `flat_region_merges_to_single_quad`. `cargo run --example stepped_terrain` shows the staircase section rendering as 45° ramps; pyramid and saddle sections may show visual artifacts (expected — later phases). Server collider accepts mesh (no `trimesh_from_mesh` warnings in log).

---

## Phase 3: Hip & Valley Corner Triangles

Handle non-coplanar 4-corner tops by emitting 2 triangles per `NonPlanar` column with diagonal choice from decision #10. Enables clean convex hips (pyramid tips) and concave valleys (cross-shaped depressions).

**Files**: `crates/voxel_map_engine/src/meshing/sloped.rs`.

**Key changes**:
- `fn emit_nonplanar_tops(tops: &[Option<ColumnTop>], out: &mut MeshBuffers)` — new pass-3b: per-column 2-triangle emission
- `fn choose_diagonal(corners: &[i32; 4]) -> Diagonal` — picks `HighToHigh` vs `LowToLow` based on max delta pair per decision #10
- `enum Diagonal { MainDiagonal, AntiDiagonal }` — internal
- `classify_column(…)` — extended to distinguish `Hip`/`Valley` via planar coplanarity check (`gradient_x * dx + gradient_z * dz + intercept` matches all 4 corners)

**Verify**: `cargo test-all` passes with `pyramid_tip_emits_hip_triangles` (4 corner cases, one per tip direction) and `cross_depression_emits_valley_triangles`. `cargo run --example stepped_terrain` shows the pyramid section with clean diagonal hip ridges and the cross depression with clean valley seams. No visible T-junction cracks between planar and non-planar neighbors (verify manually in-browser via in-camera rotation).

---

## Phase 4: Generalized Wall Rule (Saddles, Cliffs, Merged Walls)

Replace the Phase 2 stand-in wall emitter with the full rule from decision #6: per shared edge, compare the 4 corner heights (2 from each column) and emit skip / triangle / rectangle. Add 2D greedy merging for rectangular walls along the edge-perpendicular axis. Triangular saddle walls stay unmerged.

**Files**: `crates/voxel_map_engine/src/meshing/sloped.rs`.

**Key changes**:
- `fn emit_walls(tops: &[Option<ColumnTop>], out: &mut MeshBuffers)` — replaces `emit_cubic_walls_simple`; iterates shared edges once
- `fn edge_corners(a: &ColumnTop, b: &ColumnTop, axis: Axis) -> (CornerPair, CornerPair)` — extracts the 2 corner heights each column owns along the shared edge
- `enum WallEmission { Skip, Triangle { vertices: [Vec3; 3] }, Rectangle { min: Vec3, axis_width: f32, height_range: Range<f32> } }`
- `fn greedy_merge_rectangle_walls(walls: &[WallEmission]) -> Vec<WallEmission>` — 2D sweep collapses coplanar adjacent rectangular walls sharing `(material, plane)`
- `debug_assert!(triangle_area > MIN_PARRY_TRIANGLE_AREA)` in triangle emission (addresses open risk about degenerate triangles)

**Verify**: `cargo test-all` passes with `saddle_emits_triangular_walls` and `cliff_wall_merges_horizontally`. `cargo run --example stepped_terrain` shows saddle section with correct triangular wall fill and no collider-rejection warnings. Multi-unit cliff in fixture still renders fully cubic (per decision #7). Player capsule (in `cargo client` connected to `cargo server`) walks across the saddle without snagging.

---

## Phase 5: Cross-Chunk Continuity + Integration Validation

Add a multi-chunk fixture that spans a slope across a chunk boundary and exercises the padding-consistency invariant. Verify collider and mesh stay in sync client-to-server. Address the open risk that chunk-edge slopes depend on neighbor padding.

**Files**: `crates/voxel_map_engine/src/terrain.rs` (extend `SteppedGenerator` with a `cross_chunk_slope` pattern), `crates/voxel_map_engine/src/meshing/sloped.rs` (optional `debug_assert!` on padding symmetry at chunk boundaries).

**Key changes**:
- `SteppedGenerator::with_cross_chunk_pattern(…)` — new pattern: slope runs diagonally across a 2×2 chunk region
- `debug_assert!` in sloped mesher entry: if any padding voxel's classification disagrees with the equivalent interior voxel of the logical neighbor, panic with chunk coords (catches the `update_neighbor_padding` desync risk)

**Verify**: `cargo test-all` passes. `cargo run --example stepped_terrain` shows the cross-chunk slope rendering continuously with no seam at the chunk boundary. `cargo server` + `cargo client` on `stepped_terrain`-equivalent map: capsule walks smoothly across the chunk-boundary slope (manual). `cargo check-all` clean. README.md updated if the `MeshingMode` toggle is user-facing (per project rules).

## Testing Checkpoints

- **After Phase 1**: `MeshingMode` toggle exists, both call sites dispatch through it, stub proves parity with cubic. Nothing visually changes; regression risk is zero.
- **After Phase 2**: Single-axis staircases render as 45° slopes. Quads flat-shaded with tilted normals. Collider trimesh accepts the geometry. Hips, valleys, and saddles may show artifacts — deferred.
- **After Phase 3**: All heightmap shapes with single-unit steps render correctly (staircases + hips + valleys). Cliff walls and saddle fill are still approximate.
- **After Phase 4**: All heightmap shapes, including saddles and abrupt cliff-slope transitions, render correctly. Walls are greedy-merged where possible. No degenerate triangles reach the collider.
- **After Phase 5**: Multi-chunk slope continuity verified. Client mesh + server collider consistency confirmed end-to-end via manual multiplayer test. Feature is user-ready.

Next: run `/qrspi:5_plan doc/tasks/2026-04-20-smooth-voxel-slopes/`
