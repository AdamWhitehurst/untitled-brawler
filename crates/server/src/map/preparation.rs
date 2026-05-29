use std::sync::Arc;

use bevy::prelude::*;
use nostr_map_persistence::MapPersistenceRejection;
use persistence::{PendingStoreOps, StoreBackend};
use protocol::map::SavedEntity;
use protocol::{MapInstanceId, MapRegistry, NostrPublicKey};
use voxel_map_engine::config::WorldObjectSpawn;
use voxel_map_engine::persistence::{
    fs_chunk::FsChunkStore, fs_chunk_entities::FsChunkEntitiesStore, ChunkFileEnvelope,
};
use voxel_map_engine::prelude::{Homebase, MapDimensions, VoxelMapConfig};

use crate::persistence::fs_map_entities::FsMapEntitiesStore;
use crate::persistence::fs_map_meta::FsMapMetaStore;
use crate::persistence::{map_save_dir, store_map_dir_for_loading, MapMeta, WorldSavePath};

use super::{MapLoadState, MapPreparation, MapTransitionParams};

/// Returns whether an existing or newly-placeholdered map is usable for transition commit.
pub fn ensure_map_exists(
    commands: &mut Commands,
    registry: &mut MapRegistry,
    map_state_query: &Query<Ref<MapLoadState>>,
    map_params_query: &Query<(&VoxelMapConfig, &MapDimensions)>,
    save_path: &WorldSavePath,
    map_id: &MapInstanceId,
) -> MapPreparation {
    if let Some(&entity) = registry.0.get(map_id) {
        let state = map_state_query
            .get(entity)
            .expect("registered map entity must have MapLoadState");
        match &*state {
            MapLoadState::Ready => {
                let (config, dimensions) = map_params_query
                    .get(entity)
                    .expect("ready map entity must have VoxelMapConfig + MapDimensions");
                return MapPreparation::Ready {
                    entity,
                    params: transition_params(config, dimensions),
                };
            }
            MapLoadState::Blocked(reason) => return MapPreparation::Blocked(reason.clone()),
            MapLoadState::CheckingPersistence
            | MapLoadState::AwaitingMeta
            | MapLoadState::AwaitingEntities => {
                if state.is_changed() {
                    trace!(
                        ?map_id,
                        state = ?*state,
                        "map exists but is not ready for transition yet"
                    );
                }
                return MapPreparation::Pending;
            }
        }
    }

    match map_id {
        MapInstanceId::Overworld => panic!("overworld must be registered before map preparation"),
        MapInstanceId::Homebase { owner } => {
            let entity = spawn_homebase_preflight_placeholder_with_stores(
                commands,
                save_path,
                *owner,
                MapLoadState::CheckingPersistence,
            );
            registry.0.insert(map_id.clone(), entity);
            trace!(
                ?map_id,
                ?entity,
                "spawned homebase placeholder pending persistence preflight"
            );
            MapPreparation::Pending
        }
    }
}

/// Spawn a homebase placeholder with persistence stores before metadata selection completes.
pub fn spawn_homebase_preflight_placeholder_with_stores(
    commands: &mut Commands,
    save_path: &WorldSavePath,
    owner: NostrPublicKey,
    state: MapLoadState,
) -> Entity {
    let map_id = MapInstanceId::Homebase { owner };
    let canonical_map_dir = map_save_dir(&save_path.0, &map_id);
    let map_dir = Arc::new(
        store_map_dir_for_loading(&canonical_map_dir)
            .expect("homebase active revision pointer must be valid before preflight"),
    );
    commands
        .spawn((
            Homebase,
            protocol::map::Owner(owner),
            Transform::default(),
            map_id,
            state,
            StoreBackend::new(FsMapMetaStore {
                map_dir: map_dir.clone(),
            }),
            PendingStoreOps::<(), MapMeta>::default(),
            StoreBackend::new(FsMapEntitiesStore {
                map_dir: map_dir.clone(),
            }),
            PendingStoreOps::<(), Vec<SavedEntity>>::default(),
            StoreBackend::new(FsChunkEntitiesStore {
                map_dir: map_dir.clone(),
            }),
            PendingStoreOps::<IVec3, Vec<WorldObjectSpawn>>::default(),
            StoreBackend::new(FsChunkStore { map_dir }),
            PendingStoreOps::<IVec3, ChunkFileEnvelope>::default(),
        ))
        .id()
}

fn transition_params(config: &VoxelMapConfig, dimensions: &MapDimensions) -> MapTransitionParams {
    MapTransitionParams {
        seed: config.seed,
        generation_version: config.generation_version,
        bounds: dimensions.bounds,
        chunk_size: dimensions.chunk_size,
        column_y_range: dimensions.column_y_range,
    }
}

/// Convert a rejection into a preparation state without losing the detailed reason.
pub fn blocked_preparation(reason: MapPersistenceRejection) -> MapPreparation {
    MapPreparation::Blocked(reason)
}
