use crate::curl_ffi;
use crate::ProbeResult;
use curl::easy::{Easy2, Handler, WriteError};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// Handler that discards body bytes but captures headers.
struct ProbeHandler {
    headers: Vec<(String, String)>,
}

impl ProbeHandler {
    fn new() -> Self {
        Self {
            headers: Vec::new(),
        }
    }
}

impl Handler for ProbeHandler {
    fn write(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        // Discard body for probe — we only care about headers and timings
        Ok(data.len())
    }

    fn header(&mut self, data: &[u8]) -> bool {
        if let Ok(line) = std::str::from_utf8(data) {
            let trimmed = line.trim();
            if let Some((key, value)) = trimmed.split_once(':') {
                self.headers
                    .push((key.trim().to_lowercase(), value.trim().to_string()));
            }
        }
        true
    }
}

/// Probe a URL to determine range support, content length, protocol, and timing facts.
pub fn probe_url(url: &str, timeout: Duration) -> Result<ProbeResult, String> {
    curl_ffi::check_minimum_version().map_err(|e| e)?;

    let mut easy = Easy2::new(ProbeHandler::new());
    easy.url(url).map_err(|e| format!("set URL: {e}"))?;
    easy.follow_location(true)
        .map_err(|e| format!("follow location: {e}"))?;
    easy.max_redirections(10)
        .map_err(|e| format!("max redirects: {e}"))?;
    easy.timeout(timeout)
        .map_err(|e| format!("timeout: {e}"))?;
    // Use HEAD if possible; fall back to GET with nobody
    easy.nobody(true)
        .map_err(|e| format!("nobody: {e}"))?;
    // Record timing
    easy.perform().map_err(|e| format!("perform: {e}"))?;

    let handler = easy.get_ref();
    let headers = &handler.headers;

    // Extract range support
    let range_support = headers
        .iter()
        .any(|(k, v)| k == "accept-ranges" && v.to_lowercase().contains("bytes"));

    // Extract content length
    let content_length = easy.content_length_download().ok().and_then(|l| {
        if l > 0.0 {
            Some(l as u64)
        } else {
            // Try header fallback
            headers
                .iter()
                .find(|(k, _)| k == "content-length")
                .and_then(|(_, v)| v.parse::<u64>().ok())
        }
    });

    // Timings in milliseconds
    let dns_ms = easy
        .namelookup_time()
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    let connect_ms = easy
        .connect_time()
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    let tls_ms = easy
        .appconnect_time()
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
        - connect_ms;
    let ttfb_ms = easy
        .starttransfer_time()
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0);

    // Protocol
    let http_version = easy
        .response_code()
        .map(|_| {
            // Infer from headers — check for HTTP/2 indicator
            if headers
                .iter()
                .any(|(_, v)| v.contains("h2") || v.contains("HTTP/2"))
            {
                "h2".to_string()
            } else {
                "h1.1".to_string()
            }
        })
        .unwrap_or_else(|_| "unknown".to_string());

    // Connection endpoints via curl_ffi
    let raw = easy.raw();
    let (remote_endpoint, local_endpoint) = unsafe {
        let remote_ip = curl_ffi::get_primary_ip(raw);
        let remote_port = curl_ffi::get_primary_port(raw);
        let local_ip = curl_ffi::get_local_ip(raw);
        let local_port = curl_ffi::get_local_port(raw);
        (
            format!("{remote_ip}:{remote_port}"),
            format!("{local_ip}:{local_port}"),
        )
    };

    // Hash the effective URL for stable identification
    let effective_url = easy
        .effective_url()
        .ok()
        .flatten()
        .unwrap_or(url)
        .to_string();
    let mut hasher = Sha256::new();
    hasher.update(effective_url.as_bytes());
    let effective_url_hash = format!("{:x}", hasher.finalize());

    // Collect response headers
    let response_headers: Vec<(String, String)> = headers.clone();

    Ok(ProbeResult {
        effective_url_hash,
        range_support,
        content_length,
        protocol: http_version,
        remote_endpoint,
        local_endpoint,
        dns_ms,
        connect_ms,
        tls_ms: tls_ms.max(0.0),
        ttfb_ms,
        response_headers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_handler_captures_headers() {
        let mut handler = ProbeHandler::new();
        handler.header(b"Content-Type: application/octet-stream\r\n");
        handler.header(b"Accept-Ranges: bytes\r\n");
        handler.header(b"Content-Length: 1234567\r\n");

        assert_eq!(handler.headers.len(), 3);
        assert_eq!(handler.headers[0].0, "content-type");
        assert_eq!(handler.headers[1].0, "accept-ranges");
        assert_eq!(handler.headers[1].1, "bytes");
    }
}
