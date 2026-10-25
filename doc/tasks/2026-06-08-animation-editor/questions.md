# Research Questions

## Context

Focus on the `sprite_rig` crate (`crates/sprite_rig/`) — its asset schema, animation loading, pose sampling, bone hierarchy/rendering, and events — plus the `render` crate's camera/gizmo setup, the `dev` crate's `bevy_egui` usage, the Cargo workspace/`.cargo` alias conventions, and the client binary's plugin/AppState composition. The animation RON data lives under `assets/anims/`, `assets/rigs/`, and `assets/animset.ron` files.

## Questions

1. What is the exact serde-deserialized schema for an animation clip (`SpriteAnimAsset` and its `BoneTimeline`, rotation/translation/scale keyframe types, `CurveType`, blend mode, and event keyframe types), what RON files instantiate it, and how does `bevy_common_assets`/`RonAssetPlugin` deserialize these — is there any existing path that serializes the in-memory struct *back* to RON, and how faithfully would a round-trip preserve formatting and field ordering?

2. How is a clip converted into runtime playback — trace `build_animation_clips` / `build_clip_pair` and the per-channel curve builders: how are keyframe times and values mapped onto Bevy's `AnimationClip` curves, how is interpolation between keys performed (linear vs step vs slerp), and how do `duration`, `looping`, and the `FIXED_TIMESTEP_HZ` tick rate relate to clip length?

3. Given a bone timeline and a time `t`, how does the system compute a bone's resulting local `Transform` — where are the rig default offsets, z-order, and additive-vs-override blending applied, and what would be required to sample a *single* clip's pose at an arbitrary `t` independent of the locomotion/ability animation graph?

4. How is the bone/skeleton hierarchy represented and spawned (`SpriteRigAsset`, `BoneDef.parent`, joint entities, `BoneEntities`, topological sort), how are per-bone world transforms produced, and how does facing/billboarding (`update_facing_from_rotation`, `billboard_joint_roots`) affect a bone's on-screen position and rotation?

5. How do animation events fire during playback — trace `AnimationEventFired`, `add_events_to_clip`, the `EVENT_WEIGHT_THRESHOLD` gate, and the observer — and how are an event's `time` and `name` matched to playback position, including behavior around loop wrap?

6. How does the asset hot-reload path work (`AssetEvent::Modified` handling in `build_animation_clips`), what triggers a rebuild of clips/graphs when an `.anim.ron` changes on disk, and is there any existing mechanism to write an asset back to disk from within the running app?

7. How is `bevy_egui` integrated in the `dev` crate — exact version, plugin registration (`EguiPlugin`, `EguiPrimaryContextPass`), how panel systems obtain `EguiContexts` and mutate world state, and is there any existing use of egui's `Painter` / custom-drawn widgets (as opposed to standard widgets)?

8. What are the conventions for adding a new workspace crate and a runnable entry point — how do `.cargo/config.toml` aliases and `Makefile.toml` tasks define `cargo client`/`cargo server`, how would a `cargo run -p <crate> --example <name>` or new alias fit existing patterns, and how is the existing example crate (`voxel_map_engine/examples/`) structured?

9. How is the client `App` composed — the order of plugins, how `AppState::Loading → Ready` gating works via `TrackedAssets`, and where/how assets and resources are loaded during startup before `Ready` (relevant to a standalone editor app reusing `SpriteRigPlugin` loading)?

10. What camera, lighting, gizmo, and render-target infrastructure exists in `render` (`setup_camera`, `Camera3d`, `CameraOrbitState`, gizmo usage in `dev`/`client`), and is there any render-to-texture / second-camera / `RenderLayers` setup that a dedicated editor viewport panel could reuse?

11. How is client mouse/keyboard input captured (leafwing `ActionState`, the `client/src/input/` modules), and how does input coordinate with egui to avoid conflicts when the pointer is over a UI panel versus the 3D viewport?
