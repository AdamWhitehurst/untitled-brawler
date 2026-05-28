use bevy::prelude::*;
use nostr_sdk::nips::nip49::EncryptedSecretKey;
use nostr_sdk::{FromBech32, Keys, PublicKey, SecretKey, ToBech32};
use protocol::NostrPublicKey;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const ENCRYPTED_IDENTITY_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EncryptedIdentity {
    pub version: u32,
    pub ciphertext: String,
}

#[derive(Resource, Component, Clone)]
pub struct NostrKeys(Keys);

impl NostrKeys {
    pub fn from_secret(secret: SecretKey) -> Self {
        Self(Keys::new(secret))
    }

    pub fn keys(&self) -> &Keys {
        &self.0
    }

    pub fn public_key(&self) -> PublicKey {
        self.0.public_key()
    }

    pub fn protocol_public_key(&self) -> NostrPublicKey {
        NostrPublicKey(*self.public_key().as_bytes())
    }

    pub fn sign_event(
        &self,
        draft: &crate::events::NostrEventDraft,
    ) -> Result<String, crate::events::NostrEventError> {
        draft.sign_with_keys(&self.0)
    }
}

pub fn client_id_from_public_key(public: &PublicKey) -> u64 {
    u64::from_le_bytes(
        public.as_bytes()[0..8]
            .try_into()
            .expect("public key has 32 bytes"),
    )
}

pub fn npub_from_nostr_public_key(public: NostrPublicKey) -> String {
    PublicKey::from_byte_array(public.0)
        .to_bech32()
        .expect("PublicKey bech32 encoding is infallible")
}

#[derive(Resource, Default, Clone, Debug)]
pub struct StoredEncryptedIdentity(pub Option<EncryptedIdentity>);

#[derive(Resource, Default, Clone, Debug)]
pub struct LoginError(pub Option<String>);

#[derive(Message, Clone, Debug)]
pub struct SaveEncryptedIdentity(pub EncryptedIdentity);

pub fn generate_encrypted_identity(
    passphrase: &str,
) -> Result<(NostrKeys, EncryptedIdentity), String> {
    let secret = SecretKey::generate();
    encrypt_identity(secret, passphrase)
}

pub fn import_encrypted_identity(
    nsec: &str,
    passphrase: &str,
) -> Result<(NostrKeys, EncryptedIdentity), String> {
    let secret = SecretKey::parse(nsec).map_err(|error| format!("invalid nsec: {error}"))?;
    encrypt_identity(secret, passphrase)
}

pub fn unlock_identity(
    encrypted: &EncryptedIdentity,
    passphrase: &str,
) -> Result<NostrKeys, String> {
    if encrypted.version != ENCRYPTED_IDENTITY_VERSION {
        return Err(format!(
            "unsupported encrypted identity version {}",
            encrypted.version
        ));
    }

    let encrypted_key = EncryptedSecretKey::from_bech32(&encrypted.ciphertext)
        .map_err(|error| format!("invalid encrypted identity: {error}"))?;
    let secret = encrypted_key
        .decrypt(passphrase)
        .map_err(|error| format!("failed to decrypt identity: {error}"))?;
    Ok(NostrKeys::from_secret(secret))
}

pub fn decode_nsec_or_ncryptsec(
    value: &str,
    passphrase: Option<&str>,
) -> Result<SecretKey, String> {
    let trimmed = value.trim();
    if trimmed.starts_with("ncryptsec1") {
        let passphrase = passphrase.ok_or("NOSTR_IDENTITY_PASSPHRASE is required for ncryptsec")?;
        let encrypted = EncryptedSecretKey::from_bech32(trimmed)
            .map_err(|error| format!("invalid ncryptsec: {error}"))?;
        encrypted
            .decrypt(passphrase)
            .map_err(|error| format!("failed to decrypt ncryptsec: {error}"))
    } else {
        SecretKey::parse(trimmed).map_err(|error| format!("invalid nsec: {error}"))
    }
}

pub fn load_nostr_keys_from_env_or_profile(profile: Option<&str>) -> Result<NostrKeys, String> {
    let passphrase = std::env::var("NOSTR_IDENTITY_PASSPHRASE").ok();
    if let Ok(raw) = std::env::var("SERVER_NSEC") {
        return nostr_keys_from_secret_text(&raw, passphrase.as_deref());
    }
    let profile_dir = client_identity_dir(profile)?;
    load_nostr_keys_from_profile_dir(&profile_dir, passphrase.as_deref())
}

pub fn load_nostr_keys_from_profile_dir(
    profile_dir: &Path,
    passphrase: Option<&str>,
) -> Result<NostrKeys, String> {
    let encrypted = load_encrypted_identity_from_dir(profile_dir)
        .map_err(|error| format!("load profile identity: {error}"))?
        .ok_or_else(|| {
            format!(
                "SERVER_NSEC not set and no encrypted identity found at {}",
                identity_file_path_in_dir(profile_dir).display()
            )
        })?;
    let passphrase =
        passphrase.ok_or("NOSTR_IDENTITY_PASSPHRASE is required to unlock profile identity")?;
    let identity = unlock_identity(&encrypted, passphrase)?;
    Ok(identity)
}

fn nostr_keys_from_secret_text(raw: &str, passphrase: Option<&str>) -> Result<NostrKeys, String> {
    let secret = decode_nsec_or_ncryptsec(raw, passphrase)?;
    Ok(NostrKeys::from_secret(secret))
}

pub fn nostr_config_dir() -> PathBuf {
    nostr_config_dir_from(
        non_empty_env_path("XDG_CONFIG_HOME"),
        non_empty_env_path("HOME"),
    )
}

pub fn client_identity_dir(profile: Option<&str>) -> Result<PathBuf, String> {
    match profile {
        Some(profile) => Ok(profile_config_dir(profile)?),
        None => Ok(nostr_config_dir()),
    }
}

pub fn identity_file_path(profile: Option<&str>) -> Result<PathBuf, String> {
    Ok(identity_file_path_in_dir(&client_identity_dir(profile)?))
}

pub fn identity_file_path_in_dir(profile_dir: &Path) -> PathBuf {
    profile_dir.join("identity.bin")
}

fn profile_config_dir(profile: &str) -> Result<PathBuf, String> {
    validate_identity_profile(profile)?;
    Ok(nostr_config_dir().join("profiles").join(profile))
}

fn validate_identity_profile(profile: &str) -> Result<(), String> {
    if profile.is_empty() {
        return Err("Nostr identity profile must not be empty".to_string());
    }
    let mut bytes = profile.bytes();
    let first = bytes.next().expect("profile is non-empty");
    if !first.is_ascii_alphanumeric() {
        return Err(format!(
            "Nostr identity profile '{profile}' must start with an ASCII letter or digit"
        ));
    }
    if bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')) {
        Ok(())
    } else {
        Err(format!(
            "Nostr identity profile '{profile}' must contain only ASCII letters, digits, '.', '_', or '-'"
        ))
    }
}

fn nostr_config_dir_from(xdg_config_home: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
    if let Some(path) = xdg_config_home {
        return path.join("nostr");
    }
    if let Some(home) = home {
        return home.join(".config").join("nostr");
    }
    PathBuf::from(".config").join("nostr")
}

fn non_empty_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[derive(Debug)]
pub enum IdentityStoreError {
    Io(String),
    Serialize(String),
    Deserialize(String),
    VersionMismatch { expected: u32, actual: u32 },
}

impl std::fmt::Display for IdentityStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "IO error: {error}"),
            Self::Serialize(error) => write!(f, "serialize identity: {error}"),
            Self::Deserialize(error) => write!(f, "deserialize identity: {error}"),
            Self::VersionMismatch { expected, actual } => {
                write!(
                    f,
                    "identity version mismatch: expected {expected}, got {actual}"
                )
            }
        }
    }
}

impl std::error::Error for IdentityStoreError {}

pub fn save_encrypted_identity_to_dir(
    profile_dir: &Path,
    value: &EncryptedIdentity,
) -> Result<(), IdentityStoreError> {
    std::fs::create_dir_all(profile_dir).map_err(|error| {
        IdentityStoreError::Io(format!(
            "mkdir identity dir {}: {error}",
            profile_dir.display()
        ))
    })?;
    let path = identity_file_path_in_dir(profile_dir);
    let bytes = bincode::serialize(value)
        .map_err(|error| IdentityStoreError::Serialize(error.to_string()))?;
    let tmp_path = path.with_extension("bin.tmp");
    std::fs::write(&tmp_path, &bytes).map_err(|error| {
        IdentityStoreError::Io(format!(
            "write identity tmp {}: {error}",
            tmp_path.display()
        ))
    })?;
    std::fs::rename(&tmp_path, &path).map_err(|error| {
        IdentityStoreError::Io(format!(
            "rename identity {} to {}: {error}",
            tmp_path.display(),
            path.display()
        ))
    })?;
    Ok(())
}

pub fn load_encrypted_identity_from_dir(
    profile_dir: &Path,
) -> Result<Option<EncryptedIdentity>, IdentityStoreError> {
    let path = identity_file_path_in_dir(profile_dir);
    if !path.exists() {
        return Ok(None);
    }

    let bytes = std::fs::read(&path).map_err(|error| {
        IdentityStoreError::Io(format!("read identity {}: {error}", path.display()))
    })?;
    let identity: EncryptedIdentity = bincode::deserialize(&bytes)
        .map_err(|error| IdentityStoreError::Deserialize(error.to_string()))?;
    if identity.version != ENCRYPTED_IDENTITY_VERSION {
        return Err(IdentityStoreError::VersionMismatch {
            expected: ENCRYPTED_IDENTITY_VERSION,
            actual: identity.version,
        });
    }

    Ok(Some(identity))
}

fn encrypt_identity(
    secret: SecretKey,
    passphrase: &str,
) -> Result<(NostrKeys, EncryptedIdentity), String> {
    let encrypted = secret
        .encrypt(passphrase)
        .map_err(|error| format!("failed to encrypt identity: {error}"))?;
    let identity = NostrKeys::from_secret(secret);
    Ok((
        identity,
        EncryptedIdentity {
            version: ENCRYPTED_IDENTITY_VERSION,
            ciphertext: encrypted
                .to_bech32()
                .map_err(|error| format!("failed to encode ncryptsec: {error}"))?,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_identity_unlock_roundtrips() {
        let (identity, encrypted) = generate_encrypted_identity("correct horse").unwrap();
        let unlocked = unlock_identity(&encrypted, "correct horse").unwrap();

        assert_eq!(identity.public_key(), unlocked.public_key());
    }

    #[test]
    fn wrong_passphrase_fails() {
        let (_identity, encrypted) = generate_encrypted_identity("correct horse").unwrap();

        assert!(unlock_identity(&encrypted, "wrong horse").is_err());
    }

    #[test]
    fn npub_from_nostr_public_key_encodes_nip19_public_key() {
        let public = NostrPublicKey([0x2a; 32]);

        let npub = npub_from_nostr_public_key(public);

        assert_eq!(
            npub,
            "npub19g4z52329g4z52329g4z52329g4z52329g4z52329g4z52329g4qrd5mkx"
        );
        assert_eq!(PublicKey::from_bech32(&npub).unwrap().as_bytes(), &public.0);
    }

    #[test]
    fn raw_nsec_decodes() {
        let secret = SecretKey::generate();
        let nsec = secret.to_bech32().unwrap();

        let decoded = decode_nsec_or_ncryptsec(&nsec, None).unwrap();

        assert_eq!(secret, decoded);
    }

    #[test]
    fn ncryptsec_decodes_with_passphrase() {
        let secret = SecretKey::generate();
        let encrypted = secret.encrypt("server passphrase").unwrap();
        let ncryptsec = encrypted.to_bech32().unwrap();

        let decoded = decode_nsec_or_ncryptsec(&ncryptsec, Some("server passphrase")).unwrap();

        assert_eq!(secret, decoded);
    }

    #[test]
    fn ncryptsec_requires_passphrase() {
        let secret = SecretKey::generate();
        let encrypted = secret.encrypt("server passphrase").unwrap();
        let ncryptsec = encrypted.to_bech32().unwrap();

        let error = decode_nsec_or_ncryptsec(&ncryptsec, None).unwrap_err();

        assert!(error.contains("NOSTR_IDENTITY_PASSPHRASE"));
    }

    #[test]
    fn nostr_config_dir_prefers_xdg_config_home() {
        assert_eq!(
            nostr_config_dir_from(
                Some(PathBuf::from("/tmp/xdg")),
                Some(PathBuf::from("/tmp/home"))
            ),
            PathBuf::from("/tmp/xdg/nostr")
        );
    }

    #[test]
    fn nostr_config_dir_falls_back_to_home_config() {
        assert_eq!(
            nostr_config_dir_from(None, Some(PathBuf::from("/tmp/home"))),
            PathBuf::from("/tmp/home/.config/nostr")
        );
    }

    #[test]
    fn client_identity_dir_uses_profile_subdirectory() {
        assert_eq!(
            client_identity_dir(Some("alice_1")).unwrap(),
            nostr_config_dir().join("profiles").join("alice_1")
        );
    }

    #[test]
    fn identity_file_path_uses_profile_subdirectory() {
        assert_eq!(
            identity_file_path(Some("dev-server")).unwrap(),
            nostr_config_dir()
                .join("profiles")
                .join("dev-server")
                .join("identity.bin")
        );
    }

    #[test]
    fn encrypted_identity_file_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let (_identity, encrypted) = generate_encrypted_identity("profile passphrase").unwrap();

        save_encrypted_identity_to_dir(dir.path(), &encrypted).unwrap();
        let loaded = load_encrypted_identity_from_dir(dir.path())
            .unwrap()
            .expect("identity should exist");

        assert_eq!(loaded.version, encrypted.version);
        assert_eq!(loaded.ciphertext, encrypted.ciphertext);
        assert!(identity_file_path_in_dir(dir.path()).exists());
        assert!(!dir.path().join("identity.bin.tmp").exists());
    }

    #[test]
    fn server_identity_loads_from_encrypted_profile_identity() {
        let dir = tempfile::tempdir().unwrap();
        let (client_identity, encrypted) =
            generate_encrypted_identity("profile passphrase").unwrap();
        save_encrypted_identity_to_dir(dir.path(), &encrypted).unwrap();

        let server_identity =
            load_nostr_keys_from_profile_dir(dir.path(), Some("profile passphrase")).unwrap();

        assert_eq!(server_identity.public_key(), client_identity.public_key());
    }

    #[test]
    fn server_identity_profile_requires_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let (_client_identity, encrypted) =
            generate_encrypted_identity("profile passphrase").unwrap();
        save_encrypted_identity_to_dir(dir.path(), &encrypted).unwrap();

        let error = match load_nostr_keys_from_profile_dir(dir.path(), None) {
            Ok(_) => panic!("profile identity should require passphrase"),
            Err(error) => error,
        };

        assert!(error.contains("NOSTR_IDENTITY_PASSPHRASE"));
    }
    #[test]
    fn server_nsec_override_still_decodes_raw_nsec() {
        let secret = SecretKey::generate();
        let nsec = secret.to_bech32().unwrap();

        let identity = nostr_keys_from_secret_text(&nsec, None).unwrap();

        assert_eq!(identity.public_key(), Keys::new(secret).public_key());
    }

    #[test]
    fn identity_profile_rejects_path_like_values() {
        assert!(client_identity_dir(Some("../alice")).is_err());
        assert!(client_identity_dir(Some("alice/bob")).is_err());
        assert!(client_identity_dir(Some("")).is_err());
        assert!(client_identity_dir(Some("--client-id")).is_err());
    }
}
