# Research Findings

Scope: `crates/sprite_rig/` (asset schema, clip build, pose/graph, hierarchy, events, hot-reload), `crates/render/` (camera/lighting/gizmo/render-target), `crates/dev/` (`bevy_egui`), the Cargo workspace + `.cargo`/`Makefile.toml` conventions, and the client binary's plugin/AppState composition. RON data lives under `assets/anims/humanoid/`, `assets/rigs/`.

---

## Q1: Anim clip serde schema, RON instantiation, deserialization, and round-trip-to-RON

**Direct answer:** Every schema type derives `Serialize + Deserialize` and is deserialized by `bevy_common_assets::ron::RonAssetPlugin`. There is **no** path that serializes any sprite_rig struct back to RON — `ron::ser`/`to_string_pretty`/`fs::write` appear nowhere in the crate. A hypothetical round-trip would preserve data but lose formatting, the `implicit_some` header, omitted-default fields, and (because `bone_timelines` is a `HashMap`) per-bone ordering.

### Evidence — schema (all in `crates/sprite_rig/src/asset.rs`, `use serde::{Deserialize, Serialize};` at asset.rs:2)

```rust
// crates/sprite_rig/src/asset.rs:72-79  (clip root)
#[derive(Clone, Debug, Serialize, Deserialize, Asset, TypePath)]
pub struct SpriteAnimAsset {
    pub name: String,
    pub duration: f32,
    pub looping: bool,
    pub bone_timelines: HashMap<String, BoneTimeline>,
    pub events: Vec<AnimEventKeyframe>,
}
```

```rust
// crates/sprite_rig/src/asset.rs:88-95
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct BoneTimeline {
    #[serde(default)]               // only serde attr in the file → most clips omit blend_mode
    pub blend_mode: BoneBlendMode,
    pub rotation: Vec<RotationKeyframe>,
    pub translation: Vec<TranslationKeyframe>,
    pub scale: Vec<ScaleKeyframe>,
}
```

```rust
// crates/sprite_rig/src/asset.rs:98-109
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
pub enum BoneBlendMode { #[default] Override, Additive }

// asset.rs:111-130  — three keyframe structs
pub struct RotationKeyframe    { pub time: f32, pub value: f32,  pub curve: CurveType }
pub struct TranslationKeyframe { pub time: f32, pub value: Vec2, pub curve: CurveType }
pub struct ScaleKeyframe       { pub time: f32, pub value: Vec2, pub curve: CurveType }

// asset.rs:133-138
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub enum CurveType { #[default] Linear, Step }

// asset.rs:141-145
pub struct AnimEventKeyframe { pub time: f32, pub name: String }
```

`Vec2` deserializes from a RON 2-tuple `(x, y)`. Sibling assets: `SpriteAnimSetAsset` (asset.rs:148-154) `{ rig, locomotion, ability_animations, hit_react }`; `LocomotionConfig`/`LocomotionEntry` (asset.rs:157-167); rig schema `SpriteRigAsset` etc. (asset.rs:6-69, see Q4).

### Evidence — RON files & loader registration

```rust
// crates/sprite_rig/src/lib.rs:26-30  (bevy_common_assets RonAssetPlugin, import at lib.rs:9)
app.add_plugins((
    RonAssetPlugin::<SpriteRigAsset>::new(&["rig.ron"]),
    RonAssetPlugin::<SpriteAnimAsset>::new(&["anim.ron"]),
    RonAssetPlugin::<SpriteAnimSetAsset>::new(&["animset.ron"]),
));
```

Each `RonAssetPlugin::<T>::new(&[ext])` registers an `AssetLoader` keyed on extension; at load it runs `ron::de` into `T` via the derived `Deserialize` and inserts into `Assets<T>`. Pins: `ron = "0.12"`, `bevy_common_assets = { version = "0.15", features=["ron"] }` (`Cargo.toml:61-62`); sprite_rig does **not** depend on `ron` directly (`crates/sprite_rig/Cargo.toml:6-12`).

| RON file | Type | Count |
|---|---|---|
| `assets/anims/humanoid/{idle,walk,run,punch,punch2,ground_pound,jump}.anim.ron` | `SpriteAnimAsset` | 7 |
| `assets/anims/humanoid/humanoid.animset.ron` | `SpriteAnimSetAsset` | 1 |
| `assets/rigs/humanoid.rig.ron` | `SpriteRigAsset` | 1 |

Representative clip (`walk.anim.ron:1-17`) — note `#![enable(implicit_some)]` header (line 1 of every file) and tuple-struct syntax; only `jump.anim.ron` & `ground_pound.anim.ron` declare `blend_mode: Additive`:

```ron
#![enable(implicit_some)]
(
    name: "walk", duration: 0.4, looping: true,
    bone_timelines: {
        "torso": (
            rotation: [(time: 0.0, value: 5.0, curve: Linear), (time: 0.2, value: 8.0, curve: Linear), (time: 0.4, value: 5.0, curve: Linear)],
            translation: [ (time: 0.0, value: (0.0, 0.0), curve: Linear), ... ],
            scale: [],
        ),
    ...
    events: [ (time: 0.1, name: "run_footstep_left"), (time: 0.3, name: "run_footstep_right") ],
)
```

### Evidence — no serialize-back path; round-trip fidelity

Grep of sprite_rig for `to_string_pretty | ron::ser | PrettyConfig | fs::write | File::create` → **no matches**. The `Serialize` derives are never invoked. Nearest analogues live in `server` (not assets): `crates/server/src/persistence/mod.rs:756` `ron::ser::to_string_pretty(record, PrettyConfig::default())` + atomic write (the only `to_string_pretty` in the repo); `crates/server/src/chunk_entities.rs:431-501` `ron::to_string` ↔ `ron::from_str` round-trip into a DB column.

`ron 0.12` round-trip behavior, if applied to `SpriteAnimAsset`:
- **Field order**: serialized in Rust declaration order — matches authored top-level/keyframe order. But `bone_timelines: HashMap` (asset.rs:77) iterates **non-deterministically**, so per-bone order would not be preserved (same for `ability_animations`, `skins`).
- **`blend_mode`**: `#[serde(default)]` without `skip_serializing_if` → every timeline would emit `blend_mode: Override`, adding lines to the 5 files that omit it.
- **Formatting**: compact (`to_string`) or fixed-indent (`to_string_pretty`); neither reproduces column alignment, inline-vs-multiline arrays, or the `#![enable(implicit_some)]` header; `Option`s become explicit `Some(..)`/`None`.
- **Floats**: round-trip by value, textual form may differ (`0.0` vs `0`).

---

## Q2: Clip → runtime `AnimationClip` (curve builders, interpolation, duration/looping/tick)

**Direct answer:** Each `SpriteAnimAsset` becomes **two** `AnimationClip`s (override + additive) via `build_clip_pair`; per-bone channels become `AnimatableCurve`s wrapping `UnevenSampleAutoCurve` keyed on authored `time`. The per-keyframe `CurveType` (Linear/Step) is **never read** — interpolation is `UnevenSampleAutoCurve`'s default (linear for `Vec3`, slerp for `Quat`); `Step` has no runtime effect. The `looping` field is **never read** — looping is set per-`ActiveAnimation` via `.repeat()` at play time. `FIXED_TIMESTEP_HZ` (64 Hz sim tick) is unrelated and not referenced in sprite_rig; clip length is purely `set_duration(anim.duration)`.

### Evidence

```rust
// crates/sprite_rig/src/animation.rs:274-282
fn build_clip_pair(anim: &SpriteAnimAsset, bone_defaults: &[BoneAnimDefault])
    -> (AnimationClip, AnimationClip, AnimationMask, AnimationMask) {
    let (override_bones, additive_bones) = partition_bones_by_mode(anim, bone_defaults);
    let override_clip = build_override_clip(anim, bone_defaults);
    let additive_clip = build_additive_clip(anim, bone_defaults);
    (override_clip, additive_clip, override_bones, additive_bones)
}
```

`partition_bones_by_mode` (animation.rs:287-304) assigns bone index `i` → bit `1u64 << i` into override/additive `AnimationMask` by `timeline.blend_mode`. Duration set identically in both builders: `clip.set_duration(anim.duration);` (animation.rs:313, :360), from `pub duration: f32` (asset.rs:75).

Override builder per bone (animation.rs:319-341): real curves only when timeline exists **and** is `Override`; otherwise **hold-at-default filler curves** so weighted blending stays correct. Authored events attached on override side only: `add_events_to_clip(&mut clip, &anim.events)` (animation.rs:344). Additive builder (animation.rs:357-384) skips non-additive/empty bones (identity under `Add`), drops scale curves with a `warn!` (animation.rs:374-380), and intentionally omits events (doc animation.rs:354-356) to avoid double-firing.

Curve construction maps `time`→x, transformed `value`→y, all via `UnevenSampleAutoCurve::new(...)` wrapped in `AnimatableCurve::new(animated_field!(...), curve)` (import `use bevy::math::curve::sample_curves::UnevenSampleAutoCurve;` animation.rs:6):

```rust
// override rotation  animation.rs:467  (deg→rad→Z-quat; needs >=2 keys else hold identity)
.map(|k| (k.time, Quat::from_rotation_z(k.value.to_radians())))
// override translation  animation.rs:496-498  (bakes default xy offset AND z_order into value)
.map(|k| (k.time, Vec3::new(default_xy.x + k.value.x, default_xy.y + k.value.y, z_order)))
// override scale  animation.rs:529  (z forced to 1.0; <2 keys → early return, Bevy default scale kept)
.map(|k| (k.time, Vec3::new(k.value.x, k.value.y, 1.0)))
// additive rotation  animation.rs:387-406  (delta; no offset)
// additive translation animation.rs:410-429 (raw delta, z forced 0.0)
```

Hold fillers (`add_hold_at_default_curves`, animation.rs:432-453): two keys at `0.0` and `duration` holding `Quat::IDENTITY` and `Vec3::new(default_xy.x, default_xy.y, z_order)`.

**Interpolation**: `CurveType`/`k.curve` referenced nowhere outside the asset definitions. `UnevenSampleAutoCurve` interpolates via each value's `StableInterpolate` → `Vec3` linear, `Quat` slerp. No `Step` path.

**Looping**: `looping` field grep finds only the struct field. `.repeat()` applied at play time to locomotion clips only: `start_locomotion_blend` `player.play(node).repeat()` (animation.rs:842-843) and `restart_stopped_locomotion_clips` (animset.rs:325). Ability clips play **without** `.repeat()` (animset.rs:223,244), run once, cleaned up on `anim.is_finished()` (animset.rs:290-292).

**Tick rate**: `pub const FIXED_TIMESTEP_HZ: f64 = 64.0;` (`crates/protocol/src/lib.rs:70`) — used only for lightyear `tick_duration` in client/server/web mains; absent from sprite_rig. `AnimationPlayer` advances on render/`Update` `Time`, decoupled from the sim tick.

---

## Q3: Computing a bone's local `Transform` at time `t`; sampling one clip independently

**Direct answer:** Bone `Transform`s at `t` are computed **entirely by Bevy's stock `AnimationPlayer` + `AnimationGraph`** evaluator — this crate only builds curves, builds the graph, attaches the player, and drives node weights/masks. Default offsets and z-order are baked into curve **values** at build time; additive-vs-override is realized by graph `Add`/`Blend` topology + per-node masks. **There is no direct keyframe-evaluation path that bypasses the graph.**

### Evidence — where defaults/z-order/blend live

- Default xy + z baked into translation curve values (animation.rs:494-499) and hold fillers (animation.rs:446-447); originate in `BoneAnimDefault` (animation.rs:46-50) populated by `populate_anim_bone_defaults` from `bone.default_transform.translation` + slot `z_order` (animation.rs:140-151). Z is intentionally inside the curve to avoid a post-animation z-restore race (doc animation.rs:42-44).
- Graph topology in `build_graph_for_animset` (animation.rs:630-718):

```rust
// animation.rs:652-654
let master_add = graph.add_additive_blend(1.0, graph.root);
let base_pose_blend = graph.add_blend(1.0, master_add);
let locomotion_blend = graph.add_blend(1.0, base_pose_blend);
```

Locomotion clips → children of `locomotion_blend` (animation.rs:664). Ability **override** clips → masked siblings under `base_pose_blend` with `override_exclusion = (!pair.override_bones) & all_bones_mask` (animation.rs:676-682). Ability **additive** clips → children of `master_add` with `additive_exclusion = (!pair.additive_bones) & all_bones_mask` (animation.rs:685-691). Per-bone mask groups via `register_bone_mask_groups` → `graph.add_target_to_mask_group(target_id, i)` (animation.rs:754-764). Runtime priority masks recomputed in `recompute_layer_masks` (animset.rs:122-147): override chain `effective = (!layer.claims) | owned_above`; additive `node.mask = !layer.claims`.

### Evidence — graph drives bones; no bypass

`attach_animation_players` (animation.rs:785-813) inserts `AnimationPlayer::default()`, `AnimationGraphHandle(...)`, `ActiveAnimLayers`, and tags each bone entity:

```rust
// animation.rs:806-810
commands.entity(bone_entity).insert((target_id, AnimatedBy(entity)));
// target_id = AnimationTargetId::from_names(once(&Name::new(bone_name))) — same id used as the curve target
```

`update_locomotion_blend_weights` (animation.rs:866-910) computes weights from `velocity.xz().length()` via `compute_blend_weights` (animation.rs:915-948), lerps with `BLEND_LERP_SPEED = 10.0` (animation.rs:111), pushes `anim.set_weight(*current)`. Bevy's animation systems then sample each clip's `UnevenSampleAutoCurve` at the player time, blend/add per graph + masks, and write each matched bone `Transform`. Grep finds no manual `curve.sample(...)` or direct clip-to-`Transform` write in sprite_rig.

**Sampling one clip at arbitrary `t` independently** would require reproducing the build-time value transforms (translation `Vec3::new(default_xy.x+k.value.x, default_xy.y+k.value.y, z_order)`; rotation `Quat::from_rotation_z(k.value.to_radians())`; scale `(x,y,1.0)`) and interpolating bracketing keys (linear/slerp), or constructing the single `AnimationClip` from `build_override_clip` and evaluating its `AnimatableCurve`s at `t`. No such standalone sampler exists today. Nearest existing direct-`Transform` computation: `bone_transform_from_def` (spawn.rs:561-580) for the rest pose.

---

## Q4: Bone/skeleton hierarchy, spawning, world transforms, facing/billboarding

**Direct answer:** `SpriteRigAsset.bones: Vec<BoneDef>` (each with `parent: Option<String>`) is topologically sorted, then one transform-only "joint" entity per bone is parented to its parent joint or to a `JointRoot`; the name→entity map is `BoneEntities`. Per-bone world transforms come from Bevy `Transform`→`GlobalTransform` propagation (joint locals written by `AnimationPlayer`), consumed by one `SkinnedMesh`. Facing flips `JointRoot.scale.x` (±1); billboarding rotates `JointRoot` about Y to face the camera — both act on the shared ancestor, transforming all bones together.

### Evidence — structures

```rust
// crates/sprite_rig/src/asset.rs:6-14
pub struct SpriteRigAsset { pub pixels_per_unit: f32, pub bones: Vec<BoneDef>,
                            pub slots: Vec<SlotDef>, pub skins: HashMap<String, HashMap<String, AttachmentDef>> }
// asset.rs:17-22
pub struct BoneDef { pub name: String, pub parent: Option<String>, pub default_transform: BoneTransform2d }
// crates/sprite_rig/src/spawn.rs:29-31 / :40-42
#[derive(Component, Default)] pub struct BoneEntities(pub HashMap<String, Entity>);
#[derive(Component)] pub struct JointRoot;
```

`humanoid.rig.ron:1-12`: `root`(parent None) → `torso`(root) → `head`/`arm_l`/`arm_r`(torso); `leg_l`/`leg_r`(root). `pixels_per_unit: 24.0`.

### Evidence — topological sort & spawn

`topological_sort_bones` (spawn.rs:634-663) iterative fixpoint: add any bone whose parent is already added (or None) until all placed; `debug_assert!(sorted.len() > prev_len, "...cycle or missing parent")`. Sort order = canonical bone index via `build_bone_index_map` (spawn.rs:380-389) → joint/bind-pose index; `inverse_bindposes = vec![Mat4::IDENTITY; sorted_bones.len()]` (spawn.rs:254).

`spawn_sprite_rigs` (spawn.rs:131-207, on `Added<SpriteRig>`): sorts (spawn.rs:149); spawns `JointRoot` (spawn.rs:176-178); inserts character `Transform`+`Visibility`, adds `JointRoot` as child (spawn.rs:182-186, manual Transform so children have a parent `GlobalTransform` before lightyear's `add_transform` runs in `PostUpdate`); `spawn_joints` (spawn.rs:188-189); attaches one `SkinnedMesh` with `joints = joint_entities` (spawn.rs:191-201); inserts `BoneEntities` (spawn.rs:205).

`spawn_joints` (spawn.rs:525-558): per bone in sorted order spawns `(Name, transform)`, resolves `parent_entity` from `bone_map` (or `joint_root_id` when `parent: None`), `add_child`, records in `bone_map` + `joint_entities`. Initial local transform from `bone_transform_from_def` (spawn.rs:561-580): xy from `default_transform`, z from slot `z_order`, `Quat::from_rotation_z(rotation.to_radians())`, scale with z=1.0.

### Evidence — world transforms, facing, billboard

Joints are a normal parent/child chain under `JointRoot` (child of the character). Bevy propagation composes `GlobalTransform`; `AnimationPlayer` writes joint locals each frame via `AnimationTargetId` targets (animation.rs:805-811); results feed `SkinnedMesh` GPU skinning. No custom per-bone CPU world-transform code.

```rust
// update_facing_from_rotation  spawn.rs:670-690  (facing from avian Rotation projected on camera right, 0.1 deadband)
let lateral = (rotation.0 * Vec3::NEG_Z).dot(camera_right_xz);
if lateral > 0.1 { facing.set_if_neq(Facing::Left) } else if lateral < -0.1 { facing.set_if_neq(Facing::Right) }
// apply_facing_to_rig  spawn.rs:724-742  (mirror whole rig via JointRoot scale.x)
transform.scale.x = match facing { Facing::Right => 1.0, Facing::Left => -1.0 };
// billboard_joint_roots  spawn.rs:697-721  (Y-rotate JointRoot to face camera, in local frame)
let world_rotation = Quat::from_rotation_y(direction.x.atan2(direction.z));
transform.rotation = parent_rotation.inverse() * world_rotation;
```

Both registered chained in `Update` (lib.rs:61-63); per-quad screen-plane handled by GPU `SpriteRigBillboardExt` (spawn.rs:267, doc spawn.rs:692-696). Adjacent: `update_shadow_twins` (shadow_twin.rs:11-36) mirrors bone world translation into shadow quads.

---

## Q5: Animation events — firing, weight gate, observer, time/name matching, loop wrap

**Direct answer:** Events are authored as `Vec<AnimEventKeyframe>` (`time`+`name`); `add_events_to_clip` registers an `add_event_fn` callback per keyframe at `ev.time` on the **override** clip. Bevy invokes it when playback crosses that time; the callback fires `AnimationEventFired` (entity-targeted) only if `weight > EVENT_WEIGHT_THRESHOLD` (0.01). A global observer logs it. Time↔position matching and loop-wrap are handled inside Bevy's evaluator; the crate only supplies time + weight gate.

### Evidence

```rust
// asset.rs:141-145 / animset.rs:13-19
pub struct AnimEventKeyframe { pub time: f32, pub name: String }          // on SpriteAnimAsset.events (asset.rs:78)
#[derive(AnimationEvent, Clone)] pub struct AnimationEventFired { pub event_name: String }
const EVENT_WEIGHT_THRESHOLD: f32 = 0.01;
```

```rust
// crates/sprite_rig/src/animset.rs:334-345
pub fn add_events_to_clip(clip: &mut AnimationClip, events: &[AnimEventKeyframe]) {
    for ev in events {
        let event = AnimationEventFired { event_name: ev.name.clone() };
        clip.add_event_fn(ev.time, move |commands, entity, _time, weight| {
            if weight > EVENT_WEIGHT_THRESHOLD {
                commands.trigger_with(event.clone(), AnimationEventTrigger { target: entity });
            }
        });
    }
}
```

- Registered at `ev.time` (clip-local timeline); the actual sample `_time` is ignored, only `weight` gates. Strict `>` → weight exactly 0.01 suppressed. This stops near-zero-weight blended locomotion clips from firing footsteps.
- Called only from `build_override_clip` (animation.rs:344); additive side omits events (doc animation.rs:354-356) — prevents double-fire since both clips share duration.

```rust
// observer  animset.rs:348-352  (registered app.add_observer(...) at lib.rs:38)
pub fn on_animation_event_fired(trigger: On<AnimationEventFired>) {
    let player_entity = trigger.trigger().target;
    trace!(character = ?player_entity, event = %trigger.event().event_name, "animation event fired");
}
```

Matching + loop wrap: Bevy's `AnimationPlayer` advances each clip's time and invokes callbacks whose registered time falls within the frame's advanced interval, handling wrap (end-of-loop + start events both fire across the boundary). Looping driven by `.repeat()` on locomotion clips (animation.rs:843, animset.rs:326) so callbacks re-fire each loop. `event_name` is fixed at build (closure captures the instance); not re-matched at runtime. Authored examples: `run.anim.ron:90-93` (`run_footstep_left/right`), `punch.anim.ron:35-37` (`punch_whoosh`), `ground_pound.anim.ron:97-99` (`ground_pound_impact`); empty clips use `events: []` (e.g. `idle.anim.ron:26`).

---

## Q6: Asset hot-reload path; write-back-to-disk mechanism

**Direct answer:** On `AssetEvent::Modified` for a `SpriteAnimAsset`, `build_animation_clips` rebuilds both clips and `clips.insert(...)` at the **existing** `AssetId` to preserve handle identity (graph keeps working). The graph/masks are **not** rebuilt — a `blend_mode` change only logs a `warn!` and requires app restart. There is **no** mechanism anywhere to write a sprite_rig asset back to disk.

### Evidence

```rust
// crates/sprite_rig/src/animation.rs:215-222 (signature)
pub fn build_animation_clips(
    mut events: MessageReader<AssetEvent<SpriteAnimAsset>>,
    source_assets: Res<Assets<SpriteAnimAsset>>,
    mut clips: ResMut<Assets<AnimationClip>>,
    mut built: ResMut<BuiltAnimations>,
    loaded_handles: Res<LoadedAnimHandles>,
    bone_defaults: Res<AnimBoneDefaults>,
) {
```

Phase 1 (animation.rs:223-244): polls `loaded_handles`, builds a `BuiltClipPair` for any not-yet-built source. Phase 2 — Modified handler (animation.rs:246-269):

```rust
for event in events.read() {
    if let AssetEvent::Modified { id } = event {
        let Some(source) = source_assets.get(*id) else { continue };
        let Some(pair) = built.0.get(id) else { continue };
        let Some(bones) = bone_defaults.0.get(id) else { continue };
        let (override_clip, additive_clip, override_bones, additive_bones) = build_clip_pair(source, bones);
        if override_bones != pair.override_bones || additive_bones != pair.additive_bones {
            warn!(asset_id = ?id, "bone blend_mode partition changed on hot-reload; mask updates require app restart");
        }
        let _ = clips.insert(pair.override_clip.id(), override_clip);   // in-place → preserves handle identity
        let _ = clips.insert(pair.additive_clip.id(), additive_clip);
    }
}
```

Trigger: Bevy's asset watcher re-loads the changed `.anim.ron` via `RonAssetPlugin`, replaces the `SpriteAnimAsset`, emits `AssetEvent::Modified`. `build_animation_clips` is in the chained `Update` set (lib.rs:47). **Not** rebuilt: `build_anim_graphs` short-circuits `if built_graphs.0.contains_key(&animset_id) { continue; }` (animation.rs:550-551) — graphs/masks never rebuilt after first build. No `AssetEvent::Modified` reader exists for `SpriteAnimSetAsset` or `SpriteRigAsset`. Whether disk watching is active depends on the binary's `AssetPlugin`/`file_watcher` config (outside sprite_rig).

**Write-back**: none. Flow is strictly disk→memory. Nearest analogues are server persistence (Q1): `persistence/mod.rs:756-758` (serialize record + atomic write), `chunk_entities.rs:431-501` (RON↔DB round-trip) — building blocks exist but are not wired to assets.

---

## Q7: `bevy_egui` integration in `dev`

**Direct answer:** `bevy_egui 0.39.1` (+ `bevy-inspector-egui 0.36.0`, `egui 0.33.3`), gated behind the `inspector` feature. `EguiPlugin::default()` added once; panel draw systems run in the `EguiPrimaryContextPass` schedule, obtain `EguiContexts`, call `ctx_mut()`, and mutate `ResMut` resources through standard widgets. **No `Painter`/custom-drawn widgets exist** — only standard egui containers/widgets.

### Evidence

```toml
# crates/dev/Cargo.toml:14   (resolved 0.39.1; egui 0.33.3 transitively)
bevy_egui = { version = "0.39", optional = true }
bevy-inspector-egui = { version = "0.36", optional = true }
# crates/dev/Cargo.toml:6-9
inspector = ["dep:bevy_egui", "dep:bevy-inspector-egui", "dep:protocol"]
world-inspector = ["inspector"]
spawn-panel = ["inspector", "dep:voxel_map_engine"]
```

```rust
// crates/dev/src/lib.rs:45-58
#[cfg(feature = "inspector")] {
    use bevy_egui::{EguiPlugin, EguiPrimaryContextPass};
    app.add_plugins(EguiPlugin::default())
       .add_systems(EguiPrimaryContextPass, draw_root_menu.run_if(inspector_enabled));
    #[cfg(feature = "world-inspector")] app.add_plugins(panels::world_inspector::WorldInspectorPanelPlugin);
    #[cfg(feature = "spawn-panel")]     app.add_plugins((panels::spawn::SpawnPanelPlugin, TerrainPanelPlugin));
}
```

Context acquisition pattern (both hand-written panels) — `draw_root_menu` (lib.rs:99-116) and `draw_spawn_panel` (spawn.rs:171-201):

```rust
let Ok(ctx) = contexts.ctx_mut() else { trace!("...EguiContexts not ready, skipping frame"); return; };
egui::Window::new("▧ World Objects").default_width(130.0).show(ctx, |ui| { ... });
```

State mutated via `ResMut` bound to widgets: `DevInspectorState` (checkboxes, lib.rs:110); `EditingMode`, `TerrainBrushSettings`, `TerrainEditHistory`, `SpawnPanelUi` (spawn.rs:203-397; terrain.rs:90-238). `WorldObjectDefRegistry` is `Option<Res<...>>` (loads at startup; loading label until ready, spawn.rs:236-239).

Run-conditions: `inspector_enabled` (lib.rs:93-96), `spawn_panel_enabled` (spawn.rs:156-158), `world_inspector_enabled` (world_inspector.rs:19-21). World inspector wraps `bevy_inspector_egui::quick::WorldInspectorPlugin::new().run_if(...)` (world_inspector.rs:13).

Panel/window inventory: (1) `draw_root_menu` `TopBottomPanel::top` (lib.rs:99); (2) `draw_spawn_panel` `egui::Window` (spawn.rs:171) dispatching tab bodies `draw_terrain_tab`/`draw_definition_placement`/`draw_world_object_edit_tab`/`draw_status_section`; (3) `draw_terrain_controls` helper (terrain.rs:90, takes `&mut egui::Ui`); (4) third-party `WorldInspectorPlugin`.

**No Painter**: grep of `crates/dev` for `Painter|line_segment|circle|rect_filled` → no matches. Only standard widgets/containers (`TopBottomPanel`, `Window`, `Frame::group`, `Grid`, `ScrollArea`, `CollapsingHeader`, `label`, `checkbox`, `selectable_value`, `ComboBox`, `Button`, `Slider`, `DragValue`). Closest to custom: the stepper composite `draw_u32_stepper`/`draw_u8_stepper` (terrain.rs:212-238), still standard widgets.

---

## Q8: New-crate / runnable-entry-point conventions; aliases; example crate

**Direct answer:** Register a crate by adding `crates/<name>` to root `Cargo.toml` `members`; depend cross-crate via `[workspace.dependencies]` + `{ workspace = true }` (or relative `{ path = "../x" }`). Runnable entry = implicit binary (`cargo run -p <crate>` → `src/main.rs`) or explicit `[lib]`+`[[bin]]`. **No `[[example]]` declarations anywhere** — examples are auto-discovered from `examples/`. No alias runs an example; a new one would mirror `client-tracy = "run -p client --features tracy"`.

### Evidence

```toml
# root Cargo.toml:2-17  (members — all under crates/)
members = [ "crates/protocol", "crates/client", "crates/client_lightyear", "crates/server",
  "crates/server_lightyear", "crates/client_web_lightyear", "crates/web", "crates/render",
  "crates/ui", "crates/dev", "crates/voxel_map_engine", "crates/sprite_rig",
  "crates/nostr_client", "crates/nostr_map_persistence" ]
```

Cross-crate dep declared once at `Cargo.toml:40-66` (`voxel_map_engine = { path = "crates/voxel_map_engine" }` :51), consumed `voxel_map_engine = { workspace = true }` (client/Cargo.toml:17); some siblings use relative paths `render`/`ui`/`dev = { path = "../x" }` (client/Cargo.toml:32-34). `git/*` crates are in `exclude` + `[patch.crates-io]` (Cargo.toml:18-37, 80-84).

Entry points — two patterns: implicit (`client` has no `[[bin]]`; `cargo run -p client` → main.rs); explicit split for `server` (`[lib] path="src/lib.rs"` + `[[bin]] name="server"`, server/Cargo.toml:6-11) and `web` (adds `crate-type=["cdylib","rlib"]`, web/Cargo.toml:6-11). These are the only two `[[bin]]` in the workspace.

```toml
# .cargo/config.toml:1-17  (full alias table)
[alias]
client = "run -p client"
client-log = "make client-log"
client-tracy = "run -p client --features tracy"
server = "run -p server"
server-log = "make server-log"
server-tracy = "run -p server --features tracy"
web = "make web"
setup = "make setup"
check-all = "check --workspace"
build-all = "build --workspace --exclude web"
web-build = "build --release -p web --target wasm32-unknown-unknown"
test-all = "make test-all"
test-native = "make test-native"
test-wasm = "make test-wasm"
```

`cargo client`/`cargo server` = direct `cargo run -p <crate>` (no cargo-make). `*-log` delegate to cargo-make tasks that tee output (`Makefile.toml:98-114`, `NO_COLOR=1 cargo run -p client 2>&1 | tee client.log`). `Makefile.toml` is `skip_core_tasks=true`, every task `workspace=false`.

Example crate: `crates/voxel_map_engine/examples/{editing,terrain,multi_instance}.rs`, auto-discovered (no `[[example]]`). `Cargo.toml` adds `[dev-dependencies] bevy = { workspace = true, default-features = true }` because the lib's `bevy` is feature-restricted (voxel_map_engine/Cargo.toml:9-25). Each example is a standalone `App`:

```rust
// crates/voxel_map_engine/examples/multi_instance.rs:1-22
use bevy::prelude::*; use voxel_map_engine::prelude::*;
fn main() { App::new().add_plugins(DefaultPlugins).add_plugins(VoxelPlugin)
    .insert_resource(ChunkGenerationEnabled).add_systems(Startup, setup)
    .add_systems(Update, (move_camera, teleport_camera)).run(); }
```

`cargo run -p voxel_map_engine --example multi_instance` works today via auto-discovery; a short alias would take the form `<name> = "run -p <crate> --example <name>"`.

---

## Q9: Client `App` composition; `AppState` Loading→Ready gating; `SpriteRigPlugin` loading

**Direct answer:** `main()` adds `DefaultPlugins` (asset root → repo `assets/`), then `ClientPlugins`, `SharedGameplayPlugin` (installs `AppStatePlugin`), …, `RenderPlugin` (pulls in `SpriteRigPlugin`), `UiPlugin`, `DevPlugin`. `AppState` is `{Loading(default), Ready}`; `check_assets_loaded` (run only `in_state(Loading)`) transitions to `Ready` once all `TrackedAssets` are loaded **and** `RelayPoolReady` **and** `IdentityLoadComplete`. `SpriteRigPlugin`'s `Startup` `load_rig_assets` loads the rig+animset and registers both handles into `TrackedAssets`.

### Evidence — main()

```rust
// crates/client/src/main.rs:65-104 (order, abridged)
App::new()
  .add_plugins(DefaultPlugins.set(AssetPlugin {
     file_path: concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets").to_string(), ..default() }))
  .add_plugins(ClientPlugins { tick_duration: Duration::from_secs_f64(1.0 / FIXED_TIMESTEP_HZ) })
  .add_plugins(SharedGameplayPlugin)          // installs AppStatePlugin (protocol/src/lib.rs:315)
  .add_plugins(NostrClientPlugin { config: NostrClientConfig { mark_identity_load_complete_on_startup: false, .. } })
  .add_plugins(ClientNetworkPlugin { .. }).add_plugins(ClientAuthPlugin)
  .add_systems(Startup, spawn_identity_store)
  .add_systems(Update, (poll_identity_store_load, handle_identity_save_requests))
  .add_plugins(ClientGameplayPlugin).add_plugins(input::ClientInputCommandPlugin)
  .add_plugins(ClientMapPlugin).add_plugins(ClientMapPublicationPlugin)
  .add_plugins(transition::ClientTransitionPlugin)
  .add_plugins(RenderPlugin)                  // → SpriteRigPlugin (render/src/lib.rs:54)
  .add_plugins(UiPlugin).add_plugins(DevPlugin)
  .add_plugins(SharedDiagnosticsPlugin).add_plugins(ClientDiagnosticsPlugin)
  .run();
```

### Evidence — state & gating

```rust
// crates/protocol/src/app_state.rs:4-9 / :11-25
pub enum AppState { #[default] Loading, Ready }
#[derive(Resource, Default)] pub struct TrackedAssets(Vec<UntypedHandle>);
// also RelayPoolReady(pub bool), IdentityLoadComplete(pub bool)  (app_state.rs:15-19)
// AppStatePlugin (app_state.rs:29-40): init_state + 3 gate resources + check_assets_loaded.run_if(in_state(Loading))
```

```rust
// check_assets_loaded  app_state.rs:42-71 (abridged)
let assets_loaded = tracked.0.iter().all(|h| asset_server.is_loaded_with_dependencies(h));
if !assets_loaded { return } if !relay_ready.0 { return } if !identity_ready.0 { return }
next_state.set(AppState::Ready);
```

`IdentityLoadComplete.0 = true` set in client `poll_identity_store_load` (main.rs:160); `mark_identity_load_complete_on_startup: false` (main.rs:76-78) means it isn't auto-satisfied. Downstream systems gate on `Ready` (e.g. `update_facing` `in_state(Ready)`, protocol/src/lib.rs:338-340).

### Evidence — SpriteRigPlugin loading

```rust
// crates/sprite_rig/src/lib.rs:24-69 (build, abridged) — added by RenderPlugin (render/src/lib.rs:54)
app.add_plugins((RonAssetPlugin::<SpriteRigAsset>::new(&["rig.ron"]),
                 RonAssetPlugin::<SpriteAnimAsset>::new(&["anim.ron"]),
                 RonAssetPlugin::<SpriteAnimSetAsset>::new(&["animset.ron"]));
app.init_resource::<RigMeshCache/SpriteImageHandles/BuiltAnimations/LoadedAnimHandles/BuiltAnimGraphs/AnimBoneDefaults>();
app.add_systems(Startup, load_rig_assets);
app.add_observer(animset::on_animation_event_fired);
app.add_systems(Update, ( ...spawn/animation chain... ).chain());
```

```rust
// load_rig_assets  crates/sprite_rig/src/lib.rs:83-104 (abridged)
fn load_rig_assets(mut commands: Commands, asset_server: Res<AssetServer>, mut tracked: ResMut<TrackedAssets>) {
    let animset_handle = asset_server.load::<SpriteAnimSetAsset>("anims/humanoid/humanoid.animset.ron");
    let rig_handle = asset_server.load::<SpriteRigAsset>("rigs/humanoid.rig.ron");
    tracked.add(animset_handle.clone()); tracked.add(rig_handle.clone());
    commands.insert_resource(RigRegistry { entries });   // CharacterType::Humanoid → RigRegistryEntry
}
```

**Standalone-editor dependencies of `SpriteRigPlugin`**: requires `protocol::app_state::TrackedAssets` to exist in the world — only `AppStatePlugin` initializes it (app_state.rs:32); `load_rig_assets` takes `ResMut<TrackedAssets>` (lib.rs:86). Also needs `CharacterType` (protocol). Render-side materials (`SpriteRigMaterial`, `BillboardMaterial`, `ShadowOnlyMaterial`) are registered by `RenderPlugin` (render/src/lib.rs:32-34), **not** by `SpriteRigPlugin`.

---

## Q10: Camera / lighting / gizmo / render-target infra in `render`; reusable viewport

**Direct answer:** Exactly **one** camera in the whole repo — a window-targeted `Camera3d` with `CameraOrbitState`, lit by one `PointLight (MainLight)` + one `DirectionalLight`. **No** `Camera2d`, **no** second/UI camera, **no** `RenderLayers` on any entity, **no** `RenderTarget::Image`, **no** render-to-texture. Gizmos: avian `PhysicsGizmos` (toggled in `dev`) and one `gizmos.cube` brush preview in `client/src/map.rs:599`. A viewport panel would need a new second camera + `RenderTarget::Image` + `EguiUserTextures::add_image` (none exist today).

### Evidence

```rust
// crates/render/src/camera.rs:77-89 (setup_camera, abridged) — Startup (render/src/lib.rs:36)
let camera_transform = Transform::from_xyz(0.0, 18.0, -36.0).looking_at(Vec3::ZERO, Dir3::Y);
commands.spawn((Camera3d::default(), camera_transform, CameraOrbitState::default()));
// + BackgroundSphere (unlit, double_sided, NotShadowCaster/Receiver)  camera.rs:91-113
```

The `Camera3d` has **no** `Camera { target, order }` override → default `RenderTarget::Window(PrimaryWindow)`.

```rust
// CameraOrbitState  camera.rs:29-48 (re-exported lib.rs:4)
pub struct CameraOrbitState { pub target_angle: f32, pub current_angle: f32, pub current_distance: f32 }
// update_camera_orbit  camera.rs:197-210: lerp_factor = (ORBIT_LERP_SPEED * dt).min(1.0); shortest_angle_diff
```

Mutated externally by client input (`control.rs:136`, `Query<(Entity,&mut CameraOrbitState,...), With<Camera3d>>`); consumed by `follow_player` (camera.rs:256-308, `desired_offset = Quat::from_rotation_y(current_angle)*BASE_OFFSET`), `steer_lock_on_camera` (camera.rs:219-248), `update_light_position` (camera.rs:425-442). Constants: `BASE_OFFSET=(0,18,-36)`, `ORBIT_LERP_SPEED=20.0`, occlusion padding/min/lerp (camera.rs:12-21).

```rust
// setup_lighting  camera.rs:177-195
PointLight { shadows_enabled: true, .. } @ BASE_LIGHT_OFFSET=(8,16,8), tagged MainLight (follows orbit)
DirectionalLight { illuminance: lux::AMBIENT_DAYLIGHT, shadows_enabled: true } looking_to (-0.5,-1,-0.5)
```

No `AmbientLight` resource anywhere — ambient is the directional's `illuminance`.

Gizmos: (a) avian `PhysicsGizmos` via `PhysicsDebugPlugin` (dev/src/lib.rs:38), default-hidden `config.enabled=false` (lib.rs:62-66), toggled by `DevHotkeyIntent::TogglePhysicsDebug` (lib.rs:69-79). (b) only manual draw — `crates/client/src/map.rs:598-604` `gizmos.cube(...)` brush preview, `#[cfg(feature="spawn-panel")]`. No other gizmo calls.

Render-target absences (exhaustive): no `Camera2d`; one `Camera3d` spawn (all other `With<Camera3d>` are read-only queries); no `RenderTarget::Image`/`ImageRenderTarget`/`add_image`/`EguiUserTextures`/`egui::Image`/`TextureUsages::RENDER_ATTACHMENT`. The sole `RenderLayers` mention is a **doc comment** in `protocol/src/billboard/shadow_only_material.rs:14-17` documenting that a "shadow-only layer" pattern was non-viable in Bevy 0.18.

**Viewport-in-egui pattern (researched, not present)** — three parts: (a) allocate an `Image` with `TextureUsages::TEXTURE_BINDING|COPY_DST|RENDER_ATTACHMENT`; (b) spawn a second `Camera3d` with `Camera { target: RenderTarget::Image(handle.into()), order: -1, .. }` (+ optional `RenderLayers::layer(1)`); (c) `let id = egui_user_textures.add_image(handle.clone_weak());` then `ui.image(egui::load::SizedTexture::new(id, vec2(w,h)))`. Sources: Bevy `examples/3d/render_to_texture.rs` (https://github.com/bevyengine/bevy/blob/main/examples/3d/render_to_texture.rs), bevy_egui `render_to_image_widget.rs` (https://github.com/vladbat00/bevy_egui), `EguiUserTextures` (https://docs.rs/bevy_egui). Shape in this repo: new `Startup` system beside `setup_camera`, image registered via `EguiContexts`, a feature-gated panel in `crates/dev/src/panels/` drawing `egui::Image` in `EguiPrimaryContextPass`; `RenderLayers` caveat per shadow_only_material.rs:14-17.

---

## Q11: Client input capture (leafwing `ActionState`) and egui-vs-game coordination

**Direct answer:** All physical input is captured into one leafwing enum `RawClientActions` (`InputManagerPlugin::<RawClientActions>`), attached to the controlled character. egui coordination uses bevy_egui's pre-aggregated `EguiWantsInput` resource (not direct `ctx().wants_pointer_input()`): when egui wants pointer/keyboard, an ownership snapshot flips to `Ui`/`Text`, and downstream `allows_*` checks suppress world/gameplay commands. A gesture latch holds the owner from press to release.

### Evidence

```rust
// crates/client/src/input/raw.rs:5-25 (RawClientActions, abridged)
pub enum RawClientActions { Move, CameraYaw, CameraRotateLeft/Right, ToggleLockOn, Jump, Ability1..4,
  PlaceVoxel, RemoveVoxel, Delete, DevTogglePhysics, DevToggleInspector, DevToggleWorldInspector,
  DevToggleSpawnPanel, PublishHomebase }
// input_control_kind raw.rs:27-35: Move=DualAxis, CameraYaw=Axis, rest=Button
// raw_client_input_map() raw.rs:38-59: WASD→Move, Q/E→rotate, Tab→lockon, Space→Jump, 1-4→abilities,
//   LMB→PlaceVoxel, RMB→RemoveVoxel, Delete key→Delete, F3-F7→dev/PublishHomebase
```

`InputManagerPlugin::<RawClientActions>::default()` added in `ClientInputCommandPlugin::build` (input/mod.rs:34). Map + `ActionState` attached to the predicted/controlled entity (`gameplay.rs:56-63`). Pipeline runs in `FixedPreUpdate` ordered by `ClientInputSet { Capture, WriteTransport, ProduceLocalCommands, Consume }` (schedule.rs:4-10), `.before(InputSystems::BufferClientInputs)` (mod.rs:41-49). Filtered writers copy raw → `NetworkedPlayerActions` gated on ownership: `write_filtered_ability_actions` (ability.rs:17,32), `write_filtered_control_actions` (control.rs:16,41,61,193), `write_lock_on_toggle` (control.rs:128), `write_dev_hotkey_intents` (editor.rs:36), `write_world_object_command_intents` (editor.rs:75).

### Evidence — egui gate

```rust
// ownership.rs:7-79: ClientInputOwnershipSnapshot { keyboard: {Gameplay,Ui,Text}, pointer: {World,Ui,TerrainBrush,WorldObject} }
// capture_egui_input_ownership  ownership.rs:138-154  (#[cfg(feature="spawn-panel")], runs in Capture, mod.rs:62-71)
let Some(egui_wants_input) = egui_wants_input else { trace!("...not ready; preserving"); return };
apply_egui_ownership_state(&mut ownership,
    egui_wants_input.wants_keyboard_input(), egui_wants_input.wants_any_pointer_input());
// apply_egui_ownership_state  ownership.rs:82-97
ownership.keyboard = if wants_keyboard_input { Text } else { Gameplay };
ownership.pointer  = if wants_pointer_input  { Ui   } else { World };
```

Uses bevy_egui `EguiWantsInput` (resource populated from each context's `wants_pointer_input()`/`wants_keyboard_input()`): `wants_any_pointer_input()` ORs `is_pointer_over_area | wants_pointer_input | is_using_pointer | is_popup_open` (bevy_egui-0.39.1/src/input.rs:1294-1299). The repo does **not** call `ctx().wants_pointer_input()` directly — it consumes the pre-aggregated resource.

After the egui gate, `capture_editing_mode_pointer_ownership` (ownership.rs:119-136) refines pointer to `TerrainBrush`/`WorldObject`/`World` from `EditingMode` but preserves `Ui` (so egui wins). Gesture latch `update_pointer_ownership` (gestures.rs:29-70) latches owner at `just_pressed(Place/RemoveVoxel)` until release; `effective_owner` (gestures.rs:17-19) returns latched-or-current; world-object commands consult it (editor.rs:118-125). Without `spawn-panel`, `capture_egui_input_ownership` isn't scheduled (mod.rs:85-93) — no egui-pointer gate, since hand-written egui windows live behind `spawn-panel`/`inspector`.

---

## Cross-Cutting Observations

- **Two-clip override/additive split is pervasive.** Every source clip → `(override_clip, additive_clip)` + two `AnimationMask`s (Q2), realized structurally in one `AnimationGraph` (`master_add` → `base_pose_blend` → `locomotion_blend`) with per-bone mask groups (Q3). Events ride only the override clip (Q2, Q5).
- **Schema carries fields the runtime ignores.** `CurveType` (per-keyframe Linear/Step) and `SpriteAnimAsset.looping` are deserialized but never read; interpolation is `UnevenSampleAutoCurve` defaults and looping is `.repeat()` at play time (Q1, Q2).
- **Default offsets and z-order are baked into curve values**, not applied post-animation, to avoid a z-restore race (Q2/Q3, animation.rs:42-44).
- **All pose evaluation is delegated to Bevy** — no custom keyframe sampler exists; only curve build + graph weight/mask manipulation (Q3).
- **Hot-reload is one-directional and partial**: clip contents rebuild in-place at the same `AssetId`, but graphs/masks never rebuild (blend_mode changes need restart); nothing writes assets to disk (Q6). RON serialization exists only in `server` persistence (Q1).
- **`JointRoot` is the single transform pivot** for both facing (scale.x ±1) and billboarding (Y-rotation) (Q4).
- **egui is feature-gated (`inspector`/`spawn-panel`)** and uses only standard widgets; input coordination flows through `EguiWantsInput` → ownership snapshot → `allows_*` gates (Q7, Q11).
- **Single window-targeted camera; no render-to-texture or RenderLayers anywhere** (Q10).
- **Conventions for a new runnable**: add to `members`, depend via workspace deps, use implicit-bin or `[lib]`+`[[bin]]`, and either an auto-discovered `examples/` file or a `run -p ...` alias (Q8). A standalone editor reusing `SpriteRigPlugin` must also supply `AppStatePlugin` (for `TrackedAssets`) and `RenderPlugin`'s materials (Q9, Q10).

## Open Areas

- **Asset disk-watching activation**: whether Bevy's file watcher (the `file_watcher` feature / `AssetPlugin.watch_for_changes_override`) is actually enabled in the client/dev binaries was not located — the `Modified` handler exists (Q6) but its trigger depends on binary-level `AssetPlugin` config not found in `sprite_rig`. Searched: `crates/sprite_rig/`, client `main.rs` `AssetPlugin.set(...)` (only `file_path` override seen at main.rs:66-69). Not verified: any `watch_for_changes_override` / `file_watcher` feature in `Cargo.toml`.
- **Bevy's exact event-interval wrap semantics** (Q5) are described from API behavior, not from reading Bevy's `AnimationPlayer` source in this pass; the crate side (registration at `ev.time`, weight gate) is fully verified.
- **`bevy_egui` `EguiUserTextures`/`add_image` exact 0.39 signature** (Q10) was cited from docs/examples, not from the vendored 0.39.1 source in this pass.
