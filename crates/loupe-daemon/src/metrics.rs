//! Request counters and latency histograms.
//!
//! Each request kind keeps a histogram with one bucket per power of two of microseconds, which
//! costs a fixed 40 counters regardless of traffic and gives percentiles accurate to within 2x
//! (reported as the bucket's upper bound, so they never under-state latency).

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use loupe_proto::RequestStats;

const BUCKETS: usize = 40; // 2^39 us is about 6 days

#[derive(Clone, Debug)]
pub(crate) struct Histogram {
    buckets: [u64; BUCKETS],
    count: u64,
    errors: u64,
    max_us: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Histogram {
            buckets: [0; BUCKETS],
            count: 0,
            errors: 0,
            max_us: 0,
        }
    }
}

impl Histogram {
    pub(crate) fn record(&mut self, d: Duration, error: bool) {
        let us = u64::try_from(d.as_micros()).unwrap_or(u64::MAX).max(1);
        // Bucket i holds [2^i, 2^(i+1)) microseconds.
        let i = (63 - us.leading_zeros() as usize).min(BUCKETS - 1);
        self.buckets[i] += 1;
        self.count += 1;
        self.errors += u64::from(error);
        self.max_us = self.max_us.max(us);
    }

    /// Upper bound of the bucket containing the `q`-quantile (0 < q <= 1).
    pub(crate) fn percentile_us(&self, q: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let rank = ((self.count as f64 * q).ceil() as u64).clamp(1, self.count);
        let mut seen = 0;
        for (i, n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= rank {
                // Never report more than the true maximum.
                return (1u64 << (i + 1)).min(self.max_us.max(1));
            }
        }
        self.max_us
    }
}

#[derive(Debug, Default)]
pub(crate) struct Metrics {
    by_kind: Mutex<BTreeMap<&'static str, Histogram>>,
}

impl Metrics {
    pub(crate) fn record(&self, kind: &'static str, d: Duration, error: bool) {
        self.by_kind
            .lock()
            .unwrap()
            .entry(kind)
            .or_default()
            .record(d, error);
    }

    pub(crate) fn snapshot(&self) -> Vec<RequestStats> {
        self.by_kind
            .lock()
            .unwrap()
            .iter()
            .map(|(kind, h)| RequestStats {
                kind: (*kind).to_string(),
                count: h.count,
                errors: h.errors,
                p50_us: h.percentile_us(0.50),
                p95_us: h.percentile_us(0.95),
                p99_us: h.percentile_us(0.99),
                max_us: h.max_us,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn us(n: u64) -> Duration {
        Duration::from_micros(n)
    }

    #[test]
    fn percentiles_are_bucket_upper_bounds_capped_at_the_max() {
        let mut h = Histogram::default();
        for _ in 0..90 {
            h.record(us(100), false); // bucket [64, 128)
        }
        for _ in 0..9 {
            h.record(us(1_000), false); // bucket [512, 1024)
        }
        h.record(us(50_000), true); // bucket [32768, 65536)
        assert_eq!((h.count, h.errors, h.max_us), (100, 1, 50_000));
        assert_eq!(h.percentile_us(0.50), 128);
        // The 95th sample falls in the [512, 1024) bucket, reported by its upper bound.
        assert_eq!(h.percentile_us(0.95), 1024);
        assert_eq!(
            h.percentile_us(1.0),
            50_000,
            "the top percentile is capped at the true maximum"
        );
    }

    #[test]
    fn empty_and_single_sample() {
        assert_eq!(Histogram::default().percentile_us(0.99), 0);
        let mut h = Histogram::default();
        h.record(us(300), false);
        // One sample: every percentile is that sample's bucket, never above the max.
        assert_eq!((h.percentile_us(0.5), h.percentile_us(0.99)), (300, 300));
        h.record(Duration::ZERO, false);
        assert_eq!(h.count, 2);
    }

    #[test]
    fn percentiles_never_understate_and_overstate_at_most_2x() {
        let mut h = Histogram::default();
        let samples: Vec<u64> = (1..=1000).map(|i| i * 7).collect();
        for s in &samples {
            h.record(us(*s), false);
        }
        for q in [0.5, 0.9, 0.99] {
            let exact =
                samples[((samples.len() as f64 * q).ceil() as usize - 1).min(samples.len() - 1)];
            let est = h.percentile_us(q);
            assert!(est >= exact, "q={q}: estimate {est} below exact {exact}");
            assert!(
                est <= exact * 2,
                "q={q}: estimate {est} more than 2x exact {exact}"
            );
        }
    }

    #[test]
    fn metrics_group_by_kind_and_sort() {
        let m = Metrics::default();
        m.record("search", us(100), false);
        m.record("search", us(300), true);
        m.record("outline", us(10), false);
        let snap = m.snapshot();
        assert_eq!(
            snap.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(),
            ["outline", "search"]
        );
        let s = &snap[1];
        assert_eq!((s.count, s.errors, s.max_us), (2, 1, 300));
    }

    #[test]
    fn huge_durations_do_not_overflow() {
        let mut h = Histogram::default();
        h.record(Duration::from_secs(10_000_000), false);
        assert_eq!(h.count, 1);
        assert!(h.percentile_us(0.5) > 0);
    }
}
