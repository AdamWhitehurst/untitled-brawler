use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use persistence::{PersistenceError, Store};
use protocol::map::SavedEntity;

use super::{EntityFileEnvelope, ENTITY_SAVE_VERSION};

/// Filesystem-backed store for map-level entities (respawn points, etc.).
#[derive(Clone)]
pub struct FsMapEntitiesStore {
    pub map_dir: Arc<PathBuf>,
}

impl Store<(), Vec<SavedEntity>> for FsMapEntitiesStore {
    fn save(&self, _key: &(), value: &Vec<SavedEntity>) -> Result<(), PersistenceError> {
        fs::create_dir_all(self.map_dir.as_ref())
            .map_err(|e| PersistenceError::Serialize(format!("mkdir: {e}")))?;
        let path = self.map_dir.join("entities.bin");
        let envelope = EntityFileEnvelope {
            version: ENTITY_SAVE_VERSION,
            entities: value.clone(),
        };
        let bytes = bincode::serialize(&envelope)
            .map_err(|e| PersistenceError::Serialize(format!("serialize entities: {e}")))?;
        let tmp_path = path.with_extension("bin.tmp");
        fs::write(&tmp_path, &bytes)
            .map_err(|e| PersistenceError::Serialize(format!("write entities tmp: {e}")))?;
        fs::rename(&tmp_path, &path)
            .map_err(|e| PersistenceError::Serialize(format!("rename entities: {e}")))?;
        Ok(())
    }

    fn load(&self, _key: &()) -> Result<Option<Vec<SavedEntity>>, PersistenceError> {
        let path = self.map_dir.join("entities.bin");
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)
            .map_err(|e| PersistenceError::Deserialize(format!("read entities: {e}")))?;
        let envelope: EntityFileEnvelope = bincode::deserialize(&bytes)
            .map_err(|e| PersistenceError::Deserialize(format!("deserialize entities: {e}")))?;
        if envelope.version != ENTITY_SAVE_VERSION {
            return Err(PersistenceError::VersionMismatch {
                expected: ENTITY_SAVE_VERSION,
                actual: envelope.version,
            });
        }
        Ok(Some(envelope.entities))
    }
}
