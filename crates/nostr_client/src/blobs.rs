use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

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

/// Blob upload failures.
#[derive(Debug, Error)]
pub enum BlobWriteError {
    #[error("blob upload is not implemented: {0}")]
    Unsupported(String),
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
        let response = reqwest::get(url.clone())
            .await
            .map_err(|error| BlobReadError::Http(error.to_string()))?;
        let bytes = response
            .bytes()
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
