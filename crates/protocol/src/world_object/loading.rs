use bevy::asset::AssetPath;
use bevy::prelude::*;
use std::collections::HashMap;

use super::registry::WorldObjectDefRegistry;
use super::types::{WorldObjectDef, WorldObjectId};
use crate::app_state::TrackedAssets;

#[cfg(not(target_arch = "wasm32"))]
use bevy::asset::LoadedFolder;

#[cfg(target_arch = "wasm32")]
use super::registry::WorldObjectManifest;

/// Holds the folder handle returned by `load_folder("objects")` (native only).
/// Kept alive to prevent asset unloading; also used to enumerate loaded objects.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Resource)]
pub(super) struct ObjectFolderHandle(pub Handle<LoadedFolder>);

/// Holds the manifest handle (WASM only).
/// Once loaded, the insert system reads the list of IDs and starts individual loads.
#[cfg(target_arch = "wasm32")]
#[derive(Resource)]
pub(super) struct ObjectManifestHandle(pub Handle<WorldObjectManifest>);

/// Accumulates individual object handles as they are loaded from the manifest (WASM only).
/// Each handle is also added to `TrackedAssets` for load-gating.
#[cfg(target_arch = "wasm32")]
#[derive(Resource, Default)]
pub(super) struct PendingObjectHandles(pub Vec<Handle<WorldObjectDef>>);

/// Starts loading all world object definition assets at app startup.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn load_world_object_defs(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut tracked: ResMut<TrackedAssets>,
) {
    let handle = asset_server.load_folder("objects");
    tracked.add(handle.clone());
    commands.insert_resource(ObjectFolderHandle(handle));
}

#[cfg(target_arch = "wasm32")]
pub(super) fn load_world_object_defs(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut tracked: ResMut<TrackedAssets>,
) {
    let handle = asset_server.load::<WorldObjectManifest>("objects.manifest.ron");
    tracked.add(handle.clone());
    commands.insert_resource(ObjectManifestHandle(handle));
    commands.init_resource::<PendingObjectHandles>();
}

/// Loads individual object files once the manifest is ready (WASM only).
#[cfg(target_arch = "wasm32")]
pub(super) fn trigger_individual_object_loads(
    manifest_handle: Option<Res<ObjectManifestHandle>>,
    manifests: Res<Assets<WorldObjectManifest>>,
    mut pending: ResMut<PendingObjectHandles>,
    mut tracked: ResMut<TrackedAssets>,
    asset_server: Res<AssetServer>,
) {
    if !pending.0.is_empty() {
        trace!("Individual object loads already triggered");
        return;
    }
    let Some(manifest_handle) = manifest_handle else {
        trace!("Object manifest handle not yet available");
        return;
    };
    let Some(manifest) = manifests.get(&manifest_handle.0) else {
        trace!("Object manifest asset not yet loaded");
        return;
    };
    for id in &manifest.0 {
        let handle = asset_server.load::<WorldObjectDef>(format!("objects/{id}.object.ron"));
        tracked.add(handle.clone());
        pending.0.push(handle);
    }
}

/// Builds a `WorldObjectId → WorldObjectDef` map from an iterator of asset IDs.
///
/// Shared by both `insert_world_object_defs` variants and `reload_world_object_defs`.
fn collect_object_defs(
    ids: impl Iterator<Item = AssetId<WorldObjectDef>>,
    object_assets: &Assets<WorldObjectDef>,
    asset_server: &AssetServer,
) -> HashMap<WorldObjectId, WorldObjectDef> {
    let mut objects = HashMap::new();
    for id in ids {
        let Some(def) = object_assets.get(id) else {
            continue;
        };
        let Some(path) = asset_server.get_path(id) else {
            continue;
        };
        let Some(obj_id) = object_id_from_path(&path) else {
            continue;
        };
        objects.insert(obj_id, def.clone());
    }
    objects
}

/// Inserts `WorldObjectDefRegistry` once all object assets are loaded (native).
///
/// Runs only while the registry does not yet exist (gated in plugin via `run_if`).
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn insert_world_object_defs(
    mut commands: Commands,
    folder_handle: Res<ObjectFolderHandle>,
    loaded_folders: Res<Assets<LoadedFolder>>,
    object_assets: Res<Assets<WorldObjectDef>>,
    asset_server: Res<AssetServer>,
) {
    let Some(folder) = loaded_folders.get(&folder_handle.0) else {
        trace!("Object folder not yet loaded");
        return;
    };
    let ids = folder
        .handles
        .iter()
        .filter_map(|h| h.clone().try_typed::<WorldObjectDef>().ok())
        .map(|h| h.id());
    let objects = collect_object_defs(ids, &object_assets, &asset_server);
    info!("Loaded {} world object definitions", objects.len());
    commands.insert_resource(WorldObjectDefRegistry { objects });
}

/// Inserts `WorldObjectDefRegistry` once all object assets are loaded (WASM).
///
/// Runs only while the registry does not yet exist (gated in plugin via `run_if`).
#[cfg(target_arch = "wasm32")]
pub(super) fn insert_world_object_defs(
    mut commands: Commands,
    pending: Res<PendingObjectHandles>,
    object_assets: Res<Assets<WorldObjectDef>>,
    asset_server: Res<AssetServer>,
) {
    if pending.0.is_empty() {
        trace!("No pending object handles yet");
        return;
    }
    if pending.0.iter().any(|h| object_assets.get(h).is_none()) {
        trace!("Not all object assets loaded yet");
        return;
    }
    let objects = collect_object_defs(
        pending.0.iter().map(|h| h.id()),
        &*object_assets,
        &*asset_server,
    );
    info!("Loaded {} world object definitions", objects.len());
    commands.insert_resource(WorldObjectDefRegistry { objects });
}

/// Rebuilds `WorldObjectDefRegistry` when any object definition is hot-reloaded.
///
/// Runs only in `AppState::Ready`, which guarantees the registry exists.
pub(super) fn reload_world_object_defs(
    mut events: MessageReader<AssetEvent<WorldObjectDef>>,
    object_assets: Res<Assets<WorldObjectDef>>,
    asset_server: Res<AssetServer>,
    mut registry: ResMut<WorldObjectDefRegistry>,
) {
    if !events
        .read()
        .any(|e| matches!(e, AssetEvent::Modified { .. }))
    {
        return;
    }
    registry.objects = collect_object_defs(
        object_assets.iter().map(|(id, _)| id),
        &object_assets,
        &asset_server,
    );
    info!(
        "Hot-reloaded {} world object definitions",
        registry.objects.len()
    );
}

/// Derives a `WorldObjectId` from an asset path by stripping the `.object.ron` suffix.
pub(super) fn object_id_from_path(path: &AssetPath) -> Option<WorldObjectId> {
    let name = path.path().file_name()?.to_str()?;
    Some(WorldObjectId(name.strip_suffix(".object.ron")?.to_string()))
}
