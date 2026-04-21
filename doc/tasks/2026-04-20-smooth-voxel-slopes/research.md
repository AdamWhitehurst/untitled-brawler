# Research Findings

## Q1: Meshing pipeline & `block-mesh-rs` extension points

### Findings

**Inputs to `mesh_chunk_greedy`** — `crates/voxel_map_engine/src/meshing.rs:12-15`
- `voxels: &[WorldVoxel]` — flat padded array, length asserted `== shape.usize()` (`meshing.rs:16`)
- `shape: &S where S: Shape<3, Coord=u32>` — in practice `RuntimeShape::<u32,3>::new([18,18,18])` (chunk 16 + 2 padding)
- Library call passes `[0;3]..=[dims-1]` as bounds (`meshing.rs:24`); interior shrink by 1 happens in `git/block-mesh-rs/src/greedy.rs:110`.

**Voxel trait methods that drive meshing**
- `Voxel::get_visibility` (`crates/voxel_map_engine/src/types.rs:152-158`): `Air|Unset → Empty`, `Solid(_) → Opaque`. Consumed in `git/block-mesh-rs/src/greedy.rs:242-255` (face emitted only if neighbor is `Empty`).
- `MergeVoxel` (`types.rs:160-174`): `MergeValue = MergeValueFacingNeighbour = u8`; `merge_value()` returns the material byte from `Solid(m)`. `VoxelMerger::find_quad` (`merge_strategy.rs:69-72`) and `get_row_width` (`merge_strategy.rs:137-143`) reject merges across differing material bytes.

**Geometry emitted** — `meshing.rs:32-64`
- 4 verts + 6 indices per quad (`meshing.rs:32-33`); 6 face groups from `RIGHT_HANDED_Y_UP_CONFIG.faces` (`meshing.rs:40`).
- Positions: `face.quad_mesh_positions(quad, 1.0)` via `face.rs:76-89` (4 corners in U/V order, offset `+n` for positive-sign faces).
- Normals: `face.quad_mesh_normals()` (`face.rs:99-101`) returns `[signed_normal; 4]` — **flat per-quad normal, all 4 verts identical**.
- UVs: `face.tex_coords(..., flip_v=true, quad)` (`face.rs:130-168`); UV range `[0..width] × [0..height]` — a greedy NxM quad maps UVs to `[0,N]×[0,M]`.
- Attrs inserted: `ATTRIBUTE_POSITION` (`meshing.rs:57`), `ATTRIBUTE_NORMAL` (`meshing.rs:59`), `ATTRIBUTE_UV_0` (`meshing.rs:61`), `Indices::U32` (`meshing.rs:63`). No tangents, no vertex colors.
- Topology: `PrimitiveTopology::TriangleList` (`meshing.rs:53`).

**`block-mesh-rs` extension points**
- Custom `MergeStrategy` trait (`git/block-mesh-rs/src/greedy/merge_strategy.rs:9-41`); entry via `greedy_quads_with_merge_strategy` (`greedy.rs:86-117`). Template uses default `VoxelMerger<T>` via `greedy_quads` (`greedy.rs:75`).
- `OrientedBlockFace::new` accepts arbitrary `n_sign` + `AxisPermutation` (`face.rs:23-33`), but `n`/`u`/`v` are **always axis-aligned unit vectors** (`axis.rs:21-27`). **No provision for non-axis-aligned normals or sub-voxel geometry.**
- `UnorientedQuad` is plain data (`min: [u32;3]`, `width`, `height`) — no custom-geometry hooks (`quad.rs:1-35`).
- Visible-faces variant: `visible_block_faces_with_voxel_view` (`simple.rs:39-88`) allows re-interpreting the array via `V: Voxel + From<&T>`. Not used in the template.

**`git/block-mesh-rs/update.patch`** — pure dependency migration:
- Replaces `ilattice = "0.1"` with `glam = "0.30"` in `Cargo.toml`; removes all `ilattice` imports in `bounds.rs`, `geometry/axis.rs`, `geometry/face.rs`, `greedy.rs`, `simple.rs`.
- Adds a new `src/extent.rs` implementing `Extent<IVec3>`/`Extent<UVec3>` (`from_min_and_max`, `padded`, `check_positive_shape`, `is_subset_of`, `least_upper_bound`, `iter3`, `Add<UVec3>`).
- Adds `pub mod extent` to `lib.rs`; removes `pub use ilattice` re-export.
- Two whitespace-only changes in `face.rs`. **No geometry or algorithmic changes.**

## Q2: Chunk data storage & neighbor sync

### Findings

**`ChunkData`** — `crates/voxel_map_engine/src/types.rs:67-72`
- Fields: `voxels: PalettedChunk`, `fill_type: FillType`, `hash: u64`, `status: ChunkStatus`.
- `PalettedChunk` (`palette.rs:23-44`): `SingleValue{voxel,len}` or `Indirect{palette:Vec<WorldVoxel>, data:Vec<u64>, bits_per_entry:u8, len}` (packed indices, no u64-word spanning).
- `hash` computed via `DefaultHasher` over full `&[WorldVoxel]` in `compute_chunk_hash` (`types.rs:119-124`); value `0` is a sentinel.
- `fill_type` via `classify_fill_type` (`types.rs:106-117`): `Empty` / `Uniform` / `Mixed`.
- `status` is `ChunkStatus` (`types.rs:32-38`): `Empty=0, Terrain=1, Features=2, Mesh=3, Full=4`.

**Padded shape** — `crates/voxel_map_engine/src/instance.rs:40, 47, 55`
- `padded_size = chunk_size + 2` (17 interior + 1 padding each side = 18 for chunk_size=16). Flat length `18³ = 5832`.
- Stored as `RuntimeShape<u32,3>` on `VoxelMapInstance`.

**Coordinate mapping (local → padded index)** — `instance.rs:104-109`, `api.rs:31-37`
- `padded = [local.x+1, local.y+1, local.z+1]`, `index = shape.linearize(padded)`. Padding lives at indices `0` and `17`.

**`update_neighbor_padding`** — `instance.rs:124-153` (private on `VoxelMapInstance`)
- For each axis: if local coord is `0`, writes voxel into `-1` neighbor's padded slot `chunk_size`; if local coord is `chunk_size-1`, writes into `+1` neighbor's padded slot `0`.
- Marks every affected neighbor in `chunks_needing_remesh` (loaded or not).
- **Only caller**: `VoxelMapInstance::set_voxel` (`instance.rs:120`), reached via `VoxelWorld::set_voxel` (`api.rs:64`). `VoxelWorld` is a `SystemParam` (`api.rs:13-16`), not tied to a fixed schedule.
- **Initial generation does NOT call it** — padded array is written directly through `ChunkData::from_voxels` (`types.rs:93-103`) and `instance.insert_chunk_data` (`lifecycle.rs:887`). Neighbor padding sync only fires on runtime edits.

**Voxel variant inventory — non-cubic geometry?**
- `WorldVoxel` (`types.rs:8-12`): `Air`, `Unset`, `Solid(u8)`. **No shape/orientation; all variants are cubic or absent.**
- `VoxelType` (`types.rs:127-131`, network-serializable mirror): `Air`, `Solid(u8)`. Collapses `Unset → Air` (`types.rs:145`).
- `FillType` (`types.rs:22-26`): `Empty`, `Mixed`, `Uniform(WorldVoxel)`.
- **Conclusion: no existing slope/ramp/stair variant. No per-voxel shape or orientation metadata anywhere in the type system.** The `u8` in `Solid(u8)` is consumed exclusively as a `MergeValue` for greedy meshing — it currently encodes "material," not "shape."

## Q3: Async chunk pipeline (gen → features → mesh)

### Findings

**`ChunkWorkBudget`** — `crates/voxel_map_engine/src/lifecycle.rs:30-67`
- Fields: `start: Instant`, `budget: Duration`. Fixed `CHUNK_WORK_BUDGET_MS = 8` (`lifecycle.rs:36`).
- `reset()` called in `update_chunks` post-propagator (`lifecycle.rs:356`) and via `reset_chunk_budgets` on clients (`lifecycle.rs:194-198`).
- `has_time()` (`lifecycle.rs:55-57`) gates `drain_gen_queue` (`lifecycle.rs:647`) and `spawn_remesh_tasks` (`lifecycle.rs:1055`). Poll systems skip the budget (`lifecycle.rs:819-820`, `lifecycle.rs:1127`).
- Hard caps (`lifecycle.rs:39-47`): `MAX_GEN_SPAWNS_PER_FRAME=256`, `MAX_GEN_POLLS_PER_FRAME=256`, `MAX_REMESH_SPAWNS_PER_FRAME=256`, `MAX_REMESH_POLLS_PER_FRAME=256`, `MAX_PENDING_GEN_TASKS=512` (`lifecycle.rs:630, 644`), `MAX_PENDING_REMESH_TASKS=512` (`lifecycle.rs:1030, 1052`).

**`PendingRemeshes`** — `lifecycle.rs:131-140`
- Component holding `tasks: Vec<RemeshTask{chunk_pos, task: Task<Option<Mesh>>}>`.
- Push in `spawn_remesh_tasks` (`lifecycle.rs:1086`); `swap_remove` in `poll_remesh_tasks` (`lifecycle.rs:1133`).
- Poll branches (`lifecycle.rs:1148-1182`): replace existing `Mesh3d`, spawn new chunk entity, despawn (mesh `None`), or no-op.
- Feeds from `instance.chunks_needing_remesh` — populated by `set_voxel` (`instance.rs:118`, plus boundary neighbors `instance.rs:138, 150`) and by client network sync (`crates/client/src/map.rs:132`).

**`ChunkWorkTracker`** — `lifecycle.rs:159-163`
- Component: two `HashSet<IVec3>` — `generating`, `remeshing`. Prevents overlapping concurrent tasks per chunk.
- `generating`: inserted before task spawn for Terrain (`lifecycle.rs:686`), Features (`lifecycle.rs:726`), Mesh (`lifecycle.rs:743`); removed in `poll_chunk_tasks` (`lifecycle.rs:830`).
- `remeshing`: inserted in `spawn_remesh_tasks` (`lifecycle.rs:1090`); removed in `poll_remesh_tasks` (`lifecycle.rs:1134`).
- Also consulted in `crates/client/src/transition.rs:342` to guard scheduling during transitions.

**`GenQueue`** — `lifecycle.rs:165-173`
- Component wrapping `BinaryHeap<ChunkWork>`. `ChunkWork{position, effective_level, distance_to_source}` (`lifecycle.rs:83-87`).
- Min-heap on `(level, distance)` via inverted `Ord` (`lifecycle.rs:99-106`).
- Enqueues: `enqueue_new_chunks` (`lifecycle.rs:574-608`) for diff.loaded/changed; Features fast-path re-enqueue (`lifecycle.rs:713-718`); stage-advance re-enqueue in `handle_completed_chunk` (`lifecycle.rs:949-955`).
- Drain: `drain_gen_queue` `heap.pop()` (`lifecycle.rs:643`); stale entries lazy-deleted (`lifecycle.rs:651-678`).

**`MeshCache`** — `crates/voxel_map_engine/src/mesh_cache.rs:7-32`
- Component: `HashMap<u64, Handle<Mesh>>`. Key = `ChunkData.hash` (`types.rs:70`). `hash == 0` rejected as sentinel by `get`/`insert` (`mesh_cache.rs:13-14, 20-22`).
- **No wiring observed**: not read or written in `lifecycle.rs` or `generation.rs`. No system in `lib.rs:46-59` touches it. **Cache has no invalidation path and is currently unused by gen/remesh pipelines.**

**System ordering — `VoxelPlugin` `Update` chain** (`lib.rs:46-59`)
```
ensure_pending_chunks
→ collect_tickets
→ update_chunks + poll_chunk_tasks   [run_if ChunkGenerationEnabled]
  OR reset_chunk_budgets              [otherwise]
→ despawn_out_of_range_chunks
→ drain_pending_saves
→ spawn_remesh_tasks
→ poll_remesh_tasks
```

**Stage progression inside `drain_gen_queue`** (`lifecycle.rs:684-753`)
- `Terrain` → `spawn_terrain_batch` batched `GEN_BATCH_SIZE=8` (`generation.rs:16`); disk cache check first (`generation.rs:60-82`); else `generator.generate_terrain` → `ChunkData::from_voxels(_, Terrain)`.
- `Features` → uniform fast-path promotes sync + re-enqueues (`lifecycle.rs:713-718`); mixed calls `build_surface_height_map` on main thread then `spawn_features_task` (`generation.rs:104`). Task runs `generator.place_features`; returns no new chunk_data (status updated in place `lifecycle.rs:898-904`).
- `Mesh` → `spawn_mesh_task` (`generation.rs:137`); task runs `mesh_chunk_greedy` + `ChunkData::from_voxels(_, Mesh)`.
- `Full` → synchronous in-place promotion, no task (`lifecycle.rs:748`).

## Q4: Collider generation & client/server split

### Findings

**`attach_chunk_colliders`** — `crates/protocol/src/map/colliders.rs:11-40`
- Query: `(Entity, &Mesh3d, &ChildOf, Option<&Collider>)` filtered `(With<VoxelChunk>, Or<(Changed<Mesh3d>, Added<Mesh3d>)>)`. Reads `Query<&MapInstanceId>` on parent and `Res<Assets<Mesh>>`.
- If `meshes.get(&mesh_handle.0)` returns `None` → `warn!` + `continue` (`colliders.rs:21-24`). **No retry.** In practice `poll_remesh_tasks` inserts the `Mesh` asset and the `Mesh3d` component in the same system (`lifecycle.rs:1150-1151`), so the asset is present when the Added event fires.
- Calls `Collider::trimesh_from_mesh` which calls `extract_mesh_vertices_indices` expecting `Mesh::ATTRIBUTE_POSITION` as `Float32/Float32x3` and `Mesh::indices()` as `U16/U32` (`git/avian/src/collision/collider/parry/mod.rs:1174-1183, 1499-1525`). Normals/UVs unused. Flags: `TrimeshFlags::MERGE_DUPLICATE_VERTICES` (`parry/mod.rs:1176-1179`). Returns `None` on missing positions/indices, wrong format, or degenerate geometry.
- `mesh_chunk_greedy` satisfies these requirements (`meshing.rs:57-64`).

**Registration**
- **Server**: `crates/server/src/map.rs:642` — registered in `Update` unconditionally.
- **Client**: NOT in `ClientMapPlugin` (`crates/client/src/map.rs:45-55`, comment at 51-54 states it's deferred). Registered instead in `ClientTransitionPlugin` at `crates/client/src/transition.rs:434`, inside a `.chain()` with `ApplyDeferred` before it, gated `run_if(in_game)` (`transition.rs:445`). So the **client DOES attach colliders**, just inside the transition chain — not disabled.
- Client `map.rs:45-70` registers `handle_voxel_broadcasts`, `handle_section_blocks_update`, `handle_voxel_edit_ack/reject` in `Update` gated `ClientState::InGame`, and `handle_voxel_input` in `PostUpdate` after `TransformSystems::Propagate`.

**Geometry consistency**
- Server has `ChunkGenerationEnabled` inserted (`map.rs:625`); gates `update_chunks`/`poll_chunk_tasks` (`lib.rs:51`). `spawn_remesh_tasks`/`poll_remesh_tasks` unconditional (`lib.rs:55-56`).
- Client does NOT insert `ChunkGenerationEnabled`. Instead receives voxels via `ChunkDataSync` (`map.rs:95-133`), stores via `handle_chunk_data_sync`, inserts into `instance.chunks_needing_remesh` at `map.rs:132` — then the **same** `spawn_remesh_tasks`/`poll_remesh_tasks` runs and calls `mesh_chunk_greedy(&voxels, &shape)` at `lifecycle.rs:1085`.
- **Server collider and client mesh both derive from `mesh_chunk_greedy` over the same voxel payload.** Server sends `chunk_data.voxels.clone()` (paletted) in `ChunkDataSync` (`server/src/map.rs:886`); client rehydrates via `ChunkData::from_voxels(&sync.data.to_voxels(), ...)` (`client/src/map.rs:125`).
- **No explicit hash/version verification.** Chunk size mismatch is detected and errored (`client/src/map.rs:117-124`). Consistency is structural — same deterministic function, same voxel bytes.

## Q5: Voxel generation & terrain example

### Findings

**`VoxelGeneratorImpl` trait** — `crates/voxel_map_engine/src/config.rs:13-25`
- `generate_terrain(&self, chunk_pos: IVec3) -> Vec<WorldVoxel>` (line 15): returns flat `padded_size³` array.
- `place_features(&self, chunk_pos, heights: &SurfaceHeightMap) -> Vec<WorldObjectSpawn>` (lines 19-25): default impl returns empty.
- Wrapped by `VoxelGenerator(pub Arc<dyn VoxelGeneratorImpl>)` component (`config.rs:86`).
- `SurfaceHeightMap` (`config.rs:53`): `{chunk_pos: IVec3, padded_size: u32, Box<[Option<f64>]>}` — `f64` heights indexed in padded XZ.

**Terrain example** — `crates/voxel_map_engine/examples/terrain.rs:30-33`
- Uses `FlatGenerator`, defined in `crates/voxel_map_engine/src/terrain.rs:501-504` with fixed `chunk_size=16`, `shape=[18,18,18]`.
- `FlatGenerator::place_features` is default (empty) (`terrain.rs:506-510`), so `SurfaceHeightMap` is never consumed by the example.
- In the full pipeline (`generation.rs:174-205`), `build_surface_height_map` scans each XZ column top-down (`py` from `chunk_size` → 1), finds the highest solid voxel with non-solid above, records `world_y = chunk_pos.y * chunk_size + (py - 1) + 1.0` as `f64` (`generation.rs:196`).

**Surface source data & elevation quantization**
- `FlatGenerator` (the example): `flat_terrain_voxels` in `crates/voxel_map_engine/src/meshing.rs:70-84`. Pure integer threshold — line 78: `let world_y = chunk_pos.y * chunk_size as i32 + y as i32 - 1;`, then line 79: `world_y <= 0 → Solid(0)` else `Air`. **Flat plane; no column variation.** The example does NOT produce adjacent columns differing by one.
- `HeightmapGenerator::generate_heightmap_chunk` (`terrain.rs`, also in the module): `height_cache` stores noise as `f64` (`terrain.rs:277-279`). Per voxel: `world_y: i32 = chunk_pos.y * chunk_size + py - 1` (`terrain.rs:242`); condition `(world_y as f64) <= terrain_height` (`terrain.rs:245`). **This integer-vs-float comparison IS the quantization step** — no explicit `.floor()`/`.round()`. Adjacent columns with noise heights straddling an integer boundary diverge by exactly one voxel row.

**Generation pipeline** — `crates/voxel_map_engine/src/generation.rs`
- `spawn_terrain_batch` (line 44): clones generator `Arc`, spawns `AsyncComputeTaskPool` task; disk cache check (lines 60-82) before `generate_terrain` (line 90).
- `generate_terrain` (line 227): `generator.generate_terrain(position)` (line 231) → `ChunkData::from_voxels(_, Terrain)` (line 234).
- `spawn_features_task` (line 104): takes prebuilt `SurfaceHeightMap`, calls `generator.place_features` (line 118) if no saved entities.
- `spawn_mesh_task` (line 137): takes raw `Vec<WorldVoxel>`, calls `mesh_chunk_greedy` → `ChunkData::from_voxels(_, Mesh)` (lines 149, 153).

## Q6: Client rendering, material, shader, post-processing

### Findings

**`DefaultVoxelMaterial`** — `crates/voxel_map_engine/src/lifecycle.rs:113-128`
- Newtype `Resource(pub Handle<StandardMaterial>)` — uses Bevy's built-in `StandardMaterial` directly, no `Material`/`MaterialExtension` trait impl.
- Initialized at `Startup` by `init_default_material` (`lifecycle.rs:118`, registered `lib.rs:45`): `base_color = srgb(0.5, 0.7, 0.3)`, `perceptual_roughness = 0.9`, all other fields default (no texture, no emissive, no normal map, metallic=0).

**Shaders**
- **No custom shader for chunks.** Rendering routes through Bevy `bevy_pbr` forward pipeline automatically.
- `assets/shaders/` contains only `billboard.wgsl`, `sprite_rig_billboard.wgsl`, `shadow_only.wgsl` — none touch chunk meshes.

**Vertex attributes consumed** — Bevy PBR consumes the four attributes produced by `mesh_chunk_greedy` (positions, flat normals, UV0, U32 indices). No vertex colors, no tangents.

**Chunk entity assembly** — two paths, identical bundle `(VoxelChunk, Mesh3d(handle), MeshMaterial3d(material), Transform)` parented to map entity:
- Initial gen in `handle_completed_chunk` (`lifecycle.rs:914-941`).
- Remesh in `poll_remesh_tasks` (`lifecycle.rs:1148-1176`); if entity already exists, only `Mesh3d` swapped in-place (`lifecycle.rs:1151`).
- Material selection identical in both (`lifecycle.rs:919-927`, `1156-1164`): `instance.debug_colors == true` → per-chunk HSL material via `color_from_chunk_pos` (`lifecycle.rs:861-867`); else `default_material.0.clone()`.

**Post-processing, AO, normal smoothing**
- **None.** `RenderPlugin` (`crates/render/src/lib.rs:17-61`) registers only billboard/sprite-rig/shadow-only material plugins, camera+lighting setup, health-bar observers, `SpriteRigPlugin`, `FrameInterpolationPlugin`. No SSAO, no post-process pipeline, no custom render graph nodes.
- Lighting: one `PointLight` (shadows on) + one `DirectionalLight` overcast (`crates/render/src/camera.rs:39-56`). All shading is default PBR over flat per-face normals.

## Cross-Cutting Observations

- **Greedy meshing is the single source of chunk geometry truth.** Both the client-rendered `Mesh3d` and the server's `Collider::trimesh_from_mesh` are produced by `mesh_chunk_greedy` applied to the same `Vec<WorldVoxel>`. Changing meshing output changes both simultaneously.
- **All geometry is axis-aligned cubic quads.** `block-mesh-rs` `OrientedBlockFace` hard-codes unit-vector axes (`axis.rs:21-27`); `WorldVoxel::Solid(u8)` carries a material byte but no shape/orientation. Non-cubic geometry would require either (a) post-mesh modification of the quad stream, (b) a parallel mesh-producing path that bypasses `mesh_chunk_greedy`, or (c) extending the voxel type with shape metadata and writing a new merger.
- **Normals are flat per-quad** (`face.rs:99-101`). Lit appearance relies entirely on per-face normals through Bevy default PBR.
- **`ChunkData.hash` exists but is not fully wired.** The hash is computed (`types.rs:119-124`) and a `MeshCache` component keyed on it exists (`mesh_cache.rs`), but no system in the template reads or writes the cache.
- **Neighbor padding sync only fires on runtime edits.** Initial generation bakes padding directly into the flat voxel array (`types.rs:93-103`); `update_neighbor_padding` is reached only through `VoxelWorld::set_voxel` (`instance.rs:124-153`, caller `api.rs:64`). Any slope scheme that makes visibility/mesh geometry depend on neighbors must also consider this asymmetry.
- **Client and server run the same `VoxelPlugin` remesh path.** The only client/server divergence is that `ChunkGenerationEnabled` is server-only (`map.rs:625`, gate at `lib.rs:51`); remesh spawn/poll runs on both.
- **Elevation quantization on the heightmap generator is implicit.** `terrain.rs:245` — `(world_y as f64) <= terrain_height` — integer row vs float height is where "columns differ by one" originates. No `.floor()`/`.round()` in the generator.

## Open Areas

- **`SurfaceHeightMap` fidelity beyond `f64` integer surfaces** — the struct stores `Option<f64>`, so continuous surface heights are representable, but `build_surface_height_map` (`generation.rs:174-205`) populates it by locating the top **solid voxel** (an integer row), so the stored `f64` is always an integer-valued surface even though the type admits sub-voxel precision. Whether this is a design constraint or incidental is not stated in the code.
- **`MeshCache`** exists as a component but has no observed reader/writer. Its intended lifecycle (when entries are added, when invalidated) is not documented in code — the only invariants are that `hash == 0` is a sentinel and the map never shrinks internally.
- **`avian3d` vendoring** — `Collider::trimesh_from_mesh` was inspected via `git/avian/src/collision/collider/parry/mod.rs`; whether the project currently uses the submoduled `git/avian` or a crates.io `avian3d` is not verified in this research.
