use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use bevy::prelude::*;
use persistence::{PersistenceError, Store};

use super::{CHUNK_SAVE_VERSION, ChunkFileEnvelope, ZSTD_COMPRESSION_LEVEL, chunk_file_path};

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

impl FsChunkStore {
    /// Removes a chunk's on-disk file so the chunk regenerates from seed on next load.
    /// Absent file is a no-op (the desired end state already holds).
    pub fn delete(&self, key: &IVec3) -> Result<(), PersistenceError> {
        let path = chunk_file_path(&self.map_dir, *key);
        if !path.exists() {
            trace!(?path, "chunk file already absent; delete is a no-op");
            return Ok(());
        }
        fs::remove_file(&path)
            .map_err(|e| PersistenceError::Serialize(format!("delete chunk {key}: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChunkData, ChunkStatus, WorldVoxel};

    const PADDED_VOLUME_16: usize = 18 * 18 * 18;

    fn test_store(dir: &std::path::Path) -> FsChunkStore {
        FsChunkStore {
            map_dir: Arc::new(dir.to_path_buf()),
        }
    }

    fn test_envelope() -> ChunkFileEnvelope {
        let voxels = vec![WorldVoxel::Air; PADDED_VOLUME_16];
        ChunkFileEnvelope {
            version: CHUNK_SAVE_VERSION,
            chunk_size: 16,
            data: ChunkData::from_voxels(&voxels, ChunkStatus::Full),
        }
    }

    #[test]
    fn delete_removes_saved_chunk_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let pos = IVec3::new(1, -2, 3);
        store.save(&pos, &test_envelope()).unwrap();
        assert!(chunk_file_path(dir.path(), pos).exists());

        store.delete(&pos).unwrap();
        assert!(!chunk_file_path(dir.path(), pos).exists());
    }

    #[test]
    fn delete_absent_chunk_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        assert!(store.delete(&IVec3::ZERO).is_ok());
    }
}
