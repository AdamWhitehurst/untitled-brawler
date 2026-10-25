---
date: 2026-06-12T10:26:25-07:00
researcher: Claude
git_commit: 03de94d055c5b9ed0c58204f36721ecf9e601569
branch: 2026-06-08-animation-editor
repository: bevy-lightyear-template
topic: "Gizmo drag on a bone with no keyframes doesn't move the bone"
tags: [bug, client_animation_editor, sprite_rig, auto-key, clip-baking]
status: complete
last_updated: 2026-06-12
last_updated_by: Claude
---

# Bug: Single-key channels bake as hold-at-default, making the first auto-keyed key invisible

**Date**: 2026-06-12T10:26:25-07:00
**Git Commit**: 03de94d0
**Branch**: 2026-06-08-animation-editor

## User's Prompt
"Something isn't allowing me to actually move bones to update them. Adjusting a bone
that doesn't already have keyframes doesn't seem to actually move the bone."

## Summary
The gizmo + auto-key path works correctly — it inserts one keyframe into the empty
channel and live-updates its value during the drag. The bake path is what swallows it:
every per-channel clip builder in `sprite_rig` requires **>= 2 keyframes** and otherwise
falls back to a hold-at-default/identity curve, so a channel with exactly one key bakes
identically to a channel with zero keys. The working asset changes; the baked
`AnimationClip` does not. This was noted as a known limitation during Phase 8 and is now
user-blocking.

## Investigation

### Editor side (works as intended)
- `resolve_target_key` (`crates/client_animation_editor/src/panels/gizmo.rs:260`): on an
  empty channel with auto-key on, inserts one key at the playhead with the channel
  identity (rotation `0.0`, translation `Vec2::ZERO`, scale `Vec2::ONE`) and sets
  `clip_dirty`.
- The drag then updates that key's value each frame via `apply_key_edit`
  (`crates/client_animation_editor/src/edit.rs:26`), and `rebuild_dirty_clip`
  (`edit.rs:113`) rebakes via `rebuild_clip_pair_in_place` every dirty frame.
- The inspector/curve panel sample the working asset directly, so they SHOW the value
  changing while the rig stays frozen — consistent with the report.

### Bake side (root cause)
All five channel builders in `crates/sprite_rig/src/animation.rs` ignore single-key
channels:
- `add_rotation_curve` (`animation.rs:485`): `len() >= 2` → keyframe curve, **else
  hold-at-`Quat::IDENTITY`** — the single key's value is never read.
- `add_translation_curve` (`animation.rs:514`): `len() >= 2` → keyframe curve, **else
  hold at `(default_xy, z_order)`**.
- `add_scale_curve` (`animation.rs:552`): `len() < 2` → no curve (Bevy default scale).
- `add_additive_rotation_curve` / `add_additive_translation_curve`: same `< 2` early
  return on the additive partition.

The `>= 2` requirement exists because `SegmentedKeyframeCurve::new` returns `None` for
fewer than two keys (`crates/sprite_rig/src/curve.rs`) — the fallback was written for
*zero*-key channels (bones the clip doesn't animate must still be written for mask
ownership) and accidentally captures one-key channels too.

## Code References
- `crates/client_animation_editor/src/panels/gizmo.rs:260` — `resolve_target_key` auto-key insert
- `crates/sprite_rig/src/animation.rs:485` — `add_rotation_curve` `>= 2` branch
- `crates/sprite_rig/src/animation.rs:514` — `add_translation_curve` `>= 2` branch
- `crates/sprite_rig/src/animation.rs:552` — `add_scale_curve` `< 2` early return
- `crates/sprite_rig/src/curve.rs` — `SegmentedKeyframeCurve::new` `None` for < 2 keys

## Hypotheses

### H1: Single-key channels bake as hold-at-default, so the first auto-keyed key is invisible

**Hypothesis:** The builders' `>= 2 keyframes` guards route one-key channels into the
zero-key fallback (hold at identity/default), so the auto-keyed first key — and every
drag update to it — never reaches the baked `AnimationClip`.
**Prediction:** A clip whose rotation channel holds exactly one key with value 90° bakes
a curve that samples `Quat::IDENTITY` at every t.
**Test:** Static analysis — the branch at `animation.rs:485` is deterministic; no
runtime instrumentation needed.
**Decision:** Validated (by reading; the one-key path provably never reads `k.value`).

## Fixes

### F1: One key = constant curve at that key's value (addresses H1)

**Root Cause:** Confirmed per H1.
**Fix:** In each builder, treat exactly-one-key channels as a constant curve at the
key's (transformed) value: rotation holds `Quat::from_rotation_z(value)`, translation
holds `default_xy + value` (with z_order), scale holds `(value, 1.0)`; the additive
builders hold the constant delta. Zero-key channels keep the existing hold-at-default
fallback. This is the standard single-key semantic in animation tools, and it makes the
editor's first auto-keyed drag visible immediately.
**Risk:** Shared with the game runtime — any authored clip with a single-key channel
changes from "held at default" to "held at the key". Scanned `assets/anims/humanoid/`:
the only single-key channel is in the user's new `kick.anim.ron`, itself an artifact of
this bug. No committed asset is affected.
**Decision:** Approved

## Solutions
F1 implemented in `crates/sprite_rig/src/animation.rs`:
- New `add_constant_curve` helper (constant `UnevenSampleAutoCurve` over the clip
  duration) shared by the zero-key fallbacks and the new single-key bake.
- All five builders (`add_rotation_curve`, `add_translation_curve`, `add_scale_curve`,
  `add_additive_rotation_curve`, `add_additive_translation_curve`) now match on the
  keyframe slice: `[]` keeps the old fallback, `[key]` holds the key's transformed
  value, `>= 2` interpolates as before. The additive builders and `add_scale_curve`
  gained a `duration` parameter for the constant curve's domain.
- Regression test `animation::tests::single_key_channel_bakes_constant_at_key_value`:
  app-level bake of a 1-key rotation (90°) + 1-key translation ((1,2) over default
  (10,20), z 0.5) channel, driven through Bevy's real `AnimationPlugin`, asserting the
  bone `Transform` lands on the key values, not identity/default.
- Verified: full `cargo test-native` green (657 passed).
