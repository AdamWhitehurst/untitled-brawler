use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};
use persistence::{PendingStoreOps, Store, StoreBackend};
use sprite_rig::asset::{SpriteAnimAsset, SpriteAnimSetAsset};
use sprite_rig::{AnimBoneDefaults, AnimSetRef, LoadedAnimHandles};

use crate::state::{
    assign_new_clip, backup_preview_source, restore_preview_source, ClipSlot, EditorState,
    NewClipSlot, Selection,
};
use crate::store::{normalize_clip_path, ClipPath, FsAnimClipStore, FsAnimSetStore};

/// Last save/assign outcome shown in the save bar: queue confirmations and validation
/// errors from the bar itself, completions and failures from `drain_save_results`.
#[derive(Resource, Default)]
pub struct SaveStatus(pub Option<String>);

/// Which slot kind the new-clip form assigns to.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum NewSlotKind {
    #[default]
    Ability,
    Locomotion,
    HitReact,
}

/// Editable clip path that follows the working document until the user types a new path.
#[derive(Default)]
pub(crate) struct ClipPathField {
    text: String,
    synced_path: String,
}

impl ClipPathField {
    /// Replaces the field only when the working document changed outside this form.
    fn sync(&mut self, working_path: &str) {
        if self.synced_path != working_path {
            self.text = working_path.to_string();
            self.synced_path = working_path.to_string();
        }
    }
}

type ClipStore = (
    &'static StoreBackend<ClipPath, SpriteAnimAsset, FsAnimClipStore>,
    &'static mut PendingStoreOps<ClipPath, SpriteAnimAsset>,
);
type SetStore = (
    &'static StoreBackend<String, SpriteAnimSetAsset, FsAnimSetStore>,
    &'static mut PendingStoreOps<String, SpriteAnimSetAsset>,
);

/// Top bar: save buttons, clip path + load action, assignment controls, and status.
/// Loads are immediate local reads; saves run on the async pool and are drained later.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_save(
    mut contexts: EguiContexts,
    mut state: ResMut<EditorState>,
    mut status: ResMut<SaveStatus>,
    mut clip_store: Query<ClipStore>,
    mut set_store: Query<SetStore>,
    rigs: Query<&AnimSetRef>,
    mut anim_assets: ResMut<Assets<SpriteAnimAsset>>,
    mut animset_assets: ResMut<Assets<SpriteAnimSetAsset>>,
    mut loaded_handles: ResMut<LoadedAnimHandles>,
    mut bone_defaults: ResMut<AnimBoneDefaults>,
    mut clip_path: Local<ClipPathField>,
    mut new_kind: Local<NewSlotKind>,
    mut new_ability_id: Local<String>,
    mut new_threshold: Local<f32>,
) {
    let Ok(ctx) = contexts.ctx_mut() else {
        trace!("egui context not ready; skipping save bar frame");
        return;
    };
    let Ok((clip_backend, mut clip_ops)) = clip_store.single_mut() else {
        trace!("clip store entity not spawned yet; save bar waits");
        return;
    };
    let Ok((set_backend, mut set_ops)) = set_store.single_mut() else {
        trace!("animset store entity not spawned yet; save bar waits");
        return;
    };
    let Ok(animset_ref) = rigs.single() else {
        trace!("editor rig not spawned yet; save bar waits");
        return;
    };
    clip_path.sync(&state.working_path);

    egui::TopBottomPanel::top("save_bar").show(ctx, |ui| {
        ui.horizontal(|ui| {
            if ui.button("Save clip").clicked() {
                save_clip(
                    &state,
                    clip_backend,
                    &mut clip_ops,
                    &mut anim_assets,
                    &loaded_handles,
                    &mut status,
                );
            }
            if ui.button("Save animset").clicked() {
                save_animset(&state, animset_ref, set_backend, &mut set_ops, &mut status);
            }
            ui.separator();

            ui.label("clip:");
            ui.add(
                egui::TextEdit::singleline(&mut clip_path.text)
                    .hint_text("anims/humanoid/kick")
                    .desired_width(200.0),
            );
            if ui
                .add_enabled(
                    !clip_ops.has_pending() && !set_ops.has_pending(),
                    egui::Button::new("Load"),
                )
                .clicked()
            {
                load_clip(
                    &mut state,
                    &clip_path.text,
                    clip_backend,
                    &mut anim_assets,
                    &loaded_handles,
                    &mut status,
                );
            }
            egui::ComboBox::from_id_salt("new_slot_kind")
                .selected_text(kind_label(*new_kind))
                .show_ui(ui, |ui| {
                    for kind in [
                        NewSlotKind::Ability,
                        NewSlotKind::Locomotion,
                        NewSlotKind::HitReact,
                    ] {
                        ui.selectable_value(&mut *new_kind, kind, kind_label(kind));
                    }
                });
            match *new_kind {
                NewSlotKind::Ability => {
                    ui.add(
                        egui::TextEdit::singleline(&mut *new_ability_id)
                            .hint_text("ability id")
                            .desired_width(90.0),
                    );
                }
                NewSlotKind::Locomotion => {
                    ui.add(
                        egui::DragValue::new(&mut *new_threshold)
                            .speed(0.1)
                            .prefix("threshold: "),
                    );
                }
                NewSlotKind::HitReact => {}
            }
            if ui.button("Assign").clicked() {
                let slot = match *new_kind {
                    NewSlotKind::Ability => NewClipSlot::Ability {
                        id: new_ability_id.trim().to_string(),
                    },
                    NewSlotKind::Locomotion => NewClipSlot::Locomotion {
                        speed_threshold: *new_threshold,
                    },
                    NewSlotKind::HitReact => NewClipSlot::HitReact,
                };
                status.0 = Some(
                    match assign_new_clip(
                        &mut state,
                        &clip_path.text,
                        slot,
                        animset_ref,
                        &mut anim_assets,
                        &mut animset_assets,
                        &mut loaded_handles,
                        &mut bone_defaults,
                    ) {
                        Ok(rel) => format!("assigned {rel} — Save clip + Save animset to persist"),
                        Err(e) => format!("assign failed: {e}"),
                    },
                );
            }

            if let Some(message) = &status.0 {
                ui.separator();
                ui.label(message.as_str());
            }
        });
    });
}

/// Loads a user-selected authoring document after startup; this is not a gameplay asset dependency.
fn load_clip(
    state: &mut EditorState,
    input: &str,
    backend: &StoreBackend<ClipPath, SpriteAnimAsset, FsAnimClipStore>,
    anim_assets: &mut Assets<SpriteAnimAsset>,
    loaded_handles: &LoadedAnimHandles,
    status: &mut SaveStatus,
) {
    let rel = match normalize_clip_path(input) {
        Ok(rel) => rel,
        Err(load_error) => {
            trace!(%load_error, "clip load rejected invalid toolbar input");
            status.0 = Some(format!("LOAD FAILED: {load_error}"));
            return;
        }
    };
    let key = ClipPath {
        rel: rel.clone(),
        bone_order: Vec::new(),
    };
    match backend.0.load(&key) {
        Ok(Some(clip)) => {
            info!(path = %rel, "clip loaded");
            status.0 = Some(format!("loaded {rel}"));
            if assigned_slot_for_path(state, &rel).is_some() {
                restore_preview_source(state, anim_assets, loaded_handles);
            } else {
                backup_preview_source(state, anim_assets, loaded_handles);
            }
            apply_loaded_clip(state, rel, clip);
            sync_loaded_preview_source(state, anim_assets, loaded_handles);
        }
        Ok(None) => {
            warn!(path = %rel, "clip load found no file");
            status.0 = Some(format!("LOAD FAILED {rel}: file not found"));
        }
        Err(load_error) => {
            error!(path = %rel, error = %load_error, "clip load FAILED");
            status.0 = Some(format!("LOAD FAILED {rel}: {load_error}"));
        }
    }
}

/// Replaces the working document and rebakes it through its assigned or current preview slot.
fn apply_loaded_clip(state: &mut EditorState, path: String, clip: SpriteAnimAsset) {
    if let Some(slot) = assigned_slot_for_path(state, &path) {
        state.selected_clip = slot;
    }
    state.working = clip;
    state.working_path = path;
    state.playhead = 0.0;
    state.selection = Selection::None;
    state.clip_dirty = true;
}

/// Updates the selected slot's in-memory source so asset rebuilds refresh curves and masks.
fn sync_loaded_preview_source(
    state: &EditorState,
    anim_assets: &mut Assets<SpriteAnimAsset>,
    loaded_handles: &LoadedAnimHandles,
) {
    let preview_path = state
        .selected_clip_path()
        .expect("editor preview slot always has an assigned path");
    let handle = loaded_handles
        .0
        .get(preview_path)
        .unwrap_or_else(|| panic!("no loaded handle for preview clip '{preview_path}'"));
    let _ = anim_assets.insert(handle.id(), state.working.clone());
}

/// Returns the animset slot already assigned to `path`, if one exists.
fn assigned_slot_for_path(state: &EditorState, path: &str) -> Option<ClipSlot> {
    state
        .working_set
        .locomotion
        .entries
        .iter()
        .position(|entry| entry.clip == path)
        .map(ClipSlot::Locomotion)
        .or_else(|| {
            state
                .working_set
                .ability_animations
                .iter()
                .find(|(_, clip)| clip.as_str() == path)
                .map(|(id, _)| ClipSlot::Ability(id.clone()))
        })
        .or_else(|| {
            (state.working_set.hit_react.as_deref() == Some(path)).then_some(ClipSlot::HitReact)
        })
}

/// Queues a save of the working clip path and syncs the preview slot's source asset to
/// the working copy so selecting away and back matches the latest editor contents.
fn save_clip(
    state: &EditorState,
    backend: &StoreBackend<ClipPath, SpriteAnimAsset, FsAnimClipStore>,
    ops: &mut PendingStoreOps<ClipPath, SpriteAnimAsset>,
    anim_assets: &mut Assets<SpriteAnimAsset>,
    loaded_handles: &LoadedAnimHandles,
    status: &mut SaveStatus,
) {
    let rel = state.working_path.clone();
    let preview_path = state
        .selected_clip_path()
        .expect("selected slot always has an assigned path");
    if preview_path == rel {
        let handle = loaded_handles
            .0
            .get(preview_path)
            .unwrap_or_else(|| panic!("no loaded handle for preview clip '{preview_path}'"));
        let _ = anim_assets.insert(handle.id(), state.working.clone());
    } else {
        trace!(
            working_path = %rel,
            %preview_path,
            "saving unassigned working clip without replacing preview slot source"
        );
    }
    ops.spawn_save(
        &backend.0,
        ClipPath {
            rel: rel.clone(),
            bone_order: state.bone_order.clone(),
        },
        state.working.clone(),
    );
    status.0 = Some(format!("saving {rel}…"));
}

/// Queues a save of the working animset to the path its handle was loaded from.
fn save_animset(
    state: &EditorState,
    animset_ref: &AnimSetRef,
    backend: &StoreBackend<String, SpriteAnimSetAsset, FsAnimSetStore>,
    ops: &mut PendingStoreOps<String, SpriteAnimSetAsset>,
    status: &mut SaveStatus,
) {
    let rel = animset_ref
        .0
        .path()
        .expect("editor animset handle is path-loaded")
        .path()
        .to_string_lossy()
        .to_string();
    ops.spawn_save(&backend.0, rel.clone(), state.working_set.clone());
    status.0 = Some(format!("saving {rel}…"));
}

/// Polls both store ops each frame and drains results: completions update the status
/// line; failures are logged via `error!` AND shown in the bar — never silently dropped.
pub fn drain_save_results(
    mut clip_ops: Query<&mut PendingStoreOps<ClipPath, SpriteAnimAsset>>,
    mut set_ops: Query<&mut PendingStoreOps<String, SpriteAnimSetAsset>>,
    mut status: ResMut<SaveStatus>,
) {
    for mut ops in &mut clip_ops {
        ops.poll();
        for done in ops.completed_saves.drain(..) {
            info!(path = %done.key.rel, "clip saved");
            status.0 = Some(format!("saved {}", done.key.rel));
        }
        for failure in ops.save_errors.drain(..) {
            error!(path = %failure.key.rel, error = %failure.error, "clip save FAILED");
            status.0 = Some(format!(
                "SAVE FAILED {}: {}",
                failure.key.rel, failure.error
            ));
        }
    }
    for mut ops in &mut set_ops {
        ops.poll();
        for done in ops.completed_saves.drain(..) {
            info!(path = %done.key, "animset saved");
            status.0 = Some(format!("saved {}", done.key));
        }
        for failure in ops.save_errors.drain(..) {
            error!(path = %failure.key, error = %failure.error, "animset save FAILED");
            status.0 = Some(format!("SAVE FAILED {}: {}", failure.key, failure.error));
        }
    }
}

fn kind_label(kind: NewSlotKind) -> &'static str {
    match kind {
        NewSlotKind::Ability => "ability",
        NewSlotKind::Locomotion => "locomotion",
        NewSlotKind::HitReact => "hit_react",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use super::*;
    use crate::state::{ClipSlot, Playback, Selection};
    use sprite_rig::asset::{LocomotionConfig, LocomotionEntry};

    /// A loaded clip becomes the working document and selects its assigned slot.
    #[test]
    fn loaded_clip_replaces_the_working_document() {
        let mut state = editor_state();
        let (mut anim_assets, loaded_handles) = preview_assets();
        let loaded = clip("kick");

        apply_loaded_clip(
            &mut state,
            "anims/humanoid/kick.anim.ron".to_string(),
            loaded.clone(),
        );
        sync_loaded_preview_source(&state, &mut anim_assets, &loaded_handles);

        assert_eq!(state.working, loaded);
        assert_eq!(state.working_path, "anims/humanoid/kick.anim.ron");
        assert_eq!(state.selected_clip, ClipSlot::Ability("kick".to_string()));
        assert_eq!(state.playhead, 0.0);
        assert_eq!(state.selection, Selection::None);
        assert!(state.clip_dirty);
        let kick_handle = &loaded_handles.0["anims/humanoid/kick.anim.ron"];
        assert_eq!(
            anim_assets
                .get(kick_handle)
                .expect("kick source asset")
                .name,
            "kick"
        );
        crate::state::select_clip(
            &mut state,
            ClipSlot::Locomotion(0),
            &mut anim_assets,
            &loaded_handles,
        );
        crate::state::select_clip(
            &mut state,
            ClipSlot::Ability("kick".to_string()),
            &mut anim_assets,
            &loaded_handles,
        );
        assert_eq!(state.working.name, "kick");
    }

    /// An unassigned file uses the existing slot as its preview host until assignment.
    #[test]
    fn unassigned_loaded_clip_keeps_the_current_preview_slot() {
        let mut state = editor_state();
        let (mut anim_assets, loaded_handles) = preview_assets();

        backup_preview_source(&mut state, &anim_assets, &loaded_handles);
        apply_loaded_clip(
            &mut state,
            "anims/humanoid/unassigned.anim.ron".to_string(),
            clip("unassigned"),
        );
        sync_loaded_preview_source(&state, &mut anim_assets, &loaded_handles);

        assert_eq!(state.selected_clip, ClipSlot::Locomotion(0));
        assert_eq!(state.working_path, "anims/humanoid/unassigned.anim.ron");
        assert_eq!(state.working.name, "unassigned");
        assert!(state.clip_dirty);
        let idle_handle = &loaded_handles.0["anims/humanoid/idle.anim.ron"];
        assert_eq!(
            anim_assets
                .get(idle_handle)
                .expect("idle source asset")
                .name,
            "unassigned"
        );
        state.working_set.locomotion.entries.insert(
            0,
            LocomotionEntry {
                clip: "anims/humanoid/new.anim.ron".to_string(),
                speed_threshold: -1.0,
            },
        );
        crate::state::select_clip(
            &mut state,
            ClipSlot::Ability("kick".to_string()),
            &mut anim_assets,
            &loaded_handles,
        );
        assert_eq!(
            anim_assets
                .get(idle_handle)
                .expect("restored idle source")
                .name,
            "source-idle"
        );
        assert!(state.preview_source_backup.is_none());
    }

    /// Builds in-memory source assets for the assigned preview slots.
    fn preview_assets() -> (Assets<SpriteAnimAsset>, LoadedAnimHandles) {
        let mut anim_assets = Assets::default();
        let idle = anim_assets.add(clip("source-idle"));
        let kick = anim_assets.add(clip("source-kick"));
        let loaded_handles = LoadedAnimHandles(HashMap::from([
            ("anims/humanoid/idle.anim.ron".to_string(), idle),
            ("anims/humanoid/kick.anim.ron".to_string(), kick),
        ]));
        (anim_assets, loaded_handles)
    }

    /// Builds the common editor state used by load-state tests.
    fn editor_state() -> EditorState {
        EditorState {
            working: clip("idle"),
            working_path: "anims/humanoid/idle.anim.ron".to_string(),
            working_set: SpriteAnimSetAsset {
                rig: "rigs/humanoid.rig.ron".to_string(),
                locomotion: LocomotionConfig {
                    entries: vec![LocomotionEntry {
                        clip: "anims/humanoid/idle.anim.ron".to_string(),
                        speed_threshold: 0.0,
                    }],
                },
                ability_animations: BTreeMap::from([(
                    "kick".to_string(),
                    "anims/humanoid/kick.anim.ron".to_string(),
                )]),
                hit_react: None,
            },
            selected_clip: ClipSlot::Locomotion(0),
            preview_source_backup: None,
            playhead: 0.75,
            playback: Playback::Playing,
            selection: Selection::Event(0),
            bone_order: vec!["root".to_string()],
            clip_dirty: false,
        }
    }

    /// Builds the smallest valid clip used by load-state tests.
    fn clip(name: &str) -> SpriteAnimAsset {
        SpriteAnimAsset {
            name: name.to_string(),
            duration: 1.0,
            looping: true,
            bone_timelines: HashMap::new(),
            events: vec![],
        }
    }
}
