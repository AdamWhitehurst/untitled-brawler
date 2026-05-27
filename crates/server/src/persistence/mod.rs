pub mod fs_map_entities;
pub mod fs_map_meta;

use std::path::{Path, PathBuf};

use bevy::prelude::*;
use nostr_map_persistence::{MapRevision, PayloadSlotState};
use protocol::map::SavedEntity;
use protocol::MapInstanceId;
use serde::{Deserialize, Serialize};
use voxel_map_engine::config::WorldObjectSpawn;
use voxel_map_engine::persistence::ChunkFileEnvelope;

pub(crate) const META_VERSION: u32 = 1;

/// Metadata for a single map instance, saved to `map.meta.bin`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MapMeta {
    pub version: u32,
    pub seed: u64,
    pub generation_version: u32,
    pub spawn_points: Vec<Vec3>,
}

/// Server-decoded complete map save accepted by persistence preflight.
#[derive(Clone, Debug)]
pub struct ServerValidatedMapSave {
    pub meta: MapMeta,
    pub chunks: Vec<(IVec3, ChunkFileEnvelope)>,
    pub chunk_entities: Vec<(IVec3, Vec<WorldObjectSpawn>)>,
    pub map_entities: Option<Vec<SavedEntity>>,
    pub revision: MapRevision,
}

/// Server-decoded map delta accepted by persistence preflight.
#[derive(Clone, Debug)]
pub struct ServerValidatedMapDelta {
    pub revision: MapRevision,
    pub meta: PayloadSlotState<MapMeta>,
    pub chunks: Vec<(IVec3, PayloadSlotState<ChunkFileEnvelope>)>,
    pub chunk_entities: Vec<(IVec3, PayloadSlotState<Vec<WorldObjectSpawn>>)>,
    pub map_entities: PayloadSlotState<Vec<SavedEntity>>,
}

/// Resource holding the base save directory path.
#[derive(Resource)]
pub struct WorldSavePath(pub PathBuf);

impl Default for WorldSavePath {
    fn default() -> Self {
        Self(PathBuf::from("worlds"))
    }
}

/// Resolve the save directory for a `MapInstanceId` within the base save path.
pub fn map_save_dir(base: &Path, map_id: &MapInstanceId) -> PathBuf {
    match map_id {
        MapInstanceId::Overworld => base.join("overworld"),
        MapInstanceId::Homebase { owner } => base.join(format!(
            "homebase_{}",
            nostr_client::npub_from_nostr_public_key(*owner)
        )),
    }
}

pub(crate) const ENTITY_SAVE_VERSION: u32 = 1;

/// Versioned envelope wrapping entity data for on-disk persistence.
#[derive(Serialize, Deserialize)]
pub(crate) struct EntityFileEnvelope {
    pub version: u32,
    pub entities: Vec<SavedEntity>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use persistence::Store;
    use protocol::map::SavedEntityKind;
    use protocol::NostrPublicKey;
    use std::sync::Arc;

    use fs_map_entities::FsMapEntitiesStore;
    use fs_map_meta::FsMapMetaStore;

    fn test_meta_store(dir: &Path) -> FsMapMetaStore {
        FsMapMetaStore {
            map_dir: Arc::new(dir.to_path_buf()),
        }
    }

    fn test_entity_store(dir: &Path) -> FsMapEntitiesStore {
        FsMapEntitiesStore {
            map_dir: Arc::new(dir.to_path_buf()),
        }
    }

    fn owner(byte: u8) -> NostrPublicKey {
        NostrPublicKey([byte; 32])
    }

    #[test]
    fn save_load_map_meta_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_meta_store(dir.path());
        let meta = MapMeta {
            version: 1,
            seed: 42,
            generation_version: 3,
            spawn_points: vec![Vec3::new(1.0, 2.0, 3.0), Vec3::new(4.0, 5.0, 6.0)],
        };
        store.save(&(), &meta).unwrap();
        let loaded = store.load(&()).unwrap().expect("meta should exist");
        assert_eq!(loaded.seed, 42);
        assert_eq!(loaded.generation_version, 3);
        assert_eq!(loaded.spawn_points.len(), 2);
    }

    #[test]
    fn load_map_meta_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_meta_store(dir.path());
        assert!(store.load(&()).unwrap().is_none());
    }

    #[test]
    fn map_save_dir_overworld() {
        let base = Path::new("worlds");
        assert_eq!(
            map_save_dir(base, &MapInstanceId::Overworld),
            PathBuf::from("worlds/overworld")
        );
    }

    #[test]
    fn map_save_dir_homebase() {
        let base = Path::new("worlds");
        assert_eq!(
            map_save_dir(base, &MapInstanceId::Homebase { owner: owner(0x2a) }),
            PathBuf::from(
                "worlds/homebase_npub19g4z52329g4z52329g4z52329g4z52329g4z52329g4z52329g4qrd5mkx"
            )
        );
    }

    #[test]
    fn save_load_entities_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_entity_store(dir.path());
        let entities = vec![
            SavedEntity {
                kind: SavedEntityKind::RespawnPoint,
                position: Vec3::new(1.0, 2.0, 3.0),
            },
            SavedEntity {
                kind: SavedEntityKind::RespawnPoint,
                position: Vec3::new(4.0, 5.0, 6.0),
            },
        ];
        store.save(&(), &entities).unwrap();
        let loaded = store.load(&()).unwrap().expect("entities should exist");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].kind, SavedEntityKind::RespawnPoint);
        assert_eq!(loaded[0].position, Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(loaded[1].position, Vec3::new(4.0, 5.0, 6.0));
    }

    #[test]
    fn load_entities_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_entity_store(dir.path());
        assert!(store.load(&()).unwrap().is_none());
    }

    #[test]
    fn save_entities_creates_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("deep/nested");
        let store = test_entity_store(&nested);
        store.save(&(), &vec![]).unwrap();
        assert!(nested.join("entities.bin").exists());
    }

    #[test]
    fn save_entities_overwrites_previous() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_entity_store(dir.path());
        let v1 = vec![SavedEntity {
            kind: SavedEntityKind::RespawnPoint,
            position: Vec3::ZERO,
        }];
        store.save(&(), &v1).unwrap();

        let v2 = vec![
            SavedEntity {
                kind: SavedEntityKind::RespawnPoint,
                position: Vec3::ONE,
            },
            SavedEntity {
                kind: SavedEntityKind::RespawnPoint,
                position: Vec3::NEG_ONE,
            },
        ];
        store.save(&(), &v2).unwrap();

        let loaded = store.load(&()).unwrap().expect("entities should exist");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].position, Vec3::ONE);
    }

    #[test]
    fn corrupt_entities_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("entities.bin"), b"garbage data").unwrap();
        let store = test_entity_store(dir.path());
        assert!(store.load(&()).is_err());
    }

    #[test]
    fn entity_kind_serialization_roundtrip() {
        let kind = SavedEntityKind::RespawnPoint;
        let bytes = bincode::serialize(&kind).unwrap();
        let back: SavedEntityKind = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, SavedEntityKind::RespawnPoint);
    }
}
