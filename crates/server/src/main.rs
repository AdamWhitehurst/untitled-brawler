pub mod auth;
pub mod chunk_entities;
pub mod diagnostics;
pub mod gameplay;
pub mod map;
pub mod nostr_announcement;
pub mod persistence;
pub mod transition;
pub mod world_object;

use bevy::prelude::*;
use diagnostics::ServerDiagnosticsPlugin;
use gameplay::ServerGameplayPlugin;
use map::ServerMapPlugin;
use nostr_announcement::ServerAnnouncementPlugin;
use nostr_client::{load_nostr_keys_from_env_or_profile, NostrClientPlugin};
use protocol::diagnostics::SharedDiagnosticsPlugin;
use protocol::*;
use server_lightyear::{ServerNetworkConfig, ServerNetworkPlugin};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerCliOptions {
    nostr_identity_profile: Option<String>,
}

fn main() {
    let cli_options = parse_cli_options();
    let network_config = ServerNetworkConfig {
        cert_pem_path: concat!(env!("CARGO_MANIFEST_DIR"), "/../../certificates/cert.pem").into(),
        key_pem_path: concat!(env!("CARGO_MANIFEST_DIR"), "/../../certificates/key.pem").into(),
        ..Default::default()
    };
    let server_identity =
        load_nostr_keys_from_env_or_profile(cli_options.nostr_identity_profile.as_deref())
            .expect("SERVER_NSEC or profile identity.bin must contain a valid Nostr identity");

    App::new()
        .add_plugins(MinimalPlugins)
        .add_plugins(bevy::app::TerminalCtrlCHandlerPlugin)
        .add_plugins(bevy::state::app::StatesPlugin)
        .add_plugins(bevy::log::LogPlugin::default())
        .add_plugins(bevy::asset::AssetPlugin {
            file_path: concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets").to_string(),
            ..default()
        })
        .add_plugins(bevy::transform::TransformPlugin)
        .add_plugins(bevy::scene::ScenePlugin)
        // Register asset resources for voxel world mesh generation
        .add_message::<bevy::asset::AssetEvent<bevy::prelude::Mesh>>()
        .init_asset::<bevy::prelude::Mesh>()
        .init_asset::<bevy::pbr::StandardMaterial>()
        .init_asset::<bevy::shader::Shader>()
        .add_message::<bevy::asset::AssetEvent<bevy::shader::Shader>>()
        .init_asset::<bevy::image::Image>()
        .add_message::<bevy::asset::AssetEvent<bevy::image::Image>>()
        .add_plugins(lightyear::prelude::server::ServerPlugins {
            tick_duration: Duration::from_secs_f64(1.0 / FIXED_TIMESTEP_HZ),
        })
        .add_plugins(SharedGameplayPlugin)
        .add_plugins(NostrClientPlugin::default())
        .insert_resource(server_identity)
        .add_plugins(ServerNetworkPlugin {
            config: network_config,
        })
        .add_plugins(ServerAnnouncementPlugin)
        .add_plugins(ServerGameplayPlugin)
        .add_plugins(ServerMapPlugin)
        .add_plugins(SharedDiagnosticsPlugin)
        .add_plugins(ServerDiagnosticsPlugin)
        .run();
}

fn parse_cli_options() -> ServerCliOptions {
    let args: Vec<String> = std::env::args().collect();
    parse_cli_options_from(&args).unwrap_or_else(|error| panic!("{error}"))
}

fn parse_cli_options_from(args: &[String]) -> Result<ServerCliOptions, String> {
    let mut options = ServerCliOptions {
        nostr_identity_profile: None,
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn parse_cli_options_defaults_to_default_server_identity() {
        assert_eq!(
            parse_cli_options_from(&args(&["server"])).unwrap(),
            ServerCliOptions {
                nostr_identity_profile: None,
            }
        );
    }

    #[test]
    fn parse_cli_options_reads_nostr_identity_profile() {
        assert_eq!(
            parse_cli_options_from(&args(&["server", "--nostr-identity", "staging"])).unwrap(),
            ServerCliOptions {
                nostr_identity_profile: Some("staging".to_string()),
            }
        );
    }

    #[test]
    fn parse_cli_options_supports_equals_form() {
        assert_eq!(
            parse_cli_options_from(&args(&["server", "--nostr-identity=staging"])).unwrap(),
            ServerCliOptions {
                nostr_identity_profile: Some("staging".to_string()),
            }
        );
    }
}
