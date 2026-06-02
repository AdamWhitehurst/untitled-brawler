use bevy::asset::AssetPath;
use bevy::prelude::*;
use std::collections::HashMap;

use super::registry::TerrainDefRegistry;
use super::types::TerrainDef;
use crate::app_state::TrackedAssets;

#[cfg(not(target_arch = "wasm32"))]
use bevy::asset::LoadedFolder;

#[cfg(target_arch = "wasm32")]
use super::registry::TerrainManifest;

/// Holds the folder handle returned by `load_folder("terrain")` (native only).
#[cfg(not(target_arch = "wasm32"))]
#[derive(Resource)]
pub(super) struct TerrainFolderHandle(pub Handle<LoadedFolder>);

/// Holds the manifest handle (WASM only).
#[cfg(target_arch = "wasm32")]
#[derive(Resource)]
pub(super) struct TerrainManifestHandle(pub Handle<TerrainManifest>);

/// Accumulates individual terrain handles as they are loaded from the manifest (WASM only).
#[cfg(target_arch = "wasm32")]
#[derive(Resource, Default)]
pub(super) struct PendingTerrainHandles(pub Vec<Handle<TerrainDef>>);

/// Starts loading all terrain definition assets at app startup.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn load_terrain_defs(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut tracked: ResMut<TrackedAssets>,
) {
    let handle = asset_server.load_folder("terrain");
    tracked.add(handle.clone());
    commands.insert_resource(TerrainFolderHandle(handle));
}

/// Starts loading all terrain definition assets at app startup (WASM).
#[cfg(target_arch = "wasm32")]
pub(super) fn load_terrain_defs(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut tracked: ResMut<TrackedAssets>,
) {
    let handle = asset_server.load::<TerrainManifest>("terrain.manifest.ron");
    tracked.add(handle.clone());
    commands.insert_resource(TerrainManifestHandle(handle));
    commands.init_resource::<PendingTerrainHandles>();
}

/// Loads individual terrain files once the manifest is ready (WASM only).
#[cfg(target_arch = "wasm32")]
pub(super) fn trigger_individual_terrain_loads(
    manifest_handle: Option<Res<TerrainManifestHandle>>,
    manifests: Res<Assets<TerrainManifest>>,
    mut pending: ResMut<PendingTerrainHandles>,
    mut tracked: ResMut<TrackedAssets>,
    asset_server: Res<AssetServer>,
) {
    if !pending.0.is_empty() {
        trace!("Individual terrain loads already triggered");
        return;
    }
    let Some(manifest_handle) = manifest_handle else {
        trace!("Terrain manifest handle not yet available");
        return;
    };
    let Some(manifest) = manifests.get(&manifest_handle.0) else {
        trace!("Terrain manifest asset not yet loaded");
        return;
    };
    for id in &manifest.0 {
        let handle = asset_server.load::<TerrainDef>(format!("terrain/{id}.terrain.ron"));
        tracked.add(handle.clone());
        pending.0.push(handle);
    }
}

/// Builds a terrain ID to `TerrainDef` map from an iterator of asset IDs.
fn collect_terrain_defs(
    ids: impl Iterator<Item = AssetId<TerrainDef>>,
    terrain_assets: &Assets<TerrainDef>,
    asset_server: &AssetServer,
) -> HashMap<String, TerrainDef> {
    let mut terrains = HashMap::new();
    for id in ids {
        let Some(def) = terrain_assets.get(id) else {
            continue;
        };
        let Some(path) = asset_server.get_path(id) else {
            continue;
        };
        let Some(terrain_id) = terrain_id_from_path(&path) else {
            continue;
        };
        terrains.insert(terrain_id, def.clone());
    }
    terrains
}

/// Inserts `TerrainDefRegistry` once all terrain assets are loaded (native).
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn insert_terrain_defs(
    mut commands: Commands,
    folder_handle: Res<TerrainFolderHandle>,
    loaded_folders: Res<Assets<LoadedFolder>>,
    terrain_assets: Res<Assets<TerrainDef>>,
    asset_server: Res<AssetServer>,
) {
    let Some(folder) = loaded_folders.get(&folder_handle.0) else {
        trace!("Terrain folder not yet loaded");
        return;
    };
    let ids = folder
        .handles
        .iter()
        .filter_map(|h| h.clone().try_typed::<TerrainDef>().ok())
        .map(|h| h.id());
    let terrains = collect_terrain_defs(ids, &terrain_assets, &asset_server);
    info!("Loaded {} terrain definitions", terrains.len());
    commands.insert_resource(TerrainDefRegistry { terrains });
}

/// Inserts `TerrainDefRegistry` once all terrain assets are loaded (WASM).
#[cfg(target_arch = "wasm32")]
pub(super) fn insert_terrain_defs(
    mut commands: Commands,
    pending: Res<PendingTerrainHandles>,
    terrain_assets: Res<Assets<TerrainDef>>,
    asset_server: Res<AssetServer>,
) {
    if pending.0.is_empty() {
        trace!("No pending terrain handles yet");
        return;
    }
    if pending.0.iter().any(|h| terrain_assets.get(h).is_none()) {
        trace!("Not all terrain assets loaded yet");
        return;
    }
    let terrains = collect_terrain_defs(
        pending.0.iter().map(|h| h.id()),
        &*terrain_assets,
        &*asset_server,
    );
    info!("Loaded {} terrain definitions", terrains.len());
    commands.insert_resource(TerrainDefRegistry { terrains });
}

/// Derives a terrain ID from an asset path by stripping the `.terrain.ron` suffix.
fn terrain_id_from_path(path: &AssetPath) -> Option<String> {
    let name = path.path().file_name()?.to_str()?;
    Some(name.strip_suffix(".terrain.ron")?.to_string())
}
