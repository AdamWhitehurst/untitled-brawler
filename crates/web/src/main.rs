use bevy::prelude::*;
use client::auth::ClientAuthPlugin;
use client::gameplay::ClientGameplayPlugin;
use client::input::ClientInputCommandPlugin;
use client::map::ClientMapPlugin;
use client::transition::ClientTransitionPlugin;
use client_web_lightyear::WebClientPlugin;
use dev::DevPlugin;
use lightyear::prelude::client::*;
use nostr_client::{
    EncryptedIdentity, NostrClientConfig, NostrClientPlugin, SaveEncryptedIdentity,
    StoredEncryptedIdentity,
};
use protocol::*;
use render::RenderPlugin;
use std::time::Duration;
use ui::{UiClientConfig, UiPlugin};

fn main() {
    #[cfg(target_family = "wasm")]
    console_error_panic_hook::set_once();

    #[cfg(target_family = "wasm")]
    let certificate_digest = include_str!("../../../certificates/digest.txt")
        .trim()
        .to_string();
    #[cfg(not(target_family = "wasm"))]
    let certificate_digest = String::new();

    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "Untitled Brawler".to_string(),
                ..default()
            }),
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
        .add_plugins(WebIdentityStorePlugin::from_profile(web_identity_profile()))
        .add_plugins(WebClientPlugin)
        .add_plugins(ClientAuthPlugin)
        .insert_resource(UiClientConfig {
            server_addr: std::net::SocketAddr::from(([127, 0, 0, 1], 5001)),
            client_id: 0,
            certificate_digest,
            protocol_id: protocol::PROTOCOL_ID,
            private_key: protocol::PRIVATE_KEY,
        })
        .add_plugins(ClientGameplayPlugin)
        .add_plugins(ClientInputCommandPlugin)
        .add_plugins(ClientMapPlugin)
        .add_plugins(ClientTransitionPlugin)
        .add_plugins(RenderPlugin)
        .add_plugins(UiPlugin)
        .add_plugins(DevPlugin)
        .run();
}

const WEB_IDENTITY_STORAGE_PREFIX: &str = "untitled-brawler:nostr-identity";

/// Configures the browser-backed encrypted Nostr identity store.
#[derive(Clone, Resource, Debug)]
struct WebIdentityStoreConfig {
    storage_key: String,
}

/// Loads and saves the encrypted browser identity for one optional profile.
struct WebIdentityStorePlugin {
    profile: Option<String>,
}

impl WebIdentityStorePlugin {
    /// Creates a browser identity store plugin for the given profile.
    fn from_profile(profile: Option<&str>) -> Self {
        Self {
            profile: profile.map(str::to_string),
        }
    }
}

impl Plugin for WebIdentityStorePlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<SaveEncryptedIdentity>()
            .init_resource::<StoredEncryptedIdentity>()
            .insert_resource(WebIdentityStoreConfig {
                storage_key: web_identity_storage_key(self.profile.as_deref()),
            })
            .add_systems(Startup, load_web_identity)
            .add_systems(Update, save_web_identity_requests);
    }
}

/// Returns the compile-time Nostr identity profile passed by `cargo web`.
fn web_identity_profile() -> Option<&'static str> {
    match option_env!("WEB_NOSTR_IDENTITY_PROFILE").filter(|profile| !profile.is_empty()) {
        Some(profile) => {
            nostr_client::client_identity_dir(Some(profile))
                .unwrap_or_else(|error| panic!("invalid --nostr-identity value: {error}"));
            Some(profile)
        }
        None => None,
    }
}

/// Returns the browser localStorage key for the optional Nostr profile.
fn web_identity_storage_key(profile: Option<&str>) -> String {
    match profile {
        Some(profile) => format!("{WEB_IDENTITY_STORAGE_PREFIX}:profiles:{profile}"),
        None => WEB_IDENTITY_STORAGE_PREFIX.to_string(),
    }
}

/// Loads the encrypted identity from browser storage before the app becomes ready.
fn load_web_identity(
    config: Res<WebIdentityStoreConfig>,
    mut stored: ResMut<StoredEncryptedIdentity>,
    mut complete: ResMut<IdentityLoadComplete>,
) {
    match load_web_identity_from_storage(&config.storage_key) {
        Ok(identity) => {
            stored.0 = identity;
            complete.0 = true;
            info!("Web encrypted identity load complete");
        }
        Err(error) => panic!("Failed to load encrypted web identity: {error}"),
    }
}

/// Saves encrypted identity changes to browser storage.
fn save_web_identity_requests(
    config: Res<WebIdentityStoreConfig>,
    mut requests: MessageReader<SaveEncryptedIdentity>,
) {
    for request in requests.read() {
        save_web_identity_to_storage(&config.storage_key, &request.0)
            .unwrap_or_else(|error| panic!("Failed to save encrypted web identity: {error}"));
    }
}

/// Encodes an encrypted identity for browser localStorage.
fn encode_web_identity(value: &EncryptedIdentity) -> String {
    format!("{}\n{}", value.version, value.ciphertext)
}

/// Decodes an encrypted identity from browser localStorage.
fn decode_web_identity(raw: &str) -> Result<EncryptedIdentity, String> {
    let (version, ciphertext) = raw
        .split_once('\n')
        .ok_or_else(|| "stored identity is missing its version header".to_string())?;
    let version = version
        .parse::<u32>()
        .map_err(|error| format!("stored identity version is invalid: {error}"))?;
    if version == nostr_client::identity::ENCRYPTED_IDENTITY_VERSION {
        Ok(EncryptedIdentity {
            version,
            ciphertext: ciphertext.to_string(),
        })
    } else {
        Err(format!(
            "identity version mismatch: expected {}, got {version}",
            nostr_client::identity::ENCRYPTED_IDENTITY_VERSION
        ))
    }
}

#[cfg(target_family = "wasm")]
/// Returns browser localStorage or a loud error explaining why it is unavailable.
fn browser_local_storage() -> Result<web_sys::Storage, String> {
    web_sys::window()
        .ok_or_else(|| "browser window unavailable".to_string())?
        .local_storage()
        .map_err(|error| format!("read browser localStorage handle: {error:?}"))?
        .ok_or_else(|| "browser localStorage unavailable".to_string())
}

#[cfg(target_family = "wasm")]
/// Loads an encrypted identity from browser localStorage.
fn load_web_identity_from_storage(storage_key: &str) -> Result<Option<EncryptedIdentity>, String> {
    browser_local_storage()?
        .get_item(storage_key)
        .map_err(|error| format!("read localStorage key {storage_key}: {error:?}"))?
        .as_deref()
        .map(decode_web_identity)
        .transpose()
}

#[cfg(not(target_family = "wasm"))]
/// Loads no browser identity on native test builds.
fn load_web_identity_from_storage(_storage_key: &str) -> Result<Option<EncryptedIdentity>, String> {
    Ok(None)
}

#[cfg(target_family = "wasm")]
/// Saves an encrypted identity to browser localStorage.
fn save_web_identity_to_storage(
    storage_key: &str,
    value: &EncryptedIdentity,
) -> Result<(), String> {
    browser_local_storage()?
        .set_item(storage_key, &encode_web_identity(value))
        .map_err(|error| format!("write localStorage key {storage_key}: {error:?}"))
}

#[cfg(not(target_family = "wasm"))]
/// Ignores browser identity saves on native test builds.
fn save_web_identity_to_storage(
    _storage_key: &str,
    _value: &EncryptedIdentity,
) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_identity_storage_key_uses_default_profile() {
        assert_eq!(
            web_identity_storage_key(None),
            "untitled-brawler:nostr-identity"
        );
    }

    #[test]
    fn web_identity_storage_key_uses_named_profile() {
        assert_eq!(
            web_identity_storage_key(Some("alice")),
            "untitled-brawler:nostr-identity:profiles:alice"
        );
    }

    #[test]
    fn web_identity_encoding_roundtrips() {
        let identity = EncryptedIdentity {
            version: nostr_client::identity::ENCRYPTED_IDENTITY_VERSION,
            ciphertext: "ncryptsec1example".to_string(),
        };

        assert_eq!(
            decode_web_identity(&encode_web_identity(&identity))
                .unwrap()
                .ciphertext,
            identity.ciphertext
        );
    }
}

