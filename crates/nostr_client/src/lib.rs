pub mod announcement;
pub mod auth;
pub mod blobs;
pub mod events;
pub mod identity;
pub mod plugin;
pub mod relay_pool;

pub use announcement::{
    ServerAnnouncement, NOSTR_KIND_SERVER_ANNOUNCEMENT, SERVER_ANNOUNCEMENT_REPUBLISH_SECS,
    SERVER_ANNOUNCEMENT_TTL_SECS, SERVER_ANNOUNCEMENT_VERSION,
};
pub use auth::{build_identity_proof, verify_identity_proof, NOSTR_KIND_AUTH};
pub use blobs::{
    upload_blob, BlobFetchPolicy, BlobReadError, BlobRef, BlobWriteError, VerifiedBlob,
};
pub use events::{
    publish_event, NostrEventDraft, NostrEventKind, NostrEventQuery, NostrTag, VerifiedNostrEvent,
};
pub use identity::{
    client_id_from_public_key, client_identity_dir, decode_nsec_or_ncryptsec,
    generate_encrypted_identity, identity_file_path, identity_file_path_in_dir,
    import_encrypted_identity, load_encrypted_identity_from_dir,
    load_server_identity_from_env_or_profile, load_server_identity_from_profile_dir,
    npub_from_nostr_public_key, save_encrypted_identity_to_dir, unlock_identity, ClientIdentity,
    EncryptedIdentity, IdentityStoreError, LoginError, SaveEncryptedIdentity, ServerIdentity,
    StoredEncryptedIdentity,
};
pub use plugin::{NostrClientConfig, NostrClientPlugin};
pub use relay_pool::{relay_pool_ready, RelayPool};
