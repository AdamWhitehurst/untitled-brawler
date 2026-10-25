# Design Discussion

## Current State

- **Schema** (`crates/sprite_rig/src/asset.rs:72-145`): `SpriteAnimAsset { name, duration, looping, bone_timelines: HashMap<String, BoneTimeline>, events }`. `BoneTimeline { blend_mode, rotation, translation, scale }`; keyframes carry `time`, `value` (rotation `f32°`, translation/scale `Vec2`), `curve: CurveType`. `CurveType` is **only** `{ Linear, Step }` (`asset.rs:133-138`). `SpriteAnimSetAsset { rig, locomotion, ability_animations, hit_react }` (`asset.rs:148-154`).
- **Serialization is one-directional.** Deserialized by `RonAssetPlugin` (`lib.rs:26-30`); **no** serialize-back path (research Q1). `bone_timelines` is a `HashMap` → nondeterministic iteration.
- **Runtime ignores `CurveType` and `looping`** (research Q2): clips become `UnevenSampleAutoCurve`s (linear/slerp default); `CurveType` never read; looping is `.repeat()` at play time.
- **All pose evaluation is Bevy's `AnimationPlayer` + `AnimationGraph`** (research Q3). Per animset: `master_add → base_pose_blend → locomotion_blend` with per-bone mask groups (`animation.rs:630-718`); ability clips on masked layers; weights from velocity via `update_locomotion_blend_weights`/`compute_blend_weights` (`animation.rs:866-948`); runtime masks via `recompute_layer_masks` (`animset.rs:122-147`). Default xy + `z_order` baked into curve values (`animation.rs:494-499`).
- **Hot-reload is partial** (research Q6): clip contents rebuild in place at the same `AssetId` (`animation.rs:246-269`, preserving handle identity), but **graphs/masks never rebuild** — `build_anim_graphs` short-circuits (`animation.rs:550-551`); a `blend_mode` partition change only `warn!`s "requires app restart". No `Modified` handling for animset/rig assets.
- **Events** (research Q5): per-keyframe callbacks on the override clip (`animset.rs:334-345`), gated by weight; the only reactor is a `trace!` observer (`animset.rs:348`).
- **Ability playback** plays once, no `.repeat()`, cleaned up on `is_finished()` (`animset.rs:223,244,290-292`).
- **egui** (research Q7): `bevy_egui 0.39.1`, panels in `EguiPrimaryContextPass`; **only standard widgets — no `Painter` drawing anywhere**.
- **Render** (research Q10): one window-targeted `Camera3d` + `CameraOrbitState`; no render-to-texture, no `RenderLayers`.
- **Crate conventions** (research Q8/Q9): add to root `members`; a standalone app reusing `SpriteRigPlugin` must also supply `AppStatePlugin` (for `TrackedAssets`) and `RenderPlugin`'s materials.

## Desired End State

A modular `client_animation_editor` crate (lib-plugin + standalone example app) whose preview **is the real runtime**: the editor rig is a genuine animated character driven by the full animset `AnimationGraph`. To make that viable, `sprite_rig` gains two runtime capabilities the game also needs: per-keyframe `CurveType` honored at playback, and first-class graph/mask rebuild (no more "restart required"). The editor edits clips and their animset placement, auditions them solo or layered over looping locomotion, and writes canonical RON back to disk.

**Verification:**
- `cargo run -p client_animation_editor --example animation_editor` (new alias) boots, loads the humanoid rig + animset, reaches `AppState::Ready`.
- In-game regression: all 7 existing clips (pure `Linear`) play unchanged under the new `CurveType`-honoring curves (`cargo client` visual check + `cargo test-all`).
- Editing a key, a `blend_mode`, or adding a new clip updates the live preview without restart (graph rebuilds in place).
- Layered audition: with locomotion speed set (e.g. walk) and the edited clip looping on its layer, the blend matches in-game behavior.
- Scrubbing any view moves the one playhead to the same `x` in every view and updates the pose; diamond and curve node share `x`.
- Load→save of an unedited clip yields deterministic, stable canonical RON; saving a new clip also updates the `.animset.ron`.
- A `looping` clip with `t=0` ≠ `t=duration` values surfaces a warning; events are audible on playback, silent while scrubbing, and loop wrap neither double-fires nor misses.

## Patterns to Follow

- **In-place asset rebuild preserving handle identity**: the clip hot-reload pattern (`animation.rs:246-269`, `clips.insert(existing_id, new)`) — extend the same idiom to `Assets<AnimationGraph>`.
- **Real weight/layer systems, unmodified**: drive locomotion by setting a velocity value on the editor rig so `update_locomotion_blend_weights` (`animation.rs:866-910`) and `recompute_layer_masks` (`animset.rs:122-147`) run authentically; do not fork the blend logic.
- **egui context & panels**: `let Ok(ctx) = contexts.ctx_mut() else { trace!(...); return };` in `EguiPrimaryContextPass` (`dev/src/lib.rs:99-116`).
- **egui-vs-world input gating**: consume pre-aggregated `EguiWantsInput` → ownership snapshot (`client/src/input/ownership.rs:82-97`), never `ctx().wants_pointer_input()` directly.
- **RON serialize + atomic write**: `ron::ser::to_string_pretty` + atomic write (`server/src/persistence/mod.rs:756`).
- **Standalone example `App`**: `voxel_map_engine/examples/multi_instance.rs`; alias mirrors `client-tracy`.

**Do NOT follow / avoid:**
- The `build_anim_graphs` build-once short-circuit (`animation.rs:550-551`) and the restart-required `warn!` (`animation.rs:298-300` region) — these are the gap this task closes.
- `HashMap` iteration order for serialization — nondeterministic (research Q1).
- `UnevenSampleAutoCurve`'s implicit interpolation as the long-term curve representation — it discards `CurveType`.

## Design Decisions

1. **Preview = real runtime graph** (revised from CPU sampler): the editor rig is a full animated character (`SpriteRigPlugin` spawn + `attach_animation_players` + the real animset graph), minus physics/networking. Guarantees editor↔game fidelity by construction, including additive/override masking and cross-clip blending. Edits flow asset-mutation → in-place clip/graph rebuild → Bevy evaluates.

2. **`sprite_rig` runtime change A — honor `CurveType`**: replace `UnevenSampleAutoCurve` construction with a curve that interpolates per-segment (`Linear` lerp/slerp, `Step` hold-left). All 7 existing clips are pure `Linear`, so behavior is preserved; regression-verify. This makes the curve editor truthful for preview *and* game.

3. **`sprite_rig` runtime change B — first-class graph/mask rebuild**: remove the "requires app restart" warn; on partition/animset change, rebuild the `AnimationGraph` in place at the same `AssetId`, atomically replace `BuiltAnimGraphs` bookkeeping and per-bone mask groups, and reset/restart active playback (stale `AnimationNodeIndex`es on players/`ActiveAnimLayers` are invalidated by rebuild). Benefits in-game hot-reload, not just the editor.

4. **Animset integration & new clips**: the editor holds an in-memory working `SpriteAnimSetAsset`; a new clip is assigned to a slot (locomotion entry / ability / hit_react), which determines its graph position and layering; assignment triggers graph rebuild. Saving a new clip writes the `.anim.ron` **and** the updated `.animset.ron`.

5. **Audition modes**: (a) *solo* — locomotion speed 0 (idle base), edited clip playing on its layer; (b) *layered* — locomotion speed slider sets the editor rig's velocity value so real blend weights drive idle/walk/run under the edited clip, which loops on its layer (editor forces `.repeat()` or re-triggers on finish, since ability clips normally play once, `animset.rs:290-292`).

6. **Small CPU evaluator for display only**: inspector value-at-playhead, curve-plot rendering, and loop-continuity validation evaluate keyframes directly (honoring `CurveType`, same per-segment rules as decision 2). It never poses the rig — display reader, not a second source of truth.

7. **Events — playback-gated audio observer**: the editor registers an observer on `AnimationEventFired` that plays audio only while `playback == Playing`; scrub-induced fires are no-ops. Wrap semantics delegate to Bevy's evaluator — the same machinery the game's footsteps use (verify wrap behavior with one targeted test; research flagged it unread).

8. **Viewport — docked egui panels + gizmo overlay** (unchanged): the single existing `Camera3d` renders the rig full-window; time views/inspector/hierarchy dock as panels; bone gizmos overlay the render target. Bone drag converts screen delta → bone-local value (inverting the documented bakes: default xy + `z_order`, deg↔rad), mutates the keyframe per the auto-key toggle, and the clip rebuilds in place (cheap at these clip sizes; per-frame during drag is acceptable).

9. **Serialization — canonical, deterministic** (unchanged, scope extended): `to_string_pretty` + tuned `PrettyConfig` + atomic write; `bone_timelines` in rig bone-index order; default `blend_mode` skipped; covers `SpriteAnimAsset` and `SpriteAnimSetAsset`. One-time reformat of existing files on first save; stable thereafter.

10. **Crate composition** (unchanged): `client_animation_editor` lib exposing `AnimationEditorPlugin`; example app boots `DefaultPlugins` + `AppStatePlugin` + `RenderPlugin` + `SpriteRigPlugin` + editor plugin — not `ClientPlugins`. Alias `anim-editor = "run -p client_animation_editor --example animation_editor"`.

## Three-View Model (shared time axis)

- **One `t→x` mapping** owned by editor state: equal left gutters; all positions fractions of track width, never hardcoded pixels. One playhead value; its line lands at the same `x` in ruler, dope sheet, curve plot, and event lane. Playhead drives the player's seek; selection is independent state.
- **Dope sheet**: direct view of `bone_timelines`; bone row groups, non-empty channel sub-tracks, diamond `x = time/duration`; empty channels behind "+ add channel".
- **Curve editor**: selected channel only; scalar → one curve, vector → one polyline per component (green=X, blue=Y, legended); vector keys stack component nodes at one `t` under a single diamond; segment shape reflects that key's `curve` (`Step` = hold); axis labels match plotted range.
- **Event lane**: bone-independent bottom lane; drag retimes, click renames, audible on playback.
- Clicking a diamond/node selects (not scrubs); dragging retimes (x) or revalues (y); scrub from ruler, any lane, or curve plot via the same mapping.

## Data-Integrity Model

- UI is a view over one in-memory `SpriteAnimAsset` (+ working animset); every edit mutates it, all views re-read, and the runtime clip/graph rebuilds in place.
- Keys time-sorted, clamped to `[0, duration]`; a drag crossing a neighbor re-sorts; coincident times resolved deterministically (dragged key takes the slot; define exact rule in plan).
- Channel types respected; empty channels render empty, serialize as `[]`.
- Round-trip target: load → no-op save yields equivalent canonical RON, nothing invented or dropped.

## What We're NOT Doing

- No Bezier or new `CurveType` variants — `Linear`/`Step` only (runtime now honors them; Bezier is schema+UI future work).
- No rig editing (`.rig.ron` bones/slots/skins) — clips and their animset placement only.
- No 2-bone IK ("later" per task).
- No render-to-texture / second camera / `RenderLayers`.
- No undo/redo in v1 (revisit once the mutation model lands).
- No networking/nostr (`ClientPlugins`) in the editor app.
- No locomotion-config tuning UI (speeds/entries) beyond the audition speed slider.

## Open Risks

- **Graph rebuild correctness** (decision 3) is the riskiest new mechanism: stale `AnimationNodeIndex`es on `AnimationPlayer`s and `ActiveAnimLayers` after an in-place graph swap must be reset without leaking playback state; `recompute_layer_masks` should self-heal masks once layers are reset. Needs focused tests (rebuild mid-playback, mid-loop, with active ability layer).
- **`CurveType` runtime change touches live gameplay** (decision 2): all clips are `Linear` today so behavior should be identical, but the curve implementation swap needs regression verification (visual + `cargo test-all`).
- **Looped ability audition deviates from game semantics** (decision 5): in-game ability clips play once; the editor's forced loop/re-trigger is editor-only behavior — keep it in the editor (or a clearly-named `sprite_rig` helper), not in the gameplay path.
- **Editor rig vs. gameplay systems**: the editor rig needs the animation chain but not facing/billboarding fights or ability gating; which `SpriteRigPlugin`/`protocol` systems run against it (velocity component but no physics) must be pinned down during structure/plan.
- **Animset serialization fidelity**: `SpriteAnimSetAsset` lives in `implicit_some`-headered files; the canonical serializer emits explicit `Some(..)` (valid under the extension) or drops the header — pick one rule and round-trip-test it.
- **egui `Painter` is new to this repo**: all time views are custom-drawn; no in-repo precedent for coordinate/interaction patterns.
- **Bevy event wrap semantics unread** (research Open Areas): one targeted test for no-double-fire/no-miss at loop boundary.
