use serde::{Deserialize, Serialize};

/// Device/network capability snapshot.
/// JSON field names match Kotlin's CapabilityEnvelope.toJson() exactly.
///
/// Nullable fields use `skip_serializing_if` to match Kotlin's behavior of
/// omitting null fields (not serializing them as JSON null).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityEnvelope {
    pub max_safe_urgent_workers: u32,
    pub max_safe_prefetch_workers: u32,
    pub max_safe_urgent_chunk_bytes: u64,
    pub max_safe_prefetch_chunk_bytes: u64,
    pub sustained_throughput_mbps: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p10_throughput_mbps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seek_ttfb_p50_ms: Option<u64>,
    #[serde(default)]
    pub stability_penalty: f64,
    #[serde(default = "default_true")]
    pub supports_range_requests: bool,
    pub measured_at_ms: u64,
}

fn default_true() -> bool {
    true
}

impl CapabilityEnvelope {
    /// Default envelope matching Kotlin's CapabilityEnvelope.DEFAULT.
    pub fn default_envelope() -> Self {
        Self {
            max_safe_urgent_workers: 2,
            max_safe_prefetch_workers: 1,
            max_safe_urgent_chunk_bytes: 8 * 1024 * 1024,
            max_safe_prefetch_chunk_bytes: 16 * 1024 * 1024,
            sustained_throughput_mbps: 50.0,
            p10_throughput_mbps: None,
            seek_ttfb_p50_ms: None,
            stability_penalty: 0.5,
            supports_range_requests: true,
            measured_at_ms: 0,
        }
    }

    pub fn is_measured(&self) -> bool {
        self.measured_at_ms > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_omits_none_fields() {
        let envelope = CapabilityEnvelope::default_envelope();
        let json = serde_json::to_string(&envelope).unwrap();

        // None fields should be absent, not "null"
        assert!(!json.contains("p10ThroughputMbps"));
        assert!(!json.contains("seekTtfbP50Ms"));

        // Required fields should be present
        assert!(json.contains("maxSafeUrgentWorkers"));
        assert!(json.contains("sustainedThroughputMbps"));
        assert!(json.contains("measuredAtMs"));
    }

    #[test]
    fn json_includes_some_fields() {
        let mut envelope = CapabilityEnvelope::default_envelope();
        envelope.p10_throughput_mbps = Some(30.0);
        envelope.seek_ttfb_p50_ms = Some(150);

        let json = serde_json::to_string(&envelope).unwrap();
        assert!(json.contains("p10ThroughputMbps"));
        assert!(json.contains("seekTtfbP50Ms"));
    }

    #[test]
    fn json_roundtrip_matches_kotlin() {
        let envelope = CapabilityEnvelope {
            max_safe_urgent_workers: 3,
            max_safe_prefetch_workers: 1,
            max_safe_urgent_chunk_bytes: 4 * 1024 * 1024,
            max_safe_prefetch_chunk_bytes: 8 * 1024 * 1024,
            sustained_throughput_mbps: 75.5,
            p10_throughput_mbps: Some(45.0),
            seek_ttfb_p50_ms: None,
            stability_penalty: 0.1,
            supports_range_requests: true,
            measured_at_ms: 1712345678000,
        };

        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: CapabilityEnvelope = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.max_safe_urgent_workers, 3);
        assert_eq!(parsed.sustained_throughput_mbps, 75.5);
        assert_eq!(parsed.p10_throughput_mbps, Some(45.0));
        assert_eq!(parsed.seek_ttfb_p50_ms, None);
        assert_eq!(parsed.measured_at_ms, 1712345678000);
    }

    #[test]
    fn parses_kotlin_json_with_missing_optional_fields() {
        // Simulates what Kotlin's toJson() produces when optionals are null
        let kotlin_json = r#"{
            "maxSafeUrgentWorkers": 2,
            "maxSafePrefetchWorkers": 1,
            "maxSafeUrgentChunkBytes": 8388608,
            "maxSafePrefetchChunkBytes": 16777216,
            "sustainedThroughputMbps": 50.0,
            "stabilityPenalty": 0.5,
            "supportsRangeRequests": true,
            "measuredAtMs": 0
        }"#;

        let parsed: CapabilityEnvelope = serde_json::from_str(kotlin_json).unwrap();
        assert_eq!(parsed.max_safe_urgent_workers, 2);
        assert_eq!(parsed.p10_throughput_mbps, None);
        assert_eq!(parsed.seek_ttfb_p50_ms, None);
        assert_eq!(parsed.stability_penalty, 0.5);
    }
}
