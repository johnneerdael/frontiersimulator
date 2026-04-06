pub mod curl_ffi;
pub mod probe;
pub mod download;
pub mod reactor;

use serde::{Deserialize, Serialize};

/// Result of probing a URL for download capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResult {
    pub effective_url_hash: String,
    pub range_support: bool,
    pub content_length: Option<u64>,
    pub protocol: String,
    pub remote_endpoint: String,
    pub local_endpoint: String,
    pub dns_ms: f64,
    pub connect_ms: f64,
    pub tls_ms: f64,
    pub ttfb_ms: f64,
    pub response_headers: Vec<(String, String)>,
}

/// Byte sink for downloaded data. Only `Discard` is implemented in MVP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Sink {
    /// Discard bytes after page accounting (benchmark mode).
    Discard,
    /// Write to a sparse file for gap/overlap validation.
    SparseFile,
    /// Rolling ring buffer for limited content inspection.
    RollingRing,
}

impl Sink {
    pub fn is_implemented(&self) -> bool {
        matches!(self, Sink::Discard)
    }
}

impl Default for Sink {
    fn default() -> Self {
        Sink::Discard
    }
}

/// Protocol version to request via CURLOPT_HTTP_VERSION.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProtocolMode {
    Auto,
    H1,
    H2,
}

impl Default for ProtocolMode {
    fn default() -> Self {
        ProtocolMode::Auto
    }
}
