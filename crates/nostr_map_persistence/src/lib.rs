pub mod attestation;
pub mod manifest;
pub mod policy;
pub mod publish;
pub mod read;
pub mod stores;
pub mod validation;

pub use attestation::{
    attestation_signing_payload, sign_homebase_attestation, verify_homebase_attestation,
    AttestationSigner, AttestationVerifier, HOMEBASE_ATTESTATION_SIGNING_DOMAIN,
};
pub use protocol::{HomebasePayloadScope, HomebasePublicationAttestation};
pub use validation::validate_homebase_manifest_attestation;

pub use manifest::{
    compute_descriptor_root, compute_manifest_hash, manifest_event_draft, manifest_event_tags,
    manifest_hash_hex, manifest_payload_descriptor_order, map_tag_value, verify_descriptor_root,
    verify_manifest_event, verify_manifest_event_tags, verify_manifest_event_with_hash,
    ManifestHash, ManifestPayloadDescriptor, ManifestPayloadSlot, MapPersistenceRejection,
    MapRevision, NostrMapManifest, PayloadClass, PayloadKey, PayloadSlotState,
    RawChunkEntitiesPayload, RawChunkPayload, RawMapEntitiesPayload, RawMapMetaPayload,
    RawMapPayloads, RawSaveBase, RawValidatedMapDelta, RawValidatedMapSave, VerifiedManifest,
    MANIFEST_HASH_TAG, MAP_MANIFEST_SCHEMA_VERSION, MAP_TAG, NOSTR_KIND_MAP_MANIFEST,
    PREVIOUS_MANIFEST_HASH_TAG,
};
pub use nostr_client::BlobRef;
pub use policy::{ManifestTieBreak, MapPersistencePolicy, NostrMapQueryPolicy};
pub use publish::{
    build_homebase_manifest_event, build_signed_map_manifest_event,
    manifest_hash_from_signed_event_json, ClientHomebaseUpdate, MapManifestSigner,
};
pub use read::{
    assemble_raw_validated_map_save, download_payloads, fetch_manifest_ancestors,
    fetch_manifest_by_hash, latest_visible_manifest, validate_remote_map_save,
    verify_revision_chain, RemotePersistenceError, RevisionDecision,
};
pub use stores::{
    BlobFetchRequest, BlossomBlobPutStore, BlossomBlobStore, ManifestHeadQuery,
    NostrManifestByHashStore, NostrManifestPublishStore, NostrManifestStore,
};
