# Task: Smooth Single-Voxel Slopes in Chunk Terrain

Smooth the jagged edges of chunk terrain meshes and colliders so that a single-voxel elevation difference between two horizontally adjacent columns is rendered as a 45-degree slope instead of a vertical face plus a horizontal top.

The change must apply to both the visual mesh and the physics collider (server-side), and must remain consistent between client (mesh) and server (collider) representations of the same chunk voxel data.

## Visual reference

- **Before**: stepped/jagged profile — each elevation change is a vertical wall and a flat top.
- **After**: same footprint, but single-unit elevation changes are replaced with a 45-degree diagonal face connecting the lower top to the upper top.
