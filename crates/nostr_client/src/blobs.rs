use std::collections::BTreeSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::events::{NostrEventDraft, NostrEventKind, NostrTag};
use crate::NostrKeys;

/// Content-addressed blob reference used by manifests and Blossom helpers.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct BlobRef {
    pub sha256: [u8; 32],
    pub size: u64,
    pub content_type: String,
    pub urls: Vec<String>,
}

/// Verified blob bytes matching a content-addressed reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedBlob {
    pub sha256: [u8; 32],
    pub bytes: Vec<u8>,
}

/// Policy for fetching untrusted content-addressed blobs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlobFetchPolicy {
    pub max_bytes: u64,
    pub allowed_hosts: BTreeSet<String>,
}

/// Blob download and verification failures.
#[derive(Debug, Error)]
pub enum BlobReadError {
    #[error("invalid blob URL: {0}")]
    InvalidUrl(String),
    #[error("blob URL is missing a host")]
    MissingHost,
    #[error("blob host is not allowed: {0}")]
    ForbiddenHost(String),
    #[error("blob has no policy-approved URLs")]
    NoAllowedUrls,
    #[error("blob exceeds max byte limit: size={size}, max={max}")]
    TooLarge { size: u64, max: u64 },
    #[error("blob size mismatch: expected {expected}, actual {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("blob hash mismatch")]
    HashMismatch {
        expected: [u8; 32],
        actual: [u8; 32],
    },
    #[error("blob HTTP request failed: {0}")]
    Http(String),
}

/// Blossom Nostr authorization event kind from BUD-11.
pub const NOSTR_KIND_BLOSSOM_AUTH: u16 = 24242;

/// Maximum time to wait for a Blossom blob upload before failing loudly.
///
/// Without a bound a stalled upload endpoint hangs the publish task forever, so the
/// requesting client never receives a response. A timeout turns a stall into an error.
const BLOB_UPLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Signed BUD-11 Blossom authorization helper.
#[derive(Clone)]
pub struct BlossomAuth {
    keys: NostrKeys,
    ttl: Duration,
}

impl BlossomAuth {
    /// Creates Blossom authorization tokens signed by the provided Nostr keys.
    pub fn new(keys: NostrKeys, ttl: Duration) -> Self {
        Self { keys, ttl }
    }

    /// Creates Blossom authorization tokens with a five-minute validity window.
    pub fn from_keys(keys: &NostrKeys) -> Self {
        Self::new(keys.clone(), Duration::from_secs(300))
    }

    /// Builds a scoped BUD-11 upload token for one upload URL and blob hash.
    pub fn upload_token(
        &self,
        upload_url: &url::Url,
        sha256_hex: &str,
    ) -> Result<BlossomAuthToken, BlobWriteError> {
        let host = upload_url
            .host_str()
            .ok_or_else(|| {
                BlobWriteError::InvalidUrl("blob upload URL is missing a host".to_string())
            })?
            .to_ascii_lowercase();
        let expiration = SystemTime::now()
            .checked_add(self.ttl)
            .expect("Blossom auth TTL should not overflow system time")
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after UNIX_EPOCH")
            .as_secs()
            .to_string();
        let event_json = self
            .keys
            .sign_event(&NostrEventDraft {
                kind: NostrEventKind::Custom(NOSTR_KIND_BLOSSOM_AUTH),
                content: "Upload Blob".to_string(),
                tags: vec![
                    NostrTag::new("t", "upload"),
                    NostrTag::new("expiration", expiration),
                    NostrTag::new("server", host),
                    NostrTag::new("x", sha256_hex.to_ascii_lowercase()),
                ],
            })
            .map_err(|error| BlobWriteError::Auth(error.to_string()))?;
        let encoded =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(event_json.as_bytes());
        Ok(BlossomAuthToken {
            event_json,
            authorization_header: format!("Nostr {encoded}"),
        })
    }
}

/// Signed Blossom authorization token and HTTP header value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlossomAuthToken {
    pub event_json: String,
    pub authorization_header: String,
}

#[derive(Debug, Deserialize)]
struct BlossomUploadDescriptor {
    url: String,
    sha256: String,
    size: u64,
    #[serde(rename = "type")]
    content_type: String,
}

/// Blob upload failures.
#[derive(Debug, Error)]
pub enum BlobWriteError {
    #[error("invalid blob upload URL: {0}")]
    InvalidUrl(String),
    #[error("blob authorization failed: {0}")]
    Auth(String),
    #[error("invalid blob upload response: {0}")]
    InvalidResponse(String),
    #[error("blob HTTP request failed: {0}")]
    Http(String),
}

/// Verifies a Blossom/blob URL against policy before any network request.
pub fn verify_blob_url(url: &url::Url, policy: &BlobFetchPolicy) -> Result<(), BlobReadError> {
    if url.scheme() != "https" {
        return Err(BlobReadError::InvalidUrl(format!(
            "blob URL must use https: {url}"
        )));
    }
    let host = url.host_str().ok_or(BlobReadError::MissingHost)?;
    if !policy.allowed_hosts.contains(host) {
        return Err(BlobReadError::ForbiddenHost(host.to_owned()));
    }
    Ok(())
}

/// Verifies blob bytes match the expected hash and optional size.
pub fn verify_blob_bytes(
    expected_sha256: [u8; 32],
    expected_size: Option<u64>,
    bytes: Vec<u8>,
) -> Result<VerifiedBlob, BlobReadError> {
    if let Some(expected_size) = expected_size {
        let actual_size = bytes.len() as u64;
        if actual_size != expected_size {
            return Err(BlobReadError::SizeMismatch {
                expected: expected_size,
                actual: actual_size,
            });
        }
    }
    let actual: [u8; 32] = Sha256::digest(&bytes).into();
    if actual != expected_sha256 {
        return Err(BlobReadError::HashMismatch {
            expected: expected_sha256,
            actual,
        });
    }
    Ok(VerifiedBlob {
        sha256: expected_sha256,
        bytes,
    })
}

/// Uploads bytes to a Blossom-compatible endpoint and returns a content-addressed reference.
pub async fn upload_blob(
    upload_url: &str,
    bytes: Vec<u8>,
    auth: &BlossomAuth,
) -> Result<BlobRef, BlobWriteError> {
    let url = url::Url::parse(upload_url)
        .map_err(|error| BlobWriteError::InvalidUrl(error.to_string()))?;
    if url.scheme() != "https" && cfg!(not(test)) {
        return Err(BlobWriteError::InvalidUrl(format!(
            "blob upload URL must use https: {url}"
        )));
    }
    let size = bytes.len() as u64;
    let sha256: [u8; 32] = Sha256::digest(&bytes).into();
    let sha256_hex = hex::encode(sha256);
    let token = auth.upload_token(&url, &sha256_hex)?;
    // Build the client and issue the request inside `await_network` so the whole reqwest
    // operation runs in the Tokio runtime context. A `.timeout()` request registers a Tokio
    // timer when the send future is constructed, so constructing it outside the context
    // panics with "no reactor running".
    let auth_header = token.authorization_header;
    let sha_header = sha256_hex.clone();
    let put_url = url.clone();
    let response = crate::compat::await_network(async move {
        let client = reqwest::Client::builder()
            .timeout(BLOB_UPLOAD_TIMEOUT)
            .build()?;
        client
            .put(put_url)
            .header("Authorization", auth_header)
            .header("X-SHA-256", sha_header)
            .header("Content-Type", "image/png")
            .body(bytes)
            .send()
            .await
    })
    .await
    .map_err(|error| BlobWriteError::Http(error.to_string()))?;
    if !response.status().is_success() {
        return Err(BlobWriteError::Http(format!(
            "blob upload failed with status {}",
            response.status()
        )));
    }
    let descriptor: BlossomUploadDescriptor =
        crate::compat::await_network(response.json())
            .await
            .map_err(|error| BlobWriteError::InvalidResponse(error.to_string()))?;
    if descriptor.sha256.to_ascii_lowercase() != sha256_hex {
        return Err(BlobWriteError::InvalidResponse(format!(
            "uploaded blob sha256 mismatch: expected {sha256_hex}, got {}",
            descriptor.sha256
        )));
    }
    let actual_sha256 = hex::decode(&descriptor.sha256)
        .map_err(|error| BlobWriteError::InvalidResponse(format!("invalid sha256: {error}")))?;
    let actual_sha256: [u8; 32] = actual_sha256
        .try_into()
        .map_err(|_| BlobWriteError::InvalidResponse("sha256 must be 32 bytes".to_string()))?;
    if actual_sha256 != sha256 {
        return Err(BlobWriteError::InvalidResponse(
            "uploaded blob hash mismatch".to_string(),
        ));
    }
    if descriptor.size != size {
        return Err(BlobWriteError::InvalidResponse(format!(
            "uploaded blob size mismatch: expected {size}, got {}",
            descriptor.size
        )));
    }
    Ok(BlobRef {
        sha256: actual_sha256,
        size: descriptor.size,
        content_type: descriptor.content_type,
        urls: vec![descriptor.url],
    })
}

/// Fetches a blob from the first policy-approved URL whose body matches hash and size.
pub async fn fetch_and_verify_blob(
    blob: &BlobRef,
    policy: &BlobFetchPolicy,
) -> Result<VerifiedBlob, BlobReadError> {
    if blob.size > policy.max_bytes {
        return Err(BlobReadError::TooLarge {
            size: blob.size,
            max: policy.max_bytes,
        });
    }

    let mut saw_allowed_url = false;
    let mut last_error = None;
    for raw_url in &blob.urls {
        let url = match url::Url::parse(raw_url) {
            Ok(url) => url,
            Err(error) => {
                last_error = Some(BlobReadError::InvalidUrl(error.to_string()));
                continue;
            }
        };
        if let Err(error) = verify_blob_url(&url, policy) {
            last_error = Some(error);
            continue;
        }
        saw_allowed_url = true;
        let response = crate::compat::await_network(reqwest::get(url.clone()))
            .await
            .map_err(|error| BlobReadError::Http(error.to_string()))?;
        let bytes = crate::compat::await_network(response.bytes())
            .await
            .map_err(|error| BlobReadError::Http(error.to_string()))?
            .to_vec();
        match verify_blob_bytes(blob.sha256, Some(blob.size), bytes) {
            Ok(verified) => return Ok(verified),
            Err(error) => last_error = Some(error),
        }
    }

    if !saw_allowed_url {
        return Err(BlobReadError::NoAllowedUrls);
    }
    Err(last_error.unwrap_or(BlobReadError::NoAllowedUrls))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::{Event, JsonUtil, Kind, SecretKey};

    fn hash(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }

    #[test]
    fn blobs_verify_blob_bytes_accepts_matching_hash_and_size() {
        let bytes = b"payload".to_vec();
        let verified = verify_blob_bytes(hash(&bytes), Some(bytes.len() as u64), bytes.clone())
            .expect("matching blob verifies");
        assert_eq!(verified.bytes, bytes);
    }

    #[test]
    fn blobs_verify_blob_bytes_rejects_hash_tampering() {
        let error = verify_blob_bytes([9; 32], Some(7), b"payload".to_vec())
            .expect_err("wrong hash rejected");
        assert!(matches!(error, BlobReadError::HashMismatch { .. }));
    }

    #[test]
    fn blobs_verify_blob_bytes_rejects_size_tampering() {
        let bytes = b"payload".to_vec();
        let error =
            verify_blob_bytes(hash(&bytes), Some(8), bytes).expect_err("wrong size rejected");
        assert!(matches!(error, BlobReadError::SizeMismatch { .. }));
    }

    #[test]
    fn blossom_auth_upload_token_is_bud11_scoped() {
        let keys = NostrKeys::from_secret(SecretKey::generate());
        let auth = BlossomAuth::from_keys(&keys);
        let sha256_hex = hex::encode([3; 32]);

        let token = auth
            .upload_token(
                &url::Url::parse("https://Blossom.Example/upload").unwrap(),
                &sha256_hex,
            )
            .unwrap();

        assert!(token.authorization_header.starts_with("Nostr "));
        assert!(!token.authorization_header.contains('='));
        let encoded = token
            .authorization_header
            .strip_prefix("Nostr ")
            .expect("header should use Nostr scheme");
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .unwrap();
        assert_eq!(decoded, token.event_json.as_bytes());
        let event = Event::from_json(&token.event_json).unwrap();
        event.verify().unwrap();
        assert_eq!(event.kind, Kind::Custom(NOSTR_KIND_BLOSSOM_AUTH));
        assert_eq!(event.pubkey, keys.public_key());
        assert!(event
            .tags
            .iter()
            .any(|tag| tag.as_slice() == ["t", "upload"]));
        assert!(event
            .tags
            .iter()
            .any(|tag| tag.as_slice() == ["server", "blossom.example"]));
        assert!(event
            .tags
            .iter()
            .any(|tag| tag.as_slice() == ["x", sha256_hex.as_str()]));
    }

    #[test]
    fn blobs_verify_blob_url_requires_https_and_allowed_host() {
        let mut hosts = BTreeSet::new();
        hosts.insert("example.com".to_string());
        let policy = BlobFetchPolicy {
            max_bytes: 1024,
            allowed_hosts: hosts,
        };
        verify_blob_url(
            &url::Url::parse("https://example.com/blob").unwrap(),
            &policy,
        )
        .expect("allowed https URL");
        assert!(matches!(
            verify_blob_url(
                &url::Url::parse("http://example.com/blob").unwrap(),
                &policy
            ),
            Err(BlobReadError::InvalidUrl(_))
        ));
        assert!(matches!(
            verify_blob_url(&url::Url::parse("https://evil.test/blob").unwrap(), &policy),
            Err(BlobReadError::ForbiddenHost(_))
        ));
    }
}
