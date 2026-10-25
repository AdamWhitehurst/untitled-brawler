# Implementation Plan

## Overview

Build a `client_animation_editor` crate whose preview **is** the real `sprite_rig` runtime (full animset `AnimationGraph`, minus physics/networking), edits clips + their animset placement over a shared `t→x` time axis, and writes canonical RON back to disk. Two `sprite_rig` runtime capabilities land first because the live editor depends on them and the running game benefits too: honor `CurveType` at playback (Phase 1) and first-class graph/mask rebuild (Phase 2).

Phases 1–2 are `sprite_rig` runtime foundations. Phases 3–10 build the live editor. Phase 11 adds the canonical RON serializer as a pure, tested unit; Phase 12 writes it to disk — serialization lands adjacent to its only consumer (the game never serializes clips, so it has no earlier use). Each later phase degrades gracefully if a subsequent one is dropped.

**Global rule (CLAUDE.md):** never run `build`/`check`/`test` in parallel. Each phase's automated verification is a single serialized `cargo test-all` (and, where noted, a manual `cargo client` / `cargo anim-editor` run).

---

## Phase 1: Honor `CurveType` at runtime

### Changes

#### 1. Per-segment curve builder

**File**: `crates/sprite_rig/src/curve.rs`
**Action**: create

Replace `UnevenSampleAutoCurve`'s implicit interpolation with a curve that interpolates each segment by the **left** key's `CurveType` (`Linear` → `StableInterpolate::interpolate_stable` i.e. lerp for `Vec3`, slerp for `Quat`; `Step` → hold left value until the right key's time). `UnevenSampleAutoCurve` is `bevy::math::curve::sample_curves::UnevenSampleAutoCurve` (imported `animation.rs:6`).

```rust
use bevy::math::{StableInterpolate, curve::{Curve, Interval}};
use crate::asset::CurveType;

/// A curve over unevenly-spaced keyframes where each segment interpolates according to the
/// **left** key's `CurveType`: `Linear` blends left→right via `StableInterpolate`, `Step`
/// holds the left value until the right key's time. Keys must be sorted ascending by time
/// and contain at least two entries.
#[derive(Clone)]
pub struct SegmentedKeyframeCurve<T> {
    keys: Vec<(f32, T, CurveType)>,
    domain: Interval,
}

impl<T: StableInterpolate + Clone> SegmentedKeyframeCurve<T> {
    /// Builds the curve, returning `None` if fewer than two keys are supplied.
    pub fn new(keys: Vec<(f32, T, CurveType)>) -> Option<Self> {
        if keys.len() < 2 {
            return None;
        }
        let domain = Interval::new(keys[0].0, keys[keys.len() - 1].0).ok()?;
        Some(Self { keys, domain })
    }
}

impl<T: StableInterpolate + Clone> Curve<T> for SegmentedKeyframeCurve<T> {
    fn domain(&self) -> Interval {
        self.domain
    }

    fn sample_unchecked(&self, t: f32) -> T {
        // Clamp below first / above last.
        if t <= self.keys[0].0 {
            return self.keys[0].1.clone();
        }
        let last = self.keys.len() - 1;
        if t >= self.keys[last].0 {
            return self.keys[last].1.clone();
        }
        // Find the segment [i, i+1] containing t.
        let i = self
            .keys
            .partition_point(|(time, _, _)| *time <= t)
            .saturating_sub(1);
        let (t0, ref v0, curve) = self.keys[i];
        let (t1, ref v1, _) = self.keys[i + 1];
        match curve {
            CurveType::Step => v0.clone(),
            CurveType::Linear => {
                let s = ((t - t0) / (t1 - t0)).clamp(0.0, 1.0);
                v0.clone().interpolate_stable(v1, s)
            }
        }
    }
}
```

> Note: verify `Curve`/`Interval` import paths against Bevy 0.18 during implementation (`git/bevy`); the trait requires `domain()` + `sample_unchecked()`. If `Interval::new` signature differs, adjust. `StableInterpolate::interpolate_stable(self, other, t)` is the method `UnevenSampleAutoCurve` already uses internally for `Vec3`/`Quat`.

#### 2. Thread `CurveType` into the three curve builders

**File**: `crates/sprite_rig/src/animation.rs`
**Action**: modify `add_rotation_curve` (`:456-482`), `add_translation_curve` (`:485-514`), `add_scale_curve` (`:517-536`)

Replace the `UnevenSampleAutoCurve::new(...)` calls **in the real-keyframe branches** with `SegmentedKeyframeCurve::new(...)`, threading each key's `curve`. The hold-filler branches (`< 2` keys) stay `UnevenSampleAutoCurve` (single value, no `CurveType` to honor).

Rotation (`:462-473`):
```rust
if timeline.rotation.len() >= 2 {
    let keys = timeline
        .rotation
        .iter()
        .map(|k| (k.time, Quat::from_rotation_z(k.value.to_radians()), k.curve))
        .collect();
    let curve = SegmentedKeyframeCurve::new(keys)
        .expect("Rotation timeline needs >= 2 keyframes");
    clip.add_curve_to_target(
        target_id,
        AnimatableCurve::new(animated_field!(Transform::rotation), curve),
    );
} else { /* unchanged hold-at-identity */ }
```

Translation (`:493-504`) — keep the default-xy + `z_order` bake inside the mapped value:
```rust
let keys = timeline.translation.iter().map(|k| {
    (k.time, Vec3::new(default_xy.x + k.value.x, default_xy.y + k.value.y, z_order), k.curve)
}).collect();
let curve = SegmentedKeyframeCurve::new(keys).expect("Translation timeline needs >= 2 keyframes");
```

Scale (`:525-531`) — z forced to 1.0:
```rust
let keys = timeline.scale.iter()
    .map(|k| (k.time, Vec3::new(k.value.x, k.value.y, 1.0), k.curve)).collect();
let curve = SegmentedKeyframeCurve::new(keys).expect("Scale timeline needs >= 2 keyframes");
```

The additive builders (`animation.rs:387-429`) construct rotation/translation curves the same way — apply the identical `SegmentedKeyframeCurve` swap there, threading `k.curve`. (Additive scale is dropped with a `warn!`, `:374-380`; leave it.)

`CurveType` derives `Clone, Copy` — confirm `Copy` is present (`asset.rs:133`); it derives `Clone, Debug, Serialize, Deserialize, Default` today. **Add `Copy`** to the derive so `k.curve` copies cleanly:
```rust
// asset.rs:133
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
pub enum CurveType { #[default] Linear, Step }
```

#### 3. Module registration

**File**: `crates/sprite_rig/src/lib.rs`
**Action**: modify — add `pub mod curve;` beside the existing module declarations (`:1-5`).

### Verification
#### Automated
- [x] `cargo test-native` passes (sole failure was pre-existing flaky `voxel_map_engine::api::set_voxel_triggers_remesh`, passes in isolation, unrelated to this change).
- [x] New unit test in `crates/sprite_rig/src/curve.rs` (`#[cfg(test)]`): a `SegmentedKeyframeCurve<f32>` (use `f32`, which is `StableInterpolate`) with keys `[(0.0, 0.0, Step), (1.0, 10.0, Linear), (2.0, 20.0, Linear)]` — assert `sample(0.5) == 0.0` (Step holds left), `sample(1.5)` ≈ `15.0` (Linear midpoint), `sample(-1.0) == 0.0` and `sample(3.0) == 20.0` (clamps). Use `approx::assert_relative_eq!`.

#### Manual
- [x] `cargo client` — all 7 humanoid clips (idle/walk/run/punch/punch2/ground_pound/jump) play visually unchanged (all are pure `Linear`, so segmented-Linear == old auto-curve).

---

## Phase 2: First-class graph/mask rebuild in place

### Changes

#### 1. One system, one build path: first-build *and* in-place rebuild

**File**: `crates/sprite_rig/src/animation.rs`
**Action**: replace `build_anim_graphs` (`:538-578`) with a single system that handles both first-build and rebuild, plus a shared store helper.

The existing `build_graph_for_animset` (`:630-718`) is the real build logic and is reused unchanged. First-build and rebuild differ in **only two** ways: (a) `graphs.add(g)` (new handle) vs `graphs.insert(existing_id, g)` (in place, preserving the handle every rig holds); (b) rebuild must reset players so stale `AnimationNodeIndex`es don't leak. Everything else — entry lookup, `all_clips_built` gate, `BuiltAnimGraph` construction — is identical, so it lives in one helper and is never duplicated.

The two *triggers* differ (first-build is a per-frame poll until clips are ready; rebuild is event/queue-driven), so the one system reads both sources and funnels each animset id through the same path:

```rust
/// Builds each animset's `AnimationGraph` once its clips are ready, and rebuilds it in place
/// when the animset (or a clip's blend_mode partition) changes. A single build path: the only
/// branch is new-handle (`graphs.add`) vs in-place (`graphs.insert(existing_id, ..)` — mirrors
/// the clip hot-reload idiom, preserving the `AnimationGraphHandle` every rig already holds).
pub fn build_anim_graphs(
    mut animset_events: MessageReader<AssetEvent<SpriteAnimSetAsset>>,
    mut rebuild_queue: ResMut<GraphRebuildQueue>,
    registry: Res<RigRegistry>,
    animset_assets: Res<Assets<SpriteAnimSetAsset>>,
    built_anims: Res<BuiltAnimations>,
    loaded_handles: Res<LoadedAnimHandles>,
    bone_defaults: Res<AnimBoneDefaults>,
    mut built_graphs: ResMut<BuiltAnimGraphs>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
    mut players: Query<(&mut AnimationPlayer, &mut ActiveAnimLayers, &AnimSetRef)>,
) {
    // Collect the set of animset ids to (re)build this frame, deduped:
    //   - first-build: any registry entry not yet in `built_graphs` (the per-frame poll).
    //   - rebuild: animset `Modified` events + drained `GraphRebuildQueue` (clip-partition flips).
    let mut to_process: HashSet<AssetId<SpriteAnimSetAsset>> = HashSet::new();
    for entry in registry.entries.values() {
        let id = entry.animset_handle.id();
        if !built_graphs.0.contains_key(&id) {
            to_process.insert(id);
        }
    }
    for event in animset_events.read() {
        if let AssetEvent::Modified { id } = event {
            to_process.insert(*id);
        }
    }
    to_process.extend(rebuild_queue.0.drain());

    for animset_id in to_process {
        let Some(entry) = registry.entries.values().find(|e| e.animset_handle.id() == animset_id)
        else {
            trace!(?animset_id, "(re)build requested for unknown animset; ignoring");
            continue;
        };
        let Some(animset) = animset_assets.get(&entry.animset_handle) else {
            continue; // not loaded yet — expected during startup
        };
        if !all_clips_built(animset, &loaded_handles, &built_anims) {
            continue; // clips not all built yet — expected during startup / deferred rebuild
        }

        let built = build_graph_for_animset(animset, &loaded_handles, &built_anims, &bone_defaults);
        match built_graphs.0.get(&animset_id) {
            // Rebuild: swap in place at the existing handle, then reset players.
            Some(existing) => {
                let graph_handle = existing.graph_handle.clone();
                let _ = graphs.insert(graph_handle.id(), built.0);
                store_built_graph(&mut built_graphs, animset_id, graph_handle, built);
                reset_animation_players_for(animset_id, &registry, &mut players);
            }
            // First build: fresh handle, no players to reset yet.
            None => {
                let graph_handle = graphs.add(built.0);
                store_built_graph(&mut built_graphs, animset_id, graph_handle, built);
            }
        }
    }
}

/// Records the `BuiltAnimGraph` bookkeeping from a `build_graph_for_animset` result. Shared by
/// the first-build and in-place-rebuild arms so the field mapping lives in exactly one place.
fn store_built_graph(
    built_graphs: &mut BuiltAnimGraphs,
    animset_id: AssetId<SpriteAnimSetAsset>,
    graph_handle: Handle<AnimationGraph>,
    built: (
        AnimationGraph,
        HashMap<String, AnimationNodeIndex>,
        Vec<LocomotionNodeEntry>,
        AnimationNodeIndex,
        HashMap<String, AbilityNodePair>,
        AnimationMask,
    ),
) {
    built_graphs.0.insert(animset_id, BuiltAnimGraph {
        graph_handle,
        node_map: built.1,
        locomotion_entries: built.2,
        locomotion_blend_node: built.3,
        ability_nodes: built.4,
        all_bones_mask: built.5,
    });
}
```

> `build_graph_for_animset` returns the `(graph, ...)` tuple after the `graphs.add`/`insert` consumes `built.0`; reorder so `store_built_graph` receives the remaining fields (e.g. destructure `built.0` out first, or have `store_built_graph` take the tuple before the graph is moved). Tidy the exact move order during implementation — the point is one bookkeeping site, not two.

```rust
/// Clears active playback and layer state on every rig bound to `animset_id`, so that after an
/// in-place graph swap `start_locomotion_blend` + `recompute_layer_masks` rebuild a clean stack
/// against the new node indices next frame. Ability casts in flight are dropped (acceptable: a
/// graph rebuild is an authoring/hot-reload event, not a gameplay tick).
fn reset_animation_players_for(
    animset_id: AssetId<SpriteAnimSetAsset>,
    registry: &RigRegistry,
    players: &mut Query<(&mut AnimationPlayer, &mut ActiveAnimLayers, &AnimSetRef)>,
) {
    for (mut player, mut layers, anim_set_ref) in players.iter_mut() {
        if anim_set_ref.animset_id() != animset_id {
            continue;
        }
        player.stop_all();
        layers.entries.clear();
    }
}
```

> **Verify during implementation:** (a) `AnimSetRef` (`spawn.rs`, re-exported `lib.rs:18`) exposes the animset `AssetId` — add an accessor if absent. (b) `AnimationPlayer::stop_all()` exists in Bevy 0.18 (else iterate `player.playing_animations()` and `stop`). (c) `BuiltAnimGraph` field names match `:566-575`. (d) `start_locomotion_blend` re-plays locomotion when a player has no active locomotion animation — confirm it keys off "not currently playing" so a cleared player re-blends; if it only runs on `Added`, add a re-trigger condition. (e) `HashSet`/`HashMap` import — `std::collections` or bevy's `HashSet`; match the file's existing usage.

#### 2. Clip-partition change → enqueue a rebuild (replace the restart warn)

**File**: `crates/sprite_rig/src/animation.rs`
**Action**: modify `build_animation_clips` Modified handler (`:260-265`) — replace the restart-required `warn!`.

A clip-only `blend_mode` flip emits a *clip* `AssetEvent::Modified`, not an *animset* one, so the unified system above won't see it through the event reader. Instead of the old `warn!`, enqueue the affected animset(s) onto a queue the unified system drains:

```rust
// New resource (init in lib.rs): animset ids needing a graph rebuild due to a clip-partition
// (blend_mode) change, which produces no SpriteAnimSetAsset Modified event of its own.
#[derive(Resource, Default)]
pub struct GraphRebuildQueue(pub HashSet<AssetId<SpriteAnimSetAsset>>);
```

In `build_animation_clips`, when `override_bones != pair.override_bones || additive_bones != pair.additive_bones`, push every animset id whose clips reference this clip id onto `GraphRebuildQueue`. The unified `build_anim_graphs` already drains it (section 1), so there is **no second rebuild body** — the queue is just another trigger source feeding the one build path.

> To find which animsets reference a given clip id, resolve through `LoadedAnimHandles` (path→handle) and `RigRegistry`/the loaded `SpriteAnimSetAsset`s. `build_animation_clips` doesn't currently have `RigRegistry`/animset access — either add those params, or have it push the *clip* id onto the queue and let `build_anim_graphs` map clip→animset during its drain. Prefer pushing clip ids and mapping in the consumer (keeps `build_animation_clips`'s param list small); adjust `GraphRebuildQueue` to hold clip ids if so, and document the chosen direction.

**File**: `crates/sprite_rig/src/lib.rs`
**Action**: modify — `init_resource::<animation::GraphRebuildQueue>()`; `build_anim_graphs` keeps its existing slot in the `Update` `.chain()` (`:48`) — no new system is added, it just gains the rebuild responsibility.

#### 3. (Conditional) `AnimSetRef` animset-id accessor

**File**: `crates/sprite_rig/src/spawn.rs`
**Action**: modify — add `pub fn animset_id(&self) -> AssetId<SpriteAnimSetAsset>` if not present.

### Verification
#### Automated
- [x] `cargo test-native` passes.
- [x] New tests in `crates/sprite_rig/src/animation.rs` (`#[cfg(test)]`, minimal `App` with `MinimalPlugins` + asset plugins, as existing sprite_rig tests do): (1) **rebuild mid-playback** — build graph, play locomotion, modify animset, run a frame, assert no panic and `BuiltAnimGraphs` entry exists at the same `graph_handle.id()`. (2) **rebuild with active ability layer** — push an ability layer, rebuild, assert `ActiveAnimLayers.entries` cleared and `recompute_layer_masks` runs without indexing a stale node. (3) **partition flip** — flip a bone's `blend_mode` in the source clip, assert `GraphRebuildQueue` receives the clip id (queue holds clip ids; consumer maps clip→animset), plus (4) end-to-end: partition flip rebuilds graph masks in place at the same handle.

> If `MinimalPlugins`-based graph tests prove too heavy (graph build needs loaded clips), assert at the unit level on `store_built_graph` + `reset_animation_players_for` behavior instead, and cover the live path under the manual check. Note any such reduction in the phase summary.

#### Manual
- [x] `cargo client` — edit a `blend_mode` (e.g. in `jump.anim.ron`) while running; pose updates with no restart, no panic, masks correct.

---

## Phase 3: Editor crate + live-rig example app

### Changes

#### 1. New crate scaffold

**File**: `crates/client_animation_editor/Cargo.toml`
**Action**: create
```toml
[package]
name = "client_animation_editor"
version = "0.1.0"
edition = "2021"

[dependencies]
bevy = { workspace = true, features = ["bevy_animation"] }
bevy_egui = { version = "0.39" }
sprite_rig = { path = "../sprite_rig" }
render = { path = "../render" }
protocol = { workspace = true }

[dev-dependencies]
bevy = { workspace = true, default-features = true }
```

> `render`'s `RenderPlugin` registers the rig materials (`SpriteRigMaterial`, `BillboardMaterial`, `ShadowOnlyMaterial`, research Q9/Q10) and pulls in `SpriteRigPlugin` (`render/src/lib.rs:54`). Depending on `render` (not re-adding `SpriteRigPlugin`) avoids double-registration. **Confirm during implementation** whether `RenderPlugin` can be added standalone without `ClientPlugins` (it should — it's a separate plugin); if it transitively requires networking resources, add only the material registrations + `SpriteRigPlugin` directly instead and document why.

#### 2. Workspace + alias registration

**File**: `Cargo.toml` (root)
**Action**: modify — add `"crates/client_animation_editor"` to `members` (`:2-17`).

**File**: `.cargo/config.toml`
**Action**: modify — add alias:
```toml
anim-editor = "run -p client_animation_editor --example animation_editor"
```

#### 3. Editor plugin

**File**: `crates/client_animation_editor/src/lib.rs`
**Action**: create
```rust
use bevy::prelude::*;
use protocol::{CharacterType, app_state::{RelayPoolReady, IdentityLoadComplete}};
use protocol::app_state::AppState;

/// Boots the editor: spawns one humanoid rig (no physics/networking) and satisfies the
/// `AppState::Loading → Ready` gates that the client normally satisfies via nostr/identity.
pub struct AnimationEditorPlugin;

impl Plugin for AnimationEditorPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, (spawn_editor_rig, satisfy_app_state_gates));
        app.add_systems(Update, frame_editor_camera);
    }
}

/// Spawns a humanoid `SpriteRig` plus the velocity component the real locomotion blend systems
/// read (Phase 9 drives it). No avian physics body, no lightyear replication — the editor rig
/// is animation-only.
fn spawn_editor_rig(mut commands: Commands) {
    commands.spawn((
        sprite_rig::SpriteRig { character_type: CharacterType::Humanoid },
        // velocity component read by update_locomotion_blend_weights (see research Q3 — confirm
        // the exact component type the blend reads; likely avian LinearVelocity or a protocol
        // Velocity). Spawn it at zero.
        ...,
        Transform::default(),
        Visibility::default(),
    ));
}

/// The client reaches `Ready` only when `RelayPoolReady` and `IdentityLoadComplete` are true
/// (protocol app_state). The editor has no nostr/identity, so set both directly at startup;
/// `check_assets_loaded` then advances to `Ready` once the rig+animset finish loading.
fn satisfy_app_state_gates(
    mut relay: ResMut<RelayPoolReady>,
    mut identity: ResMut<IdentityLoadComplete>,
) {
    relay.0 = true;
    identity.0 = true;
}
```

> **Pin the open risk (which gameplay systems run against the editor rig):** the editor rig must run the spawn + animation chain (`spawn_sprite_rigs` → `build_*` → `attach_animation_players` → `start_locomotion_blend` → `update_locomotion_blend_weights`) but `update_facing_from_rotation`/`apply_facing_to_rig`/`billboard_joint_roots` consume an avian `Rotation`/camera and would fight a static editor camera. **Decision:** keep the full `SpriteRigPlugin` chain (facing/billboard are harmless on a zero-velocity, forward-facing rig and avoid forking the plugin); if facing/billboard visibly misbehave, gate them out via a `run_if` the editor sets `false`. Resolve empirically in the manual check; document the choice in the phase summary. Do **not** spawn `ActiveAbility`/physics so ability gating and avian systems never engage.

#### 4. Example app

**File**: `crates/client_animation_editor/examples/animation_editor.rs`
**Action**: create
```rust
use bevy::prelude::*;
use protocol::FIXED_TIMESTEP_HZ; // only if needed; editor has no sim tick

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(AssetPlugin {
            file_path: concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets").to_string(),
            ..default()
        }))
        .add_plugins(protocol::SharedGameplayPlugin) // installs AppStatePlugin (TrackedAssets)
        .add_plugins(render::RenderPlugin)           // materials + SpriteRigPlugin
        .add_plugins(bevy_egui::EguiPlugin::default())
        .add_plugins(client_animation_editor::AnimationEditorPlugin)
        .run();
}
```

> **Confirm during implementation:** the exact plugin that installs `AppStatePlugin` (research says `SharedGameplayPlugin`, protocol/src/lib.rs:315) and whether it drags in networking. If `SharedGameplayPlugin` is too heavy, add `AppStatePlugin` directly. The minimal viable set is: `DefaultPlugins` + `AppStatePlugin` + `RenderPlugin` + `EguiPlugin` + `AnimationEditorPlugin`.

### Verification
#### Automated
- [x] `cargo check -p client_animation_editor --all-targets` passes (the crate compiles; example builds).
- [x] `cargo test-native` still green.

#### Manual
- [x] `cargo anim-editor` boots, loads rig + animset, reaches `AppState::Ready`, character visible playing idle full-window under a static editor camera. (Editor resolves `SpriteRig`/`AnimSetRef` itself — `resolve_character_rig` only matches replicated entities — and defers spawn until rig/animset/images are loaded, since `spawn_sprite_rigs` panics on unloaded assets.)

---

## Phase 4: Editor state + playback transport

### Changes

#### 1. Editor state resource

**File**: `crates/client_animation_editor/src/state.rs`
**Action**: create
```rust
use bevy::prelude::*;
use sprite_rig::asset::{SpriteAnimAsset, SpriteAnimSetAsset};

/// Identifies which animset slot the working clip occupies.
#[derive(Clone, Debug)]
pub enum ClipSlot {
    Locomotion(usize),       // index into locomotion.entries
    Ability(String),         // ability id key
    HitReact,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Playback { Playing, Paused }

/// The single in-memory working model every editor view reads and every edit mutates.
#[derive(Resource)]
pub struct EditorState {
    pub working: SpriteAnimAsset,
    pub working_set: SpriteAnimSetAsset,
    pub selected_clip: ClipSlot,
    pub playhead: f32,        // seconds, [0, working.duration]
    pub playback: Playback,
}

impl EditorState {
    /// Maps a clip time `t` (seconds) to an x within `track` using a fraction of track width.
    /// Shared by every time view so one playhead lands at the same x everywhere. Never uses
    /// hardcoded pixel offsets — positions are fractions of track width.
    pub fn t_to_x(&self, t: f32, track: egui::Rect) -> f32 {
        let frac = if self.working.duration > 0.0 { t / self.working.duration } else { 0.0 };
        track.left() + frac * track.width()
    }

    /// Inverse of `t_to_x`, clamped to `[0, duration]`.
    pub fn x_to_t(&self, x: f32, track: egui::Rect) -> f32 {
        let frac = ((x - track.left()) / track.width()).clamp(0.0, 1.0);
        frac * self.working.duration
    }
}
```

> `EditorState` is initialized once the rig's animset + the default-selected clip finish loading. Add a `Startup`-after-`Ready` system (or `OnEnter(AppState::Ready)`) that clones the loaded `SpriteAnimSetAsset` + the default clip (`idle`) from `Assets<_>` into `EditorState`. Use `Option<Res<...>>` is **not** appropriate here per CLAUDE.md — instead insert `EditorState` only once assets are ready (system runs `in_state(Ready)`, inserts the resource on first run, guarded by a `Local<bool>` or `resource_exists` run condition).

#### 2. Transport panel

**File**: `crates/client_animation_editor/src/panels/transport.rs`
**Action**: create
```rust
/// Clip selector + play/pause/seek. Drives the real editor-rig `AnimationPlayer`.
pub fn draw_transport(
    mut contexts: bevy_egui::EguiContexts,
    mut state: ResMut<EditorState>,
    // query the editor rig's AnimationPlayer to seek it
) {
    let Ok(ctx) = contexts.ctx_mut() else {
        trace!("EguiContexts not ready, skipping transport frame");
        return;
    };
    egui::TopBottomPanel::bottom("transport").show(ctx, |ui| {
        // clip ComboBox (locomotion entries / abilities / hit_react) → state.selected_clip
        // Play/Pause toggle → state.playback
        // Slider 0..=duration → state.playhead
    });
}
```

#### 3. Drive the player from the playhead

**File**: `crates/client_animation_editor/src/state.rs` (or a dedicated system)
**Action**: create `drive_player_from_playhead`
```rust
/// Seeks the editor rig's `AnimationPlayer` to `state.playhead` for the selected clip's node.
/// When `Playing`, advances the playhead by `time.delta_secs()` (wrapping on loop); when
/// `Paused`, holds. Resolve the active `AnimationNodeIndex` from `BuiltAnimGraphs` for the
/// selected clip slot. egui input gating via `EguiWantsInput` (see Phase 8) — not yet needed
/// since no viewport interaction exists in this phase, but wire the resource read now.
pub fn drive_player_from_playhead(...) { ... }
```

**File**: `crates/client_animation_editor/src/lib.rs`
**Action**: modify — add `state`, `panels::transport` modules; register `draw_transport` in `bevy_egui::EguiPrimaryContextPass`; register `drive_player_from_playhead` in `Update` `in_state(Ready)`; insert/init `EditorState`.

### Verification
#### Automated
- [x] `cargo check -p client_animation_editor` passes.
- [x] `cargo test-native` green.

#### Manual
- [x] `cargo anim-editor` — select a clip from the dropdown; play/pause toggles motion; dragging the seek slider scrubs the rig pose. (Fixes along the way: previous slot's nodes released on selection change; transport always `set_seek_time` clamped below duration — bevy's speed-0 `seek_time %= duration` wrap inverts the seek pair and panics the event-trigger system; editor never fires clip events, Phase 10 does audio editor-side.)

---

## Phase 5: Three-view time editor (ruler + dope sheet, shared axis)

### Changes

#### 1. Paint helpers

**File**: `crates/client_animation_editor/src/panels/paint.rs`
**Action**: create
```rust
/// Draws a diamond (rotated square) centered at `pos` — the keyframe glyph used in every lane.
pub fn draw_diamond(painter: &egui::Painter, pos: egui::Pos2, size: f32, color: egui::Color32, selected: bool) { ... }

/// Draws the shared playhead vertical line at `x` across a lane's rect.
pub fn draw_playhead(painter: &egui::Painter, x: f32, rect: egui::Rect) { ... }

/// Equal left gutter width shared by ruler/dope sheet/curve/event lanes so the `t→x` track
/// rect aligns across all views.
pub const LANE_GUTTER: f32 = 96.0;

/// Computes the track `Rect` (gutter-excluded drawable area) from a lane's full `Rect`.
pub fn track_rect(lane: egui::Rect) -> egui::Rect { ... }
```

#### 2. Selection state

**File**: `crates/client_animation_editor/src/state.rs`
**Action**: modify — add to `EditorState`:
```rust
#[derive(Clone, Debug, PartialEq)]
pub enum Selection {
    None,
    Key { bone: String, channel: Channel, idx: usize },
    Event(usize),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel { Rotation, Translation, Scale }
// EditorState.selection: Selection
```

#### 3. Timeline panel (ruler + dope sheet)

**File**: `crates/client_animation_editor/src/panels/timeline.rs`
**Action**: create
```rust
/// Time ruler: tick marks + the shared playhead. Clicking/dragging in the ruler scrubs
/// (seeks the playhead via `state.x_to_t`).
fn draw_ruler(ui: &mut egui::Ui, state: &mut EditorState) { ... }

/// Dope sheet: read-only view of `working.bone_timelines` in bone-index order. One row group
/// per bone; non-empty channel sub-tracks (rotation/translation/scale); a diamond per key at
/// `x = t_to_x(key.time)`. The shared playhead overlays at the same x as the ruler.
/// Clicking a diamond SELECTS (hit_test_diamond) — does NOT scrub. Clicking empty lane area
/// scrubs.
fn draw_dope_sheet(ui: &mut egui::Ui, state: &mut EditorState) { ... }

/// Returns the `Selection` for a diamond near `pos`, or `None`. Tolerance in pixels.
fn hit_test_diamond(pos: egui::Pos2, state: &EditorState, lanes: &[(...)]) -> Option<Selection> { ... }

pub fn draw_timeline(mut contexts: EguiContexts, mut state: ResMut<EditorState>) {
    let Ok(ctx) = contexts.ctx_mut() else { trace!("egui not ready"); return };
    egui::CentralPanel... or TopBottomPanel::bottom — dock ruler above dope sheet, both using
    track_rect() so they share the gutter and the t→x mapping.
}
```

> Both ruler and dope sheet must derive their track `Rect` from `track_rect(lane_rect)` with the same `LANE_GUTTER`, so `t_to_x` yields the same x in each. The playhead is one `state.playhead` value drawn in both.

**File**: `crates/client_animation_editor/src/lib.rs`
**Action**: modify — add `panels::paint`, `panels::timeline`; register `draw_timeline`.

### Verification
#### Automated
- [x] `cargo check -p client_animation_editor` passes.

#### Manual
- [x] `cargo anim-editor` — diamonds sit at correct fractional positions (`time/duration`); clicking the ruler scrubs and the playhead moves to the identical x in ruler and dope sheet; clicking a diamond selects it (highlight) without moving the playhead. (Fixes: content-fitted initial panel height; 3D camera viewport synced to egui's `available_rect`, with a dedicated full-window egui camera so the UI canvas doesn't track the shrinking 3D viewport.)

---

## Phase 6: Curve editor + CPU display evaluator

### Changes

#### 1. Display-only CPU evaluator

**File**: `crates/client_animation_editor/src/eval.rs`
**Action**: create — mirrors Phase 1's per-segment rules; **never poses the rig** (display reader only).
```rust
use sprite_rig::asset::CurveType;

/// Samples a scalar channel (rotation degrees) at clip time `t`, honoring per-segment
/// `CurveType` (same Step-holds-left / Linear-lerp rules as Phase 1's runtime curve).
pub fn sample_scalar(keys: &[(f32, f32, CurveType)], t: f32) -> f32 { ... }

/// Samples a 2-component channel (translation/scale) at `t`, per component.
pub fn sample_vec2(keys: &[(f32, Vec2, CurveType)], t: f32) -> Vec2 { ... }
```

> Reuse the segment-finding + Step/Linear branch logic from Phase 1. Optionally factor a generic helper, but a small duplicated scalar/vec2 evaluator here is acceptable (it operates on raw authored values — degrees, un-baked translation — whereas Phase 1 operates on baked `Quat`/`Vec3`). Keep them separate to avoid coupling display units to runtime bakes.

#### 2. Curve editor panel

**File**: `crates/client_animation_editor/src/panels/curve.rs`
**Action**: create
```rust
/// Plots the selected channel: scalar → one polyline; vector → one polyline per component
/// (green = X, blue = Y, legended). Segment shape reflects each key's CurveType (Step = flat
/// hold). Axis labels match the plotted value range. The shared playhead overlays at the same
/// x as ruler/dope sheet. Vector keys stack component nodes at one t under a single diamond.
pub fn draw_curve_editor(mut contexts: EguiContexts, state: Res<EditorState>) { ... }
```

#### 3. Inspector panel + loop-continuity warning

**File**: `crates/client_animation_editor/src/panels/inspector.rs`
**Action**: create
```rust
/// Shows value-at-playhead for the selected channel (via eval.rs), the selected key's exact
/// time/value, and the loop-continuity warning.
pub fn draw_inspector(mut contexts: EguiContexts, state: Res<EditorState>) { ... }

/// For a `looping` clip, flags any channel whose value at t=0 differs from t=duration
/// (discontinuity that will pop on loop). Returns a human-readable message or None.
pub fn loop_continuity_warning(asset: &SpriteAnimAsset) -> Option<String> { ... }
```

**File**: `crates/client_animation_editor/src/lib.rs`
**Action**: modify — add `eval`, `panels::curve`, `panels::inspector`; register the two draw systems.

### Verification
#### Automated
- [x] `cargo check -p client_animation_editor` passes.
- [x] Unit test for `eval::sample_scalar`/`sample_vec2` (Step holds left, Linear midpoint) and `loop_continuity_warning` (mismatched endpoints → `Some`, matched → `None`) in `eval.rs`/`inspector.rs`. Add to `cargo test-all`.

#### Manual
- [ ] `cargo anim-editor` — a `Step` segment renders flat in the curve plot; the inspector's value-at-playhead matches the visible pose; a looping clip edited to have `t=0 ≠ t=duration` surfaces the warning.

---

## Phase 7: Keyframe editing (drag retime/revalue, auto-key)

### Changes

#### 1. Edit application

**File**: `crates/client_animation_editor/src/edit.rs`
**Action**: create
```rust
/// Applies a keyframe edit to the working asset: clamps `new_time` to [0, duration], updates
/// the key's value, re-sorts the channel by time, and resolves coincident times deterministically
/// — the dragged key keeps its identity and is placed immediately AFTER any pre-existing key at
/// the same time (stable sort by time, dragged key tie-broken last). Returns the key's new index
/// so selection can follow it across the re-sort.
pub fn apply_key_edit(state: &mut EditorState, sel: &Selection, new_time: f32, new_value: Value) -> usize { ... }

/// Re-runs `build_clip_pair` for the working clip and `clips.insert(existing_id, ..)` so the
/// live rig re-evaluates the edited curve. Mirrors the in-place clip idiom (animation.rs:266).
pub fn rebuild_working_clip(state: &EditorState, clips: &mut Assets<AnimationClip>, ...) { ... }
```

> **Coincident-time rule (locks design's open item):** keys are kept in a `Vec` sorted ascending by `time` using a **stable** sort; when a drag makes the dragged key's time equal a neighbor's, the dragged key is inserted directly after the existing key at that time (tie-break: dragged-last). This is deterministic and lets the user "pass through" a neighbor predictably. The evaluator (Phase 1/6) reads the left key's `CurveType`, so two coincident keys produce a zero-width segment that the segment-finder skips (`partition_point` lands past it) — no division by zero (guard `t1 == t0`).

> `rebuild_working_clip` needs the working clip's `AnimationClip` `AssetId`. Resolve it from `BuiltAnimGraphs`/`BuiltAnimations` for the selected slot. `build_clip_pair` is currently private in `animation.rs` — **make it `pub(crate)` or `pub`** and re-export, OR add a thin `sprite_rig::animation::rebuild_clip_in_place(asset, bone_defaults, override_id, additive_id, &mut clips)` helper. Prefer the latter (keeps the bake logic in `sprite_rig`). **This is a `sprite_rig` API addition — flag in the phase summary.**

#### 2. Auto-key toggle

**File**: `crates/client_animation_editor/src/edit.rs` (or `state.rs`)
**Action**: create
```rust
/// When true, viewport/curve drags create or update a key at the playhead automatically.
#[derive(Resource, Default)]
pub struct AutoKey(pub bool);
```

#### 3. Wire drags in timeline + curve views

**File**: `crates/client_animation_editor/src/panels/timeline.rs`, `panels/curve.rs`
**Action**: modify — on diamond/node drag, compute `new_time = x_to_t(pointer.x)` (dope sheet/ruler) and/or `new_value` from y (curve view), call `apply_key_edit`, then trigger `rebuild_working_clip`. Update `state.selection` to the returned index.

**File**: `crates/client_animation_editor/src/lib.rs`
**Action**: modify — add `edit` module; `init_resource::<AutoKey>()`; ensure `rebuild_working_clip` runs after edits each frame (or call inline within the edit-applying system with `ResMut<Assets<AnimationClip>>`).

### Verification
#### Automated
- [x] `cargo check -p client_animation_editor` passes.
- [x] Unit test for `apply_key_edit`: clamp at bounds (`new_time` beyond `[0,duration]` clamps), re-sort when dragging across a neighbor, coincident-time tie-break (dragged key lands after existing). In `edit.rs`. Add to `cargo test-all`.

#### Manual
- [ ] `cargo anim-editor` — drag a key in the dope sheet and in the curve view; the live pose updates immediately; dragging a key past a neighbor re-sorts (no flicker/duplication); dragging beyond the clip bounds clamps at 0 / duration.

---

## Phase 8: Bone gizmo overlay + screen-drag editing

### Changes

#### 1. Gizmo overlay + screen-drag

**File**: `crates/client_animation_editor/src/panels/gizmo.rs`
**Action**: create
```rust
/// Draws a clickable gizmo at each bone's screen position (project bone `GlobalTransform`
/// through the editor camera). Uses bevy `Gizmos` or egui `Painter` over the viewport — match
/// whichever integrates with the existing single window camera (research Q10: no second camera /
/// render-to-texture; the rig renders full-window, so overlay in egui screen space using the
/// camera's `world_to_viewport`).
pub fn draw_bone_gizmos(...) { ... }

/// Converts a screen-space drag delta on a bone into a bone-local channel value, inverting the
/// documented build-time bakes:
///   - translation: subtract default xy offset and ignore baked z_order (research Q2,
///     animation.rs:494-499) → raw authored Vec2.
///   - rotation: screen angle delta → degrees (value is authored in degrees; runtime bakes
///     deg→rad→Quat::from_rotation_z, animation.rs:467) → invert rad→deg.
///   - scale: screen delta → xy scale factors (z forced 1.0, animation.rs:529).
/// Mutates the keyframe per the AutoKey toggle, then rebuild_working_clip runs.
pub fn screen_delta_to_bone_value(delta: Vec2, bone: &str, channel: Channel, camera: &Camera, camera_tf: &GlobalTransform) -> Value { ... }
```

> **Bone screen position:** query the editor rig's bone joint entities (`BoneEntities`, `spawn.rs`) for their `GlobalTransform`, project via `camera.world_to_viewport(camera_tf, world_pos)`. egui-vs-world input: consult `EguiWantsInput` (research Q11) so a drag that starts over an egui panel doesn't also drag a bone. Since the editor has no leafwing ownership pipeline, read `bevy_egui::EguiWantsInput` directly in the gizmo system: skip bone-drag when `wants_any_pointer_input()` is true.

**File**: `crates/client_animation_editor/src/edit.rs`
**Action**: modify — add the inverse-bake helpers used by `screen_delta_to_bone_value` if shared with curve-view revalue.

**File**: `crates/client_animation_editor/src/lib.rs`
**Action**: modify — add `panels::gizmo`; register `draw_bone_gizmos` (Update, in EguiPrimaryContextPass or Update depending on draw method).

### Verification
#### Automated
- [x] `cargo check -p client_animation_editor` passes.
- [x] Unit test for `screen_delta_to_bone_value` rotation/translation inversion against a known screen basis (y-down mirror): translation solves the screen→world basis, rotation flips the mirrored angle to authored degrees, scale applies sensitivity. In `gizmo.rs`.

#### Manual
- [ ] `cargo anim-editor` — drag a bone in the viewport; the corresponding channel key mutates and the pose follows; the produced authored value equals the inverse of the documented bake (cross-check against the inspector readout).

---

## Phase 9: Audition modes (solo / layered)

### Changes

#### 1. Audition panel + velocity drive

**File**: `crates/client_animation_editor/src/panels/audition.rs`
**Action**: create
```rust
/// Locomotion speed slider. Writes the editor rig's velocity component so the REAL
/// `update_locomotion_blend_weights` + `recompute_layer_masks` drive idle/walk/run blend
/// under the edited clip. speed=0 → solo (idle base); higher → layered walk/run.
pub fn draw_audition(mut contexts: EguiContexts, mut audition: ResMut<AuditionState>) { ... }

#[derive(Resource, Default)]
pub struct AuditionState { pub speed: f32 }

/// Writes `speed` into the velocity component the blend systems read (research Q3: velocity.xz()
/// length feeds compute_blend_weights). Do not fork blend logic — set the input and let the real
/// systems run.
pub fn set_audition_velocity(audition: Res<AuditionState>, mut rig: Query<&mut Velocity, With<SpriteRig>>) { ... }
```

> **Confirm the velocity component type** the real blend reads (`update_locomotion_blend_weights`, `animation.rs:866-910` reads `velocity.xz().length()`). Set its xz to point "forward" with magnitude `speed`. Reuse the same component the editor rig was spawned with in Phase 3.

#### 2. Editor-only ability loop

**File**: `crates/client_animation_editor/src/panels/audition.rs`
**Action**: create
```rust
/// EDITOR-ONLY: forces the active ability clip to loop/re-trigger so it can be auditioned
/// continuously. In-game ability clips play once then clean up (animset.rs:290-292); this loop
/// lives ONLY in the editor and never in the gameplay path. Clearly named to prevent leakage.
pub fn force_loop_active_ability(player: &mut AnimationPlayer, node: AnimationNodeIndex) {
    if let Some(active) = player.animation_mut(node) {
        active.repeat(); // editor audition only
    }
}
```

> When the selected clip is an ability and audition is active, apply `.repeat()` to its node (or re-trigger on `is_finished()`). Keep this helper in the editor crate, not `sprite_rig`.

**File**: `crates/client_animation_editor/src/lib.rs`
**Action**: modify — add `panels::audition`; `init_resource::<AuditionState>()`; register `draw_audition`, `set_audition_velocity`, and the ability-loop driver.

### Verification
#### Automated
- [x] `cargo check -p client_animation_editor` passes.

#### Manual
- [ ] `cargo anim-editor` — speed=0 shows the idle base (solo); raising speed blends walk then run under a looping edited clip; the blend matches in-game `cargo client` behavior at equivalent speeds. Selected-ability bones no longer blend with locomotion (the ±180° rotation flip is gone).

---

## Phase 10: Event lane + playback-gated audio

### Changes

#### 1. Event lane

**File**: `crates/client_animation_editor/src/panels/events.rs`
**Action**: create
```rust
/// Bone-independent bottom lane drawn over the shared t→x track. Each `AnimEventKeyframe`
/// renders as a diamond at `t_to_x(event.time)`. Drag retimes (x → x_to_t, clamp [0,duration],
/// re-sort); click selects → rename via a text field in the inspector. Shares the playhead x.
pub fn draw_event_lane(mut contexts: EguiContexts, mut state: ResMut<EditorState>) { ... }
```

#### 2. Playback-gated audio observer

**File**: `crates/client_animation_editor/src/audio.rs`
**Action**: create
```rust
/// Plays a sound when an `AnimationEventFired` is observed, but ONLY while playback == Playing.
/// Scrub-induced fires (Paused) are no-ops, so scrubbing the timeline is silent.
pub fn on_event_play_audio(
    trigger: On<sprite_rig::AnimationEventFired>,
    playback: Res<EditorState>, // read state.playback
    // audio source / asset server to play a click
) {
    if !matches!(playback.playback, Playback::Playing) {
        trace!("animation event fired while not playing; suppressing audio (scrub)");
        return;
    }
    // play a short sound
}
```

> Register via `app.add_observer(on_event_play_audio)` in the editor plugin. The real `sprite_rig` observer (`on_animation_event_fired`, `animset.rs:348`) still runs (it only `trace!`s) — the editor adds a second observer for audio. Audio asset: a short generic click in `assets/` (reuse an existing sfx if present; otherwise note that a placeholder asset is needed — **flag in summary if no suitable asset exists**).

#### 3. Loop-wrap regression test (sprite_rig)

**File**: `crates/sprite_rig/src/animset.rs` (or a new test module)
**Action**: modify — add one targeted test asserting Bevy's event firing at the loop boundary neither double-fires nor misses.
```rust
/// At a loop wrap (clip time crosses duration→0 via .repeat()), an event at t≈0 fires exactly
/// once per loop and an event at t≈duration fires exactly once — no double-fire, no miss.
#[test]
fn loop_wrap_event_fires_once() { ... }
```

> This tests the previously-unread Bevy wrap semantics (research Q5 / Open Areas). Build a minimal `App`, a looping clip with events near both ends, advance time across the boundary, count `AnimationEventFired` triggers via a counter resource/observer. If reproducing the player tick in a test proves infeasible with `MinimalPlugins`, document the limitation and rely on the manual check; do not skip silently.

**File**: `crates/client_animation_editor/src/lib.rs`
**Action**: modify — add `panels::events`, `audio`; register `draw_event_lane`, `add_observer(on_event_play_audio)`.

### Verification
#### Automated
- [x] `cargo test-native` passes including the loop-wrap test. (Audio is NOT the planned `AnimationEventFired` observer: transport-driven speed-0 nodes never fire bevy clip events — per the Phase 6 note — so `audio.rs` detects playhead crossings editor-side, mirroring bevy's forward/wrap event windows; rename lives in the inspector. Placeholder click sfx generated at `assets/audio/editor_event_click.ogg`.)
- [x] `cargo check -p client_animation_editor` passes.

#### Manual
- [x] `cargo anim-editor` — events are audible during playback, silent while scrubbing; an event at a loop boundary fires correctly (no double/missed sound across the wrap).

---

## Phase 11: Canonical, deterministic RON serialization

Pure serializer landing immediately before its only consumer (Phase 12). The running game never serializes clips, so this has no earlier use; keeping it a pure, tested unit in `sprite_rig` (no disk I/O, no `persistence` dep) is what lets Phase 12 add the disk path cleanly.

### Changes

#### 1. Add `ron` dependency

**File**: `crates/sprite_rig/Cargo.toml`
**Action**: modify — add under `[dependencies]`: `ron = { workspace = true }` (workspace pins `ron = "0.12"`, `Cargo.toml:61`).

#### 2. Serializer module

**File**: `crates/sprite_rig/src/serialize.rs`
**Action**: create — pure string-returning functions, **no disk I/O** (the write path lands in Phase 12, keeping `sprite_rig` free of the `persistence` dep).

```rust
use ron::ser::PrettyConfig;
use crate::asset::{SpriteAnimAsset, SpriteAnimSetAsset, BoneBlendMode, BoneTimeline};

/// Header enabling RON's `implicit_some` extension; every asset file starts with this today.
const RON_HEADER: &str = "#![enable(implicit_some)]\n";

/// Serializes a clip to canonical RON: `bone_timelines` emitted in `bone_order` (rig bone-index
/// order, not `HashMap` iteration order); default `blend_mode` (`Override`) omitted; tuned
/// `PrettyConfig`. Deterministic and stable (a second serialize of the round-tripped result is
/// byte-identical). Bones present in `bone_timelines` but absent from `bone_order` are appended
/// in sorted-name order with a `debug_assert!` (a clip referencing an unknown bone is a bug).
pub fn serialize_anim(asset: &SpriteAnimAsset, bone_order: &[String]) -> String {
    // Reproject bone_timelines into an ordered representation, then hand to ron with a config
    // that matches the authored style as closely as practical. Because RON serializes a HashMap
    // nondeterministically, we serialize via an ordered intermediary: build the RON string by
    // composing per-bone fragments in bone_order, OR convert to an indexmap-like ordered struct.
    // Chosen approach: manual ordered emission of the bone_timelines map, delegating each
    // BoneTimeline's body to ron, so we control map order without a new dep.
    ...
}

/// Serializes an animset to canonical RON. `ability_animations` (a `HashMap`) is emitted in
/// sorted-key order for determinism. Resolves the `implicit_some` rule (decision below).
pub fn serialize_animset(asset: &SpriteAnimSetAsset) -> String { ... }

fn pretty_config() -> PrettyConfig {
    PrettyConfig::default()
        .struct_names(false)
        .separate_tuple_members(false)
        // tune indent/array style to match authored files during implementation
}
```

**Decisions to lock here (round-trip tested):**
- **Bone ordering**: emit `bone_timelines` entries in `bone_order` (the rig's topological bone-index order from `topological_sort_bones`, `spawn.rs:634`). `serialize_anim` takes `bone_order: &[String]` so the caller (Phase 12) passes the rig's canonical order.
- **`ability_animations` ordering** (animset): sort keys lexicographically.
- **`blend_mode` default skip**: omit `blend_mode` when `== BoneBlendMode::Override` (the `#[default]`). Since `BoneTimeline` derives plain `Serialize` (no `skip_serializing_if`), do the skip in the manual per-bone emission rather than relying on serde attrs.
- **`implicit_some` header**: emit the `#![enable(implicit_some)]` header AND write `Option` fields without explicit `Some(..)`. `SpriteAnimSetAsset` has `Option` fields (`hit_react`, etc.); under `implicit_some` a bare value deserializes into `Some`. Rule: **keep the header, emit bare values for `Some`, emit `None` for `None`.** Round-trip-test this against the existing `humanoid.animset.ron`.
- **Floats**: accept RON's default float formatting (`0.0` may render as `0`); the round-trip test asserts *structural* (value) equality, not byte equality vs. the original authored file — byte-stability is only required serialize→deserialize→**serialize** (second pass identical).

> Manual ordered emission is the elegant minimum here: serialize each `BoneTimeline` body with `ron` (it has no `HashMap`), then assemble the outer `bone_timelines: { ... }` map string in `bone_order`. This avoids adding an `indexmap` dep or a parallel ordered schema. Validate the assembled string parses back via `ron::de`.

#### 3. Module registration

**File**: `crates/sprite_rig/src/lib.rs`
**Action**: modify — add `pub mod serialize;` beside the existing module declarations (`:1-5`).

### Verification
#### Automated
- [x] `cargo test-native` passes. (PartialEq derives added to the anim asset types for the structural assertions. Reworked after review: instead of manual string assembly, a serialize-only `CanonicalAnim` wrapper orders `bone_timelines`, `ability_animations` became a `BTreeMap`, `blend_mode` skips via serde attr, and one `to_string_pretty` call with `depth_limit` + `Extensions::IMPLICIT_SOME` renders each document — keyframes one per line; event keyframes render multi-line, the one style divergence.)
- [x] Round-trip test in `crates/sprite_rig/src/serialize.rs` (`#[cfg(test)]`): for each of the 7 clips + the animset — (1) deserialize the authored RON from `assets/anims/humanoid/*.anim.ron` via `ron::de` (read with `include_str!` or `std::fs` relative to `CARGO_MANIFEST_DIR/../../assets`), (2) `serialize_anim` / `serialize_animset`, (3) `ron::de` the output, (4) assert structural equality with the original (compare field-by-field; `bone_timelines` compared as sets/maps so ordering differences are ignored — nothing invented or dropped), (5) serialize the round-tripped value again and assert **byte-identical** to step (2)'s output (stability).

> Bone order for the test: derive from `humanoid.rig.ron` via the existing topological sort, or hard-code the known order `[root, torso, head, arm_l, arm_r, leg_l, leg_r]` (research Q4) for the test only.

---

## Phase 12: Save to disk (clip + animset, new-clip assignment)

### Changes

#### 1. Custom RON `Store` impls

**File**: `crates/client_animation_editor/src/store.rs`
**Action**: create — the shipped `FileSystemStore` is JSON-only with flat single-component string keys (`git/bevy-persistence/src/filesystem.rs:73-101`), so these are custom impls calling the Phase 11 serializers with tmp+rename writes.
```rust
use persistence::{Store, PersistenceError};
use sprite_rig::asset::{SpriteAnimAsset, SpriteAnimSetAsset};
use sprite_rig::serialize::{serialize_anim, serialize_animset};
use std::path::PathBuf;

/// Identifies a clip file under the asset root, e.g. "anims/humanoid/walk".
#[derive(Clone, Debug)]
pub struct ClipPath { pub rel: String, pub bone_order: Vec<String> }

/// Writes/reads `.anim.ron` files via the canonical Phase 11 serializer with atomic tmp+rename.
#[derive(Clone)]
pub struct FsAnimClipStore { pub asset_root: PathBuf }

impl Store<ClipPath, SpriteAnimAsset> for FsAnimClipStore {
    fn save(&self, key: &ClipPath, value: &SpriteAnimAsset) -> Result<(), PersistenceError> {
        let ron = serialize_anim(value, &key.bone_order);
        let path = self.asset_root.join(format!("{}.anim.ron", key.rel));
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
        let tmp = path.with_extension("anim.ron.tmp");
        std::fs::write(&tmp, ron)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
    fn load(&self, key: &ClipPath) -> Result<Option<SpriteAnimAsset>, PersistenceError> {
        let path = self.asset_root.join(format!("{}.anim.ron", key.rel));
        if !path.exists() { return Ok(None); }
        let s = std::fs::read_to_string(&path)?;
        ron::de::from_str(&s).map(Some).map_err(|e| PersistenceError::Deserialize(e.to_string()))
    }
}

/// Writes/reads the single `.animset.ron`.
#[derive(Clone)]
pub struct FsAnimSetStore { pub asset_root: PathBuf, pub rel: String }

impl Store<(), SpriteAnimSetAsset> for FsAnimSetStore { /* serialize_animset + tmp+rename */ }
```

> `Store::save`/`load` are **synchronous** (`git/bevy-persistence/src/store.rs:48-57`), `Send + Sync + Clone + 'static`. `PersistenceError` has `From<std::io::Error>` so `?` on `std::fs` auto-converts to `Io`. Errors are never silently dropped (CLAUDE.md): they surface via `PendingStoreOps::save_errors`.

#### 2. Save panel + ops wiring

**File**: `crates/client_animation_editor/src/panels/save.rs`
**Action**: create
```rust
/// "Save clip" / "Save animset" buttons. Enqueues saves through the bevy-persistence ops
/// machinery (PendingStoreOps), which runs the sync Store on the async pool and routes
/// completions/errors back into pub vectors we drain.
pub fn draw_save(mut contexts: EguiContexts, ...) { ... }

/// Spawns a save via PendingStoreOps for the working clip.
pub fn save_clip(
    state: &EditorState,
    store: &FsAnimClipStore,
    ops: &mut persistence::PendingStoreOps<ClipPath, SpriteAnimAsset>,
) {
    ops.spawn_save(store, clip_path_for(&state.selected_clip, &state.working_set), state.working.clone());
}

/// Drains completed_saves / save_errors after poll(); logs errors as PersistenceError (never
/// silently dropped). Panics/`error!` on save failure per CLAUDE.md explicit-failure rule.
pub fn drain_save_results(...) { ... }
```

> **Integration (caller-owned — the crate has no plugin, `git/bevy-persistence/src/lib.rs:4-6`):** spawn one entity carrying `StoreBackend::new(FsAnimClipStore { .. })` + `PendingStoreOps::<ClipPath, SpriteAnimAsset>::default()`, and another for the animset store. A system queries both, calls `spawn_save` on button press, then each frame calls `ops.poll()` and drains `completed_saves`/`save_errors`. `AsyncComputeTaskPool` is initialized by `DefaultPlugins` (no manual init needed). Alternatively, since the editor is single-threaded authoring, call the `Store::save` directly and surface errors — but use `PendingStoreOps` to match the workspace pattern (structure.md decision).

#### 3. New-clip assignment

**File**: `crates/client_animation_editor/src/state.rs`
**Action**: modify
```rust
/// Assigns the working clip to an animset slot (locomotion entry / ability id / hit_react),
/// mutating `working_set`. This changes graph topology → triggers a graph rebuild (Phase 2) by
/// modifying the `SpriteAnimSetAsset` in `Assets<_>` (emits AssetEvent::Modified) OR by pushing
/// onto `GraphRebuildQueue`. Saving then writes both the `.anim.ron` and `.animset.ron`.
pub fn assign_new_clip(state: &mut EditorState, slot: ClipSlot, ...) { ... }
```

> To trigger the rebuild: after mutating `working_set`, write it back into `Assets<SpriteAnimSetAsset>` via `assets.get_mut(handle)` (which emits `Modified`) so Phase 2's `build_anim_graphs` picks it up — or push the animset id onto `GraphRebuildQueue` directly. Prefer `get_mut` (uses the real event path).

#### 4. Cargo dep

**File**: `crates/client_animation_editor/Cargo.toml`
**Action**: modify — add `persistence = { workspace = true }` (resolves to `bevy-persistence`, root `Cargo.toml:54`).

**File**: `crates/client_animation_editor/src/lib.rs`
**Action**: modify — add `store`, `panels::save`; spawn the store-backend entities at startup; register `draw_save`, `drain_save_results`, `assign_new_clip` wiring; `poll()` each frame.

### Verification
#### Automated
- [x] `cargo test-native` passes. (Deviations from the sketch: `ClipPath.rel` keeps the full animset-relative path including `.anim.ron` — no extension reassembly; `FsAnimSetStore` is keyed by `String` rel path instead of `()` so the path derives from the animset handle at save time; `assign_new_clip` also registers the working clip as a live asset — `Assets` insert + `LoadedAnimHandles` + copied bone defaults — BEFORE the animset write-back, so the build chain never asks the asset server for a not-yet-on-disk file; Save clip also syncs the in-memory `SpriteAnimAsset` so select-away-and-back matches disk.)
- [x] `cargo check -p client_animation_editor` passes.
- [x] Test for `FsAnimClipStore` save→load round-trip to a tempdir: save a clip, reload it, assert structural equality (reuses the Phase 11 serializer; confirms the Store path doesn't corrupt). In `store.rs`. (Plus animset-store round-trip, missing-file → `None`, and slot-assignment unit tests in `state.rs`.)

#### Manual
- [ ] `cargo anim-editor` — load a clip, save with no edits → the on-disk `.anim.ron` is canonical and stable (a second save is byte-identical; matches the Phase 11 round-trip). Assign a new clip to a slot, save → both `.anim.ron` and `.animset.ron` update; reload `cargo anim-editor` and the live preview places the new clip correctly.
- [ ] One-time reformat: the first save of an existing authored file rewrites it to canonical form; subsequent saves are stable.

---

## Cross-phase notes

- **`sprite_rig` API additions** (call out in PR): `pub mod curve` + `SegmentedKeyframeCurve` (P1); `CurveType: Copy` (P1); `GraphRebuildQueue`, `build_anim_graphs` gains rebuild responsibility + `store_built_graph`/`reset_animation_players_for` helpers, optional `AnimSetRef::animset_id` (P2); a `pub(crate)`→`pub` exposure or `rebuild_clip_in_place` helper for in-place working-clip rebuild (P7); `pub mod serialize` (P11).
- **README** (CLAUDE.md [HIGH]): after Phase 3 (new `cargo anim-editor` alias / crate) and Phase 12 (save behavior), review and update `README.md` for the new command + editor description.
- **No parallel builds** (CLAUDE.md [HIGHEST]): every verification step is serialized; never run two `cargo` build/check/test at once.
- **Deviation discipline**: several phases carry "confirm during implementation" notes (exact Bevy 0.18 `Curve`/`Interval`/`AnimationPlayer` signatures, the velocity component type, whether `RenderPlugin`/`SharedGameplayPlugin` drag in networking, the audio placeholder asset). If any forces a structural change beyond the outlined files, **stop and get approval before deviating.**
