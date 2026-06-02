use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use bevy::prelude::*;
use persistence::{PersistenceError, Store};

use crate::config::WorldObjectSpawn;

use super::{ENTITY_SAVE_VERSION, EntityFileEnvelope, ZSTD_COMPRESSION_LEVEL, entity_file_path};

/// Filesystem-backed store for per-chunk entity spawn data.
///
/// Key is `IVec3` (chunk position). Each map entity gets its own store
/// instance with a resolved `map_dir`.
#[derive(Clone)]
pub struct FsChunkEntitiesStore {
    pub map_dir: Arc<PathBuf>,
}

impl Store<IVec3, Vec<WorldObjectSpawn>> for FsChunkEntitiesStore {
    fn save(&self, key: &IVec3, value: &Vec<WorldObjectSpawn>) -> Result<(), PersistenceError> {
        let path = entity_file_path(&self.map_dir, *key);
        fs::create_dir_all(path.parent().expect("entity path has parent"))
            .map_err(|e| PersistenceError::Serialize(format!("mkdir entities: {e}")))?;

        let envelope = EntityFileEnvelope {
            version: ENTITY_SAVE_VERSION,
            spawns: value.clone(),
        };
        let bytes = bincode::serialize(&envelope)
            .map_err(|e| PersistenceError::Serialize(format!("serialize entities: {e}")))?;

        let tmp_path = path.with_extension("bin.tmp");
        let file = fs::File::create(&tmp_path)
            .map_err(|e| PersistenceError::Serialize(format!("create tmp: {e}")))?;
        let mut encoder = zstd::Encoder::new(file, ZSTD_COMPRESSION_LEVEL)
            .map_err(|e| PersistenceError::Serialize(format!("zstd encoder: {e}")))?;
        encoder
            .write_all(&bytes)
            .map_err(|e| PersistenceError::Serialize(format!("write entities: {e}")))?;
        encoder
            .finish()
            .map_err(|e| PersistenceError::Serialize(format!("zstd finish: {e}")))?;

        fs::rename(&tmp_path, &path)
            .map_err(|e| PersistenceError::Serialize(format!("atomic rename: {e}")))?;
        Ok(())
    }

    fn load(&self, key: &IVec3) -> Result<Option<Vec<WorldObjectSpawn>>, PersistenceError> {
        let path = entity_file_path(&self.map_dir, *key);
        if !path.exists() {
            return Ok(None);
        }

        let file = fs::File::open(&path)
            .map_err(|e| PersistenceError::Deserialize(format!("open entities: {e}")))?;
        let mut decoder = zstd::Decoder::new(file)
            .map_err(|e| PersistenceError::Deserialize(format!("zstd decoder: {e}")))?;
        let mut bytes = Vec::new();
        decoder
            .read_to_end(&mut bytes)
            .map_err(|e| PersistenceError::Deserialize(format!("read entities: {e}")))?;

        let envelope: EntityFileEnvelope = bincode::deserialize(&bytes)
            .map_err(|e| PersistenceError::Deserialize(format!("deserialize entities: {e}")))?;

        if envelope.version != ENTITY_SAVE_VERSION {
            return Err(PersistenceError::VersionMismatch {
                expected: ENTITY_SAVE_VERSION,
                actual: envelope.version,
            });
        }

        Ok(Some(envelope.spawns))
    }
}
