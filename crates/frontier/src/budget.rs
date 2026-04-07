use crate::tracker::FrontierEvent;
use serde::{Deserialize, Serialize};

/// Default constants matching Kotlin's ShadowPlayerSimulation.
const DEFAULT_STARTUP_BUFFER_MS: u64 = 3_000;
const DEFAULT_MAX_BUFFER_MS: u64 = 50_000;
const DEFAULT_SAFETY_FACTOR: f64 = 0.85;
const DEFAULT_SEARCH_TOLERANCE_MBPS: f64 = 0.1;
const DEFAULT_MIN_BITRATE_MBPS: f64 = 1.0;
const DEFAULT_MAX_BITRATE_MBPS: f64 = 2000.0;

/// Result of a single simulation run at a candidate bitrate.
#[derive(Debug, Clone)]
pub struct SimulationRun {
    pub survived: bool,
    pub rebuffer_count: u32,
    pub min_buffer_ms: u64,
}

/// Result of the binary-search shadow player simulation.
/// Matches Kotlin's ShadowPlayerSimulationResult.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationResult {
    #[serde(rename = "maxSustainableBitrateMbps")]
    pub max_sustainable_bitrate_mbps: f64,
    #[serde(rename = "safeBudgetMbps")]
    pub safe_budget_mbps: f64,
    #[serde(rename = "searchIterations")]
    pub search_iterations: u32,
    #[serde(rename = "startupBufferMs")]
    pub startup_buffer_ms: u64,
    #[serde(rename = "maxBufferMs")]
    pub max_buffer_ms: u64,
    #[serde(rename = "simulatedRebufferCount")]
    pub simulated_rebuffer_count: u32,
}

/// Shadow player budget estimator.
/// Port of Kotlin's ShadowPlayerSimulation with identical constants and algorithm.
pub struct SafeBudgetEstimator {
    startup_buffer_ms: u64,
    max_buffer_ms: u64,
    safety_factor: f64,
    search_tolerance_mbps: f64,
    min_bitrate_mbps: f64,
    max_bitrate_mbps: f64,
}

impl SafeBudgetEstimator {
    pub fn new() -> Self {
        Self {
            startup_buffer_ms: DEFAULT_STARTUP_BUFFER_MS,
            max_buffer_ms: DEFAULT_MAX_BUFFER_MS,
            safety_factor: DEFAULT_SAFETY_FACTOR,
            search_tolerance_mbps: DEFAULT_SEARCH_TOLERANCE_MBPS,
            min_bitrate_mbps: DEFAULT_MIN_BITRATE_MBPS,
            max_bitrate_mbps: DEFAULT_MAX_BITRATE_MBPS,
        }
    }

    /// Simulate playback at the given bitrate against a frontier event timeline.
    /// Mirrors Kotlin's ShadowPlayerSimulation.simulate() exactly.
    pub fn simulate(&self, events: &[FrontierEvent], candidate_bitrate_mbps: f64) -> SimulationRun {
        if events.is_empty() {
            return SimulationRun {
                survived: false,
                rebuffer_count: 0,
                min_buffer_ms: 0,
            };
        }
        if candidate_bitrate_mbps <= 0.0 {
            return SimulationRun {
                survived: true,
                rebuffer_count: 0,
                min_buffer_ms: u64::MAX,
            };
        }

        let bytes_per_ms = candidate_bitrate_mbps * 1_000_000.0 / 8.0 / 1_000.0;
        let max_buffer_bytes = self.max_buffer_ms as f64 * bytes_per_ms;
        let startup_buffer_bytes = self.startup_buffer_ms as f64 * bytes_per_ms;

        let mut buffer_bytes: f64 = 0.0;
        let mut started = false;
        let mut last_t_ms = events[0].t_ms;
        let mut rebuffer_count: u32 = 0;
        let mut min_buffer_ms: u64 = u64::MAX;

        for event in events {
            let delta_t_ms = event.t_ms.saturating_sub(last_t_ms);

            if started && delta_t_ms > 0 {
                let consumed = bytes_per_ms * delta_t_ms as f64;
                buffer_bytes -= consumed;
                if buffer_bytes < 0.0 {
                    rebuffer_count += 1;
                    buffer_bytes = 0.0;
                    started = false;
                }
            }

            buffer_bytes =
                (buffer_bytes + event.delta_contiguous_bytes as f64).min(max_buffer_bytes);

            if !started && buffer_bytes >= startup_buffer_bytes {
                started = true;
            }

            if started {
                let buffer_ms = (buffer_bytes / bytes_per_ms) as u64;
                min_buffer_ms = min_buffer_ms.min(buffer_ms);
            }

            last_t_ms = event.t_ms;
        }

        SimulationRun {
            survived: started && rebuffer_count == 0,
            rebuffer_count,
            min_buffer_ms: if started { min_buffer_ms } else { 0 },
        }
    }

    /// Binary search for the maximum sustainable bitrate.
    /// Mirrors Kotlin's ShadowPlayerSimulation.findMaxSustainableBitrate() exactly.
    pub fn find_max_sustainable_bitrate(&self, events: &[FrontierEvent]) -> SimulationResult {
        if events.len() < 2 {
            return SimulationResult {
                max_sustainable_bitrate_mbps: 0.0,
                safe_budget_mbps: 0.0,
                search_iterations: 0,
                startup_buffer_ms: self.startup_buffer_ms,
                max_buffer_ms: self.max_buffer_ms,
                simulated_rebuffer_count: 0,
            };
        }

        let mut sorted_events: Vec<&FrontierEvent> = events.iter().collect();
        sorted_events.sort_by_key(|e| e.t_ms);

        // Build owned sorted slice for simulate()
        let sorted: Vec<FrontierEvent> = sorted_events.into_iter().cloned().collect();

        let mut lo = self.min_bitrate_mbps;
        let mut hi = self.max_bitrate_mbps;
        let mut iterations: u32 = 0;

        while (hi - lo) > self.search_tolerance_mbps {
            let mid = (lo + hi) / 2.0;
            if self.simulate(&sorted, mid).survived {
                lo = mid;
            } else {
                hi = mid;
            }
            iterations += 1;
        }

        let max_sustainable = lo;
        let safe_budget = max_sustainable * self.safety_factor;

        SimulationResult {
            max_sustainable_bitrate_mbps: max_sustainable,
            safe_budget_mbps: safe_budget,
            search_iterations: iterations,
            startup_buffer_ms: self.startup_buffer_ms,
            max_buffer_ms: self.max_buffer_ms,
            simulated_rebuffer_count: self.simulate(&sorted, safe_budget).rebuffer_count,
        }
    }
}

impl Default for SafeBudgetEstimator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_events(pairs: &[(u64, u64)]) -> Vec<FrontierEvent> {
        let mut contiguous: u64 = 0;
        pairs
            .iter()
            .map(|&(t_ms, delta)| {
                contiguous += delta;
                FrontierEvent {
                    t_ms,
                    contiguous_frontier_bytes: contiguous,
                    delta_contiguous_bytes: delta,
                }
            })
            .collect()
    }

    #[test]
    fn empty_events_returns_zero() {
        let est = SafeBudgetEstimator::new();
        let result = est.find_max_sustainable_bitrate(&[]);
        assert_eq!(result.max_sustainable_bitrate_mbps, 0.0);
        assert_eq!(result.safe_budget_mbps, 0.0);
    }

    #[test]
    fn single_event_returns_zero() {
        let est = SafeBudgetEstimator::new();
        let events = make_events(&[(0, 1_000_000)]);
        let result = est.find_max_sustainable_bitrate(&events);
        assert_eq!(result.max_sustainable_bitrate_mbps, 0.0);
    }

    #[test]
    fn zero_bitrate_always_survives() {
        let est = SafeBudgetEstimator::new();
        let events = make_events(&[(0, 1_000), (1_000, 1_000)]);
        let run = est.simulate(&events, 0.0);
        assert!(run.survived);
        assert_eq!(run.rebuffer_count, 0);
    }

    #[test]
    fn high_throughput_survives_high_bitrate() {
        let est = SafeBudgetEstimator::new();
        // Deliver 100MB over 10 seconds = 80 Mbps
        let chunk = 10_000_000u64; // 10MB per event
        let events = make_events(&[
            (0, chunk),
            (1_000, chunk),
            (2_000, chunk),
            (3_000, chunk),
            (4_000, chunk),
            (5_000, chunk),
            (6_000, chunk),
            (7_000, chunk),
            (8_000, chunk),
            (9_000, chunk),
        ]);
        let result = est.find_max_sustainable_bitrate(&events);
        assert!(result.max_sustainable_bitrate_mbps > 30.0);
        assert!(result.safe_budget_mbps > 0.0);
        assert_eq!(result.simulated_rebuffer_count, 0);
    }

    #[test]
    fn safety_factor_applied() {
        let est = SafeBudgetEstimator::new();
        let events = make_events(&[
            (0, 5_000_000),
            (1_000, 5_000_000),
            (2_000, 5_000_000),
            (3_000, 5_000_000),
            (4_000, 5_000_000),
        ]);
        let result = est.find_max_sustainable_bitrate(&events);
        let ratio = result.safe_budget_mbps / result.max_sustainable_bitrate_mbps;
        assert!((ratio - 0.85).abs() < 0.01);
    }
}
