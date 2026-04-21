# Design Discussion

## Current State

- `mesh_chunk_greedy(voxels, shape)` (`crates/voxel_map_engine/src/meshing.rs:12-15`) is the **single source of geometry** consumed by both the client `Mesh3d` and the server `Collider::trimesh_from_mesh` (`crates/protocol/src/map/colliders.rs:11-40`). Server collider derives from the same mesh asset that the client renders, with no parallel geometry path.
- All emitted quads are **axis-aligned cubic faces**: `block-mesh-rs::OrientedBlockFace` hard-codes unit-vector axes (`git/block-mesh-rs/src/geometry/axis.rs:21-27`); every quad is 4 verts + 6 indices with a flat per-face normal (`git/block-mesh-rs/src/geometry/face.rs:99-101`).
- Voxel data is **shape-less**: `WorldVoxel::Solid(u8)` carries a material byte only (`crates/voxel_map_engine/src/types.rs:8-12`); `VoxelType` mirror in the protocol (`types.rs:127-131`) and `PalettedChunk` storage carry no orientation or sub-cell metadata.
- Chunks are stored as a **padded flat array** (`chunk_size + 2`³ = 18³ for chunk_size=16) with neighbor voxels baked into the 1-voxel padding band (`crates/voxel_map_engine/src/instance.rs:40, 47, 55`). Initial generation writes padding directly via `ChunkData::from_voxels` (`types.rs:93-103`); runtime `set_voxel` calls `update_neighbor_padding` to keep neighbors coherent (`instance.rs:124-153`).
- Single-unit elevation deltas between adjacent columns originate in `HeightmapGenerator::generate_heightmap_chunk` via the integer/float comparison `(world_y as f64) <= terrain_height` (`crates/voxel_map_engine/src/terrain.rs:245`) — adjacent column noise heights straddle integer boundaries and quantize to Δ=1.
- Meshing is dispatched at two call sites: initial mesh in `spawn_mesh_task` (`crates/voxel_map_engine/src/generation.rs:137`) and remesh in `spawn_remesh_tasks` (`crates/voxel_map_engine/src/lifecycle.rs:1085`). Both pass the same padded `Vec<WorldVoxel>` and shape.
- Rendering uses Bevy's default `StandardMaterial` (`crates/voxel_map_engine/src/lifecycle.rs:113-128`); no custom shader, AO, or normal smoothing — slope quads will be lit purely by their tilted face normals through PBR.

## Desired End State

- A new function `mesh_chunk_sloped(voxels, shape, dims)` in `meshing.rs` that produces a `Mesh` where single-unit top-surface elevation transitions are rendered as 45° diagonal faces, with explicit triangular hip handling at convex corners.
- A configuration toggle (per-map) selects between `mesh_chunk_greedy` (current cubic) and `mesh_chunk_sloped`. Both call sites (`generation.rs:137`, `lifecycle.rs:1085`) dispatch through the toggle.
- Server collider and client mesh stay automatically consistent because they both run through the same selected mesher over the same voxel payload (existing invariant — preserved).
- Verification: load `examples/terrain.rs` switched to `HeightmapGenerator` with sloped meshing enabled; visual inspection shows smooth diagonals instead of stepped jaggies; player capsule walks up slopes without snagging on collider edges.

## Patterns to Follow

- **Mesh asset construction shape** — match `mesh_chunk_greedy` (`meshing.rs:32-64`): `PrimitiveTopology::TriangleList`, attributes `ATTRIBUTE_POSITION` (`Float32x3`), `ATTRIBUTE_NORMAL` (`Float32x3`, flat per-face), `ATTRIBUTE_UV_0` (`Float32x2`), `Indices::U32`. No tangents, no vertex colors. This is what `Collider::trimesh_from_mesh` requires (`git/avian/src/collision/collider/parry/mod.rs:1174-1183`).
- **Padded coordinate access** — use `[local + 1]` → `shape.linearize(padded)` as in `instance.rs:104-109` and `meshing.rs` callers. Read column heights from the padded band so chunk-edge slopes have access to neighbor voxels.
- **Voxel visibility predicate** — keep using `WorldVoxel::is_solid()` semantics (Solid = solid, Air|Unset = empty), matching `Voxel::get_visibility` (`types.rs:152-158`). Do NOT introduce a parallel solidity definition.
- **Material byte carries through** — slope quads must inherit the material byte from their underlying `Solid(m)` voxel, same as cubic quads do via `MergeVoxel::merge_value` (`types.rs:160-174`). Adjacent slopes of differing materials do not get merged (also matches existing greedy behavior).
- **Pattern to NOT follow**: do NOT extend `block-mesh-rs::OrientedBlockFace` for non-axis-aligned quads. Its abstraction assumes unit-vector `n`/`u`/`v` (`axis.rs:21-27`); shoehorning slopes there would corrupt the API. The sloped mesher should bypass `block-mesh-rs` entirely and emit quads directly into `Vec<[f32;3]>` buffers.
- **Pattern to NOT follow**: do NOT touch `MeshCache` (`crates/voxel_map_engine/src/mesh_cache.rs`) — research found it has no observed reader/writer in the template. Out of scope.

## Design Decisions

1. **Togglable alternate function, not a replacement** — `mesh_chunk_greedy` stays untouched. New `mesh_chunk_sloped` lives beside it in `meshing.rs`. *Why:* the user's Q1 directive plus zero-risk fallback. *Toggle location:* a new `meshing_mode: MeshingMode` field on `VoxelMapConfig` (`config.rs:118-129`), with `MeshingMode::Cubic` as the default. Both call sites read this field via the chunk's parent `MapInstanceId` and dispatch.

2. **Inferred from voxel topology, no type-system changes** — slope shape is derived from horizontal column-height deltas at meshing time. **Zero changes to `WorldVoxel`, `VoxelType`, `PalettedChunk`, `ChunkDataSync`.** *Why:* user Q2; the underlying voxel grid stays cubic and round-trips to disk/wire unchanged. Slopes are purely a render/collider projection.

3. **Plane-signature greedy merging for top quads and rectangular walls** — every planar top quad lies on a plane `y = gradient_x · x + gradient_z · z + intercept` with integer coefficients (gradients ∈ {-1, 0, +1}, intercept ∈ `i32`). Two adjacent columns merge iff: (a) same material byte, (b) both planar (not hip/valley), (c) identical `(gradient_x, gradient_z, intercept)`. Standard 2D greedy rectangle sweep over the chunk's XZ grid extends merged regions in +x then +z. *Why:* matches the codebase's greedy convention (`mesh_chunk_greedy`); flats and slope ribbons both compress equally; integer arithmetic avoids floating-point ambiguity in plane equality. **Hip/valley quads emit per-column** (non-coplanar 4 corners → 2 triangles, not mergeable). Bypasses `block-mesh-rs::MergeStrategy` (`merge_strategy.rs:9-41`) which is axis-aligned-only.

4. **Top-surface only** — only top-facing transitions become slopes. Lateral side walls of the lower column adjacent to a slope stay cubic; this may produce a small visible "lip" at the slope's flank. *Why:* user Q4; matches the task description ("connecting the lower top to the upper top"). Documented as deliberate scope.

5. **Explicit hip triangles at convex corners** — when a column has rising neighbors on two perpendicular sides, the corner shared by both slopes uses a 2-triangle split with the diagonal running between the high corner and the low diagonal corner. Concave corners (one column lower than 3 neighbors) use the opposite diagonal. *Why:* user Q5; the corner-max algorithm produces non-planar quads naturally and a deterministic diagonal choice makes the crease unambiguous.

6. **Algorithm: corner-max heightmap with a 1-unit threshold, then plane-signature greedy merge** — four passes:

   **Pass 1 — heights**: build an `(padded_size)²` array of column top-heights (`i32`, derived from the padded voxel array; analogous to `SurfaceHeightMap` but integer-typed and indexed in padded XZ).

   **Pass 2 — column-owned corner heights**: for each column/corner pair, compute the corner height:
   - Let `H` = the 4 adjacent-column heights at this corner position.
   - `max(H) - min(H) == 0` → corner = `h` (flat).
   - `max(H) - min(H) == 1` → corner = `max(H)` (single-unit transition; columns rise to meet the higher neighbor — produces the slope).
   - `max(H) - min(H) >= 2` → corner = column's own `h` (cliff; each column owns its own corner; walls fill the gap).

   Each column's top is then classified as `Planar { gradient_x, gradient_z, intercept, material }` (when the 4 corners are coplanar) or `Hip`/`Valley { material, corners }` (when non-coplanar).

   **Pass 3 — top surface emission**: 2D greedy rectangle sweep over the chunk's XZ grid for `Planar` cells (per decision #3); hip/valley cells emit individually as 2 triangles each (per decision #10).

   **Pass 4 — wall emission**: for each shared edge between columns A and B, compare A's 2 owned corner heights along the edge with B's 2 owned corner heights at the same corner positions. Emit a quad (or triangle) filling whichever side has the higher corner height. All-pairs-match → skip; 1-mismatch → triangle (saddle); both-mismatch → full rectangular wall. Rectangular walls go through a 2D greedy merger (along the wall's perpendicular axis); triangular saddle walls emit per-edge unmerged in v1. This rule handles simple slopes, cliffs, *and* saddles uniformly — no separate "absorption" heuristic.

7. **Cliffs (Δ ≥ 2) stay fully cubic** — only the top unit of a multi-unit drop does NOT become a slope. *Why:* literal reading of the task; avoids visual ambiguity between a "smoothed cliff top" and a true 1-unit step. Re-evaluate after seeing it in motion.

8. **Heightmap assumption — single top per column** — the corner-max algorithm assumes each column has exactly one solid-air interface (no overhangs, caves open to sky, or floating islands). The current `HeightmapGenerator` and `FlatGenerator` satisfy this. *Why:* task scope is heightmap terrain; non-heightmap shapes would need a fundamentally different algorithm. Documented as a precondition.

9. **Bottom and below-surface geometry** — emit cubic faces (no slope logic). Walk the padded array per-voxel for non-top surfaces in v1; defer greedy merging of these faces until profiling shows it matters (most below-surface geometry is hidden inside the terrain). *Why:* keeps the sloped mesher self-contained without re-engaging `block-mesh-rs`. The dominant quad-count contributors are top surfaces and lateral walls, which are already greedy-merged per decisions #3 and #6.

10. **Triangle diagonal choice for non-planar top quads** — when the 4 corner heights are non-coplanar, split the quad into two triangles along the diagonal that joins the two corners with the *larger* height delta to the opposite pair. This produces a "ridge along the high diagonal" for hips and a "valley along the low diagonal" for concave corners — the natural reading.

11. **Dedicated verification example** — add a new example `crates/voxel_map_engine/examples/stepped_terrain.rs` driven by a purpose-built `SteppedGenerator` that produces deterministic test patterns: a staircase (1D slope), a pyramid (hip corners), a cross-shaped depression (concave corners), and a saddle. Spawn a map with `meshing_mode: MeshingMode::Sloped`. *Why:* `examples/terrain.rs:30-33` uses `FlatGenerator` (no column variation, `meshing.rs:70-84`) and can't exercise slope code. `HeightmapGenerator` varies but non-deterministically. A hand-crafted fixture exercises all corner classifications (flat / slope / hip / saddle / cliff) in one frame for visual verification. Out of scope: an interactive in-example toggle for A/B comparison (follow-up if needed).

## What We're NOT Doing

- Not introducing `WorldVoxel::Slope`, `VoxelType::Slope`, or any per-voxel orientation/shape metadata.
- Not modifying `block-mesh-rs` or its vendored patch.
- Not changing the network protocol (`ChunkDataSync`, `VoxelType`, palette encoding) — slopes are not transmitted.
- Not greedy-merging hip/valley triangles or triangular saddle walls (defer until profiling justifies it).
- Not greedy-merging below-surface cubic faces (per decision #9; defer).
- Not slope-smoothing multi-unit cliffs, side walls, or any non-top-surface faces.
- Not handling non-heightmap voxel topologies (overhangs, caves with sky access, floating islands) — the corner-max algorithm assumes a single top per column.
- Not adding a separate "slope collider" path — collider continues to derive from the mesh asset via `Collider::trimesh_from_mesh` unchanged.
- Not wiring `MeshCache` — out of scope; remains unused as research found.
- Not adding ambient occlusion, vertex lighting, or shader changes — slope normals are flat per-quad and rendered through default PBR (matches current cubic behavior).
- Not adding a runtime UI toggle — `meshing_mode` is a per-map config value set at map spawn.

## Open Risks

- **Chunk-edge slopes depend on neighbor padding**. Initial generation bakes neighbor voxels into the padding band before meshing (`types.rs:93-103`), so edge slopes work correctly on first mesh. But `update_neighbor_padding` only fires on `set_voxel` runtime edits (`instance.rs:124-153`); if a future feature mutates voxels through a different path that bypasses padding sync, slopes near chunk borders will desync between adjacent chunks. Mitigation: assert padding consistency in the sloped mesher with a `debug_assert!`, or re-derive padding from neighbor instances at meshing time.
- **Collider lies about voxel volume**. The collider trimesh has slope geometry but the underlying `WorldVoxel::Solid` cells are still cubic. Block-placement, voxel destruction, and any system that operates on the voxel grid (e.g., `VoxelWorld::set_voxel`) sees the cubic representation. A player standing on a slope is "above" a voxel that the grid considers a normal cube. Document this as the deliberate semantics; surface as an issue if a feature later needs sub-voxel solidity queries.
- **Test coverage for chunk-boundary slopes**. The 1-voxel padding makes single-chunk tests trivially correct, but cross-chunk slope continuity (slope on chunk A's east edge matches slope on chunk B's west edge) needs explicit test fixtures with 2+ adjacent chunks spanning the same slope or saddle. Plan for this in the structure phase.
- **Triangle-wall artifacts at steep cliff-slope junctions**. The generalized wall rule (decision #6) can emit triangular walls of up to `chunk_y_range` height at abrupt cliff/slope transitions. Tall thin triangles may cause collider triangulation warnings (`Collider::trimesh_from_mesh` rejects degenerate geometry, `parry/mod.rs:1499-1525`). Confirm during structure/implementation that minimum triangle area stays within Parry's tolerance.

---

Next: run `/qrspi:4_structure doc/tasks/2026-04-20-smooth-voxel-slopes/`
