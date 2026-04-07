use crate::curl_ffi;
use crate::Sink;
use crossbeam_channel::Sender;
use curl::easy::{Easy2, Handler, WriteError};
use frontier_telemetry::events::{Lane, TraceEvent};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

/// Page size for aggregation — 128KB matching Kotlin FrontierTracker.
pub const PAGE_SIZE: u64 = 128 * 1024;

/// State tracked during a single-range download via curl write callback.
struct DownloadHandler {
    request_id: u64,
    chunk_id: u64,
    #[allow(dead_code)]
    range_start: u64,
    bytes_received: u64,
    page_size: u64,
    current_page_bytes: u64,
    current_page_index: u64,
    event_tx: Sender<TraceEvent>,
    run_start: Instant,
    headers: Vec<(String, String)>,
    response_code: Option<u32>,
    sink: Sink,
}

impl DownloadHandler {
    fn new(
        request_id: u64,
        chunk_id: u64,
        range_start: u64,
        page_size: u64,
        event_tx: Sender<TraceEvent>,
        run_start: Instant,
        sink: Sink,
    ) -> Self {
        Self {
            request_id,
            chunk_id,
            range_start,
            bytes_received: 0,
            page_size,
            current_page_bytes: 0,
            current_page_index: range_start / page_size,
            event_tx,
            run_start,
            headers: Vec::new(),
            response_code: None,
            sink,
        }
    }

    fn timestamp_ns(&self) -> u64 {
        self.run_start.elapsed().as_nanos() as u64
    }

    fn emit_page_completed(&self) {
        let page_start = self.current_page_index * self.page_size;
        let page_end = page_start + self.current_page_bytes;
        let _ = self.event_tx.send(TraceEvent::PageCompleted {
            request_id: self.request_id,
            chunk_id: self.chunk_id,
            page_index: self.current_page_index,
            page_start_byte: page_start,
            page_end_byte: page_end,
            page_bytes: self.current_page_bytes,
            cumulative_bytes: self.bytes_received,
            timestamp_ns: self.timestamp_ns(),
        });
    }
}

impl Handler for DownloadHandler {
    fn write(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        let len = data.len() as u64;

        // Sink: discard bytes (we only track page boundaries)
        match self.sink {
            Sink::Discard => {} // no-op
            Sink::SparseFile | Sink::RollingRing => {
                // Not implemented yet
            }
        }

        // Aggregate bytes into pages
        let mut remaining = len;
        while remaining > 0 {
            let page_remaining = self.page_size - self.current_page_bytes;
            let to_consume = remaining.min(page_remaining);

            self.current_page_bytes += to_consume;
            self.bytes_received += to_consume;
            remaining -= to_consume;

            if self.current_page_bytes >= self.page_size {
                // Page complete
                self.emit_page_completed();
                self.current_page_index += 1;
                self.current_page_bytes = 0;
            }
        }

        Ok(data.len())
    }

    fn header(&mut self, data: &[u8]) -> bool {
        if let Ok(line) = std::str::from_utf8(data) {
            let trimmed = line.trim();
            // Parse HTTP status line
            if trimmed.starts_with("HTTP/") {
                if let Some(code_str) = trimmed.split_whitespace().nth(1) {
                    self.response_code = code_str.parse().ok();
                }
            }
            if let Some((key, value)) = trimmed.split_once(':') {
                self.headers
                    .push((key.trim().to_lowercase(), value.trim().to_string()));
            }
        }
        true
    }
}

/// Parameters for a single-range download.
pub struct DownloadParams {
    pub request_id: u64,
    pub chunk_id: u64,
    pub url: String,
    pub range_start: u64,
    pub range_end: u64,
    pub lane: Lane,
    pub attempt: u32,
    pub page_size: u64,
    pub timeout: Duration,
    pub sink: Sink,
}

/// Result of a completed single-range download.
pub struct DownloadResult {
    pub request_id: u64,
    pub total_bytes: u64,
    pub dns_ms: f64,
    pub connect_ms: f64,
    pub tls_ms: f64,
    pub ttfb_ms: f64,
    pub queue_ms: f64,
    pub total_ms: f64,
    pub http_version: String,
    pub connection_id: i64,
    pub new_connection: bool,
    pub response_code: u32,
    pub effective_url_hash: String,
    pub remote_endpoint: String,
    pub local_endpoint: String,
}

/// Execute a single-range download, emitting JSONL events via the channel.
pub fn download_range(
    params: &DownloadParams,
    event_tx: &Sender<TraceEvent>,
    run_start: Instant,
) -> Result<DownloadResult, String> {
    let handler = DownloadHandler::new(
        params.request_id,
        params.chunk_id,
        params.range_start,
        params.page_size,
        event_tx.clone(),
        run_start,
        params.sink,
    );

    let mut easy = Easy2::new(handler);
    easy.url(&params.url).map_err(|e| format!("set URL: {e}"))?;
    easy.follow_location(true)
        .map_err(|e| format!("follow location: {e}"))?;
    easy.timeout(params.timeout)
        .map_err(|e| format!("timeout: {e}"))?;

    // Set byte range
    let range = format!("{}-{}", params.range_start, params.range_end);
    easy.range(&range).map_err(|e| format!("set range: {e}"))?;

    // Emit request_started
    let _ = event_tx.send(TraceEvent::RequestStarted {
        request_id: params.request_id,
        chunk_id: params.chunk_id,
        lane: params.lane,
        attempt: params.attempt,
        requested_range: range.clone(),
        timestamp_ns: run_start.elapsed().as_nanos() as u64,
    });

    // Perform the transfer
    easy.perform().map_err(|e| format!("perform: {e}"))?;

    // Emit any partial page that wasn't completed
    {
        let handler = easy.get_ref();
        if handler.current_page_bytes > 0 {
            handler.emit_page_completed();
        }
    }

    // Extract timing info
    let dns_ms = easy
        .namelookup_time()
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    let connect_ms = easy
        .connect_time()
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    let appconnect_ms = easy
        .appconnect_time()
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    let tls_ms = (appconnect_ms - connect_ms).max(0.0);
    let ttfb_ms = easy
        .starttransfer_time()
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    let total_ms = easy
        .total_time()
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0);

    // Connection info via FFI
    let raw = easy.raw();
    let (connection_id, queue_us, remote_endpoint, local_endpoint) = unsafe {
        let conn_id = curl_ffi::get_conn_id(raw);
        let queue = curl_ffi::get_queue_time_us(raw);
        let remote_ip = curl_ffi::get_primary_ip(raw);
        let remote_port = curl_ffi::get_primary_port(raw);
        let local_ip = curl_ffi::get_local_ip(raw);
        let local_port = curl_ffi::get_local_port(raw);
        (
            conn_id,
            queue,
            format!("{remote_ip}:{remote_port}"),
            format!("{local_ip}:{local_port}"),
        )
    };
    let queue_ms = queue_us as f64 / 1000.0;

    // Determine if this was a new connection
    // If connect_ms > 0 and dns_ms > 0, likely a new connection
    let new_connection = connect_ms > 0.5; // heuristic: > 0.5ms connect = new

    // HTTP version from response
    let handler = easy.get_ref();
    let response_code = handler.response_code.unwrap_or(0);

    // Detect HTTP version from status line in headers
    let http_version = handler
        .headers
        .iter()
        .find(|(k, _)| k.starts_with("http/"))
        .map(|(k, _)| k.clone())
        .unwrap_or_else(|| "h1.1".to_string());

    // Effective URL hash
    let effective_url = easy
        .effective_url()
        .ok()
        .flatten()
        .unwrap_or(&params.url)
        .to_string();
    let mut hasher = Sha256::new();
    hasher.update(effective_url.as_bytes());
    let effective_url_hash = format!("{:x}", hasher.finalize());

    let total_bytes = handler.bytes_received;

    // Emit request_finished
    let _ = event_tx.send(TraceEvent::RequestFinished {
        request_id: params.request_id,
        total_bytes,
        dns_ms,
        connect_ms,
        tls_ms,
        ttfb_ms,
        queue_ms,
        total_ms,
        http_version: http_version.clone(),
        connection_id,
        new_connection,
        error_class: None,
        timestamp_ns: run_start.elapsed().as_nanos() as u64,
    });

    Ok(DownloadResult {
        request_id: params.request_id,
        total_bytes,
        dns_ms,
        connect_ms,
        tls_ms,
        ttfb_ms,
        queue_ms,
        total_ms,
        http_version,
        connection_id,
        new_connection,
        response_code,
        effective_url_hash,
        remote_endpoint,
        local_endpoint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_aggregation_logic() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let run_start = Instant::now();
        let mut handler = DownloadHandler::new(1, 0, 0, PAGE_SIZE, tx, run_start, Sink::Discard);

        // Write exactly one page worth of data
        let data = vec![0u8; PAGE_SIZE as usize];
        let written = handler.write(&data).unwrap();
        assert_eq!(written, PAGE_SIZE as usize);

        // Should have emitted one page_completed event
        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 1);
        if let TraceEvent::PageCompleted {
            page_index,
            page_bytes,
            ..
        } = &events[0]
        {
            assert_eq!(*page_index, 0);
            assert_eq!(*page_bytes, PAGE_SIZE);
        } else {
            panic!("Expected PageCompleted event");
        }
    }

    #[test]
    fn partial_page_not_emitted_until_full() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let run_start = Instant::now();
        let mut handler = DownloadHandler::new(1, 0, 0, PAGE_SIZE, tx, run_start, Sink::Discard);

        // Write half a page
        let data = vec![0u8; (PAGE_SIZE / 2) as usize];
        handler.write(&data).unwrap();

        // No event emitted yet
        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 0);
        assert_eq!(handler.bytes_received, PAGE_SIZE / 2);
    }

    #[test]
    fn multi_page_write() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let run_start = Instant::now();
        let mut handler = DownloadHandler::new(1, 0, 0, PAGE_SIZE, tx, run_start, Sink::Discard);

        // Write 2.5 pages in one call
        let size = (PAGE_SIZE * 2 + PAGE_SIZE / 2) as usize;
        let data = vec![0u8; size];
        handler.write(&data).unwrap();

        // Should have emitted 2 complete page events
        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 2);
        assert_eq!(handler.current_page_index, 2);
        assert_eq!(handler.current_page_bytes, PAGE_SIZE / 2);
    }

    #[test]
    fn range_offset_page_alignment() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let run_start = Instant::now();
        // Start at page 5 (offset = 5 * 128KB)
        let range_start = 5 * PAGE_SIZE;
        let mut handler =
            DownloadHandler::new(1, 0, range_start, PAGE_SIZE, tx, run_start, Sink::Discard);

        let data = vec![0u8; PAGE_SIZE as usize];
        handler.write(&data).unwrap();

        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 1);
        if let TraceEvent::PageCompleted { page_index, .. } = &events[0] {
            assert_eq!(*page_index, 5);
        }
    }
}
