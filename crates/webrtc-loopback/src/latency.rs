//! Latency measurement infrastructure: ring buffer and statistics.
//!
//! Provides a fixed-capacity ring buffer for latency samples and
//! functions to compute percentile/mean/stddev/jitter statistics.

pub const LATENCY_RING_CAPACITY: usize = 8192;

/// Fixed-capacity ring buffer for latency samples.
/// Overwrites the oldest sample when full.
pub struct LatencyRing {
    buffer: Vec<u64>,
    head: usize,
    count: usize,
    total_count: usize,
}

impl LatencyRing {
    pub fn new() -> Self {
        Self {
            buffer: vec![0; LATENCY_RING_CAPACITY],
            head: 0,
            count: 0,
            total_count: 0,
        }
    }

    pub fn push(&mut self, value: u64) {
        self.buffer[self.head] = value;
        self.head = (self.head + 1) % LATENCY_RING_CAPACITY;
        if self.count < LATENCY_RING_CAPACITY {
            self.count += 1;
        }
        self.total_count = self.total_count.saturating_add(1);
    }

    pub fn samples(&self) -> Vec<u64> {
        let mut result = Vec::with_capacity(self.count);
        if self.count < LATENCY_RING_CAPACITY {
            result.extend_from_slice(&self.buffer[..self.count]);
        } else {
            result.extend_from_slice(&self.buffer[self.head..]);
            result.extend_from_slice(&self.buffer[..self.head]);
        }
        result
    }

    pub fn total_count(&self) -> usize {
        self.total_count
    }
}

/// Aggregated latency statistics computed from a ring of samples.
#[derive(Default)]
pub struct LatencySummary {
    pub samples: usize,
    pub total_samples: usize,
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
    pub mean: u64,
    pub stddev: u64,
    pub jitter: u64,
}

/// Compute latency statistics from a set of samples.
///
/// Sorts the samples in-place and returns percentiles, mean, stddev, and jitter.
pub fn compute_latency_summary(mut samples: Vec<u64>, total_samples: usize) -> LatencySummary {
    if samples.is_empty() {
        return LatencySummary::default();
    }
    // Compute temporal jitter *before* sorting — this captures the actual
    // inter-frame variability in arrival order, not the spread of sorted values.
    let jitter = if samples.len() > 1 {
        let mut jitter_sum: u64 = 0;
        for window in samples.windows(2) {
            jitter_sum += window[1].abs_diff(window[0]);
        }
        jitter_sum / (samples.len() - 1) as u64
    } else {
        0
    };
    samples.sort_unstable();
    let percentile = |percent: usize| samples[(samples.len() - 1) * percent / 100];
    let sum: u64 = samples.iter().fold(0u64, |acc, &s| acc.saturating_add(s));
    let mean = sum / samples.len() as u64;
    let variance = samples
        .iter()
        .map(|&s| {
            let diff = s.abs_diff(mean);
            diff.saturating_mul(diff)
        })
        .fold(0u64, |acc, v| acc.saturating_add(v))
        / samples.len() as u64;
    let stddev = (variance as f64).sqrt() as u64;
    LatencySummary {
        samples: samples.len(),
        total_samples,
        p50: percentile(50),
        p95: percentile(95),
        p99: percentile(99),
        max: samples[samples.len() - 1],
        mean,
        stddev,
        jitter,
    }
}
