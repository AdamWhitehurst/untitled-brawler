use std::collections::{HashMap, HashSet};

use avian3d::prelude::LinearVelocity;
use bevy::{
    animation::{animated_field, AnimatedBy, AnimationTargetId},
    math::curve::sample_curves::UnevenSampleAutoCurve,
    prelude::*,
};

use crate::asset::{SpriteAnimAsset, SpriteAnimSetAsset, SpriteRigAsset};
use crate::curve::SegmentedKeyframeCurve;
use crate::spawn::{AnimSetRef, BoneEntities};
use crate::RigRegistry;
use protocol::CharacterMarker;

/// Override + additive `AnimationClip` handles derived from a single source `.anim.ron`.
///
/// Bones in the source's `bone_timelines` are partitioned by `blend_mode`. The override
/// clip carries curves for `Override` bones (plus hold-at-default fillers for unmentioned
/// bones, used by locomotion blend normalization). The additive clip carries delta curves
/// for `Additive` bones only. Both handles are always present; if a side has no bones in
/// its partition, the underlying clip is empty (no curves) and the corresponding mask is
/// 0, so trigger logic skips creating a layer entry for that side.
#[derive(Clone)]
pub struct BuiltClipPair {
    pub override_clip: Handle<AnimationClip>,
    pub additive_clip: Handle<AnimationClip>,
    pub override_bones: AnimationMask,
    pub additive_bones: AnimationMask,
}

/// Maps `SpriteAnimAsset` ID to the derived override + additive clip pair.
#[derive(Resource, Default)]
pub struct BuiltAnimations(pub HashMap<AssetId<SpriteAnimAsset>, BuiltClipPair>);

/// Maps anim asset path string to its strong `Handle<SpriteAnimAsset>`.
///
/// Keeps a strong handle alive so Bevy doesn't garbage-collect the asset before it loads.
#[derive(Resource, Default)]
pub struct LoadedAnimHandles(pub HashMap<String, Handle<SpriteAnimAsset>>);

/// Bone default transform + z-order for animation curve building.
///
/// Combines `BoneDef` position data with slot z-order so animation curves produce
/// correct z values directly, avoiding post-animation z-depth restore races.
#[derive(Clone)]
pub struct BoneAnimDefault {
    pub name: String,
    pub default_xy: Vec2,
    pub z_order: f32,
}

/// Maps each `SpriteAnimAsset` ID to its rig's bone animation defaults.
///
/// Built from animset→rig references so that clip building can convert offset-based
/// translation keyframes to absolute bone positions (with baked z-order) and auto-fill
/// missing bones.
#[derive(Resource, Default)]
pub struct AnimBoneDefaults(pub HashMap<AssetId<SpriteAnimAsset>, Vec<BoneAnimDefault>>);

/// Pre-built animation graph for one animset type, shared across character instances.
pub struct BuiltAnimGraph {
    /// Handle to the graph asset.
    pub graph_handle: Handle<AnimationGraph>,
    /// Maps locomotion clip path to its `AnimationNodeIndex` (under the locomotion blend).
    pub node_map: HashMap<String, AnimationNodeIndex>,
    /// Locomotion entries in order of speed_threshold.
    pub locomotion_entries: Vec<LocomotionNodeEntry>,
    /// Index of the locomotion blend node; used as the locomotion layer's `node_index`.
    pub locomotion_blend_node: AnimationNodeIndex,
    /// Per-ability override + additive node indices and bone-claim masks.
    pub ability_nodes: HashMap<String, AbilityNodePair>,
    /// Mask covering every registered bone group; the locomotion layer claims all of them.
    pub all_bones_mask: AnimationMask,
}

/// A single ability's override and additive graph nodes plus the bone-claim masks for each.
///
/// `override_claims` is the set of bones the ability writes in `Override` mode (zero if none);
/// the override clip is parented to `base_blend` so it participates in the priority-mask
/// chain alongside locomotion. `additive_claims` is the set of bones the ability writes in
/// `Additive` mode (zero if none); the additive clip is parented directly to the master
/// `Add` node so its delta contributions sum on top of whatever the override system wrote.
///
/// Either `override_claims` or `additive_claims` may be 0; trigger logic skips creating a
/// layer entry for a side whose mask is empty.
#[derive(Clone, Copy)]
pub struct AbilityNodePair {
    pub override_node: AnimationNodeIndex,
    pub additive_node: AnimationNodeIndex,
    pub override_claims: AnimationMask,
    pub additive_claims: AnimationMask,
}

/// A locomotion clip node and its speed threshold for blend weight calculation.
pub struct LocomotionNodeEntry {
    pub node_index: AnimationNodeIndex,
    pub speed_threshold: f32,
}

/// Pre-built animation graphs, one per animset asset.
#[derive(Resource, Default)]
pub struct BuiltAnimGraphs(pub HashMap<AssetId<SpriteAnimSetAsset>, BuiltAnimGraph>);

/// Source-clip ids whose `blend_mode` partition changed on hot-reload, requiring a graph
/// rebuild for every animset that references them. A clip-only change emits no
/// `SpriteAnimSetAsset` Modified event, so `build_animation_clips` pushes the *clip* id here
/// and `build_anim_graphs` maps clip→animset while draining (keeps the producer's param list
/// small — the consumer already has registry + animset access).
#[derive(Resource, Default)]
pub struct GraphRebuildQueue(pub HashSet<AssetId<SpriteAnimAsset>>);

/// Smoothed blend weights for locomotion clips, lerped toward target each frame.
#[derive(Component)]
pub struct LocomotionBlendWeights {
    pub weights: Vec<f32>,
}

/// Makes locomotion weights follow their target without gameplay transition smoothing.
#[derive(Component)]
pub struct ImmediateLocomotionBlend;

/// Rate at which blend weights converge toward targets (per second).
const BLEND_LERP_SPEED: f32 = 10.0;

/// Returns this frame's blend interpolation factor for the selected response mode.
fn locomotion_blend_lerp_factor(dt: f32, immediate: bool) -> f32 {
    if immediate {
        1.0
    } else {
        (BLEND_LERP_SPEED * dt).min(1.0)
    }
}

/// Populates `AnimBoneDefaults` by resolving each animset's rig and mapping its bone
/// definitions to every animation clip referenced by that animset.
///
/// Runs each frame until all animset clip→rig mappings are established.
pub fn populate_anim_bone_defaults(
    registry: Res<RigRegistry>,
    animset_assets: Res<Assets<SpriteAnimSetAsset>>,
    rig_assets: Res<Assets<SpriteRigAsset>>,
    loaded_handles: Res<LoadedAnimHandles>,
    mut bone_defaults: ResMut<AnimBoneDefaults>,
    asset_server: Res<AssetServer>,
) {
    for entry in registry.entries.values() {
        let Some(animset) = animset_assets.get(&entry.animset_handle) else {
            continue; // not loaded yet — expected during startup
        };
        let rig_handle = asset_server.load::<SpriteRigAsset>(&animset.rig);
        let Some(rig) = rig_assets.get(&rig_handle) else {
            continue; // not loaded yet — expected during startup
        };

        let slot_z_orders: HashMap<&str, f32> = rig
            .slots
            .iter()
            .map(|slot| (slot.bone.as_str(), slot.z_order))
            .collect();

        let bone_anim_defaults: Vec<BoneAnimDefault> = rig
            .bones
            .iter()
            .map(|bone| BoneAnimDefault {
                name: bone.name.clone(),
                default_xy: bone.default_transform.translation,
                z_order: slot_z_orders
                    .get(bone.name.as_str())
                    .copied()
                    .unwrap_or(0.0),
            })
            .collect();

        for clip_path in collect_animset_clip_paths(animset) {
            if let Some(anim_handle) = loaded_handles.0.get(clip_path) {
                let anim_id = anim_handle.id();
                if !bone_defaults.0.contains_key(&anim_id) {
                    bone_defaults.0.insert(anim_id, bone_anim_defaults.clone());
                }
            }
        }
    }
}

/// Loads all animation clips referenced by animset assets and records path-to-id mapping.
///
/// Polls the `RigRegistry` entries each frame until all animset clip paths are loaded.
/// Idempotent: skips animsets whose clips are already in `LoadedAnimHandles`.
pub fn load_animset_clips(
    registry: Res<RigRegistry>,
    animset_assets: Res<Assets<SpriteAnimSetAsset>>,
    asset_server: Res<AssetServer>,
    mut loaded_handles: ResMut<LoadedAnimHandles>,
) {
    for entry in registry.entries.values() {
        let Some(animset) = animset_assets.get(&entry.animset_handle) else {
            continue; // not loaded yet — expected during startup
        };

        let paths: Vec<String> = collect_animset_clip_paths(animset)
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        for clip_path in paths {
            if !loaded_handles.0.contains_key(&clip_path) {
                let handle = asset_server.load::<SpriteAnimAsset>(&clip_path);
                loaded_handles.0.insert(clip_path, handle);
            }
        }
    }
}

/// Collects all clip paths referenced by an animset.
fn collect_animset_clip_paths(animset: &SpriteAnimSetAsset) -> Vec<&str> {
    let mut paths: Vec<&str> = animset
        .locomotion
        .entries
        .iter()
        .map(|e| e.clip.as_str())
        .chain(animset.ability_animations.values().map(|s| s.as_str()))
        .collect();
    if let Some(ref clip_path) = animset.hit_react {
        paths.push(clip_path.as_str());
    }
    paths
}

/// Builds override + additive `AnimationClip` pairs from loaded `SpriteAnimAsset` data.
///
/// Uses a polling approach: iterates `LoadedAnimHandles` and builds pairs for any source
/// assets that don't yet have a built pair. Hot-reload via `AssetEvent::Modified` updates
/// both clips in place, preserving handle identity so existing graph nodes keep working.
///
/// A hot-reload that changes a bone's `blend_mode` flips the override/additive partition,
/// which the existing graph nodes' masks no longer match — the clip id is pushed onto
/// `GraphRebuildQueue` so `build_anim_graphs` rebuilds affected graphs in place this frame.
pub fn build_animation_clips(
    mut events: MessageReader<AssetEvent<SpriteAnimAsset>>,
    source_assets: Res<Assets<SpriteAnimAsset>>,
    mut clips: ResMut<Assets<AnimationClip>>,
    mut built: ResMut<BuiltAnimations>,
    loaded_handles: Res<LoadedAnimHandles>,
    bone_defaults: Res<AnimBoneDefaults>,
    mut rebuild_queue: ResMut<GraphRebuildQueue>,
) {
    // Build pairs for newly-available source assets (polling handles hot-reload timing).
    for (_path, anim_handle) in loaded_handles.0.iter() {
        let anim_id = anim_handle.id();
        if built.0.contains_key(&anim_id) {
            continue;
        }
        let Some(source) = source_assets.get(anim_id) else {
            continue;
        };
        let Some(bones) = bone_defaults.0.get(&anim_id) else {
            continue;
        };
        let (override_clip, additive_clip, override_bones, additive_bones) =
            build_clip_pair(source, bones);
        let pair = BuiltClipPair {
            override_clip: clips.add(override_clip),
            additive_clip: clips.add(additive_clip),
            override_bones,
            additive_bones,
        };
        built.0.insert(anim_id, pair);
    }

    // Hot-reload: rebuild both clips in place when the source asset is modified on disk.
    for event in events.read() {
        if let AssetEvent::Modified { id } = event {
            let Some(source) = source_assets.get(*id) else {
                continue;
            };
            let Some(pair) = built.0.get_mut(id) else {
                continue;
            };
            let Some(bones) = bone_defaults.0.get(id) else {
                continue;
            };
            let (override_clip, additive_clip, override_bones, additive_bones) =
                build_clip_pair(source, bones);
            if override_bones != pair.override_bones || additive_bones != pair.additive_bones {
                pair.override_bones = override_bones;
                pair.additive_bones = additive_bones;
                rebuild_queue.0.insert(*id);
            }
            let _ = clips.insert(pair.override_clip.id(), override_clip);
            let _ = clips.insert(pair.additive_clip.id(), additive_clip);
        }
    }
}

/// Rebuilds a source clip's override + additive `AnimationClip`s in place from an edited
/// `SpriteAnimAsset`, preserving the pair's handles so every graph node referencing them
/// re-evaluates the new curves. For live-edit tooling (the animation editor); the game's
/// hot-reload path goes through `build_animation_clips`' Modified handler instead.
/// Returns the rebuilt `(override_bones, additive_bones)` masks so callers can detect a
/// partition flip.
pub fn rebuild_clip_pair_in_place(
    source: &SpriteAnimAsset,
    bone_defaults: &[BoneAnimDefault],
    pair: &BuiltClipPair,
    clips: &mut Assets<AnimationClip>,
) -> (AnimationMask, AnimationMask) {
    let (override_clip, additive_clip, override_bones, additive_bones) =
        build_clip_pair(source, bone_defaults);
    let _ = clips.insert(pair.override_clip.id(), override_clip);
    let _ = clips.insert(pair.additive_clip.id(), additive_clip);
    (override_bones, additive_bones)
}

/// Converts a `SpriteAnimAsset` into an override + additive `AnimationClip` pair, plus the
/// bone masks identifying which bones each side animates.
fn build_clip_pair(
    anim: &SpriteAnimAsset,
    bone_defaults: &[BoneAnimDefault],
) -> (AnimationClip, AnimationClip, AnimationMask, AnimationMask) {
    let (override_bones, additive_bones) = partition_bones_by_mode(anim, bone_defaults);
    let override_clip = build_override_clip(anim, bone_defaults);
    let additive_clip = build_additive_clip(anim, bone_defaults);
    (override_clip, additive_clip, override_bones, additive_bones)
}

/// Returns `(override_bones_mask, additive_bones_mask)` partitioning the source clip's
/// animated bones by their declared `blend_mode`. Bone bit indices match the rig's
/// `bone_defaults` order (identical to `register_bone_mask_groups`).
fn partition_bones_by_mode(
    anim: &SpriteAnimAsset,
    bone_defaults: &[BoneAnimDefault],
) -> (AnimationMask, AnimationMask) {
    use crate::asset::BoneBlendMode;
    let mut override_mask: AnimationMask = 0;
    let mut additive_mask: AnimationMask = 0;
    for (i, bone) in bone_defaults.iter().enumerate() {
        if let Some(timeline) = anim.bone_timelines.get(&bone.name) {
            let bit = 1u64 << i;
            match timeline.blend_mode {
                BoneBlendMode::Override => override_mask |= bit,
                BoneBlendMode::Additive => additive_mask |= bit,
            }
        }
    }
    (override_mask, additive_mask)
}

/// Builds the override-mode clip: bones with `Override` blend mode get real curves with
/// default + z baked in; bones not in that partition (including additive ones and bones
/// not mentioned at all) get hold-at-default fillers so weighted blending stays correct
/// when this clip lives inside a Bevy `Blend` node (locomotion, base_blend).
fn build_override_clip(anim: &SpriteAnimAsset, bone_defaults: &[BoneAnimDefault]) -> AnimationClip {
    use crate::asset::BoneBlendMode;
    let mut clip = AnimationClip::default();
    clip.set_duration(anim.duration);

    for bone_default in bone_defaults {
        let target_id =
            AnimationTargetId::from_names(std::iter::once(&Name::new(bone_default.name.clone())));

        match anim.bone_timelines.get(&bone_default.name) {
            Some(timeline) if matches!(timeline.blend_mode, BoneBlendMode::Override) => {
                add_rotation_curve(&mut clip, target_id, timeline, anim.duration);
                add_translation_curve(
                    &mut clip,
                    target_id,
                    timeline,
                    bone_default.default_xy,
                    bone_default.z_order,
                    anim.duration,
                );
                add_scale_curve(&mut clip, target_id, timeline, anim.duration);
            }
            _ => {
                add_hold_at_default_curves(
                    &mut clip,
                    target_id,
                    bone_default.default_xy,
                    bone_default.z_order,
                    anim.duration,
                );
            }
        }
    }

    crate::animset::add_events_to_clip(&mut clip, &anim.events);

    clip
}

/// Builds the additive-mode clip: bones with `Additive` blend mode get delta curves
/// (no default offset, no z-order, identity-based rotation). Bones not in this partition
/// receive no curves — under a Bevy `Add` parent, missing curves contribute nothing,
/// which is the correct identity element for additive composition.
///
/// Events are intentionally omitted; they're attached to the override clip side only,
/// which prevents double-firing since both clips share the same source duration.
/// Scale curves are unsupported in additive mode and are dropped with a `warn!`.
fn build_additive_clip(anim: &SpriteAnimAsset, bone_defaults: &[BoneAnimDefault]) -> AnimationClip {
    use crate::asset::BoneBlendMode;
    let mut clip = AnimationClip::default();
    clip.set_duration(anim.duration);

    for bone_default in bone_defaults {
        let Some(timeline) = anim.bone_timelines.get(&bone_default.name) else {
            continue;
        };
        if !matches!(timeline.blend_mode, BoneBlendMode::Additive) {
            continue;
        }

        let target_id =
            AnimationTargetId::from_names(std::iter::once(&Name::new(bone_default.name.clone())));
        add_additive_rotation_curve(&mut clip, target_id, timeline, anim.duration);
        add_additive_translation_curve(&mut clip, target_id, timeline, anim.duration);
        if !timeline.scale.is_empty() {
            warn!(
                bone = %bone_default.name,
                anim = %anim.name,
                "scale curves are not supported in additive blend mode; dropped",
            );
        }
    }

    clip
}

/// Adds an additive rotation curve interpreting keyframe values as delta rotations: two
/// or more keys interpolate, exactly one holds its delta, none adds no curve (identity
/// contribution under the `Add` node).
fn add_additive_rotation_curve(
    clip: &mut AnimationClip,
    target_id: AnimationTargetId,
    timeline: &crate::asset::BoneTimeline,
    duration: f32,
) {
    let property = animated_field!(Transform::rotation);
    match timeline.rotation.as_slice() {
        [] => {}
        [key] => add_constant_curve(
            clip,
            target_id,
            property,
            Quat::from_rotation_z(key.value.to_radians()),
            duration,
        ),
        keyframes => {
            let keys = keyframes
                .iter()
                .map(|k| (k.time, Quat::from_rotation_z(k.value.to_radians()), k.curve))
                .collect();
            let curve = SegmentedKeyframeCurve::new(keys)
                .expect("Additive rotation timeline needs >= 2 keyframes");
            clip.add_curve_to_target(target_id, AnimatableCurve::new(property, curve));
        }
    }
}

/// Adds an additive translation curve interpreting keyframe values as deltas (no default
/// offset, no z-order — those are owned by the override layer underneath): two or more
/// keys interpolate, exactly one holds its delta, none adds no curve.
fn add_additive_translation_curve(
    clip: &mut AnimationClip,
    target_id: AnimationTargetId,
    timeline: &crate::asset::BoneTimeline,
    duration: f32,
) {
    let property = animated_field!(Transform::translation);
    match timeline.translation.as_slice() {
        [] => {}
        [key] => add_constant_curve(
            clip,
            target_id,
            property,
            Vec3::new(key.value.x, key.value.y, 0.0),
            duration,
        ),
        keyframes => {
            let keys = keyframes
                .iter()
                .map(|k| (k.time, Vec3::new(k.value.x, k.value.y, 0.0), k.curve))
                .collect();
            let curve = SegmentedKeyframeCurve::new(keys)
                .expect("Additive translation timeline needs >= 2 keyframes");
            clip.add_curve_to_target(target_id, AnimatableCurve::new(property, curve));
        }
    }
}

/// Adds a constant curve holding `value` across the whole clip. Shared by the zero-key
/// fallbacks (hold at default/identity) and the single-key bake (hold at the key — the
/// standard one-key semantic, and what makes the editor's first auto-keyed key visible).
fn add_constant_curve<P>(
    clip: &mut AnimationClip,
    target_id: AnimationTargetId,
    property: P,
    value: P::Property,
    duration: f32,
) where
    P: bevy::animation::animation_curves::AnimatableProperty + Clone,
    P::Property: Clone,
    UnevenSampleAutoCurve<P::Property>:
        bevy::animation::animation_curves::AnimationCompatibleCurve<P::Property>,
{
    let curve = UnevenSampleAutoCurve::new([(0.0, value.clone()), (duration, value)])
        .expect("constant curve has two strictly increasing sample times");
    clip.add_curve_to_target(target_id, AnimatableCurve::new(property, curve));
}

/// Adds identity rotation + default-position translation curves for bones not in the animation.
fn add_hold_at_default_curves(
    clip: &mut AnimationClip,
    target_id: AnimationTargetId,
    default_xy: Vec2,
    z_order: f32,
    duration: f32,
) {
    add_constant_curve(
        clip,
        target_id,
        animated_field!(Transform::rotation),
        Quat::IDENTITY,
        duration,
    );
    add_constant_curve(
        clip,
        target_id,
        animated_field!(Transform::translation),
        Vec3::new(default_xy.x, default_xy.y, z_order),
        duration,
    );
}

/// Adds a rotation curve from keyframes: two or more interpolate, exactly one holds its
/// value for the whole clip, none holds identity.
fn add_rotation_curve(
    clip: &mut AnimationClip,
    target_id: AnimationTargetId,
    timeline: &crate::asset::BoneTimeline,
    duration: f32,
) {
    let property = animated_field!(Transform::rotation);
    match timeline.rotation.as_slice() {
        [] => add_constant_curve(clip, target_id, property, Quat::IDENTITY, duration),
        [key] => add_constant_curve(
            clip,
            target_id,
            property,
            Quat::from_rotation_z(key.value.to_radians()),
            duration,
        ),
        keyframes => {
            let keys = keyframes
                .iter()
                .map(|k| (k.time, Quat::from_rotation_z(k.value.to_radians()), k.curve))
                .collect();
            let curve =
                SegmentedKeyframeCurve::new(keys).expect("Rotation timeline needs >= 2 keyframes");
            clip.add_curve_to_target(target_id, AnimatableCurve::new(property, curve));
        }
    }
}

/// Adds a translation curve with bone default offset and z-order baked in: two or more
/// keys interpolate, exactly one holds its offset for the whole clip, none holds the
/// default position.
fn add_translation_curve(
    clip: &mut AnimationClip,
    target_id: AnimationTargetId,
    timeline: &crate::asset::BoneTimeline,
    default_xy: Vec2,
    z_order: f32,
    duration: f32,
) {
    let property = animated_field!(Transform::translation);
    let position_of =
        |value: Vec2| Vec3::new(default_xy.x + value.x, default_xy.y + value.y, z_order);
    match timeline.translation.as_slice() {
        [] => add_constant_curve(
            clip,
            target_id,
            property,
            Vec3::new(default_xy.x, default_xy.y, z_order),
            duration,
        ),
        [key] => add_constant_curve(clip, target_id, property, position_of(key.value), duration),
        keyframes => {
            let keys = keyframes
                .iter()
                .map(|k| (k.time, position_of(k.value), k.curve))
                .collect();
            let curve = SegmentedKeyframeCurve::new(keys)
                .expect("Translation timeline needs >= 2 keyframes");
            clip.add_curve_to_target(target_id, AnimatableCurve::new(property, curve));
        }
    }
}

/// Adds a scale curve from keyframes: two or more interpolate, exactly one holds its
/// value, none adds no curve (Bevy's default scale (1,1,1) is correct).
fn add_scale_curve(
    clip: &mut AnimationClip,
    target_id: AnimationTargetId,
    timeline: &crate::asset::BoneTimeline,
    duration: f32,
) {
    let property = animated_field!(Transform::scale);
    match timeline.scale.as_slice() {
        [] => {}
        [key] => add_constant_curve(
            clip,
            target_id,
            property,
            Vec3::new(key.value.x, key.value.y, 1.0),
            duration,
        ),
        keyframes => {
            let keys = keyframes
                .iter()
                .map(|k| (k.time, Vec3::new(k.value.x, k.value.y, 1.0), k.curve))
                .collect();
            let curve =
                SegmentedKeyframeCurve::new(keys).expect("Scale timeline needs >= 2 keyframes");
            clip.add_curve_to_target(target_id, AnimatableCurve::new(property, curve));
        }
    }
}

/// Builds each animset's `AnimationGraph` once its clips are ready, and rebuilds it in place
/// when the animset (or a referenced clip's blend_mode partition) changes. A single build
/// path: the only branch is new-handle (`graphs.add`) vs in-place
/// (`graphs.insert(existing_id, ..)` — mirrors the clip hot-reload idiom, preserving the
/// `AnimationGraphHandle` every rig already holds). On rebuild, players bound to the animset
/// are reset so stale `AnimationNodeIndex`es never leak; `start_locomotion_blend` re-seeds
/// them later in this frame's chain.
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
    mut players: Query<(
        &mut AnimationPlayer,
        &mut crate::animset::ActiveAnimLayers,
        &AnimSetRef,
    )>,
) {
    let to_process = collect_animsets_to_build(
        &mut animset_events,
        &mut rebuild_queue,
        &registry,
        &animset_assets,
        &loaded_handles,
        &built_graphs,
    );

    for animset_id in to_process {
        let Some(entry) = registry
            .entries
            .values()
            .find(|e| e.animset_handle.id() == animset_id)
        else {
            trace!(
                ?animset_id,
                "(re)build requested for unknown animset; ignoring"
            );
            continue;
        };
        let Some(animset) = animset_assets.get(&entry.animset_handle) else {
            continue; // not loaded yet — expected during startup
        };
        if !all_clips_built(animset, &loaded_handles, &built_anims) {
            continue; // clips not all built yet — expected during startup; first-build polls again
        }

        let (
            graph,
            node_map,
            locomotion_entries,
            locomotion_blend_node,
            ability_nodes,
            all_bones_mask,
        ) = build_graph_for_animset(animset, &loaded_handles, &built_anims, &bone_defaults);

        let graph_handle = match built_graphs.0.get(&animset_id) {
            // Rebuild: swap in place at the existing handle, then reset bound players.
            Some(existing) => {
                let graph_handle = existing.graph_handle.clone();
                let _ = graphs.insert(graph_handle.id(), graph);
                reset_animation_players_for(animset_id, &mut players);
                graph_handle
            }
            // First build: fresh handle, no players bound yet.
            None => graphs.add(graph),
        };

        built_graphs.0.insert(
            animset_id,
            BuiltAnimGraph {
                graph_handle,
                node_map,
                locomotion_entries,
                locomotion_blend_node,
                ability_nodes,
                all_bones_mask,
            },
        );
    }
}

/// Collects the deduped set of animset ids to (re)build this frame:
/// - first-build: any registry entry not yet in `built_graphs` (the per-frame poll);
/// - rebuild: animset `Modified` events;
/// - rebuild: drained `GraphRebuildQueue` clip ids (blend_mode partition flips), mapped to
///   every animset whose clip paths resolve to a queued clip.
fn collect_animsets_to_build(
    animset_events: &mut MessageReader<AssetEvent<SpriteAnimSetAsset>>,
    rebuild_queue: &mut GraphRebuildQueue,
    registry: &RigRegistry,
    animset_assets: &Assets<SpriteAnimSetAsset>,
    loaded_handles: &LoadedAnimHandles,
    built_graphs: &BuiltAnimGraphs,
) -> HashSet<AssetId<SpriteAnimSetAsset>> {
    let mut to_process = HashSet::new();
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
    if rebuild_queue.0.is_empty() {
        return to_process;
    }
    let flipped_clips: HashSet<AssetId<SpriteAnimAsset>> = rebuild_queue.0.drain().collect();
    for entry in registry.entries.values() {
        let Some(animset) = animset_assets.get(&entry.animset_handle) else {
            continue; // not loaded yet — expected during startup
        };
        let references_flipped_clip = collect_animset_clip_paths(animset).iter().any(|path| {
            loaded_handles
                .0
                .get(*path)
                .is_some_and(|handle| flipped_clips.contains(&handle.id()))
        });
        if references_flipped_clip {
            to_process.insert(entry.animset_handle.id());
        }
    }
    to_process
}

/// Clears active playback and layer state on every rig bound to `animset_id`, so that after
/// an in-place graph swap `start_locomotion_blend` + `recompute_layer_masks` rebuild a clean
/// stack against the new node indices later this frame. Ability casts in flight are dropped
/// (acceptable: a graph rebuild is an authoring/hot-reload event, not a gameplay tick).
fn reset_animation_players_for(
    animset_id: AssetId<SpriteAnimSetAsset>,
    players: &mut Query<(
        &mut AnimationPlayer,
        &mut crate::animset::ActiveAnimLayers,
        &AnimSetRef,
    )>,
) {
    for (mut player, mut layers, anim_set_ref) in players.iter_mut() {
        if anim_set_ref.0.id() != animset_id {
            continue; // rig bound to a different animset — untouched by this rebuild
        }
        player.stop_all();
        layers.entries.clear();
    }
}

/// Returns true when every clip path in the animset has a built `AnimationClip`.
fn all_clips_built(
    animset: &SpriteAnimSetAsset,
    loaded_handles: &LoadedAnimHandles,
    built_anims: &BuiltAnimations,
) -> bool {
    let all_paths = animset
        .locomotion
        .entries
        .iter()
        .map(|e| &e.clip)
        .chain(animset.ability_animations.values())
        .chain(animset.hit_react.iter());

    all_paths.into_iter().all(|path| {
        loaded_handles
            .0
            .get(path)
            .and_then(|handle| built_anims.0.get(&handle.id()))
            .is_some()
    })
}

/// Constructs the per-character animation graph. Topology:
///
/// ```text
/// graph.root (implicit Blend, weight 1.0)
/// └── master_add (Add)
///     ├── base_pose_blend (Blend) — resolves the absolute "override system" pose
///     │   ├── locomotion_blend (Blend) — locomotion clips, runtime weight-blended by speed
///     │   │   ├── idle, walk, run, …
///     │   ├── ability_X_override_clip (with mask, weight 1.0)
///     │   ├── ability_Y_override_clip (with mask, weight 1.0)
///     │   └── …
///     ├── ability_X_additive_clip (with mask, weight 1.0)
///     ├── ability_Y_additive_clip (with mask, weight 1.0)
///     └── …
/// ```
///
/// `locomotion_blend` is kept as a separate inner Blend (its node_index is what's stored in
/// `BuiltAnimGraph::locomotion_blend_node`) so that `recompute_layer_masks` mutating its
/// mask only excludes locomotion clips from override-claimed bones — not the override
/// ability clips that are siblings one level up under `base_pose_blend`. Earlier we tried
/// flattening these into one Blend; that propagated the locomotion-layer mask to override
/// ability clips and prevented them from writing their claimed bones (the reason "ability
/// animations don't play at all" surfaced after Increment 2).
///
/// `master_add` (Bevy `Add`) sums children: `base_pose_blend`'s absolute pose (first child)
/// plus each additive ability's delta. Without additives, master_add has one child and
/// passes through unchanged.
fn build_graph_for_animset(
    animset: &SpriteAnimSetAsset,
    loaded_handles: &LoadedAnimHandles,
    built_anims: &BuiltAnimations,
    bone_defaults: &AnimBoneDefaults,
) -> (
    AnimationGraph,
    HashMap<String, AnimationNodeIndex>,
    Vec<LocomotionNodeEntry>,
    AnimationNodeIndex,
    HashMap<String, AbilityNodePair>,
    AnimationMask,
) {
    let mut graph = AnimationGraph::new();
    let mut node_map = HashMap::new();
    let mut locomotion_entries = Vec::new();
    let mut ability_nodes = HashMap::new();

    let bone_names = resolve_bone_names(animset, loaded_handles, bone_defaults);
    register_bone_mask_groups(&mut graph, &bone_names);
    let all_bones_mask = compute_all_bones_mask(bone_names.len());

    let master_add = graph.add_additive_blend(1.0, graph.root);
    let base_pose_blend = graph.add_blend(1.0, master_add);
    let locomotion_blend = graph.add_blend(1.0, base_pose_blend);

    for loco_entry in &animset.locomotion.entries {
        let pair = resolve_clip_pair(&loco_entry.clip, loaded_handles, built_anims);
        if pair.additive_bones != 0 {
            warn!(
                clip = %loco_entry.clip,
                "locomotion clip declared additive bones; ignored — locomotion is override-only",
            );
        }
        let node_idx = graph.add_clip(pair.override_clip.clone(), 1.0, locomotion_blend);
        node_map.insert(loco_entry.clip.clone(), node_idx);
        locomotion_entries.push(LocomotionNodeEntry {
            node_index: node_idx,
            speed_threshold: loco_entry.speed_threshold,
        });
    }

    for (ability_id, clip_path) in &animset.ability_animations {
        let pair = resolve_clip_pair(clip_path, loaded_handles, built_anims);

        // Override side: sibling of `locomotion_blend` under `base_pose_blend`.
        let override_exclusion = (!pair.override_bones) & all_bones_mask;
        let override_node = graph.add_clip_with_mask(
            pair.override_clip.clone(),
            override_exclusion,
            1.0,
            base_pose_blend,
        );

        // Additive side: direct child of `master_add`.
        let additive_exclusion = (!pair.additive_bones) & all_bones_mask;
        let additive_node = graph.add_clip_with_mask(
            pair.additive_clip.clone(),
            additive_exclusion,
            1.0,
            master_add,
        );

        ability_nodes.insert(
            ability_id.clone(),
            AbilityNodePair {
                override_node,
                additive_node,
                override_claims: pair.override_bones,
                additive_claims: pair.additive_bones,
            },
        );
    }

    if let Some(ref clip_path) = animset.hit_react {
        let pair = resolve_clip_pair(clip_path, loaded_handles, built_anims);
        let node_idx = graph.add_clip(pair.override_clip.clone(), 1.0, base_pose_blend);
        node_map.insert(clip_path.clone(), node_idx);
    }

    (
        graph,
        node_map,
        locomotion_entries,
        locomotion_blend,
        ability_nodes,
        all_bones_mask,
    )
}

/// Returns a mask with the low `bone_count` bits set; bits above that correspond to no
/// registered bone group and are kept 0 so logged masks stay clean.
fn compute_all_bones_mask(bone_count: usize) -> AnimationMask {
    debug_assert!(
        bone_count <= 64,
        "AnimationMask is u64: max 64 bones per rig, got {bone_count}",
    );
    if bone_count >= 64 {
        AnimationMask::MAX
    } else {
        (1u64 << bone_count) - 1
    }
}

/// Gets bone names from the first locomotion clip's bone defaults (all clips in an animset
/// share the same rig, so any clip's defaults give the canonical bone list).
fn resolve_bone_names(
    animset: &SpriteAnimSetAsset,
    loaded_handles: &LoadedAnimHandles,
    bone_defaults: &AnimBoneDefaults,
) -> Vec<String> {
    let first_clip_path = &animset.locomotion.entries[0].clip;
    let handle = loaded_handles
        .0
        .get(first_clip_path)
        .expect("first locomotion clip must be loaded");
    let defaults = bone_defaults
        .0
        .get(&handle.id())
        .expect("bone defaults must exist for first locomotion clip");
    defaults.iter().map(|b| b.name.clone()).collect()
}

/// Assigns each bone its own mask group in the animation graph (bone index = group index).
fn register_bone_mask_groups(graph: &mut AnimationGraph, bone_names: &[String]) {
    debug_assert!(
        bone_names.len() <= 64,
        "AnimationMask is u64: max 64 bones per rig, got {}",
        bone_names.len()
    );
    for (i, name) in bone_names.iter().enumerate() {
        let target_id = AnimationTargetId::from_names(std::iter::once(&Name::new(name.clone())));
        graph.add_target_to_mask_group(target_id, i as u32);
    }
}

/// Looks up a `BuiltClipPair` (override + additive) by source-clip path.
fn resolve_clip_pair(
    clip_path: &str,
    loaded_handles: &LoadedAnimHandles,
    built_anims: &BuiltAnimations,
) -> BuiltClipPair {
    let anim_handle = loaded_handles
        .0
        .get(clip_path)
        .unwrap_or_else(|| panic!("LoadedAnimHandles missing entry for {clip_path}"));
    built_anims
        .0
        .get(&anim_handle.id())
        .unwrap_or_else(|| panic!("BuiltAnimations missing clip for {clip_path}"))
        .clone()
}

/// Attaches `AnimationPlayer`, `AnimationTarget`, graph handle, and an empty
/// `ActiveAnimLayers` stack to characters with built graphs.
pub fn attach_animation_players(
    mut commands: Commands,
    characters: Query<
        (Entity, &BoneEntities, &AnimSetRef),
        (With<CharacterMarker>, Without<AnimationPlayer>),
    >,
    built_graphs: Res<BuiltAnimGraphs>,
) {
    for (entity, bone_entities, animset_ref) in &characters {
        let animset_id = animset_ref.0.id();
        let Some(built_graph) = built_graphs.0.get(&animset_id) else {
            continue; // graph not built yet — expected during startup
        };

        commands.entity(entity).insert((
            AnimationPlayer::default(),
            AnimationGraphHandle(built_graph.graph_handle.clone()),
            crate::animset::ActiveAnimLayers::default(),
        ));

        for (bone_name, &bone_entity) in &bone_entities.0 {
            let target_id =
                AnimationTargetId::from_names(std::iter::once(&Name::new(bone_name.clone())));
            commands
                .entity(bone_entity)
                .insert((target_id, AnimatedBy(entity)));
        }
    }
}

/// Starts all locomotion clips on players with an empty layer stack, initializes blend
/// weights, and seeds `ActiveAnimLayers` with the permanent locomotion layer entry.
///
/// An empty stack means either a newly-attached player or one just reset by an in-place
/// graph rebuild (`build_anim_graphs`) — the permanent locomotion entry is otherwise never
/// removed, so emptiness is the re-seed signal for both cases.
pub fn start_locomotion_blend(
    mut commands: Commands,
    mut query: Query<(
        Entity,
        &mut AnimationPlayer,
        &AnimSetRef,
        &AnimationGraphHandle,
        &mut crate::animset::ActiveAnimLayers,
    )>,
    built_graphs: Res<BuiltAnimGraphs>,
    mut graph_assets: ResMut<Assets<AnimationGraph>>,
) {
    for (entity, mut player, animset_ref, graph_handle, mut layers) in &mut query {
        if !layers.entries.is_empty() {
            continue; // locomotion layer already seeded
        }
        let built_graph = built_graphs
            .0
            .get(&animset_ref.0.id())
            .expect("AnimationPlayer attached but graph not built");

        let mut initial_weights = vec![0.0; built_graph.locomotion_entries.len()];
        for (i, entry) in built_graph.locomotion_entries.iter().enumerate() {
            let weight = if i == 0 { 1.0 } else { 0.0 };
            initial_weights[i] = weight;
            let anim = player.play(entry.node_index);
            anim.repeat();
            anim.set_weight(weight);
        }

        layers.entries.push(crate::animset::AnimLayer {
            id: "locomotion".to_string(),
            node_index: built_graph.locomotion_blend_node,
            claims: built_graph.all_bones_mask,
            priority: 0,
            mode: crate::animset::AnimLayerMode::Override,
            source: crate::animset::AnimLayerSource::Locomotion,
        });
        if let Some(graph) = graph_assets.get_mut(&graph_handle.0) {
            crate::animset::recompute_layer_masks(&layers.entries, graph);
        }

        commands.entity(entity).insert(LocomotionBlendWeights {
            weights: initial_weights,
        });
    }
}

/// Updates locomotion blend weights from horizontal velocity, smoothing unless the rig
/// requests immediate response for deterministic authoring or playback control.
pub fn update_locomotion_blend_weights(
    mut characters: Query<
        (
            &mut AnimationPlayer,
            &AnimSetRef,
            &LinearVelocity,
            &mut LocomotionBlendWeights,
            Has<ImmediateLocomotionBlend>,
        ),
        With<CharacterMarker>,
    >,
    built_graphs: Res<BuiltAnimGraphs>,
    time: Res<Time>,
) {
    let dt = time.delta_secs();

    for (mut player, animset_ref, velocity, mut blend_weights, immediate) in &mut characters {
        let lerp_factor = locomotion_blend_lerp_factor(dt, immediate);
        let Some(built_graph) = built_graphs.0.get(&animset_ref.0.id()) else {
            continue; // graph not built yet — expected during startup
        };
        let speed = velocity.xz().length();
        let target_weights = compute_blend_weights(speed, &built_graph.locomotion_entries);

        debug_assert_eq!(
            blend_weights.weights.len(),
            target_weights.len(),
            "LocomotionBlendWeights length mismatch with locomotion entries"
        );

        for (i, (entry, &target)) in built_graph
            .locomotion_entries
            .iter()
            .zip(target_weights.iter())
            .enumerate()
        {
            let current = &mut blend_weights.weights[i];
            *current += (target - *current) * lerp_factor;

            let anim = player
                .animation_mut(entry.node_index)
                .expect("Locomotion clip must be playing when locomotion is active");
            anim.set_weight(*current);
        }
    }
}

/// Computes 1D linear interpolation blend weights from speed and sorted threshold entries.
///
/// Returns a `Vec<f32>` of weights (summing to 1.0) where at most two adjacent entries are nonzero.
pub fn compute_blend_weights(speed: f32, entries: &[LocomotionNodeEntry]) -> Vec<f32> {
    let mut weights = vec![0.0; entries.len()];
    if entries.is_empty() {
        return weights;
    }
    if entries.len() == 1 {
        weights[0] = 1.0;
        return weights;
    }

    if speed <= entries[0].speed_threshold {
        weights[0] = 1.0;
        return weights;
    }
    if speed >= entries.last().expect("checked len >= 2").speed_threshold {
        *weights.last_mut().expect("checked len >= 2") = 1.0;
        return weights;
    }

    for i in 0..entries.len() - 1 {
        let lo = entries[i].speed_threshold;
        let hi = entries[i + 1].speed_threshold;
        if speed >= lo && speed < hi {
            let t = (speed - lo) / (hi - lo);
            weights[i] = 1.0 - t;
            weights[i + 1] = t;
            return weights;
        }
    }

    debug_assert!(false, "Speed {speed} fell through all threshold ranges");
    weights[0] = 1.0;
    weights
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entries(thresholds: &[f32]) -> Vec<LocomotionNodeEntry> {
        thresholds
            .iter()
            .enumerate()
            .map(|(i, &t)| LocomotionNodeEntry {
                node_index: AnimationNodeIndex::new(i),
                speed_threshold: t,
            })
            .collect()
    }

    #[test]
    fn blend_weights_below_minimum() {
        let entries = make_entries(&[0.0, 2.0, 6.0]);
        let w = compute_blend_weights(0.0, &entries);
        assert_eq!(w, vec![1.0, 0.0, 0.0]);
    }

    #[test]
    fn blend_weights_above_maximum() {
        let entries = make_entries(&[0.0, 2.0, 6.0]);
        let w = compute_blend_weights(10.0, &entries);
        assert_eq!(w, vec![0.0, 0.0, 1.0]);
    }

    #[test]
    fn blend_weights_midpoint() {
        let entries = make_entries(&[0.0, 2.0, 6.0]);
        let w = compute_blend_weights(1.0, &entries);
        assert_eq!(w, vec![0.5, 0.5, 0.0]);
    }

    #[test]
    fn blend_weights_exact_threshold() {
        let entries = make_entries(&[0.0, 2.0, 6.0]);
        let w = compute_blend_weights(2.0, &entries);
        assert_eq!(w, vec![0.0, 1.0, 0.0]);
    }

    #[test]
    fn blend_weights_between_upper_pair() {
        let entries = make_entries(&[0.0, 2.0, 6.0]);
        let w = compute_blend_weights(4.0, &entries);
        assert_eq!(w, vec![0.0, 0.5, 0.5]);
    }

    #[test]
    fn blend_weights_single_entry() {
        let entries = make_entries(&[0.0]);
        let w = compute_blend_weights(5.0, &entries);
        assert_eq!(w, vec![1.0]);
    }

    #[test]
    fn blend_weights_empty() {
        let entries = make_entries(&[]);
        let w = compute_blend_weights(5.0, &entries);
        assert!(w.is_empty());
    }

    /// Verifies the editor marker bypasses smoothing while gameplay rigs retain it.
    #[test]
    fn immediate_locomotion_blend_reaches_the_scrubbed_target_in_one_frame() {
        use std::time::Duration;

        let mut app = App::new();
        let animset_handle = Handle::<SpriteAnimSetAsset>::default();
        let entries = make_entries(&[0.0, 2.0, 6.0]);
        let node_indices = entries
            .iter()
            .map(|entry| entry.node_index)
            .collect::<Vec<_>>();
        app.insert_resource(BuiltAnimGraphs(HashMap::from([(
            animset_handle.id(),
            BuiltAnimGraph {
                graph_handle: Handle::default(),
                node_map: HashMap::new(),
                locomotion_entries: entries,
                locomotion_blend_node: AnimationNodeIndex::new(3),
                ability_nodes: HashMap::new(),
                all_bones_mask: 0,
            },
        )])));
        let mut time = Time::<()>::default();
        time.advance_by(Duration::from_secs_f32(1.0 / 60.0));
        app.insert_resource(time);

        let mut spawn_rig = |immediate| {
            let mut player = AnimationPlayer::default();
            for &node_index in &node_indices {
                player.play(node_index);
            }
            let mut entity = app.world_mut().spawn((
                CharacterMarker,
                AnimSetRef(animset_handle.clone()),
                LinearVelocity(Vec3::X * 6.0),
                LocomotionBlendWeights {
                    weights: vec![1.0, 0.0, 0.0],
                },
                player,
            ));
            if immediate {
                entity.insert(ImmediateLocomotionBlend);
            }
            entity.id()
        };
        let immediate_rig = spawn_rig(true);
        let smoothed_rig = spawn_rig(false);
        app.add_systems(Update, update_locomotion_blend_weights);
        app.update();

        let immediate = app
            .world()
            .entity(immediate_rig)
            .get::<LocomotionBlendWeights>()
            .expect("immediate rig must retain blend weights");
        assert_eq!(immediate.weights, vec![0.0, 0.0, 1.0]);
        let immediate_player = app
            .world()
            .entity(immediate_rig)
            .get::<AnimationPlayer>()
            .expect("immediate rig must retain its animation player");
        assert_eq!(
            immediate_player
                .animation(node_indices[2])
                .expect("run node must remain active")
                .weight(),
            1.0
        );

        let smoothed = app
            .world()
            .entity(smoothed_rig)
            .get::<LocomotionBlendWeights>()
            .expect("smoothed rig must retain blend weights");
        assert!((smoothed.weights[0] - 5.0 / 6.0).abs() < 1e-6);
        assert!((smoothed.weights[2] - 1.0 / 6.0).abs() < 1e-6);
    }

    /// Regression: a channel with exactly ONE keyframe must bake a constant curve at
    /// that key's value — not the zero-key hold-at-default fallback. (The editor's
    /// auto-key inserts a single key on first drag; it used to be invisible.)
    /// Doc: doc/bug/single_key_channel_bakes_as_default.md
    #[test]
    fn single_key_channel_bakes_constant_at_key_value() {
        use crate::asset::{BoneTimeline, CurveType, RotationKeyframe, TranslationKeyframe};

        let mut bone_timelines = HashMap::new();
        bone_timelines.insert(
            "root".to_string(),
            BoneTimeline {
                rotation: vec![RotationKeyframe {
                    time: 0.5,
                    value: 90.0,
                    curve: CurveType::Linear,
                }],
                translation: vec![TranslationKeyframe {
                    time: 0.5,
                    value: Vec2::new(1.0, 2.0),
                    curve: CurveType::Linear,
                }],
                ..Default::default()
            },
        );
        let anim = SpriteAnimAsset {
            name: "single_key".to_string(),
            duration: 1.0,
            looping: true,
            bone_timelines,
            events: vec![],
        };
        let defaults = vec![BoneAnimDefault {
            name: "root".to_string(),
            default_xy: Vec2::new(10.0, 20.0),
            z_order: 0.5,
        }];

        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            bevy::animation::AnimationPlugin,
        ));
        let clip = build_override_clip(&anim, &defaults);
        let clip_handle = app
            .world_mut()
            .resource_mut::<Assets<AnimationClip>>()
            .add(clip);
        let (graph, node) = AnimationGraph::from_clip(clip_handle);
        let graph_handle = app
            .world_mut()
            .resource_mut::<Assets<AnimationGraph>>()
            .add(graph);
        let mut player = AnimationPlayer::default();
        player.play(node).repeat();
        let player_entity = app
            .world_mut()
            .spawn((player, AnimationGraphHandle(graph_handle)))
            .id();
        let target_id =
            AnimationTargetId::from_names(std::iter::once(&Name::new("root".to_string())));
        let bone = app
            .world_mut()
            .spawn((target_id, AnimatedBy(player_entity), Transform::default()))
            .id();

        app.update();
        app.update();

        let transform = app.world().get::<Transform>(bone).expect("bone transform");
        let expected_rotation = Quat::from_rotation_z(90f32.to_radians());
        assert!(
            transform.rotation.abs_diff_eq(expected_rotation, 1e-5),
            "single rotation key must bake its value, got {:?}",
            transform.rotation
        );
        // default_xy (10, 20) + key offset (1, 2), z from z_order.
        let expected_translation = Vec3::new(11.0, 22.0, 0.5);
        assert!(
            transform
                .translation
                .abs_diff_eq(expected_translation, 1e-5),
            "single translation key must bake default + offset, got {:?}",
            transform.translation
        );
    }

    mod graph_rebuild {
        use super::super::*;
        use crate::asset::{
            BoneBlendMode, BoneTimeline, CurveType, LocomotionConfig, LocomotionEntry,
            RotationKeyframe,
        };
        use crate::RigRegistryEntry;
        use protocol::CharacterType;

        const IDLE_PATH: &str = "anims/test/idle.anim.ron";
        const PUNCH_PATH: &str = "anims/test/punch.anim.ron";

        fn test_anim(blend_mode: BoneBlendMode) -> SpriteAnimAsset {
            let mut bone_timelines = HashMap::new();
            bone_timelines.insert(
                "root".to_string(),
                BoneTimeline {
                    blend_mode,
                    rotation: vec![
                        RotationKeyframe {
                            time: 0.0,
                            value: 0.0,
                            curve: CurveType::Linear,
                        },
                        RotationKeyframe {
                            time: 1.0,
                            value: 90.0,
                            curve: CurveType::Linear,
                        },
                    ],
                    translation: vec![],
                    scale: vec![],
                },
            );
            SpriteAnimAsset {
                name: "test".to_string(),
                duration: 1.0,
                looping: true,
                bone_timelines,
                events: vec![],
            }
        }

        /// Builds an app with the clip+graph systems and one registered animset
        /// (idle locomotion + punch ability), all assets inserted manually.
        fn graph_test_app() -> (App, Handle<SpriteAnimSetAsset>, Handle<SpriteAnimAsset>) {
            let (mut app, animset_handle, punch_handle) = clip_test_app();
            app.add_systems(Update, build_anim_graphs.after(build_animation_clips));
            (app, animset_handle, punch_handle)
        }

        /// Like `graph_test_app` but with only the clip builder registered, so
        /// `GraphRebuildQueue` contents survive the frame for producer-side assertions.
        fn clip_test_app() -> (App, Handle<SpriteAnimSetAsset>, Handle<SpriteAnimAsset>) {
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, AssetPlugin::default()));
            app.init_asset::<SpriteAnimAsset>();
            app.init_asset::<SpriteAnimSetAsset>();
            app.init_asset::<AnimationClip>();
            app.init_asset::<AnimationGraph>();
            app.init_resource::<BuiltAnimations>();
            app.init_resource::<LoadedAnimHandles>();
            app.init_resource::<BuiltAnimGraphs>();
            app.init_resource::<AnimBoneDefaults>();
            app.init_resource::<GraphRebuildQueue>();
            app.add_systems(Update, build_animation_clips);

            let world = app.world_mut();
            let idle_handle = world
                .resource_mut::<Assets<SpriteAnimAsset>>()
                .add(test_anim(BoneBlendMode::Override));
            let punch_handle = world
                .resource_mut::<Assets<SpriteAnimAsset>>()
                .add(test_anim(BoneBlendMode::Override));
            let animset_handle =
                world
                    .resource_mut::<Assets<SpriteAnimSetAsset>>()
                    .add(SpriteAnimSetAsset {
                        rig: "rigs/test.rig.ron".to_string(),
                        locomotion: LocomotionConfig {
                            entries: vec![LocomotionEntry {
                                clip: IDLE_PATH.to_string(),
                                speed_threshold: 0.0,
                            }],
                        },
                        ability_animations: std::collections::BTreeMap::from([(
                            "punch".to_string(),
                            PUNCH_PATH.to_string(),
                        )]),
                        hit_react: None,
                    });

            let mut loaded = world.resource_mut::<LoadedAnimHandles>();
            loaded.0.insert(IDLE_PATH.to_string(), idle_handle.clone());
            loaded
                .0
                .insert(PUNCH_PATH.to_string(), punch_handle.clone());

            let defaults = vec![BoneAnimDefault {
                name: "root".to_string(),
                default_xy: Vec2::ZERO,
                z_order: 0.0,
            }];
            let mut bone_defaults = world.resource_mut::<AnimBoneDefaults>();
            bone_defaults.0.insert(idle_handle.id(), defaults.clone());
            bone_defaults.0.insert(punch_handle.id(), defaults);

            world.insert_resource(RigRegistry {
                entries: HashMap::from([(
                    CharacterType::Humanoid,
                    RigRegistryEntry {
                        animset_handle: animset_handle.clone(),
                        rig_handle: Handle::default(),
                    },
                )]),
            });

            (app, animset_handle, punch_handle)
        }

        /// Spawns a rig entity playing locomotion, with the given extra layers stacked on
        /// top of the permanent locomotion entry.
        fn spawn_playing_rig(
            app: &mut App,
            animset_handle: &Handle<SpriteAnimSetAsset>,
            extra_layers: Vec<crate::animset::AnimLayer>,
        ) -> Entity {
            let built = app.world().resource::<BuiltAnimGraphs>();
            let graph = built
                .0
                .get(&animset_handle.id())
                .expect("graph must be built before spawning a rig");
            let locomotion_node = graph.locomotion_entries[0].node_index;
            let locomotion_blend_node = graph.locomotion_blend_node;
            let all_bones_mask = graph.all_bones_mask;

            let mut player = AnimationPlayer::default();
            player.play(locomotion_node).repeat();
            let mut layers = crate::animset::ActiveAnimLayers::default();
            layers.entries.push(crate::animset::AnimLayer {
                id: "locomotion".to_string(),
                node_index: locomotion_blend_node,
                claims: all_bones_mask,
                priority: 0,
                mode: crate::animset::AnimLayerMode::Override,
                source: crate::animset::AnimLayerSource::Locomotion,
            });
            layers.entries.extend(extra_layers);

            app.world_mut()
                .spawn((player, layers, AnimSetRef(animset_handle.clone())))
                .id()
        }

        /// Runs two updates: one for the asset-event flush, one for the systems to react.
        fn update_twice(app: &mut App) {
            app.update();
            app.update();
        }

        #[test]
        fn rebuild_mid_playback_preserves_graph_handle() {
            let (mut app, animset_handle, _) = graph_test_app();
            app.update();
            let original_graph_id = app
                .world()
                .resource::<BuiltAnimGraphs>()
                .0
                .get(&animset_handle.id())
                .expect("first build must succeed")
                .graph_handle
                .id();
            let rig = spawn_playing_rig(&mut app, &animset_handle, vec![]);

            app.world_mut()
                .resource_mut::<Assets<SpriteAnimSetAsset>>()
                .get_mut(&animset_handle)
                .expect("animset exists");
            update_twice(&mut app);

            let built = app.world().resource::<BuiltAnimGraphs>();
            let rebuilt = built
                .0
                .get(&animset_handle.id())
                .expect("rebuild must keep the entry");
            assert_eq!(rebuilt.graph_handle.id(), original_graph_id);
            let layers = app
                .world()
                .entity(rig)
                .get::<crate::animset::ActiveAnimLayers>()
                .expect("rig keeps its layer stack");
            assert!(
                layers.entries.is_empty(),
                "rebuild must reset the rig's layer stack"
            );
        }

        #[test]
        fn rebuild_clears_active_ability_layer() {
            let (mut app, animset_handle, _) = graph_test_app();
            app.update();
            let punch_node = app
                .world()
                .resource::<BuiltAnimGraphs>()
                .0
                .get(&animset_handle.id())
                .expect("first build must succeed")
                .ability_nodes
                .get("punch")
                .expect("punch ability node exists")
                .override_node;
            let ability_entity = app.world_mut().spawn_empty().id();
            let rig = spawn_playing_rig(
                &mut app,
                &animset_handle,
                vec![crate::animset::AnimLayer {
                    id: "punch".to_string(),
                    node_index: punch_node,
                    claims: 1,
                    priority: 1,
                    mode: crate::animset::AnimLayerMode::Override,
                    source: crate::animset::AnimLayerSource::AbilityOverride { ability_entity },
                }],
            );

            app.world_mut()
                .resource_mut::<Assets<SpriteAnimSetAsset>>()
                .get_mut(&animset_handle)
                .expect("animset exists");
            update_twice(&mut app);

            let layers = app
                .world()
                .entity(rig)
                .get::<crate::animset::ActiveAnimLayers>()
                .expect("rig keeps its layer stack");
            assert!(
                layers.entries.is_empty(),
                "rebuild must drop the in-flight ability layer"
            );
        }

        #[test]
        fn partition_flip_enqueues_clip_id() {
            // Producer-only app (no build_anim_graphs draining the queue), so the
            // enqueued clip id is observable after the flip.
            let (mut app, _, punch_handle) = clip_test_app();
            app.update();

            app.world_mut()
                .resource_mut::<Assets<SpriteAnimAsset>>()
                .get_mut(&punch_handle)
                .expect("punch clip exists")
                .bone_timelines
                .get_mut("root")
                .expect("root timeline exists")
                .blend_mode = BoneBlendMode::Additive;
            update_twice(&mut app);

            let queue = app.world().resource::<GraphRebuildQueue>();
            assert!(
                queue.0.contains(&punch_handle.id()),
                "partition flip must enqueue the flipped clip id"
            );
        }

        #[test]
        fn partition_flip_rebuilds_graph_masks_in_place() {
            let (mut app, animset_handle, punch_handle) = graph_test_app();
            app.update();
            let before = app
                .world()
                .resource::<BuiltAnimGraphs>()
                .0
                .get(&animset_handle.id())
                .expect("first build must succeed");
            let original_graph_id = before.graph_handle.id();
            let punch_before = before.ability_nodes["punch"];
            assert_ne!(punch_before.override_claims, 0);
            assert_eq!(punch_before.additive_claims, 0);

            app.world_mut()
                .resource_mut::<Assets<SpriteAnimAsset>>()
                .get_mut(&punch_handle)
                .expect("punch clip exists")
                .bone_timelines
                .get_mut("root")
                .expect("root timeline exists")
                .blend_mode = BoneBlendMode::Additive;
            update_twice(&mut app);

            let after = app.world().resource::<BuiltAnimGraphs>();
            let rebuilt = after
                .0
                .get(&animset_handle.id())
                .expect("rebuild must keep the entry");
            assert_eq!(rebuilt.graph_handle.id(), original_graph_id);
            let punch_after = rebuilt.ability_nodes["punch"];
            assert_eq!(punch_after.override_claims, 0);
            assert_ne!(punch_after.additive_claims, 0);
            assert!(app.world().resource::<GraphRebuildQueue>().0.is_empty());
        }
    }
}
