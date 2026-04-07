use serde::{Deserialize, Serialize};

/// Page size in bytes — matches Kotlin FrontierTracker (128 KB).
pub const PAGE_SIZE: u64 = 128 * 1024;

/// Stall threshold in milliseconds — matches Kotlin STALL_THRESHOLD_MS.
pub const STALL_THRESHOLD_MS: u64 = 2_000;

/// A single frontier advancement event.
/// Field names match Kotlin's FrontierEvent for trace compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontierEvent {
    /// Timestamp in milliseconds.
    #[serde(rename = "tMs")]
    pub t_ms: u64,
    /// Total contiguous frontier bytes after this advancement.
    #[serde(rename = "contiguousFrontierBytes")]
    pub contiguous_frontier_bytes: u64,
    /// Bytes advanced in this event.
    #[serde(rename = "deltaContiguousBytes")]
    pub delta_contiguous_bytes: u64,
}

/// Summary metrics derived from the frontier event timeline.
/// Field names match Kotlin's FrontierMetrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontierMetrics {
    pub total_frontier_events: u32,
    pub final_frontier_bytes: u64,
    pub frontier_advance_rate_mbps: Option<f64>,
    pub frontier_stall_count: u32,
    pub max_frontier_stall_ms: u64,
}

/// Tracks contiguous playable byte progress during parallel chunk downloads.
///
/// Maintains a BitVec of 128KB page completions and computes the contiguous frontier.
/// Port of Kotlin's FrontierTracker with identical page size and stall threshold.
pub struct FrontierTracker {
    page_size: u64,
    pages: Vec<bool>,
    contiguous_frontier_bytes: u64,
    last_frontier_page_index: usize,
    events: Vec<FrontierEvent>,
}

impl FrontierTracker {
    pub fn new() -> Self {
        Self::with_page_size(PAGE_SIZE)
    }

    pub fn with_page_size(page_size: u64) -> Self {
        Self {
            page_size,
            pages: Vec::new(),
            contiguous_frontier_bytes: 0,
            last_frontier_page_index: 0,
            events: Vec::new(),
        }
    }

    /// Mark a page as completed and attempt to advance the frontier.
    pub fn on_page_completed(&mut self, page_index: u64, t_ms: u64) {
        let idx = page_index as usize;
        if idx >= self.pages.len() {
            self.pages.resize(idx + 1, false);
        }
        self.pages[idx] = true;
        self.advance_frontier(t_ms);
    }

    /// Mark pages completed from raw byte range (matching Kotlin's onBytesDownloaded interface).
    pub fn on_bytes_downloaded(&mut self, absolute_offset: u64, bytes_read: u64, t_ms: u64) {
        if bytes_read == 0 {
            return;
        }
        let first_page = (absolute_offset / self.page_size) as usize;
        let last_page = ((absolute_offset + bytes_read - 1) / self.page_size) as usize;

        if last_page >= self.pages.len() {
            self.pages.resize(last_page + 1, false);
        }
        for p in first_page..=last_page {
            self.pages[p] = true;
        }
        self.advance_frontier(t_ms);
    }

    /// Returns all frontier advancement events.
    pub fn frontier_events(&self) -> &[FrontierEvent] {
        &self.events
    }

    /// Computes summary metrics from the collected frontier timeline.
    pub fn compute_metrics(&self, elapsed_ms: u64) -> FrontierMetrics {
        let final_bytes = self.contiguous_frontier_bytes;
        let advance_rate_mbps = if elapsed_ms > 0 {
            Some(final_bytes as f64 * 8.0 / elapsed_ms as f64 / 1_000.0)
        } else {
            None
        };

        let mut stall_count = 0u32;
        let mut max_stall_ms = 0u64;

        for i in 1..self.events.len() {
            let gap_ms = self.events[i].t_ms.saturating_sub(self.events[i - 1].t_ms);
            if gap_ms > max_stall_ms {
                max_stall_ms = gap_ms;
            }
            if gap_ms > STALL_THRESHOLD_MS {
                stall_count += 1;
            }
        }

        FrontierMetrics {
            total_frontier_events: self.events.len() as u32,
            final_frontier_bytes: final_bytes,
            frontier_advance_rate_mbps: advance_rate_mbps,
            frontier_stall_count: stall_count,
            max_frontier_stall_ms: max_stall_ms,
        }
    }

    /// Current contiguous frontier in bytes.
    pub fn contiguous_frontier_bytes(&self) -> u64 {
        self.contiguous_frontier_bytes
    }

    fn advance_frontier(&mut self, t_ms: u64) {
        let previous = self.contiguous_frontier_bytes;
        let mut page_index = self.last_frontier_page_index;

        while page_index < self.pages.len() && self.pages[page_index] {
            page_index += 1;
        }

        let new_frontier_bytes = page_index as u64 * self.page_size;
        self.last_frontier_page_index = page_index;
        self.contiguous_frontier_bytes = new_frontier_bytes;

        let delta = new_frontier_bytes - previous;
        if delta > 0 {
            self.events.push(FrontierEvent {
                t_ms,
                contiguous_frontier_bytes: new_frontier_bytes,
                delta_contiguous_bytes: delta,
            });
        }
    }
}

impl Default for FrontierTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequential_pages_advance_frontier() {
        let mut tracker = FrontierTracker::new();
        tracker.on_page_completed(0, 100);
        tracker.on_page_completed(1, 200);
        tracker.on_page_completed(2, 300);

        assert_eq!(tracker.contiguous_frontier_bytes(), 3 * PAGE_SIZE);
        assert_eq!(tracker.frontier_events().len(), 3);
    }

    #[test]
    fn gap_blocks_frontier() {
        let mut tracker = FrontierTracker::new();
        tracker.on_page_completed(0, 100);
        tracker.on_page_completed(2, 200); // skip page 1

        assert_eq!(tracker.contiguous_frontier_bytes(), PAGE_SIZE); // only page 0
        assert_eq!(tracker.frontier_events().len(), 1);

        // Fill the gap
        tracker.on_page_completed(1, 300);
        assert_eq!(tracker.contiguous_frontier_bytes(), 3 * PAGE_SIZE);
        assert_eq!(tracker.frontier_events().len(), 2);
    }

    #[test]
    fn stall_detection_matches_kotlin() {
        let mut tracker = FrontierTracker::new();
        tracker.on_page_completed(0, 0);
        tracker.on_page_completed(1, 1_000); // 1s gap — not a stall
        tracker.on_page_completed(2, 4_000); // 3s gap — stall (> 2000ms)

        let metrics = tracker.compute_metrics(4_000);
        assert_eq!(metrics.frontier_stall_count, 1);
        assert_eq!(metrics.max_frontier_stall_ms, 3_000);
    }

    #[test]
    fn bytes_downloaded_computes_pages() {
        let mut tracker = FrontierTracker::new();
        // Download first 256KB (covers pages 0 and 1 at 128KB each)
        tracker.on_bytes_downloaded(0, 256 * 1024, 100);
        assert_eq!(tracker.contiguous_frontier_bytes(), 2 * PAGE_SIZE);
    }

    #[test]
    fn zero_bytes_is_noop() {
        let mut tracker = FrontierTracker::new();
        tracker.on_bytes_downloaded(0, 0, 100);
        assert_eq!(tracker.contiguous_frontier_bytes(), 0);
        assert!(tracker.frontier_events().is_empty());
    }

    #[test]
    fn metrics_advance_rate() {
        let mut tracker = FrontierTracker::new();
        for i in 0..10 {
            tracker.on_page_completed(i, i as u64 * 100);
        }
        let metrics = tracker.compute_metrics(900);
        assert!(metrics.frontier_advance_rate_mbps.unwrap() > 0.0);
        assert_eq!(metrics.final_frontier_bytes, 10 * PAGE_SIZE);
    }
}
