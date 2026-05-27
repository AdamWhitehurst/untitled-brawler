use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use bevy::prelude::*;
use persistence::{PersistenceError, Store};

use super::{chunk_file_path, ChunkFileEnvelope, CHUNK_SAVE_VERSION, ZSTD_COMPRESSION_LEVEL};

/// Filesystem-backed store for chunk terrain data.
///
/// Key is `IVec3` (chunk position). Each map entity gets its own store
/// instance with a resolved `map_dir`.
#[derive(Clone)]
pub struct FsChunkStore {
    pub map_dir: Arc<PathBuf>,
}

impl Store<IVec3, ChunkFileEnvelope> for FsChunkStore {
    fn save(&self, key: &IVec3, value: &ChunkFileEnvelope) -> Result<(), PersistenceError> {
        let path = chunk_file_path(&self.map_dir, *key);
        fs::create_dir_all(path.parent().expect("chunk path has parent"))
            .map_err(|e| PersistenceError::Serialize(format!("mkdir terrain: {e}")))?;

        let bytes = bincode::serialize(value)
            .map_err(|e| PersistenceError::Serialize(format!("serialize chunk: {e}")))?;

        let tmp_path = path.with_extension("bin.tmp");
        let file = fs::File::create(&tmp_path)
            .map_err(|e| PersistenceError::Serialize(format!("create tmp: {e}")))?;
        let mut encoder = zstd::Encoder::new(file, ZSTD_COMPRESSION_LEVEL)
            .map_err(|e| PersistenceError::Serialize(format!("zstd encoder: {e}")))?;
        encoder
            .write_all(&bytes)
            .map_err(|e| PersistenceError::Serialize(format!("write chunk: {e}")))?;
        encoder
            .finish()
            .map_err(|e| PersistenceError::Serialize(format!("zstd finish: {e}")))?;

        fs::rename(&tmp_path, &path)
            .map_err(|e| PersistenceError::Serialize(format!("atomic rename: {e}")))?;
        Ok(())
    }

    fn load(&self, key: &IVec3) -> Result<Option<ChunkFileEnvelope>, PersistenceError> {
        let path = chunk_file_path(&self.map_dir, *key);
        if !path.exists() {
            return Ok(None);
        }

        let file = std::fs::File::open(&path)
            .map_err(|e| PersistenceError::Deserialize(format!("open chunk: {e}")))?;
        let mut decoder = zstd::Decoder::new(file)
            .map_err(|e| PersistenceError::Deserialize(format!("zstd decoder: {e}")))?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut bytes)
            .map_err(|e| PersistenceError::Deserialize(format!("read chunk: {e}")))?;

        let envelope: ChunkFileEnvelope = bincode::deserialize(&bytes)
            .map_err(|e| PersistenceError::Deserialize(format!("deserialize chunk: {e}")))?;

        if envelope.version != CHUNK_SAVE_VERSION {
            return Err(PersistenceError::VersionMismatch {
                expected: CHUNK_SAVE_VERSION,
                actual: envelope.version,
            });
        }

        Ok(Some(envelope))
    }
}
