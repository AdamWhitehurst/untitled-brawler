pub mod api;
pub mod brush;
pub mod chunk;
pub mod config;
pub mod generation;
pub mod instance;
pub mod lifecycle;
pub mod mesh_cache;
pub mod meshing;
pub mod palette;
pub mod persistence;
pub mod placement;
pub mod propagator;
pub mod raycast;
pub mod terrain;
pub mod ticket;
pub mod types;

pub use ndshape::{RuntimeShape, Shape};

use bevy::prelude::*;

/// Insert this resource to enable chunk generation systems (propagator,
/// task spawning/polling). Servers insert this; clients omit it since
/// they receive chunks via network push.
#[derive(Resource)]
pub struct ChunkGenerationEnabled;

pub struct VoxelPlugin;

impl Plugin for VoxelPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<config::MapDimensions>();
        app.register_type::<brush::TerrainBrushShape>();
        app.register_type::<brush::TerrainBrushMode>();
        app.register_type::<terrain::HeightMap>();
        app.register_type::<terrain::MoistureMap>();
        app.register_type::<terrain::BiomeRules>();
        app.register_type::<terrain::BiomeRule>();
        app.register_type::<terrain::NoiseDef>();
        app.register_type::<terrain::NoiseType>();
        app.register_type::<terrain::FractalType>();
        app.register_type::<terrain::PlacementRules>();
        app.register_type::<terrain::PlacementRule>();

        let generation_enabled = resource_exists::<ChunkGenerationEnabled>;

        app.add_message::<lifecycle::ChunkSaveCompleted>()
            .add_message::<lifecycle::ChunkSaveFailed>();
        app.add_systems(Startup, lifecycle::init_default_material);
        app.add_systems(
            Update,
            (
                lifecycle::ensure_pending_chunks,
                lifecycle::collect_tickets,
                (lifecycle::update_chunks, lifecycle::poll_chunk_tasks).run_if(generation_enabled),
                lifecycle::reset_chunk_budgets.run_if(not(generation_enabled)),
                lifecycle::despawn_out_of_range_chunks,
                lifecycle::drain_pending_saves,
                lifecycle::spawn_remesh_tasks,
                lifecycle::poll_remesh_tasks,
            )
                .chain(),
        );
    }
}

pub mod prelude {
    pub use crate::api::*;
    pub use crate::brush::*;
    pub use crate::chunk::*;
    pub use crate::config::*;
    pub use crate::generation::*;
    pub use crate::instance::*;
    pub use crate::lifecycle::{ChunkSaveCompleted, ChunkSaveFailed, DefaultVoxelMaterial};
    pub use crate::mesh_cache::*;
    pub use crate::meshing::*;
    pub use crate::palette::*;
    pub use crate::placement::*;
    pub use crate::propagator::*;
    pub use crate::raycast::*;
    pub use crate::terrain::*;
    pub use crate::ticket::*;
    pub use crate::types::*;
    pub use crate::{ChunkGenerationEnabled, RuntimeShape, Shape, VoxelPlugin};
}
