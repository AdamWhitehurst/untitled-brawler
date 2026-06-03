use std::sync::Arc;

use bevy::log::{info_span, trace};
use bevy::prelude::*;
use ndshape::{RuntimeShape, Shape};
use noise::{
    Fbm, HybridMulti, MultiFractal, NoiseFn, OpenSimplex, Perlin, RidgedMulti, ScalePoint,
    Seedable, SuperSimplex, Value, Worley,
};
use serde::{Deserialize, Serialize};

use crate::config::{
    SurfaceHeightMap, VoxelGenerator, VoxelGeneratorImpl, WorldObjectPositionKind, WorldObjectSpawn,
};
use crate::meshing::flat_terrain_voxels;
use crate::placement::jittered_grid_sample;
use crate::types::WorldVoxel;

/// Base noise algorithm.
#[derive(Clone, Debug, Serialize, Deserialize, Reflect, Default)]
pub enum NoiseType {
    #[default]
    Perlin,
    OpenSimplex,
    Value,
    Worley,
    SuperSimplex,
}

/// Fractal layering applied on top of a base noise. `Raw` uses the base noise directly.
#[derive(Clone, Debug, Serialize, Deserialize, Reflect, Default)]
pub enum FractalType {
    #[default]
    Fbm,
    RidgedMulti,
    HybridMulti,
    /// No fractal layering; use raw base noise.
    Raw,
}

/// Noise sampling parameters that define how a noise function is constructed.
#[derive(Clone, Debug, Serialize, Deserialize, Reflect)]
pub struct NoiseDef {
    pub noise_type: NoiseType,
    pub fractal: FractalType,
    pub seed_offset: u32,
    pub octaves: u32,
    pub frequency: f64,
    pub lacunarity: f64,
    pub persistence: f64,
}

/// Terrain height noise configuration. Presence enables noise-based terrain;
/// absence produces a flat plane at y=0.
#[derive(Component, Clone, Debug, Serialize, Deserialize, Reflect)]
#[reflect(Component, Serialize, Deserialize)]
pub struct HeightMap {
    pub noise: NoiseDef,
    pub base_height: i32,
    pub amplitude: f64,
}

/// Moisture noise configuration for biome selection.
#[derive(Component, Clone, Debug, Serialize, Deserialize, Reflect)]
#[reflect(Component, Serialize, Deserialize)]
pub struct MoistureMap {
    pub noise: NoiseDef,
}

/// Ordered list of biome selection rules. First matching rule wins.
#[derive(Component, Clone, Debug, Serialize, Deserialize, Reflect)]
#[reflect(Component, Serialize, Deserialize)]
pub struct BiomeRules(pub Vec<BiomeRule>);

/// A single biome's selection criteria and material assignment.
#[derive(Clone, Debug, Serialize, Deserialize, Reflect)]
pub struct BiomeRule {
    pub biome_id: String,
    pub height_range: (f64, f64),
    pub moisture_range: (f64, f64),
    pub surface_material: u8,
    pub subsurface_material: u8,
    pub subsurface_depth: u32,
}

/// Rules for procedural world object placement on terrain.
#[derive(Component, Clone, Debug, Serialize, Deserialize, Reflect)]
#[reflect(Component, Serialize, Deserialize)]
pub struct PlacementRules(pub Vec<PlacementRule>);

/// A single placement rule for one type of world object.
#[derive(Clone, Debug, Serialize, Deserialize, Reflect)]
pub struct PlacementRule {
    /// WorldObjectId string (matches `.object.ron` filename stem).
    pub object_id: String,
    /// Biome IDs where this object can spawn. Empty = all biomes.
    pub allowed_biomes: Vec<String>,
    /// Average objects per chunk (before filtering). Controls Poisson disk candidate count.
    pub density: f64,
    /// Minimum distance between objects of this type within a chunk.
    pub min_spacing: f64,
    /// Maximum terrain slope (rise/run) for placement. `None` = no slope filter.
    pub slope_max: Option<f64>,
    /// Height range (world Y) where this object can spawn.
    pub height_range: (f64, f64),
}

/// Constructs a 2D noise function from a [`NoiseDef`] and world seed.
///
/// Covers all 20 combinations of [`NoiseType`] x [`FractalType`].
/// The combined seed is `(seed as u32).wrapping_add(def.seed_offset)`.
pub fn build_noise_fn(def: &NoiseDef, seed: u64) -> Box<dyn NoiseFn<f64, 2>> {
    let combined_seed = (seed as u32).wrapping_add(def.seed_offset);

    match (&def.noise_type, &def.fractal) {
        (NoiseType::Perlin, FractalType::Raw) => build_raw(Perlin::new(combined_seed), def),
        (NoiseType::OpenSimplex, FractalType::Raw) => {
            build_raw(OpenSimplex::new(combined_seed), def)
        }
        (NoiseType::Value, FractalType::Raw) => build_raw(Value::new(combined_seed), def),
        (NoiseType::Worley, FractalType::Raw) => build_raw(Worley::new(combined_seed), def),
        (NoiseType::SuperSimplex, FractalType::Raw) => {
            build_raw(SuperSimplex::new(combined_seed), def)
        }

        (NoiseType::Perlin, FractalType::Fbm) => build_fbm::<Perlin>(combined_seed, def),
        (NoiseType::OpenSimplex, FractalType::Fbm) => build_fbm::<OpenSimplex>(combined_seed, def),
        (NoiseType::Value, FractalType::Fbm) => build_fbm::<Value>(combined_seed, def),
        (NoiseType::Worley, FractalType::Fbm) => build_fbm::<Worley>(combined_seed, def),
        (NoiseType::SuperSimplex, FractalType::Fbm) => {
            build_fbm::<SuperSimplex>(combined_seed, def)
        }

        (NoiseType::Perlin, FractalType::RidgedMulti) => build_ridged::<Perlin>(combined_seed, def),
        (NoiseType::OpenSimplex, FractalType::RidgedMulti) => {
            build_ridged::<OpenSimplex>(combined_seed, def)
        }
        (NoiseType::Value, FractalType::RidgedMulti) => build_ridged::<Value>(combined_seed, def),
        (NoiseType::Worley, FractalType::RidgedMulti) => build_ridged::<Worley>(combined_seed, def),
        (NoiseType::SuperSimplex, FractalType::RidgedMulti) => {
            build_ridged::<SuperSimplex>(combined_seed, def)
        }

        (NoiseType::Perlin, FractalType::HybridMulti) => build_hybrid::<Perlin>(combined_seed, def),
        (NoiseType::OpenSimplex, FractalType::HybridMulti) => {
            build_hybrid::<OpenSimplex>(combined_seed, def)
        }
        (NoiseType::Value, FractalType::HybridMulti) => build_hybrid::<Value>(combined_seed, def),
        (NoiseType::Worley, FractalType::HybridMulti) => build_hybrid::<Worley>(combined_seed, def),
        (NoiseType::SuperSimplex, FractalType::HybridMulti) => {
            build_hybrid::<SuperSimplex>(combined_seed, def)
        }
    }
}

fn build_raw<T>(base: T, def: &NoiseDef) -> Box<dyn NoiseFn<f64, 2>>
where
    T: NoiseFn<f64, 2> + 'static,
{
    Box::new(
        ScalePoint::new(base)
            .set_x_scale(def.frequency)
            .set_y_scale(def.frequency),
    )
}

fn build_fbm<T>(seed: u32, def: &NoiseDef) -> Box<dyn NoiseFn<f64, 2>>
where
    T: Default + Seedable + NoiseFn<f64, 2> + 'static,
{
    Box::new(
        Fbm::<T>::new(seed)
            .set_octaves(def.octaves as usize)
            .set_frequency(def.frequency)
            .set_lacunarity(def.lacunarity)
            .set_persistence(def.persistence),
    )
}

fn build_ridged<T>(seed: u32, def: &NoiseDef) -> Box<dyn NoiseFn<f64, 2>>
where
    T: Default + Seedable + NoiseFn<f64, 2> + 'static,
{
    Box::new(
        RidgedMulti::<T>::new(seed)
            .set_octaves(def.octaves as usize)
            .set_frequency(def.frequency)
            .set_lacunarity(def.lacunarity)
            .set_persistence(def.persistence),
    )
}

fn build_hybrid<T>(seed: u32, def: &NoiseDef) -> Box<dyn NoiseFn<f64, 2>>
where
    T: Default + Seedable + NoiseFn<f64, 2> + 'static,
{
    Box::new(
        HybridMulti::<T>::new(seed)
            .set_octaves(def.octaves as usize)
            .set_frequency(def.frequency)
            .set_lacunarity(def.lacunarity)
            .set_persistence(def.persistence),
    )
}

/// Generates voxel data for a single padded chunk using heightmap noise.
///
/// When `moisture_map` and `biome_rules` are both provided, biome-aware material
/// selection is used. Otherwise all solid voxels use material index 0.
pub fn generate_heightmap_chunk<S: Shape<3, Coord = u32>>(
    chunk_pos: IVec3,
    seed: u64,
    height_map: &HeightMap,
    moisture_map: Option<&MoistureMap>,
    biome_rules: Option<&BiomeRules>,
    chunk_size: u32,
    padded_size: u32,
    shape: &S,
) -> Vec<WorldVoxel> {
    let height_noise = build_noise_fn(&height_map.noise, seed);
    let moisture_noise = moisture_map.map(|m| build_noise_fn(&m.noise, seed));

    let height_cache = {
        let _span = info_span!("build_height_cache").entered();
        build_height_cache(
            chunk_pos,
            &*height_noise,
            height_map,
            chunk_size,
            padded_size,
        )
    };
    let moisture_cache = moisture_noise.as_ref().map(|noise| {
        let _span = info_span!("build_moisture_cache").entered();
        build_2d_cache(chunk_pos, &**noise, chunk_size, padded_size)
    });

    let _span = info_span!("fill_voxels").entered();
    let total = shape.usize();
    let mut voxels = vec![WorldVoxel::Air; total];

    for i in 0..shape.size() {
        let [px, py, pz] = shape.delinearize(i);
        let world_y = chunk_pos.y * chunk_size as i32 + py as i32 - 1;
        let terrain_height = height_cache[xz_index(px, pz, padded_size)];

        if (world_y as f64) <= terrain_height {
            let material = pick_material(
                world_y,
                terrain_height,
                xz_index(px, pz, padded_size),
                moisture_cache.as_deref(),
                biome_rules,
            );
            voxels[i as usize] = WorldVoxel::Solid(material);
        }
    }

    voxels
}

fn xz_index(px: u32, pz: u32, padded_size: u32) -> usize {
    px as usize * padded_size as usize + pz as usize
}

fn build_height_cache(
    chunk_pos: IVec3,
    noise: &dyn NoiseFn<f64, 2>,
    height_map: &HeightMap,
    chunk_size: u32,
    padded_size: u32,
) -> Vec<f64> {
    let mut cache = vec![0.0; (padded_size as usize).pow(2)];
    for px in 0..padded_size {
        for pz in 0..padded_size {
            let world_x = chunk_pos.x * chunk_size as i32 + px as i32 - 1;
            let world_z = chunk_pos.z * chunk_size as i32 + pz as i32 - 1;
            let sample = noise.get([world_x as f64, world_z as f64]);
            cache[xz_index(px, pz, padded_size)] =
                height_map.base_height as f64 + sample * height_map.amplitude;
        }
    }
    cache
}

fn build_2d_cache(
    chunk_pos: IVec3,
    noise: &dyn NoiseFn<f64, 2>,
    chunk_size: u32,
    padded_size: u32,
) -> Vec<f64> {
    let mut cache = vec![0.0; (padded_size as usize).pow(2)];
    for px in 0..padded_size {
        for pz in 0..padded_size {
            let world_x = chunk_pos.x * chunk_size as i32 + px as i32 - 1;
            let world_z = chunk_pos.z * chunk_size as i32 + pz as i32 - 1;
            cache[xz_index(px, pz, padded_size)] = noise.get([world_x as f64, world_z as f64]);
        }
    }
    cache
}

fn pick_material(
    world_y: i32,
    terrain_height: f64,
    cache_idx: usize,
    moisture_cache: Option<&[f64]>,
    biome_rules: Option<&BiomeRules>,
) -> u8 {
    let (Some(moisture), Some(rules)) = (moisture_cache, biome_rules) else {
        return 0;
    };

    let biome = select_biome(&rules.0, terrain_height, moisture[cache_idx]);
    let depth_below_surface = (terrain_height - world_y as f64) as u32;

    if depth_below_surface < biome.subsurface_depth {
        biome.surface_material
    } else {
        biome.subsurface_material
    }
}

/// Returns the first biome whose height and moisture ranges both contain the
/// given values. Falls back to the first rule if none match.
pub fn select_biome(rules: &[BiomeRule], height: f64, moisture: f64) -> &BiomeRule {
    debug_assert!(
        !rules.is_empty(),
        "BiomeRules must contain at least one rule"
    );
    rules
        .iter()
        .find(|r| {
            height >= r.height_range.0
                && height <= r.height_range.1
                && moisture >= r.moisture_range.0
                && moisture <= r.moisture_range.1
        })
        .unwrap_or(&rules[0])
}

/// Select a biome at a single world XZ position without building full noise caches.
///
/// Samples height and moisture noise at the given point and returns the matching
/// biome rule. Used by `place_features` for sparse point lookups.
fn select_biome_at_pos<'a>(
    seed: u64,
    height_map: &HeightMap,
    moisture_map: &MoistureMap,
    biome_rules: &'a BiomeRules,
    world_x: f64,
    world_z: f64,
) -> &'a BiomeRule {
    let height_noise = build_noise_fn(&height_map.noise, seed);
    let moisture_noise = build_noise_fn(&moisture_map.noise, seed);
    let terrain_height =
        height_map.base_height as f64 + height_noise.get([world_x, world_z]) * height_map.amplitude;
    let moisture = moisture_noise.get([world_x, world_z]);
    select_biome(&biome_rules.0, terrain_height, moisture)
}

/// Terrain generator using 2D heightmap noise with biome-aware material selection.
struct HeightmapGenerator {
    seed: u64,
    height_map: HeightMap,
    moisture_map: Option<MoistureMap>,
    biome_rules: Option<BiomeRules>,
    placement_rules: Option<PlacementRules>,
    chunk_size: u32,
    padded_size: u32,
    shape: RuntimeShape<u32, 3>,
}

impl VoxelGeneratorImpl for HeightmapGenerator {
    fn generate_terrain(&self, chunk_pos: IVec3) -> Vec<WorldVoxel> {
        generate_heightmap_chunk(
            chunk_pos,
            self.seed,
            &self.height_map,
            self.moisture_map.as_ref(),
            self.biome_rules.as_ref(),
            self.chunk_size,
            self.padded_size,
            &self.shape,
        )
    }

    fn place_features(
        &self,
        chunk_pos: IVec3,
        heights: &SurfaceHeightMap,
    ) -> Vec<WorldObjectSpawn> {
        let Some(ref rules) = self.placement_rules else {
            return Vec::new();
        };
        let mut spawns = Vec::new();

        for rule in rules.0.iter() {
            let candidates = jittered_grid_sample(
                self.seed,
                chunk_pos,
                self.chunk_size,
                rule.min_spacing,
                rule.density,
            );

            for world_pos in candidates {
                let local_x =
                    (world_pos.x - chunk_pos.x as f32 * self.chunk_size as f32).floor() as u32;
                let local_z =
                    (world_pos.y - chunk_pos.z as f32 * self.chunk_size as f32).floor() as u32;
                if local_x >= self.chunk_size || local_z >= self.chunk_size {
                    trace!("place_features: candidate ({local_x}, {local_z}) out of chunk bounds");
                    continue;
                }

                let Some(height) = heights.at(local_x + 1, local_z + 1) else {
                    trace!("place_features: no surface at ({local_x}, {local_z}), skipping");
                    continue;
                };

                if height < rule.height_range.0 || height > rule.height_range.1 {
                    continue;
                }

                if let Some(slope_max) = rule.slope_max
                    && exceeds_slope(
                        local_x + 1,
                        local_z + 1,
                        heights,
                        slope_max,
                        self.padded_size,
                    )
                {
                    continue;
                }

                if !rule.allowed_biomes.is_empty()
                    && let (Some(moisture_map), Some(biome_rules)) =
                        (&self.moisture_map, &self.biome_rules)
                {
                    let biome = select_biome_at_pos(
                        self.seed,
                        &self.height_map,
                        moisture_map,
                        biome_rules,
                        world_pos.x as f64,
                        world_pos.y as f64,
                    );
                    if !rule.allowed_biomes.iter().any(|b| b == &biome.biome_id) {
                        continue;
                    }
                }

                spawns.push(WorldObjectSpawn {
                    object_id: rule.object_id.clone(),
                    position: Vec3::new(world_pos.x, height as f32, world_pos.y),
                    position_kind: WorldObjectPositionKind::PlacementBase,
                    persisted_components: Vec::new(),
                });
            }
        }
        spawns
    }
}

/// Returns `true` if the terrain slope at padded coordinate `(px, pz)` exceeds
/// `max_slope` (rise/run).
///
/// Estimates slope from the height difference to cardinal neighbors in the
/// surface height map. Indices are padded-space; the 1-voxel border is
/// populated so chunk-edge neighbors resolve without clipping.
fn exceeds_slope(
    px: u32,
    pz: u32,
    heights: &SurfaceHeightMap,
    max_slope: f64,
    padded_size: u32,
) -> bool {
    let center = match heights.at(px, pz) {
        Some(h) => h,
        None => return false,
    };

    let neighbors: [(i32, i32); 4] = [(-1, 0), (1, 0), (0, -1), (0, 1)];
    for (dx, dz) in neighbors {
        let nx = px as i32 + dx;
        let nz = pz as i32 + dz;
        if nx < 0 || nx >= padded_size as i32 || nz < 0 || nz >= padded_size as i32 {
            continue;
        }
        if let Some(nh) = heights.at(nx as u32, nz as u32) {
            let slope = (nh - center).abs();
            if slope > max_slope {
                return true;
            }
        }
    }
    false
}

/// Flat terrain generator (no noise).
pub struct FlatGenerator {
    pub chunk_size: u32,
    pub shape: RuntimeShape<u32, 3>,
}

impl VoxelGeneratorImpl for FlatGenerator {
    fn generate_terrain(&self, chunk_pos: IVec3) -> Vec<WorldVoxel> {
        flat_terrain_voxels(chunk_pos, self.chunk_size, &self.shape)
    }
}

/// Build a [`VoxelGenerator`] from terrain components passed directly.
///
/// Used by spawn functions that read the def before components have been
/// inserted onto the entity (inline construction during map spawn).
/// If no `HeightMap` is present, falls back to [`FlatGenerator`].
pub fn build_generator_from_components(
    seed: u64,
    chunk_size: u32,
    padded_size: u32,
    shape: RuntimeShape<u32, 3>,
    height: Option<HeightMap>,
    moisture: Option<MoistureMap>,
    biomes: Option<BiomeRules>,
    placement: Option<PlacementRules>,
) -> VoxelGenerator {
    debug_assert!(
        moisture.is_none() || height.is_some(),
        "MoistureMap without HeightMap is meaningless"
    );
    debug_assert!(
        biomes.is_none() || height.is_some(),
        "BiomeRules without HeightMap is meaningless"
    );
    debug_assert!(
        moisture.is_some() || biomes.is_none(),
        "BiomeRules without MoistureMap: biome selection needs moisture values"
    );

    match height {
        Some(height_map) => VoxelGenerator(Arc::new(HeightmapGenerator {
            seed,
            height_map,
            moisture_map: moisture,
            biome_rules: biomes,
            placement_rules: placement,
            chunk_size,
            padded_size,
            shape,
        })),
        None => VoxelGenerator(Arc::new(FlatGenerator { chunk_size, shape })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noise::NoiseFn;

    fn padded_shape() -> RuntimeShape<u32, 3> {
        RuntimeShape::<u32, 3>::new([18, 18, 18])
    }

    fn default_noise_def() -> NoiseDef {
        NoiseDef {
            noise_type: NoiseType::Perlin,
            fractal: FractalType::Fbm,
            seed_offset: 0,
            octaves: 4,
            frequency: 0.01,
            lacunarity: 2.0,
            persistence: 0.5,
        }
    }

    fn make_biome_rule(
        id: &str,
        height: (f64, f64),
        moisture: (f64, f64),
        surface: u8,
    ) -> BiomeRule {
        BiomeRule {
            biome_id: id.to_string(),
            height_range: height,
            moisture_range: moisture,
            surface_material: surface,
            subsurface_material: 10,
            subsurface_depth: 3,
        }
    }

    #[test]
    fn build_noise_fn_deterministic() {
        let def = default_noise_def();
        let a = build_noise_fn(&def, 42);
        let b = build_noise_fn(&def, 42);
        assert_eq!(a.get([10.0, 20.0]), b.get([10.0, 20.0]));
    }

    #[test]
    fn build_noise_fn_different_seeds() {
        let def = default_noise_def();
        let a = build_noise_fn(&def, 42);
        let b = build_noise_fn(&def, 9999);
        assert_ne!(a.get([10.0, 20.0]), b.get([10.0, 20.0]));
    }

    #[test]
    fn generate_heightmap_chunk_non_flat() {
        let height_map = HeightMap {
            noise: NoiseDef {
                frequency: 0.005,
                ..default_noise_def()
            },
            base_height: 0,
            amplitude: 20.0,
        };

        let shape = padded_shape();
        let voxels =
            generate_heightmap_chunk(IVec3::ZERO, 42, &height_map, None, None, 16, 18, &shape);
        let air_count = voxels.iter().filter(|v| **v == WorldVoxel::Air).count();
        let solid_count = voxels
            .iter()
            .filter(|v| matches!(v, WorldVoxel::Solid(_)))
            .count();

        assert!(air_count > 0, "expected some air voxels");
        assert!(solid_count > 0, "expected some solid voxels");
    }

    #[test]
    fn generate_heightmap_chunk_all_underground() {
        let height_map = HeightMap {
            noise: NoiseDef {
                frequency: 0.01,
                ..default_noise_def()
            },
            base_height: 0,
            amplitude: 2.0,
        };

        // Chunk at y=-5 means world_y ranges roughly from -80..-64, well below surface.
        let shape = padded_shape();
        let voxels = generate_heightmap_chunk(
            IVec3::new(0, -5, 0),
            42,
            &height_map,
            None,
            None,
            16,
            18,
            &shape,
        );
        let solid_count = voxels
            .iter()
            .filter(|v| matches!(v, WorldVoxel::Solid(_)))
            .count();
        let total = shape.usize();

        assert_eq!(
            solid_count, total,
            "expected all voxels to be solid underground"
        );
    }

    #[test]
    fn select_biome_exact_match() {
        let rules = vec![
            make_biome_rule("plains", (-100.0, 0.0), (-1.0, 0.0), 1),
            make_biome_rule("desert", (0.0, 100.0), (0.0, 1.0), 2),
        ];

        let result = select_biome(&rules, 50.0, 0.5);
        assert_eq!(result.biome_id, "desert");
    }

    #[test]
    fn select_biome_fallback() {
        let rules = vec![
            make_biome_rule("plains", (0.0, 10.0), (0.0, 0.5), 1),
            make_biome_rule("desert", (20.0, 30.0), (0.5, 1.0), 2),
        ];

        let result = select_biome(&rules, 999.0, 999.0);
        assert_eq!(result.biome_id, "plains");
    }

    #[test]
    fn select_biome_first_wins() {
        let rules = vec![
            make_biome_rule("first", (0.0, 100.0), (0.0, 1.0), 1),
            make_biome_rule("second", (0.0, 100.0), (0.0, 1.0), 2),
        ];

        let result = select_biome(&rules, 50.0, 0.5);
        assert_eq!(result.biome_id, "first");
    }

    #[test]
    fn generate_heightmap_chunk_with_biomes() {
        let height_map = HeightMap {
            noise: NoiseDef {
                frequency: 0.005,
                ..default_noise_def()
            },
            base_height: 0,
            amplitude: 20.0,
        };
        let moisture_map = MoistureMap {
            noise: NoiseDef {
                seed_offset: 1000,
                frequency: 0.01,
                ..default_noise_def()
            },
        };
        let biome_rules = BiomeRules(vec![
            make_biome_rule("grass", (-100.0, 100.0), (-1.0, 0.0), 1),
            make_biome_rule("sand", (-100.0, 100.0), (0.0, 1.0), 2),
        ]);

        let shape = padded_shape();
        let voxels = generate_heightmap_chunk(
            IVec3::ZERO,
            42,
            &height_map,
            Some(&moisture_map),
            Some(&biome_rules),
            16,
            18,
            &shape,
        );

        let mut materials: std::collections::HashSet<u8> = std::collections::HashSet::new();
        for v in &voxels {
            if let WorldVoxel::Solid(m) = v {
                materials.insert(*m);
            }
        }

        assert!(
            materials.len() >= 2,
            "expected at least 2 different materials, got {materials:?}"
        );
    }
}
