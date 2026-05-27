use serde::{Deserialize, Serialize};

/// Content-addressed blob reference used by map manifests and future Blossom helpers.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct BlobRef {
    pub sha256: [u8; 32],
    pub size: u64,
    pub urls: Vec<String>,
}
