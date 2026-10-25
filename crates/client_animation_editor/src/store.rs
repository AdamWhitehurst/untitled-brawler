use persistence::{PersistenceError, Store};
use sprite_rig::asset::{SpriteAnimAsset, SpriteAnimSetAsset};
use sprite_rig::serialize::{serialize_anim, serialize_animset};
use std::path::{Component, Path, PathBuf};

/// Asset root the editor reads and writes, resolved at compile time to the workspace's
/// `assets/` — the same convention as the example's `AssetPlugin` file_path. The editor
/// is a dev tool that runs from the workspace, so a compile-time path is acceptable.
pub fn editor_asset_root() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets"))
}

/// Normalizes toolbar input to the animset-relative `.anim.ron` path used by stores.
pub fn normalize_clip_path(path: &str) -> Result<String, String> {
    let path = path.trim();
    let is_asset_relative = Path::new(path)
        .components()
        .all(|component| matches!(component, Component::Normal(_)));
    if path.is_empty() {
        Err("clip path is empty".to_string())
    } else if !is_asset_relative {
        Err("clip path must be asset-relative without '.' or '..' components".to_string())
    } else if path.ends_with(".anim.ron") {
        Ok(path.to_string())
    } else {
        Ok(format!("{path}.anim.ron"))
    }
}

/// Identifies a clip file by its animset-relative path (e.g.
/// "anims/humanoid/walk.anim.ron" — exactly the string stored in the animset), plus the
/// rig bone order the canonical serializer emits `bone_timelines` in.
#[derive(Clone, Debug)]
pub struct ClipPath {
    pub rel: String,
    pub bone_order: Vec<String>,
}

/// Writes/reads `.anim.ron` files under `asset_root` via the canonical serializer, with
/// atomic tmp+rename writes.
#[derive(Clone)]
pub struct FsAnimClipStore {
    pub asset_root: PathBuf,
}

impl Store<ClipPath, SpriteAnimAsset> for FsAnimClipStore {
    fn save(&self, key: &ClipPath, value: &SpriteAnimAsset) -> Result<(), PersistenceError> {
        write_atomic(
            &self.asset_root.join(&key.rel),
            &serialize_anim(value, &key.bone_order),
        )
    }

    fn load(&self, key: &ClipPath) -> Result<Option<SpriteAnimAsset>, PersistenceError> {
        read_ron(&self.asset_root.join(&key.rel))
    }
}

/// Writes/reads `.animset.ron` files under `asset_root`, keyed by animset-relative path.
#[derive(Clone)]
pub struct FsAnimSetStore {
    pub asset_root: PathBuf,
}

impl Store<String, SpriteAnimSetAsset> for FsAnimSetStore {
    fn save(&self, key: &String, value: &SpriteAnimSetAsset) -> Result<(), PersistenceError> {
        write_atomic(&self.asset_root.join(key), &serialize_animset(value))
    }

    fn load(&self, key: &String) -> Result<Option<SpriteAnimSetAsset>, PersistenceError> {
        read_ron(&self.asset_root.join(key))
    }
}

/// Writes via a sibling `.tmp` file + rename, so a crash mid-write never truncates the
/// target file.
fn write_atomic(path: &Path, contents: &str) -> Result<(), PersistenceError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Reads and parses a RON file; `Ok(None)` when the file doesn't exist.
fn read_ron<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, PersistenceError> {
    if !path.exists() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(path)?;
    ron::de::from_str(&contents)
        .map(Some)
        .map_err(|e| PersistenceError::Deserialize(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sprite_rig::asset::{
        BoneTimeline, CurveType, LocomotionConfig, LocomotionEntry, RotationKeyframe,
    };
    use std::collections::HashMap;

    /// Per-test temp root so parallel tests never collide.
    fn unique_root(test: &str) -> PathBuf {
        std::env::temp_dir().join(format!("anim_editor_store_{}_{test}", std::process::id()))
    }

    fn test_clip() -> SpriteAnimAsset {
        let mut bone_timelines = HashMap::new();
        bone_timelines.insert(
            "root".to_string(),
            BoneTimeline {
                rotation: vec![
                    RotationKeyframe {
                        time: 0.0,
                        value: 0.0,
                        curve: CurveType::Linear,
                    },
                    RotationKeyframe {
                        time: 1.0,
                        value: 45.0,
                        curve: CurveType::Step,
                    },
                ],
                ..Default::default()
            },
        );
        SpriteAnimAsset {
            name: "kick".to_string(),
            duration: 1.0,
            looping: false,
            bone_timelines,
            events: vec![],
        }
    }

    #[test]
    fn clip_store_save_load_round_trips() {
        let root = unique_root("clip");
        let store = FsAnimClipStore {
            asset_root: root.clone(),
        };
        let key = ClipPath {
            rel: "anims/test/kick.anim.ron".to_string(),
            bone_order: vec!["root".to_string()],
        };
        let clip = test_clip();

        store.save(&key, &clip).expect("save failed");
        let loaded = store.load(&key).expect("load failed").expect("file absent");
        assert_eq!(clip, loaded);
        assert!(
            !root.join("anims/test/kick.anim.ron.tmp").exists(),
            "tmp file must be renamed away"
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn animset_store_save_load_round_trips() {
        let root = unique_root("animset");
        let store = FsAnimSetStore {
            asset_root: root.clone(),
        };
        let key = "anims/test/test.animset.ron".to_string();
        let animset = SpriteAnimSetAsset {
            rig: "rigs/test.rig.ron".to_string(),
            locomotion: LocomotionConfig {
                entries: vec![LocomotionEntry {
                    clip: "anims/test/idle.anim.ron".to_string(),
                    speed_threshold: 0.0,
                }],
            },
            ability_animations: std::collections::BTreeMap::from([(
                "kick".to_string(),
                "anims/test/kick.anim.ron".to_string(),
            )]),
            hit_react: None,
        };

        store.save(&key, &animset).expect("save failed");
        let loaded = store.load(&key).expect("load failed").expect("file absent");
        assert_eq!(animset, loaded);
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    /// Clip paths accept the toolbar's extensionless form and reject empty input.
    #[test]
    fn clip_path_normalization_matches_toolbar_input() {
        assert_eq!(
            normalize_clip_path(" anims/humanoid/kick ").expect("valid path"),
            "anims/humanoid/kick.anim.ron"
        );
        assert_eq!(
            normalize_clip_path("anims/humanoid/kick.anim.ron").expect("valid path"),
            "anims/humanoid/kick.anim.ron"
        );
        assert_eq!(
            normalize_clip_path(""),
            Err("clip path is empty".to_string())
        );
        let relative_error =
            Err("clip path must be asset-relative without '.' or '..' components".to_string());
        assert_eq!(normalize_clip_path("/tmp/kick"), relative_error.clone());
        assert_eq!(normalize_clip_path("../kick"), relative_error.clone());
        assert_eq!(normalize_clip_path("anims/../kick"), relative_error);
    }

    #[test]
    fn missing_file_loads_none() {
        let store = FsAnimClipStore {
            asset_root: unique_root("missing"),
        };
        let key = ClipPath {
            rel: "anims/test/nope.anim.ron".to_string(),
            bone_order: vec![],
        };
        assert!(store.load(&key).expect("load failed").is_none());
    }
}
