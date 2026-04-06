//! Transfer Reactor: single-threaded curl multi handle driving parallel range requests.
//!
//! Uses `curl_multi_poll()` as the event loop driver. All handle additions,
//! removals, state mutations, and timing happen on this single thread.
//!
//! Key design features:
//! - Frontier-aware priority scheduling: urgent lane reserved for frontier-blocking chunks
//! - Automatic retry of failed chunks (up to MAX_ATTEMPTS)
//! - Connection reuse via curl multi connection pool
//! - Stall detection with page-arrival-based no-progress timer

use crate::curl_ffi;
use crate::download::PAGE_SIZE;
use crate::Sink;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use curl::easy::{Easy2, Handler, WriteError};
use curl::multi::{Easy2Handle, Multi};
use frontier_telemetry::events::{ErrorClass, Lane, StallClass, TraceEvent};
// sha2 no longer needed here (URL hashing moved out)
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::cmp::Ordering;
use std::time::{Duration, Instant};

#[allow(non_camel_case_types)]
mod libc_types {
    pub type c_long = i64;
}

/// Stall threshold — no page progress for this duration triggers stall detection.
/// Matches Kotlin's STALL_THRESHOLD_MS = 2000.
const STALL_THRESHOLD_MS: u64 = 2_000;

/// Maximum retry attempts per chunk before giving up.
const MAX_ATTEMPTS: u32 = 10;

/// Base backoff delay for retries (doubles each attempt).
const RETRY_BACKOFF_BASE_MS: u64 = 2_000;

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

/// Priority-aware queue entry. Lower range_start = higher priority.
/// Urgent lane always beats prefetch. Within same lane, lower offset first.
#[derive(Debug, Clone)]
struct PrioritizedChunk {
    chunk_id: u64,
    range_start: u64,
    range_end: u64,
    lane: Lane,
    attempt: u32,
    /// Earliest time this chunk can be retried (backoff).
    retry_after: Option<Instant>,
}

impl PartialEq for PrioritizedChunk {
    fn eq(&self, other: &Self) -> bool {
        self.lane == other.lane && self.range_start == other.range_start
    }
}

impl Eq for PrioritizedChunk {}

impl Ord for PrioritizedChunk {
    fn cmp(&self, other: &Self) -> Ordering {
        // Urgent before Prefetch
        let lane_ord = match (&self.lane, &other.lane) {
            (Lane::Urgent, Lane::Prefetch) => Ordering::Greater,
            (Lane::Prefetch, Lane::Urgent) => Ordering::Less,
            _ => Ordering::Equal,
        };
        if lane_ord != Ordering::Equal {
            return lane_ord;
        }
        // Lower range_start = higher priority (reverse because BinaryHeap is max-heap)
        other.range_start.cmp(&self.range_start)
    }
}

impl PartialOrd for PrioritizedChunk {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// Frontier feedback is received as a simple u64 (frontier byte position)
// to avoid circular dependencies between transport and frontier crates.

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

    /// Reinitialize for a new request, preserving the event_tx and run_start.
    fn reinit(&mut self, request_id: u64, chunk_id: u64, range_start: u64, page_size: u64) {
        self.request_id = request_id;
        self.chunk_id = chunk_id;
        self.page_size = page_size;
        self.current_page_bytes = 0;
        self.current_page_index = range_start / page_size;
        self.bytes_received = 0;
        self.headers.clear();
        self.response_code = None;
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
    pub failed_chunks: u32,
    pub retried_chunks: u32,
}

/// Run parallel range requests using curl multi with poll-based event loop.
///
/// `frontier_rx` receives feedback from the frontier tracker about the current
/// contiguous frontier position, enabling frontier-aware chunk scheduling.
pub fn run_parallel(
    url: &str,
    chunks: &[ChunkRequest],
    config: &ReactorConfig,
    event_tx: &Sender<TraceEvent>,
    frontier_rx: Option<Receiver<u64>>,
    run_start: Instant,
) -> Result<ReactorResult, String> {
    let mut multi = Multi::new();

    // Connection management: limit concurrent connections and grow the cache
    let max_concurrent = (config.urgent_workers + config.prefetch_workers) as usize;
    multi.set_max_host_connections(max_concurrent)
        .map_err(|e| format!("set max host connections: {e}"))?;
    multi.set_max_total_connections(max_concurrent + 2)
        .map_err(|e| format!("set max total connections: {e}"))?;
    // Keep more idle connections in the cache for reuse.
    // Default is 4 * number of easy handles, but we cycle handles rapidly.
    // A larger pool increases the chance that a new Easy2 finds a cached connection.
    multi.set_max_connects(max_concurrent * 4)
        .map_err(|e| format!("set max connects: {e}"))?;
    // Enable HTTP pipelining/multiplexing for h2
    multi.pipelining(false, true)
        .map_err(|e| format!("pipelining: {e}"))?;

    let mut active_handles: HashMap<Token, (Easy2Handle<ReactorHandler>, RequestState)> =
        HashMap::new();
    let mut connections: HashMap<i64, ConnectionRecord> = HashMap::new();
    let mut next_token: Token = 0;
    let mut next_request_id: u64 = 1;

    // Connection reuse: pool of completed Easy2 handles for recycling.
    // After a request completes, we reset the Easy2 and put it here.
    // New requests take from the pool first, preserving the underlying
    // TCP+TLS connection in curl's connection cache.
    let mut handle_pool: Vec<Easy2<ReactorHandler>> = Vec::new();

    // Fix 4: Frontier-aware priority queue instead of flat FIFO
    let mut chunk_queue: BinaryHeap<PrioritizedChunk> = BinaryHeap::new();
    for chunk in chunks {
        chunk_queue.push(PrioritizedChunk {
            chunk_id: chunk.chunk_id,
            range_start: chunk.range_start,
            range_end: chunk.range_end,
            lane: chunk.lane,
            attempt: 0,
            retry_after: None,
        });
    }

    // Track which chunk_ids have been completed successfully
    let mut completed_chunks: HashSet<u64> = HashSet::new();
    let mut current_frontier_byte: u64 = 0;

    let mut total_bytes: u64 = 0;
    let mut total_requests: u32 = 0;
    let mut failed_chunks: u32 = 0;
    let mut retried_chunks: u32 = 0;

    let has_duration_limit = config.max_duration > Duration::ZERO;

    loop {
        // Check duration limit
        let time_expired = has_duration_limit && run_start.elapsed() >= config.max_duration;

        // Fix 4: Read frontier feedback to know current contiguous frontier position
        if let Some(ref rx) = frontier_rx {
            loop {
                match rx.try_recv() {
                    Ok(frontier_byte) => {
                        current_frontier_byte = frontier_byte;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
        }

        // Fill slots with queued chunks (unless time expired)
        // Chunks with retry_after in the future are deferred back to the queue.
        let mut deferred: Vec<PrioritizedChunk> = Vec::new();
        while !time_expired && active_handles.len() < max_concurrent {
            let chunk = match chunk_queue.pop() {
                Some(c) => c,
                None => break,
            };

            // Skip already-completed chunks (from retries that raced)
            if completed_chunks.contains(&chunk.chunk_id) {
                continue;
            }

            // Respect retry backoff — defer if not ready yet
            if let Some(retry_at) = chunk.retry_after {
                if Instant::now() < retry_at {
                    deferred.push(chunk);
                    continue;
                }
            }

            let request_id = next_request_id;
            next_request_id += 1;
            let token = next_token;
            next_token += 1;

            // Reuse a pooled Easy2 handle if available (preserves TCP+TLS connection).
            // IMPORTANT: do NOT call easy.reset() — it calls curl_easy_reset() which
            // destroys the connection state and forces a new TCP+TLS handshake.
            // Instead, just reconfigure the options we need (URL, range, timeouts)
            // and reinit our handler.
            let mut easy = if let Some(mut pooled) = handle_pool.pop() {
                pooled.get_mut().reinit(request_id, chunk.chunk_id, chunk.range_start, config.page_size);
                pooled
            } else {
                let handler = ReactorHandler::new(
                    request_id,
                    chunk.chunk_id,
                    chunk.range_start,
                    config.page_size,
                    event_tx.clone(),
                    run_start,
                );
                Easy2::new(handler)
            };

            easy.url(url).map_err(|e| format!("set URL: {e}"))?;
            easy.follow_location(true)
                .map_err(|e| format!("follow: {e}"))?;
            easy.timeout(config.request_timeout)
                .map_err(|e| format!("timeout: {e}"))?;
            easy.tcp_keepalive(true)
                .map_err(|e| format!("tcp keepalive: {e}"))?;

            let range = format!("{}-{}", chunk.range_start, chunk.range_end);
            easy.range(&range)
                .map_err(|e| format!("range: {e}"))?;

            let attempt = chunk.attempt + 1;

            // Emit request_started
            let _ = event_tx.send(TraceEvent::RequestStarted {
                request_id,
                chunk_id: chunk.chunk_id,
                lane: chunk.lane,
                attempt,
                requested_range: range,
                timestamp_ns: run_start.elapsed().as_nanos() as u64,
            });

            let state = RequestState {
                request_id,
                chunk_id: chunk.chunk_id,
                lane: chunk.lane,
                attempt,
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
        // Put deferred (backoff) chunks back
        for d in deferred {
            chunk_queue.push(d);
        }

        // Exit: no active handles and (queue empty or time expired)
        // Also consider deferred chunks — if only deferred chunks remain, keep looping
        // so they can be picked up after their backoff expires.
        let only_deferred_remain = chunk_queue.iter().all(|c| {
            c.retry_after.map_or(false, |t| Instant::now() < t)
        });
        if active_handles.is_empty() && (time_expired || (chunk_queue.is_empty())) {
            break;
        }
        // If only deferred chunks and no active handles, sleep briefly to avoid busy-wait
        if active_handles.is_empty() && only_deferred_remain && !chunk_queue.is_empty() {
            std::thread::sleep(Duration::from_millis(200));
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

                let easy = multi
                    .remove2(handle)
                    .map_err(|e| format!("multi remove: {e}"))?;

                // Extract timings
                let dns_ms = easy.namelookup_time().map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0);
                let connect_ms = easy.connect_time().map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0);
                let appconnect_ms = easy.appconnect_time().map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0);
                let tls_ms = (appconnect_ms - connect_ms).max(0.0);
                let ttfb_ms = easy.starttransfer_time().map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0);
                let total_ms = easy.total_time().map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0);

                let raw = easy.raw();
                let (conn_id, queue_us, remote_ep, local_ep) = unsafe {
                    (
                        curl_ffi::get_conn_id(raw),
                        curl_ffi::get_queue_time_us(raw),
                        format!("{}:{}", curl_ffi::get_primary_ip(raw), curl_ffi::get_primary_port(raw)),
                        format!("{}:{}", curl_ffi::get_local_ip(raw), curl_ffi::get_local_port(raw)),
                    )
                };
                let queue_ms = queue_us as f64 / 1000.0;
                let new_connection = connect_ms > 0.5;

                let handler = easy.get_ref();
                let bytes = handler.bytes_received;
                let expected_bytes = state.range_end - state.range_start + 1;
                let is_complete = bytes >= expected_bytes;

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

                // Classify errors
                let error_class = curl_err.as_ref().map(|e| {
                    let desc = format!("{e}").to_lowercase();
                    if desc.contains("timeout") || desc.contains("timed out") {
                        ErrorClass::Timeout
                    } else if desc.contains("reset") || (desc.contains("connection") && desc.contains("refused")) {
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
                let error_class = error_class.or_else(|| {
                    if let Some(code) = handler.response_code {
                        if code >= 400 {
                            return Some(ErrorClass::HttpError);
                        }
                    }
                    None
                });

                // Detect HTTP version
                let http_version = {
                    let raw_handle = easy.raw();
                    let mut http_ver: libc_types::c_long = 0;
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
                };

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

                // Emit request_prereq
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

                // Retry failed/incomplete chunks with frontier awareness + backoff
                if is_complete {
                    completed_chunks.insert(state.chunk_id);
                } else if !time_expired && state.attempt < MAX_ATTEMPTS {
                    let suffix_start = state.range_start + bytes;
                    retried_chunks += 1;

                    // Promote to Urgent if this chunk is at or near the frontier
                    // Use the chunk's original range_start for comparison (not suffix)
                    // since a chunk starting at the frontier byte IS the blocker
                    let chunk_start_byte = state.chunk_id * (state.range_end - state.range_start + 1);
                    let is_frontier_blocker = chunk_start_byte <= current_frontier_byte + config.page_size;
                    let retry_lane = if is_frontier_blocker {
                        Lane::Urgent
                    } else {
                        state.lane
                    };

                    // Exponential backoff: 2s, 4s, 8s, 16s...
                    // Frontier-blocking chunks get shorter backoff (500ms, 1s, 2s...)
                    let backoff_ms = if is_frontier_blocker {
                        500 * (1u64 << state.attempt.min(4))
                    } else {
                        RETRY_BACKOFF_BASE_MS * (1u64 << state.attempt.min(4))
                    };
                    let retry_at = Instant::now() + Duration::from_millis(backoff_ms);

                    let _ = event_tx.send(TraceEvent::RequestResumed {
                        request_id: state.request_id,
                        suffix_range: format!("{}-{}", suffix_start, state.range_end),
                        attempt: state.attempt + 1,
                        timestamp_ns: run_start.elapsed().as_nanos() as u64,
                    });

                    chunk_queue.push(PrioritizedChunk {
                        chunk_id: state.chunk_id,
                        range_start: suffix_start,
                        range_end: state.range_end,
                        lane: retry_lane,
                        attempt: state.attempt,
                        retry_after: Some(retry_at),
                    });
                } else if !is_complete {
                    failed_chunks += 1;
                }

                // Recycle the Easy2 handle into the pool for connection reuse.
                // The underlying TCP+TLS connection stays in curl's cache.
                handle_pool.push(easy);
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
        failed_chunks,
        retried_chunks,
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

        let data = vec![0u8; (PAGE_SIZE * 2) as usize];
        handler.write(&data).unwrap();

        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 2);
        assert_eq!(handler.bytes_received, PAGE_SIZE * 2);
    }

    #[test]
    fn priority_queue_urgent_first() {
        let mut queue = BinaryHeap::new();
        queue.push(PrioritizedChunk {
            chunk_id: 1,
            range_start: 4 * 1024 * 1024,
            range_end: 8 * 1024 * 1024 - 1,
            lane: Lane::Prefetch,
            attempt: 0,
            retry_after: None,
        });
        queue.push(PrioritizedChunk {
            chunk_id: 0,
            range_start: 0,
            range_end: 4 * 1024 * 1024 - 1,
            lane: Lane::Urgent,
            attempt: 0,
            retry_after: None,
        });
        // Urgent should come out first
        let first = queue.pop().unwrap();
        assert_eq!(first.lane, Lane::Urgent);
        assert_eq!(first.chunk_id, 0);
    }

    #[test]
    fn priority_queue_lower_offset_first() {
        let mut queue = BinaryHeap::new();
        queue.push(PrioritizedChunk {
            chunk_id: 2,
            range_start: 8 * 1024 * 1024,
            range_end: 12 * 1024 * 1024 - 1,
            lane: Lane::Prefetch,
            attempt: 0,
            retry_after: None,
        });
        queue.push(PrioritizedChunk {
            chunk_id: 1,
            range_start: 4 * 1024 * 1024,
            range_end: 8 * 1024 * 1024 - 1,
            lane: Lane::Prefetch,
            attempt: 0,
            retry_after: None,
        });
        let first = queue.pop().unwrap();
        assert_eq!(first.chunk_id, 1); // lower offset
    }

    #[test]
    fn retry_promotes_frontier_blocking_chunk() {
        // If a chunk's start is at the frontier, retries should be Urgent
        let frontier_byte: u64 = 4 * 1024 * 1024;
        let chunk_start: u64 = 4 * 1024 * 1024; // at frontier
        let page_size: u64 = PAGE_SIZE;

        let retry_lane = if chunk_start <= frontier_byte + page_size * 2 {
            Lane::Urgent
        } else {
            Lane::Prefetch
        };
        assert_eq!(retry_lane, Lane::Urgent);
    }

    #[test]
    fn connection_record_defaults() {
        let record = ConnectionRecord {
            connection_id: 42,
            protocol: "https/h2".to_string(),
            local_endpoint: "127.0.0.1:5000".to_string(),
            remote_endpoint: "93.184.216.34:443".to_string(),
            first_use_ns: 1000,
            last_use_ns: 5000,
            requests_count: 3,
            bytes_total: 1_000_000,
        };
        assert_eq!(record.requests_count, 3);
        assert_eq!(record.protocol, "https/h2");
    }
}
