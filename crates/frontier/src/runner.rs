//! Benchmark runner that wires FrontierTracker + SafeBudgetEstimator to trace events.
//!
//! Consumes PageCompleted events from the transfer reactor and advances the frontier,
//! then produces a CapabilityEnvelope from the observed metrics.

use crate::budget::SafeBudgetEstimator;
use crate::envelope::CapabilityEnvelope;
use crate::tracker::FrontierTracker;
use crossbeam_channel::{Receiver, Sender};
use frontier_telemetry::events::TraceEvent;
use std::time::Instant;

/// Result of a frontier-tracked benchmark run.
pub struct FrontierRunResult {
    pub envelope: CapabilityEnvelope,
    pub frontier_events_count: u32,
    pub final_frontier_bytes: u64,
    pub frontier_advance_rate_mbps: Option<f64>,
    pub frontier_stall_count: u32,
    pub max_frontier_stall_ms: u64,
    pub max_sustainable_bitrate_mbps: f64,
    pub safe_budget_mbps: f64,
    /// Wall-clock run duration in ms.
    pub run_duration_ms: u64,
    /// Time from last frontier advance to run end (the tail drain gap).
    pub tail_drain_ms: u64,
}

/// Processes trace events from the reactor, tracking frontier advancement and
/// re-emitting frontier_advanced events. Returns a CapabilityEnvelope when done.
pub fn run_frontier_tracker(
    event_rx: Receiver<TraceEvent>,
    frontier_tx: Sender<TraceEvent>,
    feedback_tx: Option<Sender<u64>>,
    run_start: Instant,
    config: FrontierRunConfig,
) -> FrontierRunResult {
    let mut tracker = FrontierTracker::with_page_size(config.page_size);
    let page_size = config.page_size;

    // Process all events from the reactor
    for event in &event_rx {
        match &event {
            TraceEvent::PageCompleted {
                page_index,
                timestamp_ns,
                ..
            } => {
                let t_ms = (*timestamp_ns / 1_000_000) as u64;
                let prev_frontier = tracker.contiguous_frontier_bytes();
                tracker.on_page_completed(*page_index, t_ms);
                let new_frontier = tracker.contiguous_frontier_bytes();

                // Emit frontier_advanced if frontier moved
                if new_frontier > prev_frontier {
                    let _ = frontier_tx.send(TraceEvent::FrontierAdvanced {
                        frontier_byte: new_frontier,
                        frontier_page: new_frontier / page_size,
                        contiguous_bytes: new_frontier,
                        timestamp_ns: *timestamp_ns,
                    });

                    // Send feedback to reactor for frontier-aware scheduling
                    if let Some(ref fb_tx) = feedback_tx {
                        let _ = fb_tx.send(new_frontier);
                    }
                }
            }
            _ => {}
        }

        // Forward all events to the output channel
        let _ = frontier_tx.send(event);
    }

    // Use wall-clock elapsed time for metrics computation.
    // This is critical: the frontier may stop advancing long before the run ends,
    // and the tail drain (time from last frontier advance to run end) must be
    // accounted for in the budget simulation.
    let wall_elapsed_ms = run_start.elapsed().as_millis() as u64;
    let elapsed_ms = wall_elapsed_ms.max(1); // avoid division by zero
    let metrics = tracker.compute_metrics(elapsed_ms);

    // Build the event list for simulation, including a synthetic tail-drain event.
    // The tail-drain event has zero delta bytes at the run end timestamp, which
    // forces the simulator to drain the buffer for the entire no-progress tail.
    // Without this, the simulator only sees events up to the last frontier advance
    // and ignores the potentially long gap to run end — dramatically overstating
    // the sustainable bitrate.
    let mut sim_events: Vec<crate::tracker::FrontierEvent> = tracker.frontier_events().to_vec();
    if let Some(last) = sim_events.last() {
        let last_event_ms = last.t_ms;
        if wall_elapsed_ms > last_event_ms + 100 {
            // Add a zero-delta event at run end to force tail drain
            sim_events.push(crate::tracker::FrontierEvent {
                t_ms: wall_elapsed_ms,
                contiguous_frontier_bytes: last.contiguous_frontier_bytes,
                delta_contiguous_bytes: 0,
            });
        }
    }

    // Run shadow player simulation with tail-drain-aware events
    let estimator = SafeBudgetEstimator::new();
    let sim_result = estimator.find_max_sustainable_bitrate(&sim_events);

    // Compute tail drain: gap between last frontier advance and run end
    let tail_drain_ms = tracker.frontier_events()
        .last()
        .map(|last| wall_elapsed_ms.saturating_sub(last.t_ms))
        .unwrap_or(wall_elapsed_ms);

    // Stability penalty must include the tail drain gap (frontier freeze)
    let effective_max_stall = metrics.max_frontier_stall_ms.max(tail_drain_ms);
    let effective_stall_count = if tail_drain_ms > 2_000 {
        metrics.frontier_stall_count + 1
    } else {
        metrics.frontier_stall_count
    };

    // Build capability envelope
    let envelope = CapabilityEnvelope {
        max_safe_urgent_workers: config.urgent_workers,
        max_safe_prefetch_workers: config.prefetch_workers,
        max_safe_urgent_chunk_bytes: config.chunk_size_bytes,
        max_safe_prefetch_chunk_bytes: config.chunk_size_bytes * 2,
        sustained_throughput_mbps: metrics
            .frontier_advance_rate_mbps
            .unwrap_or(0.0),
        p10_throughput_mbps: None, // requires multiple runs
        seek_ttfb_p50_ms: None, // requires seek testing
        stability_penalty: compute_stability_penalty(
            effective_stall_count,
            effective_max_stall,
        ),
        supports_range_requests: config.range_support,
        measured_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    };

    FrontierRunResult {
        envelope,
        frontier_events_count: metrics.total_frontier_events,
        final_frontier_bytes: metrics.final_frontier_bytes,
        frontier_advance_rate_mbps: metrics.frontier_advance_rate_mbps,
        frontier_stall_count: effective_stall_count,
        max_frontier_stall_ms: effective_max_stall,
        max_sustainable_bitrate_mbps: sim_result.max_sustainable_bitrate_mbps,
        safe_budget_mbps: sim_result.safe_budget_mbps,
        run_duration_ms: wall_elapsed_ms,
        tail_drain_ms,
    }
}

/// Configuration for the frontier runner.
pub struct FrontierRunConfig {
    pub page_size: u64,
    pub chunk_size_bytes: u64,
    pub urgent_workers: u32,
    pub prefetch_workers: u32,
    pub range_support: bool,
}

/// Compute stability penalty from stall metrics.
/// 0.0 = perfectly stable, 1.0 = very unstable.
fn compute_stability_penalty(stall_count: u32, max_stall_ms: u64) -> f64 {
    let count_penalty = (stall_count as f64 * 0.1).min(0.5);
    let duration_penalty = if max_stall_ms > 5_000 {
        0.5
    } else if max_stall_ms > 2_000 {
        0.3
    } else {
        0.0
    };
    (count_penalty + duration_penalty).min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frontier_runner_processes_pages() {
        let (reactor_tx, reactor_rx) = crossbeam_channel::unbounded();
        let (frontier_tx, frontier_rx) = crossbeam_channel::unbounded();
        let run_start = Instant::now();

        let config = FrontierRunConfig {
            page_size: 128 * 1024,
            chunk_size_bytes: 4 * 1024 * 1024,
            urgent_workers: 2,
            prefetch_workers: 1,
            range_support: true,
        };

        // Send page events
        for i in 0..10u64 {
            reactor_tx
                .send(TraceEvent::PageCompleted {
                    request_id: 1,
                    chunk_id: 0,
                    page_index: i,
                    page_start_byte: i * 128 * 1024,
                    page_end_byte: (i + 1) * 128 * 1024,
                    page_bytes: 128 * 1024,
                    cumulative_bytes: (i + 1) * 128 * 1024,
                    timestamp_ns: (i + 1) * 100_000_000, // 100ms apart
                })
                .unwrap();
        }
        drop(reactor_tx);

        let result = run_frontier_tracker(reactor_rx, frontier_tx, None, run_start, config);

        assert_eq!(result.final_frontier_bytes, 10 * 128 * 1024);
        assert!(result.frontier_events_count > 0);
        assert!(result.envelope.sustained_throughput_mbps > 0.0);
        assert!(result.envelope.supports_range_requests);

        // Check that frontier_advanced events were emitted
        let events: Vec<_> = frontier_rx.try_iter().collect();
        let frontier_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, TraceEvent::FrontierAdvanced { .. }))
            .collect();
        assert_eq!(frontier_events.len(), 10);
    }

    #[test]
    fn stability_penalty_ranges() {
        assert_eq!(compute_stability_penalty(0, 0), 0.0);
        assert!(compute_stability_penalty(3, 1000) > 0.0);
        assert!(compute_stability_penalty(5, 6000) >= 0.5);
        assert!(compute_stability_penalty(10, 10000) <= 1.0);
    }
}
