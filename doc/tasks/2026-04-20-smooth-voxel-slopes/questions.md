# Research Questions

## Context

Focus on the chunk terrain pipeline in `crates/voxel_map_engine/` (voxel storage, meshing, async chunk lifecycle) and its downstream consumers: collider generation in `crates/protocol/src/map/colliders.rs` and the vendored `git/block-mesh-rs/` library. Also cover how the terrain example (`crates/voxel_map_engine/examples/terrain.rs`) drives voxel data into that pipeline and how client/server each consume the resulting chunk entity.

## Questions

1. How does `crates/voxel_map_engine/src/meshing.rs::mesh_chunk_greedy` convert chunk voxel data into a Bevy `Mesh`? Detail the inputs it consumes, which trait methods on voxel types (`Voxel`, `MergeVoxel` in `types.rs`) drive face visibility and merging, the geometry it emits (quad count, vertex layout, position/normal/UV attributes, how per-face flat normals are produced), and what extension points `block-mesh-rs` (under `git/block-mesh-rs/src/`, including `greedy.rs`, `simple.rs`, and `geometry/`) exposes for producing non-axis-aligned or custom quads. Include what `git/block-mesh-rs/update.patch` modifies.

2. How is chunk voxel data stored, indexed, and kept in sync with neighbors? Cover the `ChunkData` layout and coordinate conventions in `crates/voxel_map_engine/src/types.rs` and `instance.rs`, how `update_neighbor_padding` propagates voxels across chunk boundaries and when it runs, and what voxel variants currently exist (`WorldVoxel`, `VoxelType`, `FillType`) — specifically whether any variant already expresses non-cubic geometry (slopes, ramps, stairs) or carries per-voxel shape/orientation metadata.

3. How does the async chunk pipeline in `crates/voxel_map_engine/src/lifecycle.rs` and `generation.rs` orchestrate mesh generation, caching, and remeshing? Cover `ChunkWorkBudget`, `PendingRemeshes`, `ChunkWorkTracker`, `GenQueue`, the `MeshCache` hash key and what invalidates it, and the ordering of gen → features → mesh tasks through `spawn_mesh_task` and `poll_chunk_tasks`.

4. How is the collider derived from a chunk mesh in `crates/protocol/src/map/colliders.rs::attach_chunk_colliders`, and how is chunk geometry work divided between client and server? Cover what `Collider::trimesh_from_mesh` expects from a Bevy mesh, when the system runs relative to mesh handle availability, why it is registered only on the server (`crates/server/src/map.rs`) and skipped on the client (`crates/client/src/map.rs`), and whether anything enforces that the client-rendered mesh and the server-generated collider describe the same geometry for the same voxel data.

5. How is chunk voxel data actually produced? Trace the `VoxelGenerator` trait (`crates/voxel_map_engine/src/config.rs`) and the terrain example (`crates/voxel_map_engine/examples/terrain.rs`, `src/generation.rs`): what decides whether a voxel is solid, how `SurfaceHeightMap` is built and consumed, and what source data causes horizontally adjacent columns to differ by exactly one unit of elevation.

6. How is the chunk mesh rendered on the client? Cover the material inserted alongside the mesh (`DefaultVoxelMaterial`), the shader(s) it binds, what vertex attributes the shader consumes (particularly normals and UVs), and whether any client-side post-processing, ambient occlusion, or normal-smoothing already exists.
