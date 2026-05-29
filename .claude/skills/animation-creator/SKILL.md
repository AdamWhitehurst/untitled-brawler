---
name: animation-creator
description: "Author or modify sprite-rig character animations tied to abilities. MUST trigger when: (1) adding a new ability animation (.anim.ron), (2) changing keyframes, timing, or events on an existing animation, (3) syncing animation duration with ability phase changes, (4) debugging animation playback issues (snap-to-idle, wiggling rotations, mistimed effects). Does NOT trigger for ability gameplay tweaks that don't touch animation files."
---

# Animation Creator

Workflow for authoring sprite-rig animations. The system is data-driven (RON assets + animset registration, no Rust changes needed), but several non-obvious constraints make it fragile. Read this before editing any `.anim.ron`.

## Critical invariants

1. **Animation duration MUST equal ability total duration in seconds.**
   - Tick rate is **64 Hz** (`crates/protocol/src/lib.rs::FIXED_TIMESTEP_HZ`).
   - `duration_seconds = (startup + active + recovery) / 64.0`.
   - Animation longer than ability lifetime → cut mid-clip; locomotion takes the bones back and snaps to idle.
   - Animation shorter → ends early; bones hold last pose until ability ends, then snap to idle.

2. **`OnTickEffects` fire only during `AbilityPhase::Active`.**
   - Effect `tick:` is an **offset from active phase start**, not ability start.
   - Active phase must be long enough to contain the highest tick offset.
   - Startup = visual windup only. Recovery = visual settle only. Animation plays through all three phases; `ActiveAbility` lives the full duration.

3. **Final pose must match locomotion idle pose.** The bone mask releases at end of recovery — any difference produces a snap.

4. **Bones absent from `bone_timelines` are driven by locomotion** (idle/walk/run blends) during the ability. List every bone you want under animation control.

## Stepwise workflow

### 1. Gather context (read in parallel)

- `assets/abilities/{ability_id}.ability.ron` — `AbilityPhases`, `OnTickEffects` timing.
- `assets/anims/{rig}/{rig}.animset.ron` — confirm or add `ability_animations` entry.
- `assets/rigs/{rig}.rig.ron` — bone hierarchy, defaults, `pixels_per_unit`, slot z-orders.
- A reference clip (`punch.anim.ron`, `run.anim.ron`) as template.
- `file assets/sprites/{rig}/*.png` — pixel sizes ÷ `pixels_per_unit` = world size. Required to predict reach.

### 2. Co-design timing (animation ↔ ability)

Don't accept the ability's existing phases blindly. Sketch the visual beats (anticipation → contact → settle → hold), assign tick budgets, then update **both files together** if the ability needs to grow or shrink to match. Common shape:

| Beat | Phase | Notes |
|---|---|---|
| Anticipation / windup | Startup | Long enough to read the action coming |
| Contact / impact | Active (tick 0…N) | First effect at impact frame |
| Trailing effects | Active (tick N…end) | Subsequent shockwaves, projectiles |
| Settle to idle | Recovery | Final pose = idle pose for clean handoff |

Conversion: `effect_time_seconds = startup/64 + active_offset_tick/64`.

### 3. Author the .anim.ron

Required structure (use `assets/anims/{rig}/punch.anim.ron` as template):

```ron
#![enable(implicit_some)]
(
    name: "ability_id",
    duration: <seconds>,
    looping: false,
    bone_timelines: { /* one per animated bone */ },
    events: [ /* (time: <s>, name: "string") */ ],
)
```

Keyframe rules:

- **First keyframe at `time: 0.0`, last at `time: duration`.** Both endpoints should hold the neutral pose so the clip closes cleanly.
- **Translation `value` is an offset from the bone's default position** (rig.ron). The build adds the default in.
- **Rotation `value` is in degrees.** Positive = CCW.
- **Mirror-symmetric limbs**: use OPPOSITE signs (e.g. `arm_l: +X`, `arm_r: -X`). This is the natural pose for raised hands, side kicks, etc., because bones are not pre-mirrored in the rig — only screen-side.
- **Same-sign rotation on both limbs** is rare and produces visually asymmetric motion (one limb crosses the body); only use deliberately.

### 4. Avoid rotation interpolation pitfalls

`UnevenSampleAutoCurve` does shortest-arc slerp on quaternions. Failure modes:

- Two adjacent rotation keyframes **exactly ±180° apart** are mathematically ambiguous — slerp may pick either arc. Insert an intermediate keyframe (e.g. ±90°) to pin the path.
- An anticipation cock (e.g. `-20°` before a `+180°` main rotation) often forces the slerp to take the wrong arc — the arm appears to "wiggle" backward through 0° instead of swinging forward. Either drop the cock OR add intermediate keyframes that force a clean direction.
- **Rule of thumb: no two adjacent rotation keyframes more than ~150° apart.** For a 0→180° windup, write `0 → 90 → 180`.

The billboard shader (`assets/shaders/sprite_rig_billboard.wgsl`) supports the full ±π rotation range. If you observe rotations bouncing back at ±90° (visible angle = real angle until 90°, then folds backward), the shader's signed-cosine extraction has regressed — see the shader comment block.

### 5. Register the animation

Add one entry to `assets/anims/{rig}/{rig}.animset.ron`:

```ron
ability_animations: {
    "existing_id": "...",
    "your_new_id": "anims/{rig}/your_new_id.anim.ron",
},
```

No code changes, no manifest updates, no preload list. The asset loader (`crates/sprite_rig/src/animation.rs::load_animset_clips`) discovers this each frame.

### 6. Add timing events (optional)

For sound or VFX hooks at specific frames:

```ron
events: [ (time: 0.875, name: "ground_pound_impact") ],
```

`AnimationEventFired` triggers fire at the listed time; observers in gameplay/VFX systems react. Reuse existing event names if one fits; otherwise define and document the new name.

### 7. Verify

- **Restart the client if hot-reload doesn't pick up changes.** RON syntax errors silently skip the rebuild.
- Trigger the ability in-game (`cargo client` + `cargo server`). Compilation alone proves nothing — animations are runtime-only behavior.
- Watch for the symptoms in the table below and fix at the listed cause.

## Diagnosing common symptoms

| Symptom | Cause | Fix |
|---|---|---|
| Bones snap to idle mid-animation | `duration` > ability total ticks / 64 | Match `duration` to `(startup + active + recovery) / 64` |
| Effect doesn't fire | Effect tick exceeds active duration, or wrong phase | Effect ticks are offsets from active start; expand `active` |
| Rotation wiggles, doesn't reach target | Slerp ambiguity at ≥150° leg, or anticipation cock fighting main rotation | Insert intermediate keyframes ≤90° apart; drop the cock |
| Hand can't reach overhead | Sprite proportions (chibi rig: 1.0-unit arms vs. 3.33-unit head) | Combine large rotation with positive Y translation on the arm bone (e.g. `+4.2`) |
| Limb passes through body | Bones aren't pre-mirrored; same-sign rotation crosses one limb | Use mirrored signs for symmetric motion |
| Rotations fold back at ±90° | Billboard shader regression — `cos θz` extracted as `length()` instead of signed `col1.y` | Restore `assets/shaders/sprite_rig_billboard.wgsl` signed-cosine extraction |
| Animation plays partially or not at all | Animset entry missing/typo, or hot-reload skipped after RON parse error | Verify entry, restart client |
| Rotation appears reversed when facing left | `JointRoot.scale.x = -1` mirrors Z rotations visually — expected | Author for right-facing; left-facing flips automatically |

## Debugging philosophy

When animation behavior diverges from what the keyframe math should produce, suspect the **rendering pipeline** before adding workaround keyframes. The slerp implementation is well-defined; if your unambiguous path produces wrong output, the shader or bone-mask system is more likely the cause than your keyframes.

Always check sprite dimensions and bone defaults before promising a visual outcome — chibi proportions can make "raise hands above head" require translation, not just rotation, because the arm sprite is shorter than the head is tall.

## File map

| Path | Role |
|---|---|
| `assets/anims/{rig}/*.anim.ron` | Animation clip definitions |
| `assets/anims/{rig}/{rig}.animset.ron` | Maps ability IDs → clip paths |
| `assets/abilities/*.ability.ron` | Phase + effect timing |
| `assets/rigs/*.rig.ron` | Bone hierarchy, defaults, sprite metadata |
| `assets/sprites/{rig}/*.png` | Per-bone sprites |
| `assets/shaders/sprite_rig_billboard.wgsl` | Per-bone rendering with Z-rotation extraction |
| `crates/sprite_rig/src/animation.rs` | Clip building, animation graph |
| `crates/sprite_rig/src/animset.rs` | Ability triggers, return-to-locomotion |
| `crates/protocol/src/ability/effects.rs` | OnTickEffects gating to Active phase |
| `crates/protocol/src/ability/activation.rs` | Phase advancement, despawn at end of recovery |
| `crates/protocol/src/lib.rs` | `FIXED_TIMESTEP_HZ = 64.0` |
