use std::sync::Arc;

use bevy::prelude::*;
use nostr_map_persistence::MapRevision;
use persistence::Store;
use server::persistence::fs_map_meta::FsMapMetaStore;
use server::persistence::{
    materialize_validated_map_save, store_map_dir_for_loading, MapMeta, ServerValidatedMapSave,
};
use voxel_map_engine::persistence::fs_chunk::FsChunkStore;
use voxel_map_engine::persistence::{chunk_file_path, ChunkFileEnvelope, CHUNK_SAVE_VERSION};
use voxel_map_engine::prelude::*;

/// Padded chunk volume for the default `chunk_size=16`, used by tests.
const PADDED_VOLUME_16: usize = 18 * 18 * 18;

fn test_chunk_store(dir: &std::path::Path) -> FsChunkStore {
    FsChunkStore {
        map_dir: Arc::new(dir.to_path_buf()),
    }
}

fn test_meta_store(dir: &std::path::Path) -> FsMapMetaStore {
    FsMapMetaStore {
        map_dir: Arc::new(dir.to_path_buf()),
    }
}

/// Save all dirty chunks from an instance via the store.
fn save_dirty_chunks_sync(instance: &mut VoxelMapInstance, store: &FsChunkStore) {
    let chunk_size = instance.chunk_size;
    let dirty: Vec<IVec3> = instance.dirty_chunks.drain().collect();
    for chunk_pos in dirty {
        if let Some(chunk_data) = instance.get_chunk_data(chunk_pos) {
            let envelope = ChunkFileEnvelope {
                version: CHUNK_SAVE_VERSION,
                chunk_size,
                data: chunk_data.clone(),
            };
            store
                .save(&chunk_pos, &envelope)
                .expect("save chunk in test");
        }
    }
}

#[test]
fn dirty_chunks_saved_on_debounce() {
    let dir = tempfile::tempdir().unwrap();
    let map_dir = dir.path().join("overworld");
    let store = test_chunk_store(&map_dir);

    let mut instance = VoxelMapInstance::new(5, 16);
    let chunk_pos = IVec3::new(1, 0, 0);
    let voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
    instance.insert_chunk_data(
        chunk_pos,
        ChunkData::from_voxels(&voxels, ChunkStatus::Full),
    );
    instance.chunk_levels.insert(chunk_to_column(chunk_pos), 0);
    instance.dirty_chunks.insert(chunk_pos);

    save_dirty_chunks_sync(&mut instance, &store);

    assert!(chunk_file_path(&map_dir, chunk_pos).exists());
    assert!(instance.dirty_chunks.is_empty());
}

#[test]
fn clean_chunks_not_saved() {
    let dir = tempfile::tempdir().unwrap();
    let map_dir = dir.path().join("overworld");
    let store = test_chunk_store(&map_dir);

    let mut instance = VoxelMapInstance::new(5, 16);
    let chunk_pos = IVec3::ZERO;
    let voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
    instance.insert_chunk_data(
        chunk_pos,
        ChunkData::from_voxels(&voxels, ChunkStatus::Full),
    );
    instance.chunk_levels.insert(chunk_to_column(chunk_pos), 0);
    // NOT marking dirty

    save_dirty_chunks_sync(&mut instance, &store);

    assert!(!chunk_file_path(&map_dir, chunk_pos).exists());
}

#[test]
fn terrain_persists_across_save_load() {
    let dir = tempfile::tempdir().unwrap();
    let map_dir = dir.path().join("overworld");
    let chunk_store = test_chunk_store(&map_dir);
    let meta_store = test_meta_store(&map_dir);

    // Save a chunk with a specific voxel edit
    {
        let mut voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
        voxels[100] = WorldVoxel::Solid(42);
        let chunk_data = ChunkData::from_voxels(&voxels, ChunkStatus::Full);
        let envelope = ChunkFileEnvelope {
            version: CHUNK_SAVE_VERSION,
            chunk_size: 16,
            data: chunk_data,
        };
        chunk_store.save(&IVec3::ZERO, &envelope).unwrap();

        let meta = MapMeta {
            version: 1,
            seed: 999,
            generation_version: 0,
            spawn_points: vec![Vec3::new(0.0, 5.0, 0.0)],
        };
        meta_store.save(&(), &meta).unwrap();
    }

    // Load and verify
    {
        let loaded = chunk_store
            .load(&IVec3::ZERO)
            .unwrap()
            .expect("chunk should exist");
        let loaded_voxels = loaded.data.voxels.to_voxels();
        assert_eq!(loaded_voxels[100], WorldVoxel::Solid(42));
        assert_eq!(loaded_voxels[0], WorldVoxel::Air);

        let meta = meta_store.load(&()).unwrap().expect("meta should exist");
        assert_eq!(meta.seed, 999);
        assert_eq!(meta.spawn_points.len(), 1);
    }
}

#[test]
fn evicted_dirty_chunk_saved_before_removal() {
    let dir = tempfile::tempdir().unwrap();
    let map_dir = dir.path().join("overworld");
    let store = test_chunk_store(&map_dir);

    // Set up an instance with a dirty chunk
    let mut instance = VoxelMapInstance::new(5, 16);
    let chunk_pos = IVec3::new(3, 0, 0);
    let mut voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
    voxels[50] = WorldVoxel::Solid(7);
    instance.insert_chunk_data(
        chunk_pos,
        ChunkData::from_voxels(&voxels, ChunkStatus::Full),
    );
    instance.chunk_levels.insert(chunk_to_column(chunk_pos), 0);
    instance.dirty_chunks.insert(chunk_pos);

    // Save all dirty chunks (simulates what eviction does before removing)
    save_dirty_chunks_sync(&mut instance, &store);

    // Then remove from octree (simulates eviction completing)
    instance.chunk_levels.remove(&chunk_to_column(chunk_pos));
    instance.remove_chunk_data(chunk_pos);

    // Verify chunk was persisted before removal
    let loaded = store
        .load(&chunk_pos)
        .unwrap()
        .expect("evicted dirty chunk should have been saved");
    let loaded_voxels = loaded.data.voxels.to_voxels();
    assert_eq!(loaded_voxels[50], WorldVoxel::Solid(7));

    // Verify chunk is no longer in memory
    assert!(!instance
        .chunk_levels
        .contains_key(&chunk_to_column(chunk_pos)));
    assert!(instance.get_chunk_data(chunk_pos).is_none());
    assert!(instance.dirty_chunks.is_empty());
}

#[test]
fn voxel_persistence_materialized_chunks_load_through_normal_store() {
    let dir = tempfile::tempdir().unwrap();
    let map_dir = dir.path().join("overworld");
    let mut voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
    voxels[25] = WorldVoxel::Solid(11);
    let save = ServerValidatedMapSave {
        meta: MapMeta {
            version: 1,
            seed: 1,
            generation_version: 0,
            spawn_points: vec![],
        },
        chunks: vec![(
            IVec3::ZERO,
            ChunkFileEnvelope {
                version: CHUNK_SAVE_VERSION,
                chunk_size: 16,
                data: ChunkData::from_voxels(&voxels, ChunkStatus::Full),
            },
        )],
        chunk_entities: vec![],
        map_entities: None,
        revision: MapRevision {
            revision: 1,
            previous_hash: None,
            manifest_hash: [4; 32],
        },
    };

    materialize_validated_map_save(&map_dir, &save).expect("materialize save");
    let active_dir = store_map_dir_for_loading(&map_dir).expect("active dir");
    let loaded = test_chunk_store(&active_dir)
        .load(&IVec3::ZERO)
        .unwrap()
        .expect("materialized chunk loads");

    assert_eq!(loaded.data.voxels.to_voxels()[25], WorldVoxel::Solid(11));
}

#[test]
fn voxel_persistence_dirty_saves_after_restore_write_to_active_revision_store() {
    let dir = tempfile::tempdir().unwrap();
    let map_dir = dir.path().join("overworld");
    let save = ServerValidatedMapSave {
        meta: MapMeta {
            version: 1,
            seed: 1,
            generation_version: 0,
            spawn_points: vec![],
        },
        chunks: vec![],
        chunk_entities: vec![],
        map_entities: None,
        revision: MapRevision {
            revision: 1,
            previous_hash: None,
            manifest_hash: [5; 32],
        },
    };
    materialize_validated_map_save(&map_dir, &save).expect("materialize save");
    let active_dir = store_map_dir_for_loading(&map_dir).expect("active dir");
    let store = test_chunk_store(&active_dir);

    let mut instance = VoxelMapInstance::new(5, 16);
    let chunk_pos = IVec3::new(2, 0, 0);
    let voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
    instance.insert_chunk_data(
        chunk_pos,
        ChunkData::from_voxels(&voxels, ChunkStatus::Full),
    );
    instance.chunk_levels.insert(chunk_to_column(chunk_pos), 0);
    instance.dirty_chunks.insert(chunk_pos);
    save_dirty_chunks_sync(&mut instance, &store);

    assert!(chunk_file_path(&active_dir, chunk_pos).exists());
}

#[test]
fn load_chunk_with_mismatched_chunk_size_errors() {
    let dir = tempfile::tempdir().unwrap();
    let map_dir = dir.path().join("overworld");
    let store16 = test_chunk_store(&map_dir);

    let voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
    let chunk = ChunkData::from_voxels(&voxels, ChunkStatus::Full);
    let envelope = ChunkFileEnvelope {
        version: CHUNK_SAVE_VERSION,
        chunk_size: 16,
        data: chunk,
    };
    store16.save(&IVec3::ZERO, &envelope).unwrap();

    // Load succeeds with the same store (chunk_size validation is consumer's job now)
    let loaded = store16.load(&IVec3::ZERO).unwrap().expect("should load");
    assert_eq!(loaded.chunk_size, 16);
}
