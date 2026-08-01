use bevy::prelude::*;
use bevy_egui::egui;
use sprite_rig::asset::{SpriteAnimAsset, SpriteAnimSetAsset};
use sprite_rig::{AnimBoneDefaults, AnimSetRef, BuiltAnimGraphs, LoadedAnimHandles};

/// Identifies which animset slot the working clip occupies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipSlot {
    /// Index into `locomotion.entries`.
    Locomotion(usize),
    /// Ability id key in `ability_animations`.
    Ability(String),
    HitReact,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Playback {
    Playing,
    Paused,
}

/// A keyframe channel of a bone timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Rotation,
    Translation,
    Scale,
}

/// What the user has selected in the time views.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Selection {
    #[default]
    None,
    /// A keyframe: bone name, channel, index into that channel's key vec.
    Key {
        bone: String,
        channel: Channel,
        idx: usize,
    },
    /// An `AnimEventKeyframe` index in `working.events`.
    Event(usize),
}

/// The single in-memory working model every editor view reads and every edit mutates.
#[derive(Resource)]
pub struct EditorState {
    /// Working copy of the selected clip's source asset.
    pub working: SpriteAnimAsset,
    /// Working copy of the animset (slot assignments, Phase 12 saves it).
    pub working_set: SpriteAnimSetAsset,
    pub selected_clip: ClipSlot,
    /// Seconds, in `[0, working.duration]`.
    pub playhead: f32,
    pub playback: Playback,
    pub selection: Selection,
    /// Rig bone names in bone-index order; fixes the dope sheet's row order (the working
    /// clip's `bone_timelines` is a `HashMap`).
    pub bone_order: Vec<String>,
    /// Set by edits; `rebuild_dirty_clip` rebakes the live clip and clears it.
    pub clip_dirty: bool,
}

impl EditorState {
    /// Maps a clip time `t` (seconds) to an x within `track` as a fraction of track width.
    /// Shared by every time view so one playhead lands at the same x everywhere.
    pub fn t_to_x(&self, t: f32, track: egui::Rect) -> f32 {
        let frac = if self.working.duration > 0.0 {
            t / self.working.duration
        } else {
            0.0
        };
        track.left() + frac * track.width()
    }

    /// Inverse of `t_to_x`, clamped to `[0, duration]`.
    pub fn x_to_t(&self, x: f32, track: egui::Rect) -> f32 {
        let frac = ((x - track.left()) / track.width()).clamp(0.0, 1.0);
        frac * self.working.duration
    }

    /// The clip path the selected slot refers to in the working animset, if assigned.
    pub fn selected_clip_path(&self) -> Option<&str> {
        match &self.selected_clip {
            ClipSlot::Locomotion(i) => self
                .working_set
                .locomotion
                .entries
                .get(*i)
                .map(|e| e.clip.as_str()),
            ClipSlot::Ability(id) => self
                .working_set
                .ability_animations
                .get(id)
                .map(|s| s.as_str()),
            ClipSlot::HitReact => self.working_set.hit_react.as_deref(),
        }
    }
}

/// Inserts `EditorState` once the rig's animset and the default clip (first locomotion
/// entry, idle) are loaded, cloning both into the working model. Runs until it succeeds;
/// the `resource_exists` run condition keeps it off afterwards.
pub fn init_editor_state(
    mut commands: Commands,
    rigs: Query<&AnimSetRef>,
    animset_assets: Res<Assets<SpriteAnimSetAsset>>,
    anim_assets: Res<Assets<SpriteAnimAsset>>,
    loaded_handles: Res<LoadedAnimHandles>,
    bone_defaults: Res<AnimBoneDefaults>,
) {
    let Ok(animset_ref) = rigs.single() else {
        trace!("editor rig not spawned yet; EditorState waits");
        return;
    };
    let Some(animset) = animset_assets.get(&animset_ref.0) else {
        trace!("animset not loaded yet; EditorState waits");
        return;
    };
    let default_slot = ClipSlot::Locomotion(0);
    let default_path = &animset.locomotion.entries[0].clip;
    let Some(default_handle) = loaded_handles.0.get(default_path) else {
        trace!("default clip not loaded yet; EditorState waits");
        return;
    };
    let Some(working) = anim_assets.get(default_handle) else {
        trace!("default clip not loaded yet; EditorState waits");
        return;
    };
    let Some(bone_order) = bone_defaults
        .0
        .get(&default_handle.id())
        .map(|defaults| defaults.iter().map(|b| b.name.clone()).collect())
    else {
        trace!("bone defaults not populated yet; EditorState waits");
        return;
    };

    commands.insert_resource(EditorState {
        working: working.clone(),
        working_set: animset.clone(),
        selected_clip: default_slot,
        playhead: 0.0,
        playback: Playback::Playing,
        selection: Selection::None,
        bone_order,
        clip_dirty: false,
    });
}

/// Re-clones `working` from the newly selected slot's clip asset and resets the playhead.
/// Called by the transport when the user picks a different clip.
pub fn select_clip(
    state: &mut EditorState,
    slot: ClipSlot,
    anim_assets: &Assets<SpriteAnimAsset>,
    loaded_handles: &LoadedAnimHandles,
) {
    state.selected_clip = slot;
    let Some(asset) = state
        .selected_clip_path()
        .and_then(|path| loaded_handles.0.get(path))
        .and_then(|handle| anim_assets.get(handle))
    else {
        // A slot with no assigned/loaded clip is a bug: the transport only offers slots
        // read from the loaded animset.
        panic!(
            "selected clip slot {:?} has no loaded clip asset",
            state.selected_clip
        );
    };
    state.working = asset.clone();
    state.playhead = 0.0;
    state.selection = Selection::None;
}

/// How the new-clip form attaches the working clip to the animset.
#[derive(Clone, Debug, PartialEq)]
pub enum NewClipSlot {
    Locomotion { speed_threshold: f32 },
    Ability { id: String },
    HitReact,
}

/// Assigns the current working clip to a NEW animset slot under `new_path` (the
/// `.anim.ron` suffix is appended if missing). Registers the clip as a live asset first —
/// source asset, `LoadedAnimHandles` entry, and bone defaults (copied from the selected
/// clip; same rig) — so next frame's build chain (`load_animset_clips` →
/// `build_animation_clips` → `build_anim_graphs`) treats it as already loaded instead of
/// asking the asset server for a file that doesn't exist on disk yet. The animset
/// write-back via `get_mut` emits `AssetEvent::Modified`, the real graph-rebuild trigger.
/// Ends by selecting the new slot. Persisting is a separate step (Save clip + Save
/// animset). Returns the resolved clip path, or a validation error for the status line.
#[allow(clippy::too_many_arguments)]
pub fn assign_new_clip(
    state: &mut EditorState,
    new_path: &str,
    slot: NewClipSlot,
    animset_ref: &AnimSetRef,
    anim_assets: &mut Assets<SpriteAnimAsset>,
    animset_assets: &mut Assets<SpriteAnimSetAsset>,
    loaded_handles: &mut LoadedAnimHandles,
    bone_defaults: &mut AnimBoneDefaults,
) -> Result<String, String> {
    let new_path = new_path.trim();
    if new_path.is_empty() {
        return Err("clip path is empty".to_string());
    }
    let new_path = if new_path.ends_with(".anim.ron") {
        new_path.to_string()
    } else {
        format!("{new_path}.anim.ron")
    };
    if loaded_handles.0.contains_key(&new_path) {
        return Err(format!("'{new_path}' is already assigned"));
    }
    validate_slot(&slot, &state.working_set)?;

    let defaults = state
        .selected_clip_path()
        .and_then(|path| loaded_handles.0.get(path))
        .and_then(|handle| bone_defaults.0.get(&handle.id()))
        .cloned()
        .expect("selected clip has bone defaults (rig resolved before the editor opened)");
    let handle = anim_assets.add(state.working.clone());
    bone_defaults.0.insert(handle.id(), defaults);
    loaded_handles.0.insert(new_path.clone(), handle);

    let new_slot = insert_slot_assignment(&mut state.working_set, slot, new_path.clone());
    *animset_assets
        .get_mut(&animset_ref.0)
        .expect("editor animset asset exists") = state.working_set.clone();

    select_clip(state, new_slot, anim_assets, loaded_handles);
    Ok(new_path)
}

/// Rejects slot specs that would silently displace an existing assignment.
fn validate_slot(slot: &NewClipSlot, working_set: &SpriteAnimSetAsset) -> Result<(), String> {
    match slot {
        NewClipSlot::Ability { id } if id.is_empty() => Err("ability id is empty".to_string()),
        NewClipSlot::Ability { id } if working_set.ability_animations.contains_key(id) => {
            Err(format!("ability '{id}' already has a clip"))
        }
        NewClipSlot::HitReact if working_set.hit_react.is_some() => {
            Err("hit_react already has a clip".to_string())
        }
        NewClipSlot::Locomotion { speed_threshold }
            if working_set
                .locomotion
                .entries
                .iter()
                .any(|e| e.speed_threshold == *speed_threshold) =>
        {
            Err(format!(
                "a locomotion entry with threshold {speed_threshold} already exists (blend weights divide by threshold gaps)"
            ))
        }
        _ => Ok(()),
    }
}

/// Inserts the slot assignment into the working animset and returns the resulting
/// `ClipSlot`. Locomotion entries stay sorted by `speed_threshold` (the blend tree
/// requires ascending thresholds).
pub(crate) fn insert_slot_assignment(
    working_set: &mut SpriteAnimSetAsset,
    slot: NewClipSlot,
    path: String,
) -> ClipSlot {
    match slot {
        NewClipSlot::Locomotion { speed_threshold } => {
            let idx = working_set
                .locomotion
                .entries
                .partition_point(|e| e.speed_threshold <= speed_threshold);
            working_set.locomotion.entries.insert(
                idx,
                sprite_rig::asset::LocomotionEntry {
                    clip: path,
                    speed_threshold,
                },
            );
            ClipSlot::Locomotion(idx)
        }
        NewClipSlot::Ability { id } => {
            working_set.ability_animations.insert(id.clone(), path);
            ClipSlot::Ability(id)
        }
        NewClipSlot::HitReact => {
            working_set.hit_react = Some(path);
            ClipSlot::HitReact
        }
    }
}

/// Drives the editor rig's `AnimationPlayer` from the transport: ensures the selected
/// clip's node(s) are playing at speed 0 (the transport owns time for the selected clip —
/// Bevy's natural advance would fight the playhead), advances the playhead by `dt` when
/// `Playing` (wrapping at the clip duration), and seeks the node(s) to the playhead.
///
/// When the selection changes, the previous slot's nodes are released first — abilities
/// and hit_react are stopped (otherwise they stay frozen at their last pose and keep
/// writing their bones over the new selection); a previously selected locomotion node
/// instead gets its natural speed back, since locomotion must keep playing
/// (`update_locomotion_blend_weights` requires it). For ability slots both the override
/// and additive nodes are driven.
pub fn drive_player_from_playhead(
    mut state: ResMut<EditorState>,
    mut players: Query<(&mut AnimationPlayer, &AnimSetRef)>,
    built_graphs: Res<BuiltAnimGraphs>,
    anim_assets: Res<Assets<SpriteAnimAsset>>,
    loaded_handles: Res<LoadedAnimHandles>,
    time: Res<Time>,
    mut last_applied: Local<Option<ClipSlot>>,
) {
    let Ok((mut player, animset_ref)) = players.single_mut() else {
        trace!("editor rig player not ready; transport idle");
        return;
    };
    let Some(built) = built_graphs.0.get(&animset_ref.0.id()) else {
        trace!("anim graph not built yet; transport idle");
        return;
    };

    if last_applied.as_ref() != Some(&state.selected_clip) {
        if let Some(previous) = last_applied.as_ref() {
            release_slot_nodes(previous, &state, built, &mut player);
        }
        *last_applied = Some(state.selected_clip.clone());
    }

    if state.playback == Playback::Playing {
        let duration = state.working.duration.max(f32::EPSILON);
        state.playhead = (state.playhead + time.delta_secs()) % duration;
    }

    // Strictly below the clip duration: bevy's advance wraps `seek_time %= duration` even
    // at speed 0, which inverts the (last_seek_time, seek_time) pair when seeked exactly
    // to the duration and panics the event-trigger system on the inverted slice range.
    let applied_t = state
        .playhead
        .min(state.working.duration.next_down())
        .max(0.0);
    let selected = slot_nodes(&state.selected_clip, &state.working_set, built);
    for &node in &selected {
        let anim = match player.animation_mut(node) {
            Some(anim) => anim,
            None => player.play(node),
        };
        anim.repeat();
        anim.set_speed(0.0);
        // set_seek_time (not seek_to): no event range is queued, so scrubbing fires no
        // clip events. Transport-driven nodes can't fire them anyway — advance normalizes
        // the seek pair before triggers run.
        anim.set_seek_time(applied_t);
    }

    let locomotion_durations = state
        .working_set
        .locomotion
        .entries
        .iter()
        .map(|entry| {
            let handle = loaded_handles
                .0
                .get(&entry.clip)
                .expect("locomotion entry must reference a loaded clip");
            anim_assets
                .get(handle)
                .expect("loaded locomotion handle must reference an animation asset")
                .duration
        })
        .collect::<Vec<_>>();
    drive_unselected_locomotion_nodes(
        &mut player,
        &built.locomotion_entries,
        &locomotion_durations,
        &selected,
        state.playback,
        applied_t,
    );
}

/// Keeps every paused locomotion source on the shared playhead so changing the audition
/// speed reveals a deterministic blend instead of poses frozen by prior playback history.
fn drive_unselected_locomotion_nodes(
    player: &mut AnimationPlayer,
    entries: &[sprite_rig::animation::LocomotionNodeEntry],
    durations: &[f32],
    selected: &[AnimationNodeIndex],
    playback: Playback,
    playhead: f32,
) {
    assert_eq!(
        entries.len(),
        durations.len(),
        "locomotion graph and animset entry counts must match"
    );
    for (entry, &duration) in entries.iter().zip(durations) {
        assert!(
            duration > 0.0,
            "locomotion clips must have positive duration"
        );
        if !selected.contains(&entry.node_index) {
            if let Some(animation) = player.animation_mut(entry.node_index) {
                match playback {
                    Playback::Playing => {
                        animation.set_speed(1.0);
                    }
                    Playback::Paused => {
                        animation
                            .set_speed(0.0)
                            .set_seek_time(playhead.rem_euclid(duration));
                    }
                }
            } else {
                trace!("locomotion node not playing yet; start_locomotion_blend seeds it");
            }
        }
    }
}

/// Mirrors the in-game layer stack for the selected ability so the preview composes under
/// REAL masks: pushes an override (and, when claimed, additive) `AnimLayer` for the
/// ability's nodes and recomputes masks, exactly like `trigger_ability_animations` does
/// for a cast. Without this, locomotion keeps writing the ability's claimed bones and the
/// 50/50 quaternion blend flips when a rotation crosses ±180° relative to the base pose.
///
/// Editor layers use `AnimLayerSource::Locomotion` — the only source
/// `cleanup_finished_ability_layers` never drops (there is no `ActiveAbility` entity to
/// tie them to) — and ids prefixed `editor:` so this system can own their lifecycle.
pub fn sync_ability_preview_layers(
    state: Res<EditorState>,
    mut rigs: Query<(
        &mut sprite_rig::ActiveAnimLayers,
        &AnimSetRef,
        &AnimationGraphHandle,
    )>,
    built_graphs: Res<BuiltAnimGraphs>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
) {
    let Ok((mut layers, animset_ref, graph_handle)) = rigs.single_mut() else {
        trace!("editor rig not ready; no preview layers");
        return;
    };
    let Some(built) = built_graphs.0.get(&animset_ref.0.id()) else {
        trace!("anim graph not built yet; no preview layers");
        return;
    };
    if !layers.entries.iter().any(|e| e.id == "locomotion") {
        // The permanent locomotion entry is seeded by start_locomotion_blend; adding
        // editor layers to an empty stack would make it skip seeding entirely.
        trace!("locomotion layer not seeded yet; preview layers wait");
        return;
    }

    let desired = match &state.selected_clip {
        ClipSlot::Ability(id) => built
            .ability_nodes
            .get(id)
            .map(|pair| {
                let mut entries = Vec::new();
                if pair.override_claims != 0 {
                    entries.push(sprite_rig::AnimLayer {
                        id: format!("editor:{id}"),
                        node_index: pair.override_node,
                        claims: pair.override_claims,
                        priority: 1,
                        mode: sprite_rig::AnimLayerMode::Override,
                        source: sprite_rig::AnimLayerSource::Locomotion,
                    });
                }
                if pair.additive_claims != 0 {
                    entries.push(sprite_rig::AnimLayer {
                        id: format!("editor:{id}:additive"),
                        node_index: pair.additive_node,
                        claims: pair.additive_claims,
                        priority: 0,
                        mode: sprite_rig::AnimLayerMode::Additive,
                        source: sprite_rig::AnimLayerSource::Locomotion,
                    });
                }
                entries
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };

    let current: Vec<&sprite_rig::AnimLayer> = layers
        .entries
        .iter()
        .filter(|e| e.id.starts_with("editor:"))
        .collect();
    let in_sync = current.len() == desired.len()
        && current
            .iter()
            .zip(&desired)
            .all(|(c, d)| c.id == d.id && c.node_index == d.node_index && c.claims == d.claims);
    if in_sync {
        return; // selection unchanged since last sync — steady state
    }

    layers.entries.retain(|e| !e.id.starts_with("editor:"));
    layers.entries.extend(desired);
    if let Some(graph) = graphs.get_mut(&graph_handle.0) {
        sprite_rig::recompute_layer_masks(&layers.entries, graph);
    } else {
        debug_assert!(
            false,
            "rig holds an AnimationGraphHandle with no graph asset"
        );
    }
}

/// Releases the transport's hold on a slot's nodes after the selection moves elsewhere:
/// locomotion nodes return to natural speed (they must keep playing); ability/hit_react
/// nodes are stopped so their frozen pose no longer blends over the new selection.
fn release_slot_nodes(
    slot: &ClipSlot,
    state: &EditorState,
    built: &sprite_rig::animation::BuiltAnimGraph,
    player: &mut AnimationPlayer,
) {
    let nodes = slot_nodes(slot, &state.working_set, built);
    match slot {
        ClipSlot::Locomotion(_) => {
            for node in nodes {
                if let Some(anim) = player.animation_mut(node) {
                    anim.set_speed(1.0);
                }
            }
        }
        ClipSlot::Ability(_) | ClipSlot::HitReact => {
            for node in nodes {
                player.stop(node);
            }
        }
    }
}

/// Resolves the graph node(s) a slot drives. Abilities have an override and an additive
/// node; the others have one.
fn slot_nodes(
    slot: &ClipSlot,
    working_set: &SpriteAnimSetAsset,
    built: &sprite_rig::animation::BuiltAnimGraph,
) -> Vec<AnimationNodeIndex> {
    match slot {
        ClipSlot::Locomotion(i) => built
            .locomotion_entries
            .get(*i)
            .map(|e| vec![e.node_index])
            .unwrap_or_default(),
        ClipSlot::Ability(id) => built
            .ability_nodes
            .get(id)
            .map(|pair| {
                let mut nodes = vec![pair.override_node];
                if pair.additive_claims != 0 {
                    nodes.push(pair.additive_node);
                }
                nodes
            })
            .unwrap_or_default(),
        ClipSlot::HitReact => working_set
            .hit_react
            .as_ref()
            .and_then(|path| built.node_map.get(path))
            .map(|&node| vec![node])
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sprite_rig::asset::{LocomotionConfig, LocomotionEntry};

    fn animset_with_thresholds(thresholds: &[f32]) -> SpriteAnimSetAsset {
        SpriteAnimSetAsset {
            rig: String::new(),
            locomotion: LocomotionConfig {
                entries: thresholds
                    .iter()
                    .map(|&speed_threshold| LocomotionEntry {
                        clip: format!("clip_{speed_threshold}"),
                        speed_threshold,
                    })
                    .collect(),
            },
            ability_animations: std::collections::BTreeMap::new(),
            hit_react: None,
        }
    }

    #[test]
    fn locomotion_assignment_inserts_sorted_by_threshold() {
        let mut set = animset_with_thresholds(&[0.0, 6.0]);
        let slot = insert_slot_assignment(
            &mut set,
            NewClipSlot::Locomotion {
                speed_threshold: 2.0,
            },
            "walk.anim.ron".to_string(),
        );
        assert_eq!(slot, ClipSlot::Locomotion(1));
        let thresholds: Vec<f32> = set
            .locomotion
            .entries
            .iter()
            .map(|e| e.speed_threshold)
            .collect();
        assert_eq!(thresholds, vec![0.0, 2.0, 6.0]);
        assert_eq!(set.locomotion.entries[1].clip, "walk.anim.ron");
    }

    #[test]
    fn ability_and_hit_react_assignments() {
        let mut set = animset_with_thresholds(&[0.0]);
        let slot = insert_slot_assignment(
            &mut set,
            NewClipSlot::Ability {
                id: "kick".to_string(),
            },
            "kick.anim.ron".to_string(),
        );
        assert_eq!(slot, ClipSlot::Ability("kick".to_string()));
        assert_eq!(set.ability_animations["kick"], "kick.anim.ron");

        let slot =
            insert_slot_assignment(&mut set, NewClipSlot::HitReact, "hit.anim.ron".to_string());
        assert_eq!(slot, ClipSlot::HitReact);
        assert_eq!(set.hit_react.as_deref(), Some("hit.anim.ron"));
    }

    #[test]
    fn validate_slot_rejects_conflicts() {
        let mut set = animset_with_thresholds(&[0.0, 2.0]);
        set.ability_animations
            .insert("punch".to_string(), "punch.anim.ron".to_string());
        set.hit_react = Some("hit.anim.ron".to_string());

        assert!(validate_slot(
            &NewClipSlot::Ability {
                id: "punch".to_string()
            },
            &set
        )
        .is_err());
        assert!(validate_slot(&NewClipSlot::Ability { id: String::new() }, &set).is_err());
        assert!(validate_slot(&NewClipSlot::HitReact, &set).is_err());
        assert!(validate_slot(
            &NewClipSlot::Locomotion {
                speed_threshold: 2.0
            },
            &set
        )
        .is_err());
        assert!(validate_slot(
            &NewClipSlot::Locomotion {
                speed_threshold: 4.0
            },
            &set
        )
        .is_ok());
        assert!(validate_slot(
            &NewClipSlot::Ability {
                id: "kick".to_string()
            },
            &set
        )
        .is_ok());
    }

    /// Verifies paused sources wrap the shared playhead by their own clip durations.
    #[test]
    fn paused_locomotion_nodes_follow_the_shared_playhead() {
        let entries = [0, 1, 2].map(|i| sprite_rig::animation::LocomotionNodeEntry {
            node_index: AnimationNodeIndex::new(i),
            speed_threshold: i as f32,
        });
        let selected = [entries[0].node_index];
        let mut player = AnimationPlayer::default();
        for entry in &entries {
            player.play(entry.node_index).set_seek_time(0.25);
        }

        drive_unselected_locomotion_nodes(
            &mut player,
            &entries,
            &[2.0, 0.4, 0.7],
            &selected,
            Playback::Paused,
            1.25,
        );

        assert_eq!(
            player
                .animation(entries[0].node_index)
                .expect("selected locomotion node should remain active")
                .seek_time(),
            0.25
        );
        for (entry, expected_seek_time) in entries[1..].iter().zip([0.05, 0.55]) {
            let animation = player
                .animation(entry.node_index)
                .expect("locomotion node should remain active");
            assert_eq!(animation.speed(), 0.0);
            assert!((animation.seek_time() - expected_seek_time).abs() < 1e-6);
        }
    }

    /// Verifies resuming playback restarts unselected sources without seeking them.
    #[test]
    fn playing_locomotion_nodes_resume_without_seeking() {
        let entries = [0, 1].map(|i| sprite_rig::animation::LocomotionNodeEntry {
            node_index: AnimationNodeIndex::new(i),
            speed_threshold: i as f32,
        });
        let mut player = AnimationPlayer::default();
        for entry in &entries {
            player
                .play(entry.node_index)
                .set_speed(0.0)
                .set_seek_time(0.25);
        }

        drive_unselected_locomotion_nodes(
            &mut player,
            &entries,
            &[0.4, 0.7],
            &[],
            Playback::Playing,
            1.25,
        );

        for entry in &entries {
            let animation = player
                .animation(entry.node_index)
                .expect("locomotion node should remain active");
            assert_eq!(animation.speed(), 1.0);
            assert_eq!(animation.seek_time(), 0.25);
        }
    }
}
