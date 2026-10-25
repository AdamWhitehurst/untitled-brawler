use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::edit::apply_key_edit;
use crate::panels::gizmo::GizmoMode;
use crate::panels::paint::{draw_diamond, draw_playhead, track_rect};
use crate::state::{Channel, EditorState, Playback, Selection};

const RULER_HEIGHT: f32 = 22.0;
const CHANNEL_ROW_HEIGHT: f32 = 18.0;
const BONE_HEADER_HEIGHT: f32 = 18.0;
const DIAMOND_SIZE: f32 = 5.0;
/// Pixel radius within which a click counts as hitting a diamond.
const HIT_TOLERANCE: f32 = 10.0;

/// Timeline panel: time ruler docked above the dope sheet, both sharing the same gutter
/// and `t→x` mapping so the playhead lands at the identical x in each. The initial panel
/// height fits the working clip's rows exactly, capped at a fraction of the window (the
/// ScrollArea handles overflow beyond the cap).
pub fn draw_timeline(
    mut contexts: EguiContexts,
    mut state: ResMut<EditorState>,
    mut gizmo_mode: ResMut<GizmoMode>,
    mut dragging_key: Local<bool>,
) {
    let Ok(ctx) = contexts.ctx_mut() else {
        trace!("egui context not ready; skipping timeline frame");
        return;
    };
    let bones = collect_bone_channels(&state);
    let content_height: f32 = RULER_HEIGHT
        + bones
            .iter()
            .map(|(_, channels)| BONE_HEADER_HEIGHT + channels.len() as f32 * CHANNEL_ROW_HEIGHT)
            .sum::<f32>()
        + 12.0;
    let height_cap = ctx.content_rect().height() * 0.45;
    egui::TopBottomPanel::bottom("timeline")
        .resizable(true)
        .default_height(content_height.min(height_cap))
        .show(ctx, |ui| {
            draw_ruler(ui, &mut state);
            egui::ScrollArea::vertical().show(ui, |ui| {
                draw_dope_sheet(ui, &mut state, &mut gizmo_mode, &bones, &mut dragging_key);
            });
        });
}

/// Every rig bone with ALL three channels (empty ones included — they're the only way to
/// see and arm a channel that has no keys yet), in `bone_order`. Drives both the dope
/// sheet rows and the panel's content-fitted initial height.
fn collect_bone_channels(state: &EditorState) -> Vec<(String, Vec<(Channel, Vec<f32>)>)> {
    state
        .bone_order
        .iter()
        .map(|bone| {
            let timeline = state.working.bone_timelines.get(bone);
            let times = |channel: Channel| -> Vec<f32> {
                let Some(timeline) = timeline else {
                    return Vec::new();
                };
                match channel {
                    Channel::Rotation => timeline.rotation.iter().map(|k| k.time).collect(),
                    Channel::Translation => timeline.translation.iter().map(|k| k.time).collect(),
                    Channel::Scale => timeline.scale.iter().map(|k| k.time).collect(),
                }
            };
            let channels = [Channel::Rotation, Channel::Translation, Channel::Scale]
                .into_iter()
                .map(|channel| (channel, times(channel)))
                .collect();
            (bone.clone(), channels)
        })
        .collect()
}

/// Time ruler: tick marks + the shared playhead. Clicking/dragging scrubs (seeks the
/// playhead via `x_to_t`, pausing playback so the pose tracks the pointer).
fn draw_ruler(ui: &mut egui::Ui, state: &mut EditorState) {
    let (lane, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), RULER_HEIGHT),
        egui::Sense::click_and_drag(),
    );
    let track = track_rect(lane);
    let painter = ui.painter_at(lane);

    let tick_color = ui.visuals().weak_text_color();
    let divisions = 10;
    for i in 0..=divisions {
        let t = state.working.duration * i as f32 / divisions as f32;
        let x = state.t_to_x(t, track);
        let major = i % 2 == 0;
        let tick_top = if major {
            lane.top() + 4.0
        } else {
            lane.top() + 10.0
        };
        painter.line_segment(
            [egui::pos2(x, tick_top), egui::pos2(x, lane.bottom())],
            egui::Stroke::new(1.0, tick_color),
        );
        if major && i < divisions {
            painter.text(
                egui::pos2(x + 3.0, lane.top()),
                egui::Align2::LEFT_TOP,
                format!("{t:.2}"),
                egui::FontId::proportional(10.0),
                tick_color,
            );
        }
    }

    if let Some(pos) = response.interact_pointer_pos() {
        if response.clicked() || response.dragged() {
            scrub_to(state, pos.x, track);
        }
    }
    draw_playhead(&painter, state.t_to_x(state.playhead, track), lane);
}

/// Dope sheet over every rig bone in `bone_order`, all three channel sub-tracks per
/// bone; a diamond per key at `t_to_x(key.time)`. Clicking a diamond selects it (no
/// scrub); dragging a diamond retimes the key live (clamped, re-sorting past
/// neighbors); clicking/dragging empty lane area scrubs. Any press inside a channel row
/// also sets the [`GizmoMode`] to that channel — clicking an empty row is how a keyless
/// channel gets armed for the gizmo's first auto-key.
fn draw_dope_sheet(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    gizmo_mode: &mut GizmoMode,
    bones: &[(String, Vec<(Channel, Vec<f32>)>)],
    dragging_key: &mut bool,
) {
    let total_height: f32 = bones
        .iter()
        .map(|(_, channels)| BONE_HEADER_HEIGHT + channels.len() as f32 * CHANNEL_ROW_HEIGHT)
        .sum();
    let (region, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), total_height.max(1.0)),
        egui::Sense::click_and_drag(),
    );
    let track = track_rect(region);
    let painter = ui.painter_at(region);
    let text_color = ui.visuals().text_color();
    let weak_color = ui.visuals().weak_text_color();
    let key_color = egui::Color32::from_rgb(190, 170, 90);

    let mut diamonds: Vec<(egui::Pos2, Selection)> = Vec::new();
    let mut rows: Vec<(egui::Rect, Channel)> = Vec::new();
    let mut y = region.top();
    for (bone, channels) in bones {
        painter.text(
            egui::pos2(region.left() + 4.0, y + BONE_HEADER_HEIGHT / 2.0),
            egui::Align2::LEFT_CENTER,
            bone,
            egui::FontId::proportional(11.0),
            text_color,
        );
        y += BONE_HEADER_HEIGHT;
        for (channel, times) in channels {
            let row = egui::Rect::from_min_size(
                egui::pos2(region.left(), y),
                egui::vec2(region.width(), CHANNEL_ROW_HEIGHT),
            );
            painter.text(
                egui::pos2(region.left() + 14.0, row.center().y),
                egui::Align2::LEFT_CENTER,
                channel_label(*channel),
                egui::FontId::proportional(10.0),
                weak_color,
            );
            painter.line_segment(
                [
                    egui::pos2(track.left(), row.center().y),
                    egui::pos2(track.right(), row.center().y),
                ],
                egui::Stroke::new(1.0, ui.visuals().faint_bg_color),
            );
            for (idx, time) in times.iter().enumerate() {
                let pos = egui::pos2(state.t_to_x(*time, track), row.center().y);
                let selection = Selection::Key {
                    bone: bone.clone(),
                    channel: *channel,
                    idx,
                };
                draw_diamond(
                    &painter,
                    pos,
                    DIAMOND_SIZE,
                    key_color,
                    state.selection == selection,
                );
                diamonds.push((pos, selection));
            }
            rows.push((row, *channel));
            y += CHANNEL_ROW_HEIGHT;
        }
    }

    if let Some(pos) = response.interact_pointer_pos() {
        if response.drag_started() {
            // Hit-test the press origin: drag_started fires after the drag threshold,
            // by which time the pointer may have left the diamond.
            let grab_pos = response
                .ctx
                .input(|i| i.pointer.press_origin())
                .unwrap_or(pos);
            arm_channel_under(grab_pos, &rows, gizmo_mode);
            match hit_test_diamond(grab_pos, &diamonds) {
                Some(selection) => {
                    state.selection = selection;
                    *dragging_key = true;
                }
                None => scrub_to(state, pos.x, track),
            }
        } else if response.clicked() {
            arm_channel_under(pos, &rows, gizmo_mode);
            match hit_test_diamond(pos, &diamonds) {
                Some(selection) => state.selection = selection,
                None => scrub_to(state, pos.x, track),
            }
        } else if response.dragged() {
            if *dragging_key {
                apply_key_edit(state, state.x_to_t(pos.x, track), None);
            } else {
                scrub_to(state, pos.x, track);
            }
        }
    }
    if response.drag_stopped() {
        *dragging_key = false;
    }
    draw_playhead(&painter, state.t_to_x(state.playhead, track), region);
}

/// Sets the gizmo mode to the channel of the row under `pos`, if any — pressing anywhere
/// in a row (diamond, empty track, or label) arms that channel for gizmo drags.
fn arm_channel_under(pos: egui::Pos2, rows: &[(egui::Rect, Channel)], gizmo_mode: &mut GizmoMode) {
    if let Some((_, channel)) = rows.iter().find(|(rect, _)| rect.contains(pos)) {
        gizmo_mode.0 = *channel;
    }
}

/// Returns the `Selection` for the diamond nearest `pos` within `HIT_TOLERANCE`, or `None`.
fn hit_test_diamond(pos: egui::Pos2, diamonds: &[(egui::Pos2, Selection)]) -> Option<Selection> {
    diamonds
        .iter()
        .map(|(p, sel)| (p.distance(pos), sel))
        .filter(|(d, _)| *d <= HIT_TOLERANCE)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, sel)| sel.clone())
}

/// Seeks the playhead to the time under `x`, pausing playback so the pose holds.
/// Shared with the event lane so empty-space drags scrub identically in every lane.
pub(crate) fn scrub_to(state: &mut EditorState, x: f32, track: egui::Rect) {
    state.playhead = state.x_to_t(x, track);
    state.playback = Playback::Paused;
}

fn channel_label(channel: Channel) -> &'static str {
    match channel {
        Channel::Rotation => "rotation",
        Channel::Translation => "translation",
        Channel::Scale => "scale",
    }
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

    fn editor_state() -> EditorState {
        let mut bone_timelines = HashMap::new();
        bone_timelines.insert(
            "torso".to_string(),
            BoneTimeline {
                rotation: vec![RotationKeyframe {
                    time: 0.25,
                    value: 10.0,
                    curve: CurveType::Linear,
                }],
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
            bone_order: vec!["root".to_string(), "torso".to_string()],
            clip_dirty: false,
        }
    }

    /// Every rig bone gets all three channel rows — including bones with no timeline and
    /// channels with no keys — so keyless channels are visible and armable.
    #[test]
    fn all_bones_get_all_channel_rows() {
        let rows = collect_bone_channels(&editor_state());
        assert_eq!(rows.len(), 2);
        for (i, bone) in ["root", "torso"].iter().enumerate() {
            assert_eq!(rows[i].0, *bone);
            let channels: Vec<Channel> = rows[i].1.iter().map(|(c, _)| *c).collect();
            assert_eq!(
                channels,
                vec![Channel::Rotation, Channel::Translation, Channel::Scale]
            );
        }
        // root has no timeline at all — every channel row is empty.
        assert!(rows[0].1.iter().all(|(_, times)| times.is_empty()));
        // torso's single rotation key shows; its other channels are empty rows.
        assert_eq!(rows[1].1[0].1, vec![0.25]);
        assert!(rows[1].1[1].1.is_empty());
        assert!(rows[1].1[2].1.is_empty());
    }
}
