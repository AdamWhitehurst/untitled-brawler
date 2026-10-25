# Task: Animation Editor

Build a `cargo client --example animation_editor` mode for creating and editing sprite-rig animations (`.anim.ron`), housed in a **new modular `client_animation_editor` crate** designed so it can later be embedded into the in-game client. It is a "three-linked-view" 2D skeletal editor (the pattern Spine/DragonBones/Unity converge on): an immediate-mode (egui) visual surface over the existing animation RON data — decode → mutate in-memory struct on edit → re-serialize.

## The three linked views (one shared playhead)

- **Viewport** — where a bone sits in space; drag the bone directly to pose it (FK; 2D so rotation is a scalar, translation a 2-vec). Optional 2-bone IK for legs later. Dragging on an existing key edits it; dragging between keys creates a key or prompts, per an auto-key toggle. Bone gizmos drawn over the Bevy render target.
- **Dope sheet** — when keyframes fire; a direct view of `bone_timelines`. Each bone is a labeled row group; each non-empty channel (rotation/translation/scale) is a sub-track. Diamond x = `time / duration`. Empty channels (e.g. all-empty `scale`) hidden behind a collapsed "+ add channel" affordance.
- **Curve editor** — how the value moves between keys; the `CurveType` enum made visible. Selected channel only. Per-key handles switch a segment's curve (Linear/Step/Bezier/etc.) and shape easing.

## Requirements / Criteria

- **Modularity:** logic lives in `client_animation_editor`, structured to support future in-game integration (not example-only).
- **Shared time axis:** all time views use one `t→x` mapping (same origin, width, scale, equal left gutters). Playhead is one value; its line lands at the same x in every view; scrubbing any view updates all. A keyframe's diamond and its curve node share the same x. Positions are fractions of track width, never hardcoded pixels.
- **Playhead sync:** scrubbing updates the viewport pose, highlights active keys, and moves the curve cursor simultaneously.
- **Curve editor lines:** scalar (rotation) → one curve; vector (translation/scale) → one curve per component (e.g. green=X, blue=Y), legended. One node per component per key; vector keys stack two nodes at one time under a single dope-sheet diamond. Polyline hits every node; segment shape reflects that key's `curve`. Axis labels match the actual plotted range.
- **Selection vs. playhead:** independent state; coincide only when scrubbed onto the selected key. One selection drives inspector, hierarchy, lane highlight, and curve channel together. Inspector always shows the selected key's real bone/channel/time/value/curve.
- **Data integrity:** UI is a view over one data object; every edit mutates it and all views re-read. Edits round-trip to equivalent RON (nothing invented or dropped). Keys stay time-sorted and clamped to `[0, duration]`; define overlap/reorder rules. Channel types respected (rotation scalar°, translation/scale 2-vec); empty channels render empty.
- **Events:** the `events` array gets its own bone-independent lane at the bottom (e.g. `footstep_L`/`footstep_R`), draggable in time, click-to-rename, audible on playback. Fire on time-crossing; loop wrap doesn't double-fire or miss.
- **Looping & playback:** if `looping`, value at `t=0` must equal value at `t=duration` else warn (no silent pop). Viewport pose = interpolated sample of all channels at the playhead. Frame grid/snapping derives from fps + duration.
- **Interaction:** scrub from ruler, any lane, the event lane, or the curve plot via the same mapping. Clicking a diamond/node selects (not scrubs); dragging retimes (x) or revalues (y).
