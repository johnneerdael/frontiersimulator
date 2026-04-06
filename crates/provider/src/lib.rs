pub mod realdebrid;
pub mod premiumize;

use serde::{Deserialize, Serialize};
use std::fmt;

/// Stable identity for a debrid asset, independent of ephemeral direct URLs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AssetKey {
    pub provider: String,
    pub item_id: String,
    pub file_index: Option<u32>,
    pub size_bytes: u64,
}

impl AssetKey {
    pub fn canonical(&self) -> String {
        match self.file_index {
            Some(idx) => format!("{}:{}:{}:{}", self.provider, self.item_id, idx, self.size_bytes),
            None => format!("{}:{}:{}", self.provider, self.item_id, self.size_bytes),
        }
    }
}

impl fmt::Display for AssetKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.canonical())
    }
}

/// Reference to a candidate asset for benchmarking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetRef {
    pub key: AssetKey,
    pub filename: String,
}

/// Detailed information about an asset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetDetails {
    pub key: AssetKey,
    pub filename: String,
    pub mime_type: Option<String>,
}

/// A resolved direct download URL (ephemeral).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedAsset {
    pub key: AssetKey,
    pub direct_url: String,
    pub filename: String,
    pub size_bytes: u64,
    pub resolved_at_ms: u64,
    pub expiry_hint: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_meta: Option<serde_json::Value>,
}

/// Errors from provider operations.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    #[error("asset not found: {0}")]
    NotFound(String),
    #[error("rate limited, retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
    #[error("network error: {0}")]
    Network(String),
    #[error("provider error: {0}")]
    Other(String),
}

/// Common trait for debrid provider adapters.
/// Designed for Real-Debrid + Premiumize now, extensible for Torbox + Easy-Debrid.
pub trait DebridProvider {
    fn name(&self) -> &str;
    fn list_candidates(&self) -> Result<Vec<AssetRef>, ProviderError>;
    fn inspect(&self, asset: &AssetRef) -> Result<AssetDetails, ProviderError>;
    fn resolve_direct_url(&self, asset: &AssetRef) -> Result<ResolvedAsset, ProviderError>;
}
