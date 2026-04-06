//! Transfer Reactor: single-threaded curl multi handle driving parallel range requests.
//!
//! Uses `curl_multi_poll()` as the event loop driver. All handle additions,
//! removals, state mutations, and timing happen on this single thread.

use crate::curl_ffi;
use crate::download::PAGE_SIZE;
use crate::Sink;
use crossbeam_channel::Sender;
use curl::easy::{Easy2, Handler, WriteError};
use curl::multi::{Easy2Handle, Multi};
use frontier_telemetry::events::{ErrorClass, Lane, StallClass, TraceEvent};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[allow(non_camel_case_types)]
mod libc_types {
    pub type c_long = i64;
}

/// Stall threshold — no page progress for this duration triggers stall detection.
/// Matches Kotlin's STALL_THRESHOLD_MS = 2000.
const STALL_THRESHOLD_MS: u64 = 2_000;

/// Token to identify an easy handle within the multi handle.
type Token = usize;

/// Configuration for a reactor run.
pub struct ReactorConfig {
    pub urgent_workers: u32,
    pub prefetch_workers: u32,
    pub page_size: u64,
    pub sink: Sink,
    pub stall_timeout: Duration,
    pub request_timeout: Duration,
    /// Maximum benchmark duration. Once elapsed, no new chunks are started.
    /// In-flight requests complete normally. Zero means no limit.
    pub max_duration: Duration,
}

impl Default for ReactorConfig {
    fn default() -> Self {
        Self {
            urgent_workers: 2,
            prefetch_workers: 1,
            page_size: PAGE_SIZE,
            sink: Sink::Discard,
            stall_timeout: Duration::from_millis(STALL_THRESHOLD_MS),
            request_timeout: Duration::from_secs(120),
            max_duration: Duration::from_secs(120),
        }
    }
}

/// A chunk to be downloaded as a range request.
#[derive(Debug, Clone)]
pub struct ChunkRequest {
    pub chunk_id: u64,
    pub range_start: u64,
    pub range_end: u64,
    pub lane: Lane,
}

/// Tracks state for an in-flight request within the reactor.
struct RequestState {
    request_id: u64,
    chunk_id: u64,
    lane: Lane,
    attempt: u32,
    range_start: u64,
    range_end: u64,
    bytes_received: u64,
    last_progress_at: Instant,
    stalled: bool,
}

/// Connection facts from a completed or in-progress transfer.
#[derive(Debug, Clone)]
pub struct ConnectionRecord {
    pub connection_id: i64,
    pub protocol: String,
    pub local_endpoint: String,
    pub remote_endpoint: String,
    pub first_use_ns: u64,
    pub last_use_ns: u64,
    pub requests_count: u32,
    pub bytes_total: u64,
}

/// Handler for reactor-managed downloads.
struct ReactorHandler {
    request_id: u64,
    chunk_id: u64,
    page_size: u64,
    current_page_bytes: u64,
    current_page_index: u64,
    bytes_received: u64,
    event_tx: Sender<TraceEvent>,
    run_start: Instant,
    headers: Vec<(String, String)>,
    response_code: Option<u32>,
}

impl ReactorHandler {
    fn new(
        request_id: u64,
        chunk_id: u64,
        range_start: u64,
        page_size: u64,
        event_tx: Sender<TraceEvent>,
        run_start: Instant,
    ) -> Self {
        Self {
            request_id,
            chunk_id,
            page_size,
            current_page_bytes: 0,
            current_page_index: range_start / page_size,
            bytes_received: 0,
            event_tx,
            run_start,
            headers: Vec::new(),
            response_code: None,
        }
    }

    fn timestamp_ns(&self) -> u64 {
        self.run_start.elapsed().as_nanos() as u64
    }
}

impl Handler for ReactorHandler {
    fn write(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        let len = data.len() as u64;
        let mut remaining = len;

        while remaining > 0 {
            let page_remaining = self.page_size - self.current_page_bytes;
            let to_consume = remaining.min(page_remaining);

            self.current_page_bytes += to_consume;
            self.bytes_received += to_consume;
            remaining -= to_consume;

            if self.current_page_bytes >= self.page_size {
                let page_start = self.current_page_index * self.page_size;
                let _ = self.event_tx.send(TraceEvent::PageCompleted {
                    request_id: self.request_id,
                    chunk_id: self.chunk_id,
                    page_index: self.current_page_index,
                    page_start_byte: page_start,
                    page_end_byte: page_start + self.current_page_bytes,
                    page_bytes: self.current_page_bytes,
                    cumulative_bytes: self.bytes_received,
                    timestamp_ns: self.timestamp_ns(),
                });
                self.current_page_index += 1;
                self.current_page_bytes = 0;
            }
        }

        Ok(data.len())
    }

    fn header(&mut self, data: &[u8]) -> bool {
        if let Ok(line) = std::str::from_utf8(data) {
            let trimmed = line.trim();
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

/// Result of a reactor run with multiple parallel range requests.
pub struct ReactorResult {
    pub total_bytes: u64,
    pub total_requests: u32,
    pub total_connections: u32,
    pub duration_ms: f64,
    pub connections: Vec<ConnectionRecord>,
}

/// Run parallel range requests using curl multi with poll-based event loop.
pub fn run_parallel(
    url: &str,
    chunks: &[ChunkRequest],
    config: &ReactorConfig,
    event_tx: &Sender<TraceEvent>,
    run_start: Instant,
) -> Result<ReactorResult, String> {
    let multi = Multi::new();

    let max_concurrent = (config.urgent_workers + config.prefetch_workers) as usize;
    let mut active_handles: HashMap<Token, (Easy2Handle<ReactorHandler>, RequestState)> =
        HashMap::new();
    let mut connections: HashMap<i64, ConnectionRecord> = HashMap::new();
    let mut next_token: Token = 0;
    let mut next_request_id: u64 = 1;
    let mut chunk_queue: Vec<ChunkRequest> = chunks.to_vec();
    // Reverse so we can pop from the end (FIFO order)
    chunk_queue.reverse();

    let mut total_bytes: u64 = 0;
    let mut total_requests: u32 = 0;

    let has_duration_limit = config.max_duration > Duration::ZERO;

    loop {
        // Check duration limit — stop queueing new chunks if time is up
        let time_expired = has_duration_limit && run_start.elapsed() >= config.max_duration;

        // Fill slots with queued chunks (unless time expired)
        while !time_expired && active_handles.len() < max_concurrent {
            let chunk = match chunk_queue.pop() {
                Some(c) => c,
                None => break,
            };

            let request_id = next_request_id;
            next_request_id += 1;
            let token = next_token;
            next_token += 1;

            let handler = ReactorHandler::new(
                request_id,
                chunk.chunk_id,
                chunk.range_start,
                config.page_size,
                event_tx.clone(),
                run_start,
            );

            let mut easy = Easy2::new(handler);
            easy.url(url).map_err(|e| format!("set URL: {e}"))?;
            easy.follow_location(true)
                .map_err(|e| format!("follow: {e}"))?;
            easy.timeout(config.request_timeout)
                .map_err(|e| format!("timeout: {e}"))?;

            let range = format!("{}-{}", chunk.range_start, chunk.range_end);
            easy.range(&range)
                .map_err(|e| format!("range: {e}"))?;

            // Emit request_started
            let _ = event_tx.send(TraceEvent::RequestStarted {
                request_id,
                chunk_id: chunk.chunk_id,
                lane: chunk.lane,
                attempt: 1,
                requested_range: range,
                timestamp_ns: run_start.elapsed().as_nanos() as u64,
            });

            let state = RequestState {
                request_id,
                chunk_id: chunk.chunk_id,
                lane: chunk.lane,
                attempt: 1,
                range_start: chunk.range_start,
                range_end: chunk.range_end,
                bytes_received: 0,
                last_progress_at: Instant::now(),
                stalled: false,
            };

            let mut handle = multi
                .add2(easy)
                .map_err(|e| format!("multi add: {e}"))?;
            handle.set_token(token).map_err(|e| format!("set token: {e}"))?;
            active_handles.insert(token, (handle, state));
        }

        // Nothing left to do — either all chunks processed, or time expired and in-flight done
        if active_handles.is_empty() && (chunk_queue.is_empty() || time_expired) {
            break;
        }

        // Poll for activity (100ms timeout)
        let mut waitfds: Vec<curl::multi::WaitFd> = Vec::new();
        multi
            .poll(&mut waitfds, Duration::from_millis(100))
            .map_err(|e| format!("multi poll: {e}"))?;

        // Drive transfers
        multi.perform().map_err(|e| format!("multi perform: {e}"))?;

        // Check for stalls (page-arrival-based no-progress timer)
        let now = Instant::now();
        let stall_tokens: Vec<Token> = active_handles
            .iter()
            .filter_map(|(token, (handle, state))| {
                let handler = handle.get_ref();
                let current_bytes = handler.bytes_received;

                if current_bytes > state.bytes_received {
                    // Progress was made — will update below
                    None
                } else if !state.stalled
                    && now.duration_since(state.last_progress_at) > config.stall_timeout
                {
                    Some(*token)
                } else {
                    None
                }
            })
            .collect();

        // Update progress timestamps
        for (_token, (handle, state)) in active_handles.iter_mut() {
            let handler = handle.get_ref();
            if handler.bytes_received > state.bytes_received {
                state.bytes_received = handler.bytes_received;
                state.last_progress_at = Instant::now();
            }
        }

        // Emit stall events
        for token in &stall_tokens {
            if let Some((_, state)) = active_handles.get_mut(token) {
                state.stalled = true;
                let _ = event_tx.send(TraceEvent::RequestStalled {
                    request_id: state.request_id,
                    stall_class: StallClass::MidstreamNoProgress,
                    no_progress_duration_ms: config.stall_timeout.as_millis() as u64,
                    timestamp_ns: run_start.elapsed().as_nanos() as u64,
                });
            }
        }

        // Process completed transfers
        let mut completed_tokens = Vec::new();
        multi.messages(|msg| {
            if let Some(token) = msg.token().ok() {
                let result_err = msg.result().map(|r| r.err()).unwrap_or(None);
                completed_tokens.push((token, result_err));
            }
        });

        for (token, curl_err) in completed_tokens {
            if let Some((handle, state)) = active_handles.remove(&token) {
                total_requests += 1;

                // Extract timing and connection info from the easy handle
                let easy = multi
                    .remove2(handle)
                    .map_err(|e| format!("multi remove: {e}"))?;

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

                let raw = easy.raw();
                let (conn_id, queue_us, remote_ep, local_ep) = unsafe {
                    (
                        curl_ffi::get_conn_id(raw),
                        curl_ffi::get_queue_time_us(raw),
                        format!(
                            "{}:{}",
                            curl_ffi::get_primary_ip(raw),
                            curl_ffi::get_primary_port(raw)
                        ),
                        format!(
                            "{}:{}",
                            curl_ffi::get_local_ip(raw),
                            curl_ffi::get_local_port(raw)
                        ),
                    )
                };
                let queue_ms = queue_us as f64 / 1000.0;
                let new_connection = connect_ms > 0.5;

                let handler = easy.get_ref();
                let bytes = handler.bytes_received;
                total_bytes += bytes;

                // Emit final partial page
                if handler.current_page_bytes > 0 {
                    let page_start = handler.current_page_index * handler.page_size;
                    let _ = event_tx.send(TraceEvent::PageCompleted {
                        request_id: state.request_id,
                        chunk_id: state.chunk_id,
                        page_index: handler.current_page_index,
                        page_start_byte: page_start,
                        page_end_byte: page_start + handler.current_page_bytes,
                        page_bytes: handler.current_page_bytes,
                        cumulative_bytes: handler.bytes_received,
                        timestamp_ns: handler.timestamp_ns(),
                    });
                }

                // Effective URL hash
                let eff_url = easy
                    .effective_url()
                    .ok()
                    .flatten()
                    .unwrap_or("")
                    .to_string();
                let mut hasher = Sha256::new();
                hasher.update(eff_url.as_bytes());
                let _url_hash = format!("{:x}", hasher.finalize());

                // Classify errors properly
                let error_class = curl_err.as_ref().map(|e| {
                    let desc = format!("{e}").to_lowercase();
                    if desc.contains("timeout") || desc.contains("timed out") {
                        ErrorClass::Timeout
                    } else if desc.contains("reset") || desc.contains("connection") && desc.contains("refused") {
                        ErrorClass::ConnectionReset
                    } else if desc.contains("resolve") || desc.contains("dns") {
                        ErrorClass::DnsFailure
                    } else if desc.contains("ssl") || desc.contains("tls") || desc.contains("certificate") {
                        ErrorClass::TlsFailure
                    } else if desc.contains("recv") || desc.contains("eof") || desc.contains("closed") {
                        ErrorClass::RecvError
                    } else if bytes == 0 && handler.response_code.map_or(false, |c| c >= 400) {
                        ErrorClass::HttpError
                    } else {
                        ErrorClass::Other
                    }
                });

                // Also classify as error when curl succeeded but we got 0 bytes or HTTP error
                let error_class = error_class.or_else(|| {
                    if let Some(code) = handler.response_code {
                        if code >= 400 {
                            return Some(ErrorClass::HttpError);
                        }
                    }
                    None
                });

                // Detect actual HTTP version from response status line
                let http_version = handler.response_code
                    .and_then(|_| {
                        // Look for HTTP/2 or HTTP/1.1 in captured status line headers
                        for (k, _) in &handler.headers {
                            if k.contains("http/2") || k == "http/2" {
                                return Some("h2".to_string());
                            }
                        }
                        None
                    })
                    .unwrap_or_else(|| {
                        // Use CURLINFO_HTTP_VERSION if available, fallback heuristic
                        let raw_handle = easy.raw();
                        let mut http_ver: libc_types::c_long = 0;
                        // CURLINFO_HTTP_VERSION = CURLINFO_LONG + 46 = 0x200000 + 46
                        let curlinfo_http_version = 0x200000 + 46;
                        let got_version = unsafe {
                            curl_sys::curl_easy_getinfo(
                                raw_handle,
                                curlinfo_http_version,
                                &mut http_ver as *mut libc_types::c_long,
                            ) == curl_sys::CURLE_OK
                        };
                        if got_version {
                            match http_ver {
                                2 => "h1.1".to_string(),
                                3 => "h2".to_string(),
                                4 => "h3".to_string(),
                                _ => format!("http/{http_ver}"),
                            }
                        } else {
                            "unknown".to_string()
                        }
                    });

                // Determine protocol string for connection registry
                let protocol_str = format!(
                    "{}{}",
                    if tls_ms > 0.0 { "https/" } else { "http/" },
                    http_version
                );

                // Emit request_finished
                let _ = event_tx.send(TraceEvent::RequestFinished {
                    request_id: state.request_id,
                    total_bytes: bytes,
                    dns_ms,
                    connect_ms,
                    tls_ms,
                    ttfb_ms,
                    queue_ms,
                    total_ms,
                    http_version: http_version.clone(),
                    connection_id: conn_id,
                    new_connection,
                    error_class,
                    timestamp_ns: run_start.elapsed().as_nanos() as u64,
                });

                // Emit request_prereq (connection facts)
                let _ = event_tx.send(TraceEvent::RequestPrereq {
                    request_id: state.request_id,
                    connection_id: conn_id,
                    new_connection,
                    local_endpoint: local_ep.clone(),
                    remote_endpoint: remote_ep.clone(),
                    protocol: protocol_str.clone(),
                    timestamp_ns: run_start.elapsed().as_nanos() as u64,
                });

                // Update connection registry
                let ts_ns = run_start.elapsed().as_nanos() as u64;
                let conn = connections.entry(conn_id).or_insert_with(|| ConnectionRecord {
                    connection_id: conn_id,
                    protocol: protocol_str,
                    local_endpoint: local_ep,
                    remote_endpoint: remote_ep,
                    first_use_ns: ts_ns,
                    last_use_ns: ts_ns,
                    requests_count: 0,
                    bytes_total: 0,
                });
                conn.last_use_ns = ts_ns;
                conn.requests_count += 1;
                conn.bytes_total += bytes;

                // If stalled and this was an urgent request, queue suffix resume
                if state.stalled && state.lane == Lane::Urgent && bytes < (state.range_end - state.range_start + 1) {
                    let suffix_start = state.range_start + bytes;
                    let _ = event_tx.send(TraceEvent::RequestResumed {
                        request_id: state.request_id,
                        suffix_range: format!("{}-{}", suffix_start, state.range_end),
                        attempt: state.attempt + 1,
                        timestamp_ns: run_start.elapsed().as_nanos() as u64,
                    });

                    // Re-queue the remaining range
                    chunk_queue.push(ChunkRequest {
                        chunk_id: state.chunk_id,
                        range_start: suffix_start,
                        range_end: state.range_end,
                        lane: state.lane,
                    });
                }
            }
        }
    }

    // Emit connection_lifecycle events
    for conn in connections.values() {
        let _ = event_tx.send(TraceEvent::ConnectionLifecycle {
            connection_id: conn.connection_id,
            protocol: conn.protocol.clone(),
            local_endpoint: conn.local_endpoint.clone(),
            remote_endpoint: conn.remote_endpoint.clone(),
            requests_count: conn.requests_count,
            bytes_total: conn.bytes_total,
            first_use_ns: conn.first_use_ns,
            last_use_ns: conn.last_use_ns,
        });
    }

    let duration_ms = run_start.elapsed().as_secs_f64() * 1000.0;

    Ok(ReactorResult {
        total_bytes,
        total_requests,
        total_connections: connections.len() as u32,
        duration_ms,
        connections: connections.into_values().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reactor_handler_page_aggregation() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let run_start = Instant::now();
        let mut handler = ReactorHandler::new(1, 0, 0, PAGE_SIZE, tx, run_start);

        // Write 2 full pages
        let data = vec![0u8; (PAGE_SIZE * 2) as usize];
        handler.write(&data).unwrap();

        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 2);
        assert_eq!(handler.bytes_received, PAGE_SIZE * 2);
    }

    #[test]
    fn chunk_request_queue_ordering() {
        let chunks = vec![
            ChunkRequest {
                chunk_id: 0,
                range_start: 0,
                range_end: 1024,
                lane: Lane::Urgent,
            },
            ChunkRequest {
                chunk_id: 1,
                range_start: 1025,
                range_end: 2048,
                lane: Lane::Prefetch,
            },
        ];
        let mut queue = chunks.clone();
        queue.reverse();
        // First pop should give chunk 0 (urgent)
        let first = queue.pop().unwrap();
        assert_eq!(first.chunk_id, 0);
        assert_eq!(first.lane, Lane::Urgent);
    }

    #[test]
    fn connection_record_defaults() {
        let record = ConnectionRecord {
            connection_id: 42,
            protocol: "https".to_string(),
            local_endpoint: "127.0.0.1:5000".to_string(),
            remote_endpoint: "93.184.216.34:443".to_string(),
            first_use_ns: 1000,
            last_use_ns: 5000,
            requests_count: 3,
            bytes_total: 1_000_000,
        };
        assert_eq!(record.requests_count, 3);
        assert_eq!(record.bytes_total, 1_000_000);
    }
}
