# Structure Outline

## Approach

The preview **is** the real runtime: a full `SpriteRigPlugin` character driven by the real animset `AnimationGraph`, minus physics/networking. Two `sprite_rig` runtime capabilities (honor `CurveType`, first-class graph rebuild) land first as standalone, regression-verifiable changes that also benefit the game. Canonical RON serialization lands as a tested unit. Then a new `client_animation_editor` crate boots the live rig, layers in custom-painted time views over a single shared `t→x` axis, adds keyframe/bone editing that mutates one in-memory `SpriteAnimAsset` (rebuilding the clip/graph in place), and finally writes canonical RON back to disk.

Phases 1–3 are `sprite_rig`/serialization foundations (each independently valuable to the game). Phases 4+ build the editor; each later phase degrades gracefully if a subsequent one is dropped.

---

## Phase 1: Honor `CurveType` at runtime

Replace `UnevenSampleAutoCurve` construction with a per-segment curve that interpolates by each key's `curve` (`Linear` lerp/slerp, `Step` hold-left). All 7 existing clips are pure `Linear`, so behavior is preserved.

**Files**: `crates/sprite_rig/src/animation.rs` (curve builders ~`:460-530`), possibly a new `crates/sprite_rig/src/curve.rs`.
**Key changes**:
- `fn build_segment_curve<T: StableInterpolate>(keys: &[(f32, T, CurveType)]) -> impl Curve<T>` — new; honors `Step`/`Linear` per segment.
- Replace `UnevenSampleAutoCurve::new(...)` call sites in override/additive rotation/translation/scale builders with the new curve, threading `k.curve`.

**Verify**: `cargo test-all` passes; add a unit test asserting `Step` holds left and `Linear` interpolates at a midpoint. `cargo client` visual check — all 7 clips play unchanged.

---

## Phase 2: First-class graph/mask rebuild in place

On animset/partition change, rebuild the `AnimationGraph` at the same `AssetId`, replace `BuiltAnimGraphs` bookkeeping + per-bone mask groups, and reset active playback so stale `AnimationNodeIndex`es don't leak. Removes the "requires app restart" `warn!`.

**Files**: `crates/sprite_rig/src/animation.rs` (`build_anim_graphs` short-circuit `:550-551`, Modified handler `:246-269`), `crates/sprite_rig/src/animset.rs` (`recompute_layer_masks` `:122-147`, `ActiveAnimLayers`).
**Key changes**:
- `fn rebuild_anim_graph(animset_id: AssetId<SpriteAnimSetAsset>, ...)` — new; `graphs.insert(existing_id, new_graph)` (mirrors clip in-place idiom), rebuilds mask groups, resets players.
- Remove build-once short-circuit; add `AssetEvent::Modified` reader for `SpriteAnimSetAsset`.
- `fn reset_animation_players_for(...)` — clear `AnimationPlayer` active animations + `ActiveAnimLayers` so `recompute_layer_masks` self-heals.

**Verify**: `cargo test-all` + new tests: rebuild mid-playback, mid-loop, with active ability layer (no panic, masks correct, playback resumes). `cargo client`: edit a `blend_mode` in a `.anim.ron` live → no restart needed.

---

## Phase 3: Canonical, deterministic RON serialization

Serialize `SpriteAnimAsset` and `SpriteAnimSetAsset` to canonical RON: `bone_timelines` in rig bone-index order, default `blend_mode` skipped, tuned `PrettyConfig`, atomic write. No UI yet — pure function + round-trip test.

**Files**: new `crates/sprite_rig/src/serialize.rs`; `crates/sprite_rig/Cargo.toml` (add `ron` dep).
**Key changes**:
- `fn serialize_anim(asset: &SpriteAnimAsset, bone_order: &[String]) -> String` — new; deterministic ordering, `implicit_some` header rule decided here.
- `fn serialize_animset(asset: &SpriteAnimSetAsset) -> String` — new.
- Pure string-returning functions only — no disk I/O here; the write path lands in Phase 12 as `bevy-persistence` `Store` impls. Keeps `sprite_rig` free of the `persistence` dep.

**Verify**: round-trip test — load each of the 7 clips + the animset, serialize, deserialize, assert structural equality (nothing invented/dropped); assert a second serialize is byte-identical (stable).

---

## Phase 4: Editor crate + live-rig example app

New `client_animation_editor` crate (lib `AnimationEditorPlugin` + `examples/animation_editor.rs`) that boots `DefaultPlugins + AppStatePlugin + RenderPlugin + SpriteRigPlugin + editor plugin`, spawns the humanoid rig, reaches `AppState::Ready`, and renders the animating character full-window. No editing UI yet.

**Files**: new `crates/client_animation_editor/` (`Cargo.toml`, `src/lib.rs`, `examples/animation_editor.rs`); root `Cargo.toml` `members`; `.cargo/config.toml` alias.
**Key changes**:
- `pub struct AnimationEditorPlugin;` — spawns editor rig (velocity component, no physics/networking), satisfies `AppState` gates (set `RelayPoolReady`/`IdentityLoadComplete` true directly).
- Alias `anim-editor = "run -p client_animation_editor --example animation_editor"`.
- Pin which `SpriteRigPlugin` spawn/animation systems run vs. facing/billboard/ability gating (open risk — resolve here).

**Verify**: `cargo anim-editor` boots, loads rig+animset, reaches `Ready`, character visible playing idle (static camera framing the rig; camera controls out of scope — orbit input lives in client input systems, excluded here).

---

## Phase 5: Editor state + playback transport

Introduce the in-memory working model (one `SpriteAnimAsset` + working animset) and a minimal egui panel: clip selector + play/pause/seek that drives the editor rig's `AnimationPlayer`. Establishes the single mutation source all later views read.

**Files**: `crates/client_animation_editor/src/state.rs` (new), `src/panels/transport.rs` (new), `src/lib.rs`.
**Key changes**:
- `#[derive(Resource)] struct EditorState { working: SpriteAnimAsset, working_set: SpriteAnimSetAsset, selected_clip: ClipSlot, playhead: f32, playback: Playback }` — new; `enum Playback { Playing, Paused }`.
- `fn t_to_x(&self, t: f32, track: Rect) -> f32` / `x_to_t` — shared time-axis mapping owned by state.
- `fn drive_player_from_playhead(...)` — seeks the real `AnimationPlayer`; egui gate via `EguiWantsInput` (per research Q11).

**Verify**: `cargo anim-editor` — select a clip, play/pause, scrub slider moves the rig pose; `EguiWantsInput` gating in place (exercised for real by viewport interactions in Phase 9).

---

## Phase 6: Three-view time editor (ruler + dope sheet, shared axis)

Custom `Painter`-drawn ruler and dope sheet over one `t→x` mapping; one playhead landing at the same `x` in every view. Read-only view of `bone_timelines` (bone-row groups, channel sub-tracks, diamonds at `time/duration`); scrub from ruler or any lane seeks the player. Selection is independent state.

**Files**: `src/panels/timeline.rs` (new), `src/panels/paint.rs` (new helpers), `state.rs`.
**Key changes**:
- `fn draw_ruler(ui, painter, state)` / `fn draw_dope_sheet(ui, painter, state)` — equal left gutters, fractional positions.
- `enum Selection { None, Key { bone, channel, idx }, Event(usize) }` on `EditorState`.
- `fn hit_test_diamond(pos, state) -> Option<Selection>` — click selects (not scrub).

**Verify**: `cargo anim-editor` — diamonds at correct fractions; clicking ruler scrubs and moves playhead identically across ruler+dope sheet; clicking a diamond selects without scrubbing.

---

## Phase 7: Curve editor + CPU display evaluator

Curve-plot view for the selected channel (scalar→one curve, vector→per-component polylines, `Step`=hold), plus a display-only CPU evaluator for value-at-playhead readout and loop-continuity validation. Never poses the rig.

**Files**: `src/panels/curve.rs` (new), `src/eval.rs` (new), `src/panels/inspector.rs` (new).
**Key changes**:
- `fn sample_channel(keys: &[K], t: f32) -> f32|Vec2` — per-segment, honors `CurveType` (same rules as Phase 1).
- `fn draw_curve_editor(ui, painter, state)` — green=X/blue=Y legend, axis labels match plotted range, playhead shares `x`.
- `fn loop_continuity_warning(asset) -> Option<String>` — flags `looping` clip with `t=0 ≠ t=duration`.

**Verify**: `cargo anim-editor` — `Step` segments render flat; inspector value matches pose at playhead; a looping clip with mismatched endpoints surfaces a warning.

---

## Phase 8: Keyframe editing (drag retime/revalue, auto-key)

Drag diamonds to retime (x) or revalue (y), with time-sort, `[0,duration]` clamp, deterministic coincident-time resolution, and an auto-key toggle. Each edit mutates the working `SpriteAnimAsset` and rebuilds the clip in place → live preview updates.

**Files**: `state.rs`, `src/panels/timeline.rs`, `src/panels/curve.rs`, `src/edit.rs` (new).
**Key changes**:
- `fn apply_key_edit(state, sel: Selection, new_time: f32, new_value: Value)` — clamps, re-sorts, resolves coincident times (dragged key takes slot).
- `fn rebuild_working_clip(...)` — re-runs `build_clip_pair` + `clips.insert(existing_id, ..)` for the working clip.
- `#[derive(Resource)] struct AutoKey(bool)`.

**Verify**: `cargo anim-editor` — drag a key in dope sheet/curve view; pose updates live; dragging across a neighbor re-sorts; clamp holds at bounds.

---

## Phase 9: Bone gizmo overlay + screen-drag editing

Bone gizmos overlaid on the render target; dragging a bone converts screen delta → bone-local value (inverting baked default xy + `z_order`, deg↔rad), mutating the keyframe per the auto-key toggle. Clip rebuilds in place each drag frame.

**Files**: `src/panels/gizmo.rs` (new), `edit.rs`.
**Key changes**:
- `fn draw_bone_gizmos(gizmos, camera, rig, state)` — overlay at bone screen positions.
- `fn screen_delta_to_bone_value(delta, bone, channel, camera) -> Value` — inverts research Q2/Q3 bakes.

**Verify**: `cargo anim-editor` — drag a bone in the viewport; the corresponding channel key mutates and pose follows; values match the inverse of the documented bakes.

---

## Phase 10: Audition modes (solo / layered)

Locomotion speed slider sets the editor rig's velocity value so the **real** `update_locomotion_blend_weights` + `recompute_layer_masks` drive idle/walk/run blend under the edited clip; the edited ability clip loops on its layer (editor-only forced `.repeat()`/re-trigger, kept out of the gameplay path).

**Files**: `src/panels/audition.rs` (new), `src/lib.rs`.
**Key changes**:
- `fn set_audition_velocity(rig, speed: f32)` — writes the velocity component the real blend systems read.
- `fn force_loop_active_ability(player, ...)` — editor-only loop/re-trigger helper (clearly named, not in gameplay).

**Verify**: `cargo anim-editor` — speed=0 shows solo (idle base); raising speed blends walk/run under the looping edited clip, matching in-game behavior.

---

## Phase 11: Event lane + playback-gated audio

Bottom event lane (drag retimes, click renames) plus an audio observer on `AnimationEventFired` that plays only while `playback == Playing` (scrub-induced fires are no-ops). Verify Bevy wrap semantics with one targeted test.

**Files**: `src/panels/events.rs` (new), `src/audio.rs` (new), `crates/sprite_rig/` (one wrap test).
**Key changes**:
- `fn draw_event_lane(ui, painter, state)` — bone-independent lane at shared `x`.
- `fn on_event_play_audio(trigger: On<AnimationEventFired>, playback: Res<...>)` — gated observer.

**Verify**: `cargo anim-editor` — events audible on play, silent while scrubbing; `cargo test-all` wrap test asserts no double-fire / no-miss at loop boundary.

---

## Phase 12: Save to disk (clip + animset, new-clip assignment)

Write canonical RON (Phase 3) for the edited clip; assigning a new clip to a slot (locomotion/ability/hit_react) updates the working animset, triggers graph rebuild (Phase 2), and saving writes both the `.anim.ron` and `.animset.ron`. One-time reformat of existing files on first save; stable thereafter. Disk I/O goes through `bevy-persistence` `Store` impls (the workspace pattern), not bespoke write helpers — note the shipped `FileSystemStore` is JSON-only with flat keys, so these are custom impls calling the Phase 3 serializers with tmp+rename writes.

**Files**: `src/store.rs` (new), `src/panels/save.rs` (new), `src/state.rs`; editor `Cargo.toml` gains `persistence = { workspace = true }`.
**Key changes**:
- `struct FsAnimClipStore { asset_root: PathBuf }` — `impl Store<ClipPath, SpriteAnimAsset>`; `save` = `serialize_anim` + tmp+rename, `load` for round-trip checks.
- `struct FsAnimSetStore { asset_root: PathBuf }` — `impl Store<(), SpriteAnimSetAsset>` via `serialize_animset`.
- `fn save_clip(state, ops: &mut PendingStoreOps<...>)` / `fn save_animset(...)` — saves via the Store ops machinery; errors surface as `PersistenceError`, never silently dropped.
- `fn assign_new_clip(state, slot: ClipSlot)` — mutates working animset → rebuild graph.

**Verify**: `cargo anim-editor` — load→no-op save yields stable canonical RON (matches Phase 3 round-trip); assigning + saving a new clip persists to both files and the live preview places it correctly.

---

## Testing Checkpoints

- **After 1**: `cargo test-all` green; `Step`/`Linear` curves correct; all 7 game clips visually unchanged.
- **After 2**: graph rebuilds in place mid-playback/mid-loop/with-ability with no restart and no panic.
- **After 3**: every clip + the animset round-trips structurally and re-serializes byte-stable.
- **After 4**: `cargo anim-editor` boots to `Ready` with an animating rig.
- **After 5**: clip select + transport drive the live pose.
- **After 6**: one playhead at a shared `x` across ruler+dope sheet; click-to-select vs scrub separated.
- **After 7**: curve plot honors `CurveType`; value-at-playhead + loop-continuity warning correct.
- **After 8**: keyframe drags mutate the working asset and update the live preview.
- **After 9**: bone-drag editing maps screen delta → correct bone-local value.
- **After 10**: layered audition blend matches in-game.
- **After 11**: events audible on play, silent on scrub, correct at loop wrap.
- **After 12**: load→save stable via the `Store` impls; new-clip assignment persists to both RON files.

## Notes on slicing

- Phases 1–3 are not UI slices but are genuine vertical units (schema→runtime/serializer→test) and independently valuable to the game; the design explicitly frames them as shared runtime capabilities, so they precede the editor rather than being editor-internal.
- The riskiest mechanism (graph rebuild, design open-risk #1) is isolated in Phase 2 and verified before any editor code depends on it.
- egui `Painter` is new to the repo (no precedent); Phase 6 establishes the coordinate/interaction pattern that Phases 7–11 reuse.
