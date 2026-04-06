//! Transfer Reactor: fixed worker pool with persistent connections.
//!
//! Each worker is a dedicated thread with a long-lived Easy2 handle that
//! keeps its TCP+TLS connection alive across sequential range requests.
//! This matches the Kotlin DualLaneScheduler's closed-loop design.
//!
//! Workers pull chunks from a shared priority queue. The frontier tracker
//! feeds back the current frontier position, enabling frontier-aware
//! chunk promotion to the urgent lane.

use crate::curl_ffi;
use crate::download::PAGE_SIZE;
use crate::Sink;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use curl::easy::{Easy2, Handler, WriteError};
use frontier_telemetry::events::{ErrorClass, Lane, StallClass, TraceEvent};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::cmp::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[allow(non_camel_case_types)]
mod libc_types {
    pub type c_long = i64;
}

/// Stall threshold — matches Kotlin's STALL_THRESHOLD_MS = 2000.
const STALL_THRESHOLD_MS: u64 = 2_000;

/// Maximum retry attempts per chunk.
const MAX_ATTEMPTS: u32 = 10;

/// Base backoff delay for retries.
const RETRY_BACKOFF_BASE_MS: u64 = 2_000;

/// Configuration for a reactor run.
pub struct ReactorConfig {
    pub urgent_workers: u32,
    pub prefetch_workers: u32,
    pub page_size: u64,
    pub sink: Sink,
    pub stall_timeout: Duration,
    pub request_timeout: Duration,
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
            request_timeout: Duration::from_secs(60),
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

/// Priority queue entry.
#[derive(Debug, Clone)]
struct PrioritizedChunk {
    chunk_id: u64,
    range_start: u64,
    range_end: u64,
    lane: Lane,
    attempt: u32,
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
        // Lower range_start = higher priority (BinaryHeap is max-heap)
        other.range_start.cmp(&self.range_start)
    }
}

impl PartialOrd for PrioritizedChunk {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Connection facts.
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

/// Shared state between workers and the coordinator.
struct SharedState {
    chunk_queue: BinaryHeap<PrioritizedChunk>,
    completed_chunks: HashSet<u64>,
    frontier_byte: u64,
    total_bytes: u64,
    total_requests: u32,
    retried_chunks: u32,
    failed_chunks: u32,
    connections: HashMap<i64, ConnectionRecord>,
    time_expired: bool,
}

/// Handler for worker downloads.
struct WorkerHandler {
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

impl WorkerHandler {
    fn new(page_size: u64, event_tx: Sender<TraceEvent>, run_start: Instant) -> Self {
        Self {
            request_id: 0,
            chunk_id: 0,
            page_size,
            current_page_bytes: 0,
            current_page_index: 0,
            bytes_received: 0,
            event_tx,
            run_start,
            headers: Vec::new(),
            response_code: None,
        }
    }

    fn reinit(&mut self, request_id: u64, chunk_id: u64, range_start: u64) {
        self.request_id = request_id;
        self.chunk_id = chunk_id;
        self.current_page_bytes = 0;
        self.current_page_index = range_start / self.page_size;
        self.bytes_received = 0;
        self.headers.clear();
        self.response_code = None;
    }

    fn timestamp_ns(&self) -> u64 {
        self.run_start.elapsed().as_nanos() as u64
    }
}

impl Handler for WorkerHandler {
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
                self.headers.push((key.trim().to_lowercase(), value.trim().to_string()));
            }
        }
        true
    }
}

/// Result of a reactor run.
pub struct ReactorResult {
    pub total_bytes: u64,
    pub total_requests: u32,
    pub total_connections: u32,
    pub duration_ms: f64,
    pub connections: Vec<ConnectionRecord>,
    pub failed_chunks: u32,
    pub retried_chunks: u32,
}

/// Run parallel range requests using a fixed pool of persistent worker threads.
/// Each worker keeps its TCP+TLS connection alive across sequential requests.
pub fn run_parallel(
    url: &str,
    chunks: &[ChunkRequest],
    config: &ReactorConfig,
    event_tx: &Sender<TraceEvent>,
    frontier_rx: Option<Receiver<u64>>,
    run_start: Instant,
) -> Result<ReactorResult, String> {
    let max_concurrent = (config.urgent_workers + config.prefetch_workers) as usize;

    // Shared state protected by mutex
    let shared = Arc::new(Mutex::new(SharedState {
        chunk_queue: BinaryHeap::new(),
        completed_chunks: HashSet::new(),
        frontier_byte: 0,
        total_bytes: 0,
        total_requests: 0,
        retried_chunks: 0,
        failed_chunks: 0,
        connections: HashMap::new(),
        time_expired: false,
    }));

    // Populate queue
    {
        let mut state = shared.lock().unwrap();
        for chunk in chunks {
            state.chunk_queue.push(PrioritizedChunk {
                chunk_id: chunk.chunk_id,
                range_start: chunk.range_start,
                range_end: chunk.range_end,
                lane: chunk.lane,
                attempt: 0,
                retry_after: None,
            });
        }
    }

    let mut next_request_id = Arc::new(Mutex::new(1u64));

    // Spawn worker threads — each keeps a persistent Easy2 handle
    let mut worker_handles = Vec::new();
    for worker_id in 0..max_concurrent {
        let url = url.to_string();
        let shared = Arc::clone(&shared);
        let event_tx = event_tx.clone();
        let next_rid = Arc::clone(&next_request_id);
        let page_size = config.page_size;
        let request_timeout = config.request_timeout;
        let stall_timeout = config.stall_timeout;
        let max_duration = config.max_duration;

        let handle = std::thread::spawn(move || {
            // Create ONE Easy2 per worker — persists for the entire run
            let handler = WorkerHandler::new(page_size, event_tx.clone(), run_start);
            let mut easy = Easy2::new(handler);
            easy.url(&url).unwrap();
            easy.follow_location(true).unwrap();
            easy.timeout(request_timeout).unwrap();
            easy.tcp_keepalive(true).unwrap();

            loop {
                // Check time
                if max_duration > Duration::ZERO && run_start.elapsed() >= max_duration {
                    let mut state = shared.lock().unwrap();
                    state.time_expired = true;
                    break;
                }

                // Pop next chunk from shared queue
                let chunk = {
                    let mut state = shared.lock().unwrap();
                    if state.time_expired {
                        break;
                    }

                    // Try to find a ready chunk (respecting backoff)
                    let mut deferred = Vec::new();
                    let mut found = None;
                    while let Some(c) = state.chunk_queue.pop() {
                        if state.completed_chunks.contains(&c.chunk_id) {
                            continue;
                        }
                        if let Some(retry_at) = c.retry_after {
                            if Instant::now() < retry_at {
                                deferred.push(c);
                                continue;
                            }
                        }
                        found = Some(c);
                        break;
                    }
                    for d in deferred {
                        state.chunk_queue.push(d);
                    }
                    found
                };

                let chunk = match chunk {
                    Some(c) => c,
                    None => {
                        // No chunks ready — sleep briefly and check again
                        std::thread::sleep(Duration::from_millis(50));
                        // Check if we're truly done
                        let state = shared.lock().unwrap();
                        if state.chunk_queue.is_empty() && !state.time_expired {
                            break;
                        }
                        continue;
                    }
                };

                // Assign request ID
                let request_id = {
                    let mut rid = next_rid.lock().unwrap();
                    let id = *rid;
                    *rid += 1;
                    id
                };

                let attempt = chunk.attempt + 1;
                let range = format!("{}-{}", chunk.range_start, chunk.range_end);

                // Reinit handler for new request
                easy.get_mut().reinit(request_id, chunk.chunk_id, chunk.range_start);

                // Set new range (URL stays the same — connection reused!)
                easy.range(&range).unwrap();

                // Emit request_started
                let _ = event_tx.send(TraceEvent::RequestStarted {
                    request_id,
                    chunk_id: chunk.chunk_id,
                    lane: chunk.lane,
                    attempt,
                    requested_range: range.clone(),
                    timestamp_ns: run_start.elapsed().as_nanos() as u64,
                });

                // Perform the transfer — reuses existing TCP+TLS connection
                let curl_err = easy.perform().err();

                let handler = easy.get_ref();
                let bytes = handler.bytes_received;
                let expected = chunk.range_end - chunk.range_start + 1;
                let is_complete = bytes >= expected;

                // Emit final partial page
                if handler.current_page_bytes > 0 {
                    let page_start = handler.current_page_index * handler.page_size;
                    let _ = event_tx.send(TraceEvent::PageCompleted {
                        request_id,
                        chunk_id: chunk.chunk_id,
                        page_index: handler.current_page_index,
                        page_start_byte: page_start,
                        page_end_byte: page_start + handler.current_page_bytes,
                        page_bytes: handler.current_page_bytes,
                        cumulative_bytes: bytes,
                        timestamp_ns: handler.timestamp_ns(),
                    });
                }

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
                    } else {
                        ErrorClass::Other
                    }
                });
                let error_class = error_class.or_else(|| {
                    if let Some(code) = handler.response_code {
                        if code >= 400 { return Some(ErrorClass::HttpError); }
                    }
                    None
                });

                // HTTP version
                let http_version = {
                    let mut http_ver: libc_types::c_long = 0;
                    let curlinfo_http_version = 0x200000 + 46;
                    let got = unsafe {
                        curl_sys::curl_easy_getinfo(raw, curlinfo_http_version, &mut http_ver as *mut libc_types::c_long) == curl_sys::CURLE_OK
                    };
                    if got {
                        match http_ver { 2 => "h1.1", 3 => "h2", 4 => "h3", _ => "unknown" }.to_string()
                    } else {
                        "unknown".to_string()
                    }
                };

                let protocol_str = format!("{}{}", if tls_ms > 0.0 { "https/" } else { "http/" }, http_version);

                // Emit request_finished
                let _ = event_tx.send(TraceEvent::RequestFinished {
                    request_id,
                    total_bytes: bytes,
                    dns_ms, connect_ms, tls_ms, ttfb_ms, queue_ms, total_ms,
                    http_version: http_version.clone(),
                    connection_id: conn_id,
                    new_connection,
                    error_class,
                    timestamp_ns: run_start.elapsed().as_nanos() as u64,
                });

                // Emit request_prereq
                let _ = event_tx.send(TraceEvent::RequestPrereq {
                    request_id,
                    connection_id: conn_id,
                    new_connection,
                    local_endpoint: local_ep.clone(),
                    remote_endpoint: remote_ep.clone(),
                    protocol: protocol_str.clone(),
                    timestamp_ns: run_start.elapsed().as_nanos() as u64,
                });

                // Update shared state
                {
                    let mut state = shared.lock().unwrap();
                    state.total_bytes += bytes;
                    state.total_requests += 1;

                    // Connection registry
                    let ts_ns = run_start.elapsed().as_nanos() as u64;
                    let conn = state.connections.entry(conn_id).or_insert_with(|| ConnectionRecord {
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

                    if is_complete {
                        state.completed_chunks.insert(chunk.chunk_id);
                    } else if !state.time_expired && attempt < MAX_ATTEMPTS {
                        let suffix_start = chunk.range_start + bytes;
                        state.retried_chunks += 1;

                        let chunk_start = chunk.chunk_id * (chunk.range_end - chunk.range_start + 1);
                        let is_frontier_blocker = chunk_start <= state.frontier_byte + page_size;
                        let retry_lane = if is_frontier_blocker { Lane::Urgent } else { chunk.lane };

                        let backoff_ms = if is_frontier_blocker {
                            500 * (1u64 << (attempt - 1).min(4))
                        } else {
                            RETRY_BACKOFF_BASE_MS * (1u64 << (attempt - 1).min(4))
                        };

                        let _ = event_tx.send(TraceEvent::RequestResumed {
                            request_id,
                            suffix_range: format!("{}-{}", suffix_start, chunk.range_end),
                            attempt: attempt + 1,
                            timestamp_ns: run_start.elapsed().as_nanos() as u64,
                        });

                        state.chunk_queue.push(PrioritizedChunk {
                            chunk_id: chunk.chunk_id,
                            range_start: suffix_start,
                            range_end: chunk.range_end,
                            lane: retry_lane,
                            attempt,
                            retry_after: Some(Instant::now() + Duration::from_millis(backoff_ms)),
                        });
                    } else if !is_complete {
                        state.failed_chunks += 1;
                    }
                }
            }
        });

        worker_handles.push(handle);
    }

    // Coordinator: read frontier feedback and update shared state
    if let Some(frontier_rx) = frontier_rx {
        let shared_coord = Arc::clone(&shared);
        std::thread::spawn(move || {
            loop {
                match frontier_rx.try_recv() {
                    Ok(frontier_byte) => {
                        if let Ok(mut state) = shared_coord.lock() {
                            state.frontier_byte = frontier_byte;
                        }
                    }
                    Err(TryRecvError::Empty) => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(TryRecvError::Disconnected) => break,
                }
            }
        });
    }

    // Wait for all workers
    for handle in worker_handles {
        let _ = handle.join();
    }

    let state = shared.lock().unwrap();

    // Emit connection_lifecycle events
    for conn in state.connections.values() {
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
        total_bytes: state.total_bytes,
        total_requests: state.total_requests,
        total_connections: state.connections.len() as u32,
        duration_ms,
        connections: state.connections.values().cloned().collect(),
        failed_chunks: state.failed_chunks,
        retried_chunks: state.retried_chunks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_queue_urgent_first() {
        let mut queue = BinaryHeap::new();
        queue.push(PrioritizedChunk {
            chunk_id: 1, range_start: 4*1024*1024, range_end: 8*1024*1024-1,
            lane: Lane::Prefetch, attempt: 0, retry_after: None,
        });
        queue.push(PrioritizedChunk {
            chunk_id: 0, range_start: 0, range_end: 4*1024*1024-1,
            lane: Lane::Urgent, attempt: 0, retry_after: None,
        });
        assert_eq!(queue.pop().unwrap().lane, Lane::Urgent);
    }

    #[test]
    fn priority_queue_lower_offset_first() {
        let mut queue = BinaryHeap::new();
        queue.push(PrioritizedChunk {
            chunk_id: 2, range_start: 8*1024*1024, range_end: 12*1024*1024-1,
            lane: Lane::Prefetch, attempt: 0, retry_after: None,
        });
        queue.push(PrioritizedChunk {
            chunk_id: 1, range_start: 4*1024*1024, range_end: 8*1024*1024-1,
            lane: Lane::Prefetch, attempt: 0, retry_after: None,
        });
        assert_eq!(queue.pop().unwrap().chunk_id, 1);
    }

    #[test]
    fn connection_record_defaults() {
        let record = ConnectionRecord {
            connection_id: 42, protocol: "https/h2".to_string(),
            local_endpoint: "127.0.0.1:5000".to_string(),
            remote_endpoint: "93.184.216.34:443".to_string(),
            first_use_ns: 1000, last_use_ns: 5000,
            requests_count: 3, bytes_total: 1_000_000,
        };
        assert_eq!(record.requests_count, 3);
    }

    #[test]
    fn worker_handler_page_aggregation() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let run_start = Instant::now();
        let mut handler = WorkerHandler::new(PAGE_SIZE, tx, run_start);
        handler.reinit(1, 0, 0);
        let data = vec![0u8; (PAGE_SIZE * 2) as usize];
        handler.write(&data).unwrap();
        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 2);
        assert_eq!(handler.bytes_received, PAGE_SIZE * 2);
    }
}
