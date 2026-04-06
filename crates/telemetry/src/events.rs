use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// All JSONL trace event types emitted by the frontier simulator.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event_type")]
pub enum TraceEvent {
    #[serde(rename = "run_started")]
    RunStarted {
        run_id: String,
        asset_key: String,
        config: serde_json::Value,
        timestamp_ns: u64,
    },

    #[serde(rename = "probe_completed")]
    ProbeCompleted {
        effective_url_hash: String,
        range_support: bool,
        content_length: Option<u64>,
        protocol: String,
        dns_ms: f64,
        connect_ms: f64,
        tls_ms: f64,
        ttfb_ms: f64,
        remote_endpoint: String,
        local_endpoint: String,
        timestamp_ns: u64,
    },

    #[serde(rename = "request_started")]
    RequestStarted {
        request_id: u64,
        chunk_id: u64,
        lane: Lane,
        attempt: u32,
        requested_range: String,
        timestamp_ns: u64,
    },

    #[serde(rename = "request_prereq")]
    RequestPrereq {
        request_id: u64,
        connection_id: i64,
        new_connection: bool,
        local_endpoint: String,
        remote_endpoint: String,
        protocol: String,
        timestamp_ns: u64,
    },

    #[serde(rename = "headers_received")]
    HeadersReceived {
        request_id: u64,
        response_code: u32,
        served_range: Option<String>,
        effective_url_hash: String,
        timestamp_ns: u64,
    },

    #[serde(rename = "page_completed")]
    PageCompleted {
        request_id: u64,
        chunk_id: u64,
        page_index: u64,
        page_start_byte: u64,
        page_end_byte: u64,
        page_bytes: u64,
        cumulative_bytes: u64,
        timestamp_ns: u64,
    },

    #[serde(rename = "request_stalled")]
    RequestStalled {
        request_id: u64,
        stall_class: StallClass,
        no_progress_duration_ms: u64,
        timestamp_ns: u64,
    },

    #[serde(rename = "request_resumed")]
    RequestResumed {
        request_id: u64,
        suffix_range: String,
        attempt: u32,
        timestamp_ns: u64,
    },

    #[serde(rename = "request_finished")]
    RequestFinished {
        request_id: u64,
        total_bytes: u64,
        dns_ms: f64,
        connect_ms: f64,
        tls_ms: f64,
        ttfb_ms: f64,
        queue_ms: f64,
        total_ms: f64,
        http_version: String,
        connection_id: i64,
        new_connection: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_class: Option<ErrorClass>,
        timestamp_ns: u64,
    },

    #[serde(rename = "frontier_advanced")]
    FrontierAdvanced {
        frontier_byte: u64,
        frontier_page: u64,
        contiguous_bytes: u64,
        timestamp_ns: u64,
    },

    #[serde(rename = "connection_lifecycle")]
    ConnectionLifecycle {
        connection_id: i64,
        protocol: String,
        local_endpoint: String,
        remote_endpoint: String,
        requests_count: u32,
        bytes_total: u64,
        first_use_ns: u64,
        last_use_ns: u64,
    },

    #[serde(rename = "run_completed")]
    RunCompleted {
        run_id: String,
        total_bytes: u64,
        total_requests: u32,
        total_connections: u32,
        duration_ms: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        capability_envelope: Option<serde_json::Value>,
        summary: HashMap<String, serde_json::Value>,
        timestamp_ns: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    Urgent,
    Prefetch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StallClass {
    QueuedByClient,
    DnsConnectTlsSlow,
    HighTtfb,
    MidstreamNoProgress,
    ResetEofRecv,
    LocalCancellation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Timeout,
    ConnectionReset,
    DnsFailure,
    TlsFailure,
    HttpError,
    RecvError,
    Cancelled,
    Other,
}
