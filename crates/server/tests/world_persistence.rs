use std::sync::Arc;

use bevy::prelude::*;
use nostr_map_persistence::{MapPersistenceRejection, MapRevision, PayloadSlotState};
use persistence::Store;
use server::map::seed_from_nostr_public_key;
use server::persistence::fs_map_entities::FsMapEntitiesStore;
use server::persistence::fs_map_meta::FsMapMetaStore;
use server::persistence::{
    active_pointer_path, assemble_validated_map_save, cleanup_materialization_staging,
    map_save_dir, materialize_validated_map_save, revision_dir_name, store_map_dir_for_loading,
    FsAcceptedMapHeadStore, FsLocalMapHeadStore, MapMeta, SaveBase, ServerValidatedMapDelta,
    ServerValidatedMapSave, REVISIONS_DIR, STAGING_DIR,
};
use voxel_map_engine::config::{WorldObjectPositionKind, WorldObjectSpawn};
use voxel_map_engine::persistence::fs_chunk::FsChunkStore;
use voxel_map_engine::persistence::fs_chunk_entities::FsChunkEntitiesStore;
use voxel_map_engine::persistence::{chunk_file_path, ChunkFileEnvelope, CHUNK_SAVE_VERSION};
use voxel_map_engine::prelude::*;

use protocol::map::{SavedEntity, SavedEntityKind};
use protocol::{MapInstanceId, NostrPublicKey};

/// Padded chunk volume for the default `chunk_size=16`, used by tests.
const PADDED_VOLUME_16: usize = 18 * 18 * 18;

fn owner(byte: u8) -> NostrPublicKey {
    NostrPublicKey([byte; 32])
}

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

fn test_entity_store(dir: &std::path::Path) -> FsMapEntitiesStore {
    FsMapEntitiesStore {
        map_dir: Arc::new(dir.to_path_buf()),
    }
}

fn test_chunk_entities_store(dir: &std::path::Path) -> FsChunkEntitiesStore {
    FsChunkEntitiesStore {
        map_dir: Arc::new(dir.to_path_buf()),
    }
}

fn save_chunk(store: &FsChunkStore, pos: IVec3, chunk_size: u32, data: &ChunkData) {
    let envelope = ChunkFileEnvelope {
        version: CHUNK_SAVE_VERSION,
        chunk_size,
        data: data.clone(),
    };
    store.save(&pos, &envelope).unwrap();
}

#[test]
fn world_persistence_local_valid_map_meta_is_available_for_filesystem_preflight() {
    let tmp = tempfile::tempdir().unwrap();
    let map_dir = map_save_dir(tmp.path(), &MapInstanceId::Overworld);
    let store = test_meta_store(&map_dir);
    let meta = MapMeta {
        version: 1,
        seed: 4242,
        generation_version: 3,
        spawn_points: vec![Vec3::new(1.0, 2.0, 3.0)],
    };
    store.save(&(), &meta).expect("save meta");

    let loaded = store
        .load(&())
        .expect("load meta")
        .expect("filesystem preflight should see local metadata");
    assert_eq!(loaded.seed, 4242);
    assert_eq!(loaded.generation_version, 3);
}

#[test]
fn world_persistence_missing_homebase_meta_keeps_deterministic_seed_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let map_id = MapInstanceId::Homebase { owner: owner(9) };
    let map_dir = map_save_dir(tmp.path(), &map_id);
    let store = test_meta_store(&map_dir);
    assert!(store.load(&()).expect("load missing meta").is_none());
    assert_eq!(
        seed_from_nostr_public_key(owner(9)),
        u64::from_le_bytes([9; 8])
    );
}

fn remote_revision(byte: u8, revision: u64, previous_hash: Option<[u8; 32]>) -> MapRevision {
    MapRevision {
        revision,
        previous_hash,
        manifest_hash: [byte; 32],
    }
}

fn remote_chunk(fill: u8) -> ChunkFileEnvelope {
    let mut voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
    voxels[0] = WorldVoxel::Solid(fill);
    ChunkFileEnvelope {
        version: CHUNK_SAVE_VERSION,
        chunk_size: 16,
        data: ChunkData::from_voxels(&voxels, ChunkStatus::Full),
    }
}

fn remote_meta(seed: u64) -> MapMeta {
    MapMeta {
        version: 1,
        seed,
        generation_version: 0,
        spawn_points: vec![Vec3::new(0.0, 5.0, 0.0)],
    }
}

fn remote_spawn(object_id: &str) -> WorldObjectSpawn {
    WorldObjectSpawn {
        object_id: object_id.to_string(),
        position: Vec3::new(1.0, 2.0, 3.0),
        position_kind: WorldObjectPositionKind::PlacementBase,
        persisted_components: vec![],
    }
}

fn remote_save(seed: u64, revision: MapRevision) -> ServerValidatedMapSave {
    ServerValidatedMapSave {
        meta: remote_meta(seed),
        chunks: vec![(IVec3::ZERO, remote_chunk(7))],
        chunk_entities: vec![(IVec3::ZERO, vec![remote_spawn("crate")])],
        map_entities: Some(vec![SavedEntity {
            kind: SavedEntityKind::RespawnPoint,
            position: Vec3::new(0.0, 5.0, 0.0),
        }]),
        revision,
    }
}

#[test]
fn remote_restore_rematerializing_same_revision_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let map_dir = map_save_dir(tmp.path(), &MapInstanceId::Overworld);
    let revision = remote_revision(1, 1, None);
    let save = remote_save(1234, revision.clone());

    materialize_validated_map_save(&map_dir, &save).expect("first materialize");
    // Re-materializing the same content-addressed revision (e.g. a server restart that
    // re-selects the same remote head) must succeed. Heads live at the map's top-level dir,
    // so promotion must not require an accepted-head file inside the revision snapshot.
    materialize_validated_map_save(&map_dir, &save).expect("second materialize is idempotent");

    let active_dir = store_map_dir_for_loading(&map_dir).expect("active dir");
    assert_eq!(
        test_meta_store(&active_dir)
            .load(&())
            .unwrap()
            .unwrap()
            .seed,
        1234
    );
    assert_eq!(
        FsAcceptedMapHeadStore {
            map_dir: Arc::new(map_dir),
        }
        .load(&())
        .unwrap()
        .unwrap(),
        revision
    );
}

#[test]
fn remote_restore_self_heals_incomplete_revision_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let map_dir = map_save_dir(tmp.path(), &MapInstanceId::Overworld);
    let revision = remote_revision(1, 1, None);
    let save = remote_save(1234, revision.clone());

    materialize_validated_map_save(&map_dir, &save).expect("first materialize");

    // Simulate a pre-migration remnant: a revision directory missing the completeness marker.
    let revision_dir = map_dir
        .join(REVISIONS_DIR)
        .join(revision_dir_name(&revision));
    std::fs::remove_file(revision_dir.join("map.meta.bin")).expect("remove meta marker");

    materialize_validated_map_save(&map_dir, &save).expect("self-heals incomplete revision dir");
    let active_dir = store_map_dir_for_loading(&map_dir).expect("active dir");
    assert!(test_meta_store(&active_dir).load(&()).unwrap().is_some());
}

#[test]
fn remote_restore_missing_local_save_materializes_meta_chunks_and_entities() {
    let tmp = tempfile::tempdir().unwrap();
    let map_dir = map_save_dir(tmp.path(), &MapInstanceId::Overworld);
    let revision = remote_revision(1, 1, None);
    let save = remote_save(1234, revision.clone());

    materialize_validated_map_save(&map_dir, &save).expect("materialize remote save");
    let active_dir = store_map_dir_for_loading(&map_dir).expect("active dir");
    assert_ne!(active_dir, map_dir);

    assert_eq!(
        test_meta_store(&active_dir)
            .load(&())
            .unwrap()
            .unwrap()
            .seed,
        1234
    );
    assert!(test_chunk_store(&active_dir)
        .load(&IVec3::ZERO)
        .unwrap()
        .is_some());
    assert_eq!(
        test_chunk_entities_store(&active_dir)
            .load(&IVec3::ZERO)
            .unwrap()
            .unwrap()[0]
            .object_id,
        "crate"
    );
    assert_eq!(
        test_entity_store(&active_dir)
            .load(&())
            .unwrap()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        FsAcceptedMapHeadStore {
            map_dir: Arc::new(map_dir),
        }
        .load(&())
        .unwrap()
        .unwrap(),
        revision
    );
}

#[test]
fn remote_restore_delta_chain_replays_from_filesystem_base() {
    let base_revision = remote_revision(1, 1, None);
    let base = remote_save(10, base_revision.clone());
    let next_revision = remote_revision(2, 2, Some(base_revision.manifest_hash));
    let assembled = assemble_validated_map_save(
        SaveBase::Snapshot(base),
        vec![ServerValidatedMapDelta {
            revision: next_revision.clone(),
            meta: PayloadSlotState::Present(remote_meta(20)),
            chunks: vec![(IVec3::ONE, PayloadSlotState::Present(remote_chunk(8)))],
            chunk_entities: vec![],
            map_entities: PayloadSlotState::Absent,
        }],
    )
    .expect("assemble delta chain");

    assert_eq!(assembled.meta.seed, 20);
    assert_eq!(assembled.chunks.len(), 2);
    assert_eq!(assembled.revision, next_revision);
}

#[test]
fn remote_restore_incomplete_slot_rejected() {
    let result = assemble_validated_map_save(
        SaveBase::Empty,
        vec![ServerValidatedMapDelta {
            revision: remote_revision(1, 1, None),
            meta: PayloadSlotState::Absent,
            chunks: vec![],
            chunk_entities: vec![],
            map_entities: PayloadSlotState::Absent,
        }],
    );

    assert!(matches!(
        result,
        Err(MapPersistenceRejection::Incomplete(_))
    ));
}

#[test]
fn remote_restore_tombstone_removes_slot() {
    let base_revision = remote_revision(1, 1, None);
    let base = remote_save(10, base_revision.clone());
    let assembled = assemble_validated_map_save(
        SaveBase::Snapshot(base),
        vec![ServerValidatedMapDelta {
            revision: remote_revision(2, 2, Some(base_revision.manifest_hash)),
            meta: PayloadSlotState::Absent,
            chunks: vec![(IVec3::ZERO, PayloadSlotState::Tombstoned)],
            chunk_entities: vec![(IVec3::ZERO, PayloadSlotState::Tombstoned)],
            map_entities: PayloadSlotState::Empty,
        }],
    )
    .expect("assemble tombstone delta");

    assert!(assembled.chunks.is_empty());
    assert!(assembled.chunk_entities.is_empty());
    assert_eq!(assembled.map_entities.unwrap().len(), 0);
}

#[test]
fn remote_restore_accepted_head_written_after_files() {
    let tmp = tempfile::tempdir().unwrap();
    let map_dir = map_save_dir(tmp.path(), &MapInstanceId::Overworld);
    let save = remote_save(77, remote_revision(3, 3, None));

    materialize_validated_map_save(&map_dir, &save).expect("materialize remote save");
    let active_dir = store_map_dir_for_loading(&map_dir).expect("active dir");

    // Content lives in the active revision dir; head pointers live at the map top-level dir.
    assert!(active_dir.join("map.meta.bin").exists());
    assert!(map_dir.join("accepted_head.bin").exists());
    assert!(map_dir.join("local_head.bin").exists());
    assert!(!active_dir.join("accepted_head.bin").exists());
    assert!(active_pointer_path(&map_dir).exists());
}

#[test]
fn remote_restore_seeds_local_head_for_next_publish_revision() {
    let tmp = tempfile::tempdir().unwrap();
    let map_dir = map_save_dir(tmp.path(), &MapInstanceId::Overworld);
    let save = remote_save(77, remote_revision(9, 5, None));

    materialize_validated_map_save(&map_dir, &save).expect("materialize remote save");

    let local_head = FsLocalMapHeadStore {
        map_dir: Arc::new(map_dir.clone()),
    }
    .load(&())
    .expect("load local head")
    .expect("local head present at map top-level after restore");
    assert_eq!(local_head.local_revision_number, 5);

    // A post-restore publish must descend from the restored head (6), not reset to 1.
    let next = server::map::remote_publish::next_publish_revision_number(
        Some(&local_head),
        &server::persistence::RemotePublishJournal::default(),
        &server::map::remote_publish::PendingRemotePublishDeltas::default(),
        &server::map::remote_publish::PendingPublishBySaveId::default(),
    );
    assert_eq!(next, 6);
}

#[test]
fn remote_restore_staging_cleanup_removes_interrupted_revisions() {
    let tmp = tempfile::tempdir().unwrap();
    let map_dir = map_save_dir(tmp.path(), &MapInstanceId::Overworld);
    let staging = map_dir.join(STAGING_DIR).join("interrupted");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::create_dir_all(&map_dir).unwrap();
    std::fs::write(
        active_pointer_path(&map_dir).with_extension("tmp"),
        b"partial",
    )
    .unwrap();

    cleanup_materialization_staging(&map_dir).expect("cleanup staging");

    assert!(!staging.exists());
    assert!(!active_pointer_path(&map_dir).with_extension("tmp").exists());
}

#[test]
fn remote_restore_divergent_chain_preserves_filesystem() {
    let tmp = tempfile::tempdir().unwrap();
    let map_dir = map_save_dir(tmp.path(), &MapInstanceId::Overworld);
    test_meta_store(&map_dir)
        .save(&(), &remote_meta(55))
        .unwrap();

    let result = assemble_validated_map_save(
        SaveBase::Snapshot(remote_save(10, remote_revision(1, 1, None))),
        vec![ServerValidatedMapDelta {
            revision: remote_revision(2, 2, Some([9; 32])),
            meta: PayloadSlotState::Present(remote_meta(99)),
            chunks: vec![],
            chunk_entities: vec![],
            map_entities: PayloadSlotState::Absent,
        }],
    );

    assert!(matches!(result, Err(MapPersistenceRejection::Divergent(_))));
    assert_eq!(
        test_meta_store(&map_dir).load(&()).unwrap().unwrap().seed,
        55
    );
}

/// Save all dirty chunks from an instance via the store.
fn save_dirty_chunks_sync(instance: &mut VoxelMapInstance, store: &FsChunkStore) {
    let chunk_size = instance.chunk_size;
    let dirty: Vec<IVec3> = instance.dirty_chunks.drain().collect();
    for chunk_pos in dirty {
        if let Some(chunk_data) = instance.get_chunk_data(chunk_pos) {
            save_chunk(store, chunk_pos, chunk_size, chunk_data);
        }
    }
}

#[test]
fn terrain_persists_across_server_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let map_dir = tmp.path().join("overworld");
    let chunk_store = test_chunk_store(&map_dir);
    let meta_store = test_meta_store(&map_dir);

    // First run: save chunk data and metadata
    {
        let mut voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
        voxels[100] = WorldVoxel::Solid(42);
        let chunk_data = ChunkData::from_voxels(&voxels, ChunkStatus::Full);
        save_chunk(&chunk_store, IVec3::ZERO, 16, &chunk_data);

        let meta = MapMeta {
            version: 1,
            seed: 999,
            generation_version: 0,
            spawn_points: vec![Vec3::new(0.0, 5.0, 0.0)],
        };
        meta_store.save(&(), &meta).expect("save meta");
    }

    // Second run: verify data loads correctly
    {
        let loaded = chunk_store
            .load(&IVec3::ZERO)
            .expect("load chunk")
            .expect("chunk should exist");

        let loaded_voxels = loaded.data.voxels.to_voxels();
        assert_eq!(loaded_voxels[100], WorldVoxel::Solid(42));
        assert_eq!(loaded_voxels[0], WorldVoxel::Air);

        let meta = meta_store
            .load(&())
            .expect("load meta")
            .expect("meta should exist");
        assert_eq!(meta.seed, 999);
        assert_eq!(meta.spawn_points.len(), 1);
    }
}

#[test]
fn multiple_chunks_persist_independently() {
    let tmp = tempfile::tempdir().unwrap();
    let map_dir = tmp.path().join("overworld");
    let store = test_chunk_store(&map_dir);

    let positions = [
        IVec3::new(0, 0, 0),
        IVec3::new(1, 0, 0),
        IVec3::new(-1, 2, 3),
    ];

    // Save three chunks with distinct data
    for (i, &pos) in positions.iter().enumerate() {
        let mut voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
        voxels[i + 10] = WorldVoxel::Solid(i as u8 + 1);
        let chunk_data = ChunkData::from_voxels(&voxels, ChunkStatus::Full);
        save_chunk(&store, pos, 16, &chunk_data);
    }

    // Verify each loads independently with correct data
    for (i, &pos) in positions.iter().enumerate() {
        let loaded = store.load(&pos).unwrap().expect("chunk should exist");
        let voxels = loaded.data.voxels.to_voxels();
        assert_eq!(voxels[i + 10], WorldVoxel::Solid(i as u8 + 1));
    }

    // Verify files exist on disk
    for &pos in &positions {
        assert!(chunk_file_path(&map_dir, pos).exists());
    }
}

#[test]
fn map_save_dir_routes_correctly() {
    let base = std::path::Path::new("/tmp/test_worlds");
    assert_eq!(
        map_save_dir(base, &MapInstanceId::Overworld),
        std::path::PathBuf::from("/tmp/test_worlds/overworld")
    );
    assert_eq!(
        map_save_dir(base, &MapInstanceId::Homebase { owner: owner(0x2a) }),
        std::path::PathBuf::from(
            "/tmp/test_worlds/homebase_npub19g4z52329g4z52329g4z52329g4z52329g4z52329g4z52329g4qrd5mkx"
        )
    );
}

#[test]
fn dirty_instance_save_then_reload() {
    let tmp = tempfile::tempdir().unwrap();
    let map_dir = tmp.path().join("overworld");
    let store = test_chunk_store(&map_dir);

    // Create instance, make edits, save dirty chunks
    let mut instance = VoxelMapInstance::new(5, 16);
    let chunk_pos = IVec3::ZERO;
    let voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
    instance.insert_chunk_data(
        chunk_pos,
        ChunkData::from_voxels(&voxels, ChunkStatus::Full),
    );
    instance.chunk_levels.insert(chunk_to_column(chunk_pos), 0);

    // Mutate a voxel (marks chunk dirty)
    instance.set_voxel(IVec3::new(5, 5, 5), WorldVoxel::Solid(99));
    assert!(instance.dirty_chunks.contains(&chunk_pos));

    // Save dirty chunks
    save_dirty_chunks_sync(&mut instance, &store);
    assert!(instance.dirty_chunks.is_empty());

    // Reload from disk and verify the edit persisted
    let loaded = store
        .load(&chunk_pos)
        .unwrap()
        .expect("chunk should exist on disk");
    let local = IVec3::new(5, 5, 5);
    let padded = [
        (local.x + 1) as u32,
        (local.y + 1) as u32,
        (local.z + 1) as u32,
    ];
    let index = RuntimeShape::<u32, 3>::new([18, 18, 18]).linearize(padded) as usize;
    assert_eq!(loaded.data.voxels.get(index), WorldVoxel::Solid(99));
}

#[test]
fn meta_and_chunks_coexist_in_map_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let map_dir = tmp.path().join("overworld");
    let chunk_store = test_chunk_store(&map_dir);
    let meta_store = test_meta_store(&map_dir);

    // Save metadata
    let meta = MapMeta {
        version: 1,
        seed: 42,
        generation_version: 1,
        spawn_points: vec![Vec3::new(10.0, 20.0, 30.0)],
    };
    meta_store.save(&(), &meta).unwrap();

    // Save a chunk
    let voxels = vec![WorldVoxel::Solid(1); PADDED_VOLUME_16];
    save_chunk(
        &chunk_store,
        IVec3::ZERO,
        16,
        &ChunkData::from_voxels(&voxels, ChunkStatus::Full),
    );

    // Both should exist and load independently
    assert!(map_dir.join("map.meta.bin").exists());
    assert!(map_dir.join("terrain").exists());

    let loaded_meta = meta_store.load(&()).unwrap().expect("meta exists");
    assert_eq!(loaded_meta.seed, 42);

    let loaded_chunk = chunk_store
        .load(&IVec3::ZERO)
        .unwrap()
        .expect("chunk exists");
    assert_eq!(loaded_chunk.data.voxels.get(0), WorldVoxel::Solid(1));
}

#[test]
fn multiple_maps_save_independently() {
    let tmp = tempfile::tempdir().unwrap();
    let ow_dir = map_save_dir(tmp.path(), &MapInstanceId::Overworld);
    let hb_dir = map_save_dir(tmp.path(), &MapInstanceId::Homebase { owner: owner(42) });
    let ow_store = test_chunk_store(&ow_dir);
    let hb_store = test_chunk_store(&hb_dir);

    let mut ow_voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
    ow_voxels[0] = WorldVoxel::Solid(1);
    save_chunk(
        &ow_store,
        IVec3::ZERO,
        16,
        &ChunkData::from_voxels(&ow_voxels, ChunkStatus::Full),
    );

    let mut hb_voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
    hb_voxels[0] = WorldVoxel::Solid(99);
    save_chunk(
        &hb_store,
        IVec3::ZERO,
        16,
        &ChunkData::from_voxels(&hb_voxels, ChunkStatus::Full),
    );

    let ow_loaded = ow_store.load(&IVec3::ZERO).unwrap().unwrap();
    let hb_loaded = hb_store.load(&IVec3::ZERO).unwrap().unwrap();
    assert_eq!(ow_loaded.data.voxels.get(0), WorldVoxel::Solid(1));
    assert_eq!(hb_loaded.data.voxels.get(0), WorldVoxel::Solid(99));
}

#[test]
fn homebase_metadata_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let hb_dir = map_save_dir(tmp.path(), &MapInstanceId::Homebase { owner: owner(123) });
    let store = test_meta_store(&hb_dir);

    let meta = MapMeta {
        version: 1,
        seed: 123,
        generation_version: 0,
        spawn_points: vec![Vec3::new(0.0, 5.0, 0.0)],
    };
    store.save(&(), &meta).unwrap();

    let loaded = store.load(&()).unwrap().expect("meta should exist");
    assert_eq!(loaded.seed, 123);
}

#[test]
fn homebase_entities_saved_separately() {
    let tmp = tempfile::tempdir().unwrap();
    let ow_dir = map_save_dir(tmp.path(), &MapInstanceId::Overworld);
    let hb_dir = map_save_dir(tmp.path(), &MapInstanceId::Homebase { owner: owner(1) });
    let ow_store = test_entity_store(&ow_dir);
    let hb_store = test_entity_store(&hb_dir);

    ow_store
        .save(
            &(),
            &vec![SavedEntity {
                kind: SavedEntityKind::RespawnPoint,
                position: Vec3::ZERO,
            }],
        )
        .unwrap();
    hb_store
        .save(
            &(),
            &vec![
                SavedEntity {
                    kind: SavedEntityKind::RespawnPoint,
                    position: Vec3::ONE,
                },
                SavedEntity {
                    kind: SavedEntityKind::RespawnPoint,
                    position: Vec3::NEG_ONE,
                },
            ],
        )
        .unwrap();

    assert_eq!(ow_store.load(&()).unwrap().unwrap().len(), 1);
    assert_eq!(hb_store.load(&()).unwrap().unwrap().len(), 2);
}

#[test]
fn map_save_dir_different_homebases_are_isolated() {
    let base = std::path::Path::new("worlds");
    let dir1 = map_save_dir(base, &MapInstanceId::Homebase { owner: owner(1) });
    let dir2 = map_save_dir(base, &MapInstanceId::Homebase { owner: owner(2) });
    assert_ne!(dir1, dir2);
    assert_eq!(
        dir1,
        std::path::PathBuf::from(
            "worlds/homebase_npub1qyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqs8j9gdm"
        )
    );
    assert_eq!(
        dir2,
        std::path::PathBuf::from(
            "worlds/homebase_npub1qgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpq2yz0cn"
        )
    );
}

#[test]
fn overworld_and_homebase_dirs_are_isolated() {
    let base = std::path::Path::new("worlds");
    let ow = map_save_dir(base, &MapInstanceId::Overworld);
    let hb = map_save_dir(base, &MapInstanceId::Homebase { owner: owner(1) });
    assert_ne!(ow, hb);
}

#[test]
fn entities_persist_across_server_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let map_dir = tmp.path().join("overworld");
    let entity_store = test_entity_store(&map_dir);
    let meta_store = test_meta_store(&map_dir);

    // First run: save respawn points and metadata
    {
        let entities = vec![
            SavedEntity {
                kind: SavedEntityKind::RespawnPoint,
                position: Vec3::new(0.0, 5.0, 0.0),
            },
            SavedEntity {
                kind: SavedEntityKind::RespawnPoint,
                position: Vec3::new(10.0, 20.0, 30.0),
            },
        ];
        entity_store.save(&(), &entities).expect("save entities");

        let meta = MapMeta {
            version: 1,
            seed: 999,
            generation_version: 0,
            spawn_points: vec![Vec3::new(0.0, 5.0, 0.0), Vec3::new(10.0, 20.0, 30.0)],
        };
        meta_store.save(&(), &meta).expect("save meta");
    }

    // Verify entities.bin exists on disk
    assert!(map_dir.join("entities.bin").exists());

    // Second run: verify entities load correctly from disk
    {
        let loaded = entity_store
            .load(&())
            .expect("load entities")
            .expect("entities should exist");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].kind, SavedEntityKind::RespawnPoint);
        assert_eq!(loaded[0].position, Vec3::new(0.0, 5.0, 0.0));
        assert_eq!(loaded[1].position, Vec3::new(10.0, 20.0, 30.0));

        let meta = meta_store
            .load(&())
            .expect("load meta")
            .expect("meta should exist");
        assert_eq!(meta.spawn_points.len(), 2);
        assert!(meta.spawn_points.contains(&Vec3::new(0.0, 5.0, 0.0)));
        assert!(meta.spawn_points.contains(&Vec3::new(10.0, 20.0, 30.0)));
    }
}

#[test]
fn entities_and_chunks_coexist_in_map_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let map_dir = tmp.path().join("overworld");
    let entity_store = test_entity_store(&map_dir);
    let chunk_store = test_chunk_store(&map_dir);
    let meta_store = test_meta_store(&map_dir);

    // Save entities
    let entities = vec![SavedEntity {
        kind: SavedEntityKind::RespawnPoint,
        position: Vec3::new(5.0, 10.0, 15.0),
    }];
    entity_store.save(&(), &entities).unwrap();

    // Save a chunk
    let voxels = vec![WorldVoxel::Solid(1); PADDED_VOLUME_16];
    save_chunk(
        &chunk_store,
        IVec3::ZERO,
        16,
        &ChunkData::from_voxels(&voxels, ChunkStatus::Full),
    );

    // Save metadata
    let meta = MapMeta {
        version: 1,
        seed: 42,
        generation_version: 0,
        spawn_points: vec![Vec3::new(5.0, 10.0, 15.0)],
    };
    meta_store.save(&(), &meta).unwrap();

    // All three coexist and load independently
    assert!(map_dir.join("entities.bin").exists());
    assert!(map_dir.join("map.meta.bin").exists());
    assert!(map_dir.join("terrain").exists());

    let loaded_entities = entity_store.load(&()).unwrap().unwrap();
    assert_eq!(loaded_entities.len(), 1);

    let loaded_meta = meta_store.load(&()).unwrap().expect("meta exists");
    assert_eq!(loaded_meta.seed, 42);

    let loaded_chunk = chunk_store
        .load(&IVec3::ZERO)
        .unwrap()
        .expect("chunk exists");
    assert_eq!(loaded_chunk.data.voxels.get(0), WorldVoxel::Solid(1));
}
