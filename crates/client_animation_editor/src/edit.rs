use bevy::prelude::*;
use sprite_rig::{AnimBoneDefaults, BuiltAnimations, LoadedAnimHandles};

use crate::state::{Channel, EditorState, Selection};

/// A keyframe value: scalar for rotation (degrees), vec2 for translation/scale.
#[derive(Clone, Copy, Debug)]
pub enum KeyValue {
    Scalar(f32),
    Vec2(Vec2),
}

/// Applies a keyframe edit to the working asset: clamps `new_time` to `[0, duration]`,
/// optionally updates the key's value, and re-sorts the channel by time. Coincident times
/// resolve deterministically — the dragged key keeps its identity and lands immediately
/// AFTER any pre-existing key at the same time. Selection follows the key across the
/// re-sort; the new index is returned. Sets `clip_dirty` so the live clip rebakes.
///
/// Panics if the selection is not a key or the value kind mismatches the channel — both
/// are caller bugs (drags originate from a key selection in that channel).
pub fn apply_key_edit(
    state: &mut EditorState,
    new_time: f32,
    new_value: Option<KeyValue>,
) -> usize {
    let Selection::Key { bone, channel, idx } = state.selection.clone() else {
        panic!("apply_key_edit without a key selection");
    };
    let clamped_t = new_time.clamp(0.0, state.working.duration);
    let timeline = state
        .working
        .bone_timelines
        .get_mut(&bone)
        .unwrap_or_else(|| panic!("selected bone '{bone}' missing from working clip"));

    let new_idx = match channel {
        Channel::Rotation => {
            let key = &mut timeline.rotation[idx];
            key.time = clamped_t;
            match new_value {
                Some(KeyValue::Scalar(v)) => key.value = v,
                Some(KeyValue::Vec2(_)) => panic!("vec2 value for scalar rotation channel"),
                None => {}
            }
            reinsert_sorted(&mut timeline.rotation, idx, |k| k.time)
        }
        Channel::Translation => {
            let key = &mut timeline.translation[idx];
            key.time = clamped_t;
            match new_value {
                Some(KeyValue::Vec2(v)) => key.value = v,
                Some(KeyValue::Scalar(_)) => panic!("scalar value for vec2 translation channel"),
                None => {}
            }
            reinsert_sorted(&mut timeline.translation, idx, |k| k.time)
        }
        Channel::Scale => {
            let key = &mut timeline.scale[idx];
            key.time = clamped_t;
            match new_value {
                Some(KeyValue::Vec2(v)) => key.value = v,
                Some(KeyValue::Scalar(_)) => panic!("scalar value for vec2 scale channel"),
                None => {}
            }
            reinsert_sorted(&mut timeline.scale, idx, |k| k.time)
        }
    };

    state.selection = Selection::Key {
        bone,
        channel,
        idx: new_idx,
    };
    state.clip_dirty = true;
    new_idx
}

/// Retimes the selected animation event: clamps `new_time` to `[0, duration]`, re-sorts
/// `working.events` by time (dragged-last tie-break, like keys), selection follows, and
/// marks the clip dirty so the rebake keeps the baked clip's events in sync. Returns the
/// new index.
///
/// Panics without an event selection — drags originate from an event diamond.
pub fn apply_event_retime(state: &mut EditorState, new_time: f32) -> usize {
    let Selection::Event(idx) = state.selection else {
        panic!("apply_event_retime without an event selection");
    };
    state.working.events[idx].time = new_time.clamp(0.0, state.working.duration);
    let new_idx = reinsert_sorted(&mut state.working.events, idx, |e| e.time);
    state.selection = Selection::Event(new_idx);
    state.clip_dirty = true;
    new_idx
}

/// Deletes the selected keyframe or event on Delete. Gated on `wants_keyboard_input`
/// (typing in a text field is text, not a hotkey) and on no active pointer press (the
/// gizmo and timeline drags hold the selection — deleting mid-drag would yank the key
/// out from under them).
pub fn apply_delete_hotkey(mut contexts: bevy_egui::EguiContexts, mut state: ResMut<EditorState>) {
    let Ok(ctx) = contexts.ctx_mut() else {
        trace!("egui context not ready; skipping delete hotkey");
        return;
    };
    if ctx.wants_keyboard_input() {
        return; // a text field owns the keyboard — expected, not a hotkey context
    }
    if ctx.input(|i| i.pointer.primary_down()) {
        trace!("delete pressed during a pointer press; ignored (drag owns the selection)");
        return;
    }
    if ctx.input(|i| i.key_pressed(bevy_egui::egui::Key::Delete)) {
        delete_selected(&mut state);
    }
}

/// Deletes the selected keyframe (or event) from the working clip, clears the selection,
/// and marks the clip dirty so the live rig rebakes. No-op when nothing is selected.
pub fn delete_selected(state: &mut EditorState) {
    match state.selection.clone() {
        Selection::Key { bone, channel, idx } => {
            let timeline = state
                .working
                .bone_timelines
                .get_mut(&bone)
                .unwrap_or_else(|| panic!("selected bone '{bone}' missing from working clip"));
            match channel {
                Channel::Rotation => drop(timeline.rotation.remove(idx)),
                Channel::Translation => drop(timeline.translation.remove(idx)),
                Channel::Scale => drop(timeline.scale.remove(idx)),
            }
        }
        Selection::Event(idx) => {
            state.working.events.remove(idx);
        }
        Selection::None => {
            trace!("delete pressed with nothing selected; ignoring");
            return;
        }
    }
    state.selection = Selection::None;
    state.clip_dirty = true;
}

/// Re-inserts `keys[idx]` at its time-sorted position and returns the new index. The
/// moved key lands AFTER every key with time <= its own (dragged-last tie-break), so
/// passing through a neighbor is deterministic.
fn reinsert_sorted<K>(keys: &mut Vec<K>, idx: usize, time_of: impl Fn(&K) -> f32) -> usize {
    let key = keys.remove(idx);
    let new_idx = keys.partition_point(|k| time_of(k) <= time_of(&key));
    keys.insert(new_idx, key);
    new_idx
}

/// Rebakes the working clip's override + additive `AnimationClip`s in place whenever an
/// edit marked it dirty, so the live rig re-evaluates the edited curves this frame.
pub fn rebuild_dirty_clip(
    mut state: ResMut<EditorState>,
    loaded_handles: Res<LoadedAnimHandles>,
    built_anims: Res<BuiltAnimations>,
    bone_defaults: Res<AnimBoneDefaults>,
    mut clips: ResMut<Assets<AnimationClip>>,
) {
    if !state.clip_dirty {
        return; // nothing edited this frame — steady state
    }
    state.clip_dirty = false;

    let path = state
        .selected_clip_path()
        .expect("dirty working clip implies an assigned slot");
    let handle = loaded_handles
        .0
        .get(path)
        .unwrap_or_else(|| panic!("no loaded handle for working clip '{path}'"));
    let pair = built_anims
        .0
        .get(&handle.id())
        .unwrap_or_else(|| panic!("no built clip pair for working clip '{path}'"));
    let defaults = bone_defaults
        .0
        .get(&handle.id())
        .unwrap_or_else(|| panic!("no bone defaults for working clip '{path}'"));

    sprite_rig::animation::rebuild_clip_pair_in_place(&state.working, defaults, pair, &mut clips);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ClipSlot, Playback};
    use sprite_rig::asset::{
        BoneTimeline, CurveType, LocomotionConfig, RotationKeyframe, SpriteAnimAsset,
        SpriteAnimSetAsset,
    };
    use std::collections::HashMap;

    fn editor_state(times: &[f32]) -> EditorState {
        let mut bone_timelines = HashMap::new();
        bone_timelines.insert(
            "root".to_string(),
            BoneTimeline {
                rotation: times
                    .iter()
                    .enumerate()
                    .map(|(i, &time)| RotationKeyframe {
                        time,
                        value: i as f32 * 10.0,
                        curve: CurveType::Linear,
                    })
                    .collect(),
                ..Default::default()
            },
        );
        EditorState {
            working: SpriteAnimAsset {
                name: "test".to_string(),
                duration: 1.0,
                looping: true,
                bone_timelines,
                events: vec![],
            },
            working_path: "anims/test/test.anim.ron".to_string(),
            working_set: SpriteAnimSetAsset {
                rig: String::new(),
                locomotion: LocomotionConfig { entries: vec![] },
                ability_animations: std::collections::BTreeMap::new(),
                hit_react: None,
            },
            selected_clip: ClipSlot::HitReact,
            preview_source_backup: None,
            playhead: 0.0,
            playback: Playback::Paused,
            selection: Selection::None,
            bone_order: vec!["root".to_string()],
            clip_dirty: false,
        }
    }

    fn select(state: &mut EditorState, idx: usize) {
        state.selection = Selection::Key {
            bone: "root".to_string(),
            channel: Channel::Rotation,
            idx,
        };
    }

    fn times(state: &EditorState) -> Vec<f32> {
        state.working.bone_timelines["root"]
            .rotation
            .iter()
            .map(|k| k.time)
            .collect()
    }

    #[test]
    fn new_time_clamps_to_clip_bounds() {
        let mut state = editor_state(&[0.0, 0.5, 1.0]);
        select(&mut state, 1);
        apply_key_edit(&mut state, -5.0, None);
        assert_eq!(times(&state)[0], 0.0);

        let mut state = editor_state(&[0.0, 0.5, 1.0]);
        select(&mut state, 1);
        let idx = apply_key_edit(&mut state, 99.0, None);
        assert_eq!(idx, 2);
        assert_eq!(times(&state)[2], 1.0);
    }

    #[test]
    fn dragging_past_neighbor_resorts_and_selection_follows() {
        let mut state = editor_state(&[0.0, 0.5, 1.0]);
        select(&mut state, 0);
        let idx = apply_key_edit(&mut state, 0.7, None);
        assert_eq!(idx, 1);
        assert_eq!(times(&state), vec![0.5, 0.7, 1.0]);
        // The dragged key kept its identity (value 0.0 from original idx 0).
        assert_eq!(state.working.bone_timelines["root"].rotation[1].value, 0.0);
        assert_eq!(
            state.selection,
            Selection::Key {
                bone: "root".to_string(),
                channel: Channel::Rotation,
                idx: 1
            }
        );
        assert!(state.clip_dirty);
    }

    #[test]
    fn coincident_time_places_dragged_key_after_existing() {
        let mut state = editor_state(&[0.0, 0.5, 1.0]);
        select(&mut state, 0);
        let idx = apply_key_edit(&mut state, 0.5, None);
        assert_eq!(idx, 1);
        assert_eq!(times(&state), vec![0.5, 0.5, 1.0]);
        // Pre-existing key (value 10.0) stays first; dragged key (value 0.0) lands after.
        let rotation = &state.working.bone_timelines["root"].rotation;
        assert_eq!(rotation[0].value, 10.0);
        assert_eq!(rotation[1].value, 0.0);
    }

    fn with_events(times: &[f32]) -> EditorState {
        let mut state = editor_state(&[0.0, 1.0]);
        state.working.events = times
            .iter()
            .enumerate()
            .map(|(i, &time)| sprite_rig::asset::AnimEventKeyframe {
                time,
                name: format!("ev{i}"),
            })
            .collect();
        state
    }

    #[test]
    fn delete_selected_key_removes_and_clears_selection() {
        let mut state = editor_state(&[0.0, 0.5, 1.0]);
        select(&mut state, 1);
        delete_selected(&mut state);
        assert_eq!(times(&state), vec![0.0, 1.0]);
        assert_eq!(state.selection, Selection::None);
        assert!(state.clip_dirty);
    }

    #[test]
    fn delete_selected_event_removes_and_clears_selection() {
        let mut state = with_events(&[0.2, 0.8]);
        state.selection = Selection::Event(0);
        delete_selected(&mut state);
        assert_eq!(state.working.events.len(), 1);
        assert_eq!(state.working.events[0].name, "ev1");
        assert_eq!(state.selection, Selection::None);
        assert!(state.clip_dirty);
    }

    #[test]
    fn delete_with_no_selection_is_a_no_op() {
        let mut state = editor_state(&[0.0, 1.0]);
        delete_selected(&mut state);
        assert_eq!(times(&state), vec![0.0, 1.0]);
        assert!(!state.clip_dirty);
    }

    #[test]
    fn event_retime_clamps_and_marks_dirty() {
        let mut state = with_events(&[0.2, 0.8]);
        state.selection = Selection::Event(1);
        let idx = apply_event_retime(&mut state, 99.0);
        assert_eq!(idx, 1);
        assert_eq!(state.working.events[1].time, 1.0);
        assert!(state.clip_dirty);
    }

    #[test]
    fn event_retime_past_neighbor_resorts_and_selection_follows() {
        let mut state = with_events(&[0.2, 0.8]);
        state.selection = Selection::Event(1);
        let idx = apply_event_retime(&mut state, 0.1);
        assert_eq!(idx, 0);
        // The dragged event kept its identity across the re-sort.
        assert_eq!(state.working.events[0].name, "ev1");
        assert_eq!(state.working.events[1].name, "ev0");
        assert_eq!(state.selection, Selection::Event(0));
    }

    #[test]
    fn value_edit_applies() {
        let mut state = editor_state(&[0.0, 0.5, 1.0]);
        select(&mut state, 1);
        apply_key_edit(&mut state, 0.5, Some(KeyValue::Scalar(45.0)));
        assert_eq!(state.working.bone_timelines["root"].rotation[1].value, 45.0);
    }
}
