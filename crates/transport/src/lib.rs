pub mod curl_ffi;
pub mod download;
pub mod probe;
pub mod reactor;

use curl::easy::{Easy2, Handler, HttpVersion};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

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

impl ProtocolMode {
    pub fn http_version(self) -> Option<HttpVersion> {
        match self {
            ProtocolMode::Auto => None,
            ProtocolMode::H1 => Some(HttpVersion::V11),
            ProtocolMode::H2 => Some(HttpVersion::V2),
        }
    }

    pub fn apply_to<H: Handler>(self, easy: &mut Easy2<H>) -> Result<(), curl::Error> {
        if let Some(version) = self.http_version() {
            easy.http_version(version)?;
        }
        Ok(())
    }
}

#[allow(non_camel_case_types)]
type curl_c_long = i64;

pub fn http_version_name_from_curl_info(http_ver: curl_c_long) -> &'static str {
    match http_ver {
        2 => "h1.1",
        3 => "h2",
        4 => "h3",
        _ => "unknown",
    }
}

pub fn negotiated_http_version(raw: *mut curl_sys::CURL) -> String {
    let mut http_ver: curl_c_long = 0;
    let curlinfo_http_version = 0x200000 + 46;
    let got = unsafe {
        curl_sys::curl_easy_getinfo(
            raw,
            curlinfo_http_version,
            &mut http_ver as *mut curl_c_long,
        ) == curl_sys::CURLE_OK
    };
    if got {
        http_version_name_from_curl_info(http_ver).to_string()
    } else {
        "unknown".to_string()
    }
}

impl fmt::Display for ProtocolMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            ProtocolMode::Auto => "auto",
            ProtocolMode::H1 => "h1",
            ProtocolMode::H2 => "h2",
        };
        f.write_str(value)
    }
}

impl FromStr for ProtocolMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(ProtocolMode::Auto),
            "h1" => Ok(ProtocolMode::H1),
            "h2" => Ok(ProtocolMode::H2),
            other => Err(format!("unsupported protocol mode '{other}'")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_mode_parses_cli_values() {
        assert_eq!("auto".parse::<ProtocolMode>().unwrap(), ProtocolMode::Auto);
        assert_eq!("h1".parse::<ProtocolMode>().unwrap(), ProtocolMode::H1);
        assert_eq!("h2".parse::<ProtocolMode>().unwrap(), ProtocolMode::H2);
        assert!("h3".parse::<ProtocolMode>().is_err());
    }

    #[test]
    fn protocol_mode_maps_to_curl_versions() {
        assert!(matches!(ProtocolMode::Auto.http_version(), None));
        assert!(matches!(
            ProtocolMode::H1.http_version(),
            Some(HttpVersion::V11)
        ));
        assert!(matches!(
            ProtocolMode::H2.http_version(),
            Some(HttpVersion::V2)
        ));
    }

    #[test]
    fn curl_http_version_mapping_is_stable() {
        assert_eq!(http_version_name_from_curl_info(2), "h1.1");
        assert_eq!(http_version_name_from_curl_info(3), "h2");
        assert_eq!(http_version_name_from_curl_info(4), "h3");
        assert_eq!(http_version_name_from_curl_info(99), "unknown");
    }
}
