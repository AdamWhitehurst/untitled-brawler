pub mod fs_chunk;
pub mod fs_chunk_entities;

use std::path::{Path, PathBuf};

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::config::WorldObjectSpawn;
use crate::types::ChunkData;

pub const CHUNK_SAVE_VERSION: u32 = 4;
pub(crate) const ZSTD_COMPRESSION_LEVEL: i32 = 3;

/// Versioned envelope wrapping chunk data on disk.
///
/// `chunk_size` records the map's edge length at save time so load-time
/// validation can reject data authored under a different configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChunkFileEnvelope {
    pub version: u32,
    pub chunk_size: u32,
    pub data: ChunkData,
}

/// Returns the file path for a chunk at the given position within a map directory.
pub fn chunk_file_path(map_dir: &Path, chunk_pos: IVec3) -> PathBuf {
    map_dir.join("terrain").join(format!(
        "chunk_{}_{}_{}.bin",
        chunk_pos.x, chunk_pos.y, chunk_pos.z
    ))
}

/// Parse a chunk filename like `chunk_1_-2_3.bin` into an `IVec3`.
pub fn parse_chunk_filename(name: &str) -> Option<IVec3> {
    let name = name.strip_prefix("chunk_")?.strip_suffix(".bin")?;
    let last_sep = name.rfind('_')?;
    let z: i32 = name[last_sep + 1..].parse().ok()?;
    let rest = &name[..last_sep];
    let mid_sep = rest.rfind('_')?;
    let y: i32 = rest[mid_sep + 1..].parse().ok()?;
    let x: i32 = rest[..mid_sep].parse().ok()?;
    Some(IVec3::new(x, y, z))
}

pub(crate) const ENTITY_SAVE_VERSION: u32 = 3;

/// Versioned envelope wrapping per-chunk entity spawn data on disk.
#[derive(Serialize, Deserialize)]
pub(crate) struct EntityFileEnvelope {
    pub version: u32,
    pub spawns: Vec<WorldObjectSpawn>,
}

/// File path for per-chunk entity data.
pub fn entity_file_path(map_dir: &Path, chunk_pos: IVec3) -> PathBuf {
    map_dir.join("entities").join(format!(
        "chunk_{}_{}_{}.entities.bin",
        chunk_pos.x, chunk_pos.y, chunk_pos.z
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PersistedComponent, WorldObjectPositionKind};
    use crate::types::{ChunkStatus, WorldVoxel};
    use persistence::Store;
    use std::sync::Arc;

    use fs_chunk::FsChunkStore;
    use fs_chunk_entities::FsChunkEntitiesStore;

    /// Padded chunk volume for the default `chunk_size=16`, used by tests.
    const PADDED_VOLUME_16: usize = 18 * 18 * 18;

    fn test_chunk_store(dir: &Path) -> FsChunkStore {
        FsChunkStore {
            map_dir: Arc::new(dir.to_path_buf()),
        }
    }

    fn test_entity_store(dir: &Path) -> FsChunkEntitiesStore {
        FsChunkEntitiesStore {
            map_dir: Arc::new(dir.to_path_buf()),
        }
    }

    #[test]
    fn save_load_chunk_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_chunk_store(dir.path());
        let pos = IVec3::new(1, -2, 3);
        let mut voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
        voxels[100] = WorldVoxel::Solid(5);
        let chunk = ChunkData::from_voxels(&voxels, ChunkStatus::Full);
        let envelope = ChunkFileEnvelope {
            version: CHUNK_SAVE_VERSION,
            chunk_size: 16,
            data: chunk.clone(),
        };

        store.save(&pos, &envelope).unwrap();
        let loaded = store.load(&pos).unwrap().expect("chunk should exist");
        assert_eq!(loaded.data.voxels.to_voxels(), chunk.voxels.to_voxels());
    }

    #[test]
    fn load_nonexistent_chunk_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_chunk_store(dir.path());
        assert!(store.load(&IVec3::ZERO).unwrap().is_none());
    }

    #[test]
    fn save_chunk_creates_directories() {
        let dir = tempfile::tempdir().unwrap();
        let map_dir = dir.path().join("deep/nested/map");
        let store = test_chunk_store(&map_dir);
        let voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
        let envelope = ChunkFileEnvelope {
            version: CHUNK_SAVE_VERSION,
            chunk_size: 16,
            data: ChunkData::from_voxels(&voxels, ChunkStatus::Full),
        };
        store.save(&IVec3::ZERO, &envelope).unwrap();
        assert!(map_dir.join("terrain").exists());
    }

    #[test]
    fn corrupt_chunk_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_chunk_store(dir.path());
        let path = chunk_file_path(dir.path(), IVec3::ZERO);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not valid data").unwrap();
        assert!(store.load(&IVec3::ZERO).is_err());
    }

    #[test]
    fn parse_chunk_filename_valid() {
        assert_eq!(
            parse_chunk_filename("chunk_1_2_3.bin"),
            Some(IVec3::new(1, 2, 3))
        );
        assert_eq!(parse_chunk_filename("chunk_0_0_0.bin"), Some(IVec3::ZERO));
    }

    #[test]
    fn parse_chunk_filename_negative_coords() {
        assert_eq!(
            parse_chunk_filename("chunk_-1_0_2.bin"),
            Some(IVec3::new(-1, 0, 2))
        );
        assert_eq!(
            parse_chunk_filename("chunk_-10_-20_-30.bin"),
            Some(IVec3::new(-10, -20, -30))
        );
    }

    #[test]
    fn parse_chunk_filename_invalid() {
        assert_eq!(parse_chunk_filename("not_a_chunk.bin"), None);
        assert_eq!(parse_chunk_filename("chunk_1_2.bin"), None);
        assert_eq!(parse_chunk_filename("chunk_a_b_c.bin"), None);
        assert_eq!(parse_chunk_filename("chunk_1_2_3.txt"), None);
    }

    #[test]
    fn chunk_data_zstd_compression_reduces_size() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_chunk_store(dir.path());
        let mut voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
        for i in 0..100 {
            voxels[i] = WorldVoxel::Solid((i % 5) as u8);
        }
        let chunk = ChunkData::from_voxels(&voxels, ChunkStatus::Full);
        let envelope = ChunkFileEnvelope {
            version: CHUNK_SAVE_VERSION,
            chunk_size: 16,
            data: chunk.clone(),
        };
        store.save(&IVec3::ZERO, &envelope).unwrap();

        let path = chunk_file_path(dir.path(), IVec3::ZERO);
        let compressed_size = std::fs::metadata(&path).unwrap().len();
        let raw_size = bincode::serialize(&envelope).unwrap().len() as u64;

        assert!(
            compressed_size < raw_size / 2,
            "compressed {compressed_size} should be < half of raw {raw_size}"
        );
    }

    fn sample_spawns() -> Vec<WorldObjectSpawn> {
        vec![
            WorldObjectSpawn {
                object_id: "tree_oak".to_string(),
                position: Vec3::new(1.0, 2.0, 3.0),
                position_kind: WorldObjectPositionKind::Final,
                persisted_components: Vec::new(),
            },
            WorldObjectSpawn {
                object_id: "rock_large".to_string(),
                position: Vec3::new(-4.0, 0.0, 5.5),
                position_kind: WorldObjectPositionKind::Final,
                persisted_components: Vec::new(),
            },
        ]
    }

    fn assert_spawns_eq(a: &[WorldObjectSpawn], b: &[WorldObjectSpawn]) {
        assert_eq!(a.len(), b.len(), "spawn count mismatch");
        for (i, (sa, sb)) in a.iter().zip(b.iter()).enumerate() {
            assert_eq!(sa.object_id, sb.object_id, "object_id mismatch at {i}");
            assert_eq!(sa.position, sb.position, "position mismatch at {i}");
            assert_eq!(
                sa.position_kind, sb.position_kind,
                "position_kind mismatch at {i}"
            );
        }
    }

    #[test]
    fn chunk_entities_preserve_final_position_kind_without_persisted_components() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_entity_store(dir.path());
        let pos = IVec3::new(4, 0, -2);
        let spawns = vec![WorldObjectSpawn {
            object_id: "tree_oak".to_string(),
            position: Vec3::new(1.0, 2.0, 3.0),
            position_kind: WorldObjectPositionKind::Final,
            persisted_components: Vec::new(),
        }];

        store.save(&pos, &spawns).unwrap();
        let loaded = store.load(&pos).unwrap().expect("entities should exist");

        assert_eq!(loaded[0].position_kind, WorldObjectPositionKind::Final);
        assert!(loaded[0].persisted_components.is_empty());
    }

    #[test]
    fn save_load_chunk_entities_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_entity_store(dir.path());
        let pos = IVec3::new(3, -1, 7);
        let spawns = sample_spawns();

        store.save(&pos, &spawns).unwrap();
        let loaded = store.load(&pos).unwrap().expect("entities should exist");
        assert_spawns_eq(&spawns, &loaded);
    }

    #[test]
    fn load_nonexistent_entities_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_entity_store(dir.path());
        assert!(store.load(&IVec3::ZERO).unwrap().is_none());
    }

    #[test]
    fn empty_entities_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_entity_store(dir.path());
        let spawns: Vec<WorldObjectSpawn> = Vec::new();

        store.save(&IVec3::ZERO, &spawns).unwrap();
        let loaded = store
            .load(&IVec3::ZERO)
            .unwrap()
            .expect("entities file should exist");
        assert!(loaded.is_empty());
    }

    #[test]
    fn empty_entities_file_is_authoritative_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_entity_store(dir.path());
        store.save(&IVec3::ZERO, &Vec::new()).unwrap();
        let loaded = store.load(&IVec3::ZERO).unwrap();
        assert!(matches!(loaded, Some(spawns) if spawns.is_empty()));
    }

    #[test]
    fn chunk_entities_preserve_rotation_persisted_component() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_entity_store(dir.path());
        let pos = IVec3::new(2, 0, -1);
        let rotation_component = PersistedComponent {
            type_path: "protocol::world_object::types::WorldObjectRotationSnapshot".to_string(),
            ron_data: "(x:0.0,y:0.5,z:0.0,w:0.8660254)".to_string(),
        };
        let spawns = vec![WorldObjectSpawn {
            object_id: "tree_oak".to_string(),
            position: Vec3::new(1.0, 2.0, 3.0),
            position_kind: WorldObjectPositionKind::Final,
            persisted_components: vec![rotation_component.clone()],
        }];

        store.save(&pos, &spawns).unwrap();
        let loaded = store.load(&pos).unwrap().expect("entities should exist");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].persisted_components.len(), 1);
        assert_eq!(
            loaded[0].persisted_components[0].type_path,
            rotation_component.type_path
        );
        assert_eq!(
            loaded[0].persisted_components[0].ron_data,
            rotation_component.ron_data
        );
    }
}
