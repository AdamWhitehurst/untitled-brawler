pub mod diagnostics;
pub mod gameplay;
pub mod input;
pub mod map;
pub mod transition;
pub mod world_object;

use bevy::prelude::*;
use client::auth::ClientAuthPlugin;
use client::map_publication::ClientMapPublicationPlugin;
use client::persistence::fs_encrypted_identity::{nostr_identity_dir, FsEncryptedIdentityStore};
use client_lightyear::{ClientNetworkConfig, ClientNetworkPlugin};
use dev::DevPlugin;
use diagnostics::ClientDiagnosticsPlugin;
use gameplay::ClientGameplayPlugin;
use lightyear::prelude::client::*;
use map::ClientMapPlugin;
use nostr_client::{
    EncryptedIdentity, LoginError, NostrClientConfig, NostrClientPlugin, SaveEncryptedIdentity,
    StoredEncryptedIdentity,
};
use persistence::{PendingStoreOps, StoreBackend};
use protocol::diagnostics::SharedDiagnosticsPlugin;
use protocol::*;
use render::RenderPlugin;
use std::{path::PathBuf, sync::Arc, time::Duration};
use ui::{UiClientConfig, UiPlugin};

#[derive(Resource, Clone, Debug)]
struct IdentityStoreConfig {
    base_dir: Arc<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ClientCliOptions {
    nostr_identity_profile: Option<String>,
}

fn main() {
    let cli_options = parse_cli_options();

    let network_config = ClientNetworkConfig {
        certificate_digest: include_str!("../../../certificates/digest.txt")
            .trim()
            .to_string(),
        ..Default::default()
    };
    let identity_store_config = IdentityStoreConfig {
        base_dir: Arc::new(
            nostr_identity_dir(cli_options.nostr_identity_profile.as_deref())
                .expect("invalid --nostr-identity value"),
        ),
    };

    // UI fills server_addr, certificate_digest, and client_id from the selected Nostr server and identity before connecting.
    let ui_config = UiClientConfig {
        server_addr: network_config.server_addr,
        client_id: network_config.client_id,
        certificate_digest: network_config.certificate_digest.clone(),
        protocol_id: network_config.protocol_id,
        private_key: network_config.private_key,
    };

    App::new()
        .add_plugins(DefaultPlugins.set(AssetPlugin {
            file_path: concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets").to_string(),
            ..default()
        }))
        .add_plugins(ClientPlugins {
            tick_duration: Duration::from_secs_f64(1.0 / FIXED_TIMESTEP_HZ),
        })
        .add_plugins(SharedGameplayPlugin)
        .add_plugins(NostrClientPlugin {
            config: NostrClientConfig {
                mark_identity_load_complete_on_startup: false,
                ..Default::default()
            },
        })
        .add_plugins(ClientNetworkPlugin {
            config: network_config,
        })
        .add_plugins(ClientAuthPlugin)
        .add_message::<SaveEncryptedIdentity>()
        .init_resource::<StoredEncryptedIdentity>()
        .init_resource::<LoginError>()
        .insert_resource(identity_store_config)
        .add_systems(Startup, spawn_identity_store)
        .add_systems(
            Update,
            (poll_identity_store_load, handle_identity_save_requests),
        )
        .insert_resource(ui_config) // Override default UiClientConfig
        .add_plugins(ClientGameplayPlugin)
        .add_plugins(input::ClientInputCommandPlugin)
        .add_plugins(ClientMapPlugin)
        .add_plugins(ClientMapPublicationPlugin)
        .add_plugins(transition::ClientTransitionPlugin)
        .add_plugins(RenderPlugin)
        .add_plugins(UiPlugin)
        .add_plugins(DevPlugin)
        .add_plugins(SharedDiagnosticsPlugin)
        .add_plugins(ClientDiagnosticsPlugin)
        .run();
}

fn parse_cli_options() -> ClientCliOptions {
    let args: Vec<String> = std::env::args().collect();
    parse_cli_options_from(&args).unwrap_or_else(|error| panic!("{error}"))
}

fn parse_cli_options_from(args: &[String]) -> Result<ClientCliOptions, String> {
    let mut options = ClientCliOptions::default();
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--nostr-identity" {
            index += 1;
            let Some(profile) = args.get(index) else {
                return Err("--nostr-identity requires a profile name".to_string());
            };
            options.nostr_identity_profile = Some(profile.clone());
        } else if let Some(profile) = arg.strip_prefix("--nostr-identity=") {
            options.nostr_identity_profile = Some(profile.to_string());
        }
        index += 1;
    }
    Ok(options)
}

fn spawn_identity_store(mut commands: Commands, config: Res<IdentityStoreConfig>) {
    let store = FsEncryptedIdentityStore {
        base_dir: config.base_dir.clone(),
    };
    let mut ops = PendingStoreOps::<(), EncryptedIdentity>::default();
    ops.spawn_load(&store, ());
    commands.spawn((
        Name::new("Encrypted Identity Store"),
        StoreBackend::new(store),
        ops,
    ));
}

fn poll_identity_store_load(
    mut query: Query<(
        &StoreBackend<(), EncryptedIdentity, FsEncryptedIdentityStore>,
        &mut PendingStoreOps<(), EncryptedIdentity>,
    )>,
    mut stored: ResMut<StoredEncryptedIdentity>,
    mut complete: ResMut<IdentityLoadComplete>,
) {
    let Ok((_store, mut ops)) = query.single_mut() else {
        trace!("poll_identity_store_load: identity store entity not spawned yet");
        return;
    };

    ops.poll();
    for (_key, loaded) in ops.completed_loads.drain(..) {
        stored.0 = loaded;
        complete.0 = true;
        info!("Encrypted identity load complete");
    }
    if let Some((_key, error)) = ops.load_errors.pop() {
        panic!("Failed to load encrypted identity: {error}");
    }
    ops.completed_saves.clear();
    if let Some(failure) = ops.save_errors.pop() {
        panic!("Failed to save encrypted identity: {}", failure.error);
    }
}

fn handle_identity_save_requests(
    mut requests: MessageReader<SaveEncryptedIdentity>,
    mut query: Query<(
        &StoreBackend<(), EncryptedIdentity, FsEncryptedIdentityStore>,
        &mut PendingStoreOps<(), EncryptedIdentity>,
    )>,
) {
    let Ok((store, mut ops)) = query.single_mut() else {
        trace!("handle_identity_save_requests: identity store entity not spawned yet");
        return;
    };

    for request in requests.read() {
        ops.spawn_save(&store.0, (), request.0.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn parse_cli_options_defaults_to_default_identity() {
        assert_eq!(
            parse_cli_options_from(&args(&["client"])).unwrap(),
            ClientCliOptions {
                nostr_identity_profile: None,
            }
        );
    }

    #[test]
    fn parse_cli_options_reads_nostr_identity_profile() {
        assert_eq!(
            parse_cli_options_from(&args(&["client", "--nostr-identity", "alice"])).unwrap(),
            ClientCliOptions {
                nostr_identity_profile: Some("alice".to_string()),
            }
        );
    }

    #[test]
    fn parse_cli_options_supports_equals_form() {
        assert_eq!(
            parse_cli_options_from(&args(&["client", "--nostr-identity=bob"])).unwrap(),
            ClientCliOptions {
                nostr_identity_profile: Some("bob".to_string()),
            }
        );
    }
}
