pub mod edit;
pub mod eval;
pub mod panels;
pub mod state;
pub mod store;

use avian3d::prelude::LinearVelocity;
use bevy::prelude::*;
use bevy_egui::EguiPrimaryContextPass;
use protocol::app_state::{AppState, IdentityLoadComplete, RelayPoolReady};
use protocol::billboard::billboard_material::BillboardMaterial;
use protocol::billboard::shadow_only_material::ShadowOnlyMaterial;
use protocol::billboard::sprite_rig_material::SpriteRigMaterial;
use protocol::{CharacterMarker, CharacterType};

/// Boots the live-preview editor: the real `sprite_rig` runtime (materials + spawn +
/// animation chain) minus physics/networking, one humanoid rig, and a static camera.
///
/// Registers the rig materials and `SpriteRigPlugin` directly instead of depending on
/// `render::RenderPlugin`: that plugin's camera systems take avian's `SpatialQuery`
/// (panics without `PhysicsPlugins`) and query lightyear's `Controlled`, neither of
/// which exists in the editor.
pub struct AnimationEditorPlugin;

impl Plugin for AnimationEditorPlugin {
    fn build(&self, app: &mut App) {
        // bevy_egui's auto-setup would attach the primary egui context to the first
        // camera it sees — our viewport-shrunk 3D camera — making egui's canvas track
        // that viewport and feedback-shrink it to nothing. The editor owns the setup
        // instead: a dedicated full-window UI camera (spawned in `setup_editor_scene`)
        // carries `PrimaryEguiContext`. Requires `EguiPlugin` to be added first.
        app.world_mut()
            .get_resource_mut::<bevy_egui::EguiGlobalSettings>()
            .expect("add EguiPlugin before AnimationEditorPlugin")
            .auto_create_primary_context = false;

        app.add_plugins(bevy::pbr::MaterialPlugin::<BillboardMaterial>::default());
        app.add_plugins(bevy::pbr::MaterialPlugin::<SpriteRigMaterial>::default());
        app.add_plugins(bevy::pbr::MaterialPlugin::<ShadowOnlyMaterial>::default());
        app.add_plugins(sprite_rig::SpriteRigPlugin);
        app.add_systems(
            Startup,
            (
                satisfy_app_state_gates,
                setup_editor_scene,
                setup_save_stores,
            ),
        );
        app.add_systems(Update, spawn_editor_rig);
        app.add_systems(
            Update,
            (
                state::init_editor_state.run_if(
                    in_state(AppState::Ready).and(not(resource_exists::<state::EditorState>)),
                ),
                state::drive_player_from_playhead
                    .after(sprite_rig::animation::update_locomotion_blend_weights)
                    .run_if(resource_exists::<state::EditorState>),
                state::sync_ability_preview_layers.run_if(resource_exists::<state::EditorState>),
                edit::rebuild_dirty_clip.run_if(resource_exists::<state::EditorState>),
                panels::audition::set_audition_velocity
                    .before(sprite_rig::animation::update_locomotion_blend_weights),
                panels::save::drain_save_results,
            ),
        );
        app.init_resource::<panels::audition::AuditionState>();
        app.init_resource::<panels::gizmo::GizmoMode>();
        app.init_resource::<panels::save::SaveStatus>();
        // Panel order fixes the layout: the right inspector claims its column first so
        // every bottom panel spans the remaining width (keeping the shared t→x mapping
        // aligned across ruler/dope sheet/curve); bottom panels stack in registration
        // order (transport lowest, then event lane, then timeline, then curve). Viewport
        // sync runs last
        // so it sees the frame's final available_rect.
        app.add_systems(
            EguiPrimaryContextPass,
            (
                // Before the panels so the frame draws the post-delete state.
                edit::apply_delete_hotkey,
                panels::save::draw_save,
                panels::inspector::draw_inspector,
                panels::transport::draw_transport,
                panels::events::draw_event_lane,
                panels::timeline::draw_timeline,
                panels::curve::draw_curve_editor,
                sync_camera_viewport,
                // After the panels so EguiWantsInput reflects this frame's UI, and after
                // viewport sync so gizmo projection uses the final viewport rect.
                panels::gizmo::draw_bone_gizmos,
            )
                .chain()
                .run_if(resource_exists::<state::EditorState>),
        );
    }
}

/// Spawns a humanoid rig driven by the real spawn + animation chain
/// (`spawn_sprite_rigs` → graph build → locomotion blend), once the rig, animset, and all
/// sprite images are loaded — `spawn_sprite_rigs` panics on unloaded assets, an invariant
/// the game satisfies by replicating characters in only after `AppState::Ready`.
///
/// Resolves `SpriteRig`/`AnimSetRef`/`Facing` itself instead of going through
/// `resolve_character_rig`: that system only matches replicated entities
/// (`Predicted`/`Replicated`/`Interpolated`), which the editor rig is not.
///
/// `LinearVelocity` is the input `update_locomotion_blend_weights` reads; zero holds the
/// rig in idle until the audition controls (Phase 9) drive it. No physics body, no
/// replication — the editor rig is animation-only.
fn spawn_editor_rig(
    mut commands: Commands,
    existing: Query<(), With<CharacterMarker>>,
    registry: Res<sprite_rig::RigRegistry>,
    sprite_images: Res<sprite_rig::SpriteImageHandles>,
    asset_server: Res<AssetServer>,
) {
    if !existing.is_empty() {
        return; // rig already spawned — steady state
    }
    let entry = registry
        .entries
        .get(&CharacterType::Humanoid)
        .expect("RigRegistry missing Humanoid entry");
    if !asset_server.is_loaded_with_dependencies(&entry.rig_handle)
        || !asset_server.is_loaded_with_dependencies(&entry.animset_handle)
    {
        trace!("editor rig/animset assets still loading; retrying next frame");
        return;
    }
    let all_images_loaded = sprite_images
        .0
        .values()
        .all(|handle| asset_server.is_loaded_with_dependencies(handle));
    if sprite_images.0.is_empty() || !all_images_loaded {
        trace!("editor sprite images still loading; retrying next frame");
        return;
    }
    commands.spawn((
        CharacterMarker,
        CharacterType::Humanoid,
        sprite_rig::SpriteRig(entry.rig_handle.clone()),
        sprite_rig::AnimSetRef(entry.animset_handle.clone()),
        sprite_rig::Facing::Right,
        LinearVelocity::ZERO,
        sprite_rig::animation::ImmediateLocomotionBlend,
        Transform::default(),
        Visibility::default(),
    ));
}

/// The client reaches `AppState::Ready` only when `RelayPoolReady` and
/// `IdentityLoadComplete` are set (by nostr/identity systems). The editor has neither, so
/// set both at startup; `check_assets_loaded` then advances to `Ready` once the rig,
/// animset, and sprite images finish loading.
fn satisfy_app_state_gates(
    mut relay: ResMut<RelayPoolReady>,
    mut identity: ResMut<IdentityLoadComplete>,
) {
    relay.0 = true;
    identity.0 = true;
}

/// Shrinks the editor camera's render viewport to the screen area egui leaves over, so
/// panels never cover the rig — the scene re-centers itself in the remaining space. Runs
/// after all panels in `EguiPrimaryContextPass` so `available_rect` reflects this frame's
/// final layout. Converts egui's logical points to the physical pixels `Viewport` wants.
fn sync_camera_viewport(
    mut contexts: bevy_egui::EguiContexts,
    mut cameras: Query<&mut Camera, With<Camera3d>>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
) {
    let Ok(ctx) = contexts.ctx_mut() else {
        trace!("egui context not ready; viewport unchanged");
        return;
    };
    let Ok(window) = windows.single() else {
        trace!("primary window missing; viewport unchanged");
        return;
    };
    let Ok(mut camera) = cameras.single_mut() else {
        trace!("editor camera missing; viewport unchanged");
        return;
    };

    let avail = ctx.available_rect();
    let scale = window.scale_factor();
    let window_size = UVec2::new(window.physical_width(), window.physical_height());
    let position = UVec2::new(
        (avail.left() * scale).round() as u32,
        (avail.top() * scale).round() as u32,
    )
    .min(window_size.saturating_sub(UVec2::ONE));
    let size = UVec2::new(
        (avail.width() * scale).round() as u32,
        (avail.height() * scale).round() as u32,
    )
    .min(window_size.saturating_sub(position))
    .max(UVec2::ONE);

    camera.viewport = Some(bevy::camera::Viewport {
        physical_position: position,
        physical_size: size,
        ..default()
    });
}

/// Spawns the two persistence backend entities (clip + animset), each pairing a
/// filesystem store rooted at the workspace `assets/` with its pending-ops component.
/// bevy-persistence has no plugin — consumers own spawning, polling, and draining.
fn setup_save_stores(mut commands: Commands) {
    use persistence::{PendingStoreOps, StoreBackend};

    let asset_root = store::editor_asset_root();
    commands.spawn((
        StoreBackend::<store::ClipPath, sprite_rig::asset::SpriteAnimAsset, _>::new(
            store::FsAnimClipStore {
                asset_root: asset_root.clone(),
            },
        ),
        PendingStoreOps::<store::ClipPath, sprite_rig::asset::SpriteAnimAsset>::default(),
    ));
    commands.spawn((
        StoreBackend::<String, sprite_rig::asset::SpriteAnimSetAsset, _>::new(
            store::FsAnimSetStore { asset_root },
        ),
        PendingStoreOps::<String, sprite_rig::asset::SpriteAnimSetAsset>::default(),
    ));
}

/// Static camera framing the rig at the origin, plus a directional light. The rig
/// billboards its joints toward the camera (`billboard_joint_roots`), so a fixed
/// camera always sees the sprite plane face-on.
fn setup_editor_scene(mut commands: Commands) {
    // The humanoid rig spans roughly y ∈ [-2.5, 0.5] around the entity origin
    // (root bone sits at y = -1.75), so the camera aims at its vertical center.
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(0.0, -0.75, -16.0).looking_at(Vec3::new(0.0, -0.75, 0.0), Vec3::Y),
    ));
    // Dedicated full-window egui camera: renders the UI above the 3D view (order 1, no
    // clear) and keeps egui's canvas independent of the 3D camera's shrinking viewport.
    commands.spawn((
        bevy_egui::PrimaryEguiContext,
        Camera2d,
        Camera {
            order: 1,
            clear_color: bevy::camera::ClearColorConfig::None,
            ..default()
        },
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 10_000.0,
            shadows_enabled: false,
            ..default()
        },
        Transform::default().looking_to(Vec3::new(-0.5, -1.0, -0.5), Vec3::Y),
    ));
}
