use nostr_client::{BlobRef, VerifiedBlob};
use persistence::{AsyncStore, BoxedStoreFuture, PersistenceError};
use protocol::{MapInstanceId, NostrPublicKey};

use crate::manifest::{ManifestHash, NostrMapManifest};
use crate::policy::{MapPersistencePolicy, NostrMapQueryPolicy};
use crate::publish::manifest_hash_from_signed_event_json;
use crate::read::{fetch_manifest_by_hash, latest_visible_manifest};

/// Query for the latest visible manifest descendant of an accepted local head.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ManifestHeadQuery {
    pub owner: NostrPublicKey,
    pub map_id: MapInstanceId,
    pub accepted_head: Option<ManifestHash>,
}

/// Request to fetch and verify a content-addressed blob.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlobFetchRequest {
    pub blob: BlobRef,
    pub max_bytes: u64,
}

/// Async read-only store for latest visible map manifests.
#[derive(Clone)]
pub struct NostrManifestStore {
    pub client: nostr_client::events::NostrEventClient,
    pub policy: NostrMapQueryPolicy,
}

impl AsyncStore<ManifestHeadQuery, NostrMapManifest> for NostrManifestStore {
    fn load<'a>(
        &'a self,
        key: &'a ManifestHeadQuery,
    ) -> BoxedStoreFuture<'a, Result<Option<NostrMapManifest>, PersistenceError>> {
        Box::pin(async move {
            latest_visible_manifest(&self.client, key.owner, &key.map_id, self.policy.clone())
                .await
                .map_err(|error| PersistenceError::Deserialize(error.to_string()))
        })
    }

    fn save<'a>(
        &'a self,
        _key: &'a ManifestHeadQuery,
        _value: &'a NostrMapManifest,
    ) -> BoxedStoreFuture<'a, Result<(), PersistenceError>> {
        Box::pin(async {
            Err(PersistenceError::Serialize(
                "NostrManifestStore is read-only".to_string(),
            ))
        })
    }
}

/// Async read-only store for manifests addressed by manifest hash.
#[derive(Clone)]
pub struct NostrManifestByHashStore {
    pub client: nostr_client::events::NostrEventClient,
    pub owner: NostrPublicKey,
    pub map_id: MapInstanceId,
    pub policy: NostrMapQueryPolicy,
}

impl AsyncStore<ManifestHash, NostrMapManifest> for NostrManifestByHashStore {
    fn load<'a>(
        &'a self,
        key: &'a ManifestHash,
    ) -> BoxedStoreFuture<'a, Result<Option<NostrMapManifest>, PersistenceError>> {
        Box::pin(async move {
            fetch_manifest_by_hash(
                &self.client,
                self.owner,
                &self.map_id,
                *key,
                self.policy.clone(),
            )
            .await
            .map_err(|error| PersistenceError::Deserialize(error.to_string()))
        })
    }

    fn save<'a>(
        &'a self,
        _key: &'a ManifestHash,
        _value: &'a NostrMapManifest,
    ) -> BoxedStoreFuture<'a, Result<(), PersistenceError>> {
        Box::pin(async {
            Err(PersistenceError::Serialize(
                "NostrManifestByHashStore is read-only".to_string(),
            ))
        })
    }
}

/// Async read-only store for Blossom/content-addressed blob fetches.
#[derive(Clone)]
pub struct BlossomBlobStore {
    pub policy: MapPersistencePolicy,
}

impl AsyncStore<BlobFetchRequest, VerifiedBlob> for BlossomBlobStore {
    fn load<'a>(
        &'a self,
        key: &'a BlobFetchRequest,
    ) -> BoxedStoreFuture<'a, Result<Option<VerifiedBlob>, PersistenceError>> {
        Box::pin(async move {
            let policy = nostr_client::blobs::BlobFetchPolicy {
                max_bytes: key.max_bytes.min(self.policy.max_blob_bytes),
                allowed_hosts: self.policy.allowed_blossom_hosts.clone(),
            };
            nostr_client::blobs::fetch_and_verify_blob(&key.blob, &policy)
                .await
                .map(Some)
                .map_err(|error| PersistenceError::Deserialize(error.to_string()))
        })
    }

    fn save<'a>(
        &'a self,
        _key: &'a BlobFetchRequest,
        _value: &'a VerifiedBlob,
    ) -> BoxedStoreFuture<'a, Result<(), PersistenceError>> {
        Box::pin(async {
            Err(PersistenceError::Serialize(
                "BlossomBlobStore read-path adapter is read-only".to_string(),
            ))
        })
    }
}

/// Async write-only store for Blossom/content-addressed blob uploads.
#[derive(Clone)]
pub struct BlossomBlobPutStore {
    pub upload_url: String,
    pub auth: nostr_client::BlossomAuth,
}

impl AsyncStore<BlobRef, Vec<u8>> for BlossomBlobPutStore {
    fn load<'a>(
        &'a self,
        _key: &'a BlobRef,
    ) -> BoxedStoreFuture<'a, Result<Option<Vec<u8>>, PersistenceError>> {
        Box::pin(async {
            Err(PersistenceError::Deserialize(
                "BlossomBlobPutStore does not support load".to_string(),
            ))
        })
    }

    fn save<'a>(
        &'a self,
        key: &'a BlobRef,
        value: &'a Vec<u8>,
    ) -> BoxedStoreFuture<'a, Result<(), PersistenceError>> {
        Box::pin(async move {
            nostr_client::blobs::verify_blob_bytes(key.sha256, Some(key.size), value.clone())
                .map_err(|error| {
                    PersistenceError::Serialize(format!("verify upload blob bytes: {error}"))
                })?;
            let uploaded =
                nostr_client::blobs::upload_blob(&self.upload_url, value.clone(), &self.auth)
                    .await
                    .map_err(|error| {
                        PersistenceError::Serialize(format!("upload Blossom blob: {error}"))
                    })?;
            if uploaded.sha256 != key.sha256 || uploaded.size != key.size {
                return Err(PersistenceError::Serialize(format!(
                    "uploaded blob ref mismatch: expected {} bytes {:?}, got {} bytes {:?}",
                    key.size, key.sha256, uploaded.size, uploaded.sha256
                )));
            }
            Ok(())
        })
    }
}

/// Async write-only store for publishing signed Nostr manifest events.
#[derive(Clone)]
pub struct NostrManifestPublishStore {
    pub client: nostr_client::events::NostrEventClient,
}

impl AsyncStore<ManifestHash, String> for NostrManifestPublishStore {
    fn load<'a>(
        &'a self,
        _key: &'a ManifestHash,
    ) -> BoxedStoreFuture<'a, Result<Option<String>, PersistenceError>> {
        Box::pin(async {
            Err(PersistenceError::Deserialize(
                "NostrManifestPublishStore does not support load".to_string(),
            ))
        })
    }

    fn save<'a>(
        &'a self,
        key: &'a ManifestHash,
        value: &'a String,
    ) -> BoxedStoreFuture<'a, Result<(), PersistenceError>> {
        Box::pin(async move {
            let actual_hash = manifest_hash_from_signed_event_json(value).map_err(|error| {
                PersistenceError::Serialize(format!("verify manifest publish event: {error}"))
            })?;
            if actual_hash != *key {
                return Err(PersistenceError::Serialize(format!(
                    "manifest publish hash mismatch: expected {:?}, got {:?}",
                    key, actual_hash
                )));
            }
            nostr_client::events::publish_event(&self.client, value.clone())
                .await
                .map_err(|error| {
                    PersistenceError::Serialize(format!("publish Nostr manifest event: {error}"))
                })
        })
    }
}
