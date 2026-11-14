//! A high dynamic range latency histogram.
//!
//! The layout is Gil Tene's HdrHistogram: values fall into buckets that
//! double in width, and each bucket is split into the same number of
//! linear sub-buckets, enough that any recorded value is reported with
//! three significant decimal digits. Recording is an index computation and
//! one increment; a quantile is one pass over the counts. A histogram
//! tracking 1 ns to an hour holds 33,792 counters (264 KiB), whatever it
//! records.
//!
//! It is written here rather than taken from a crate because the workspace
//! is at its direct dependency budget, and because the harness needs only
//! recording, merging and quantiles.

use std::time::Duration;

/// Significant decimal digits every recorded value keeps.
const SIGNIFICANT_DIGITS: u32 = 3;

/// The largest value the default histogram tracks: one hour in
/// nanoseconds. Larger values are recorded as this one and counted in
/// [`Histogram::saturated`].
pub const DEFAULT_HIGHEST: u64 = 3_600_000_000_000;

/// Counts of values from 1 to a highest trackable value, each kept to three
/// significant digits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Histogram {
    counts: Vec<u64>,
    highest: u64,
    /// log2 of the sub-buckets in a bucket's upper half.
    half_magnitude: u32,
    /// Every value at or below this mask lands in bucket 0.
    sub_bucket_mask: u64,
    total: u64,
    min: u64,
    max: u64,
    sum: u128,
    saturated: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Histogram::new(DEFAULT_HIGHEST)
    }
}

impl Histogram {
    /// A histogram tracking 1 to `highest` (at least 2) with three
    /// significant digits.
    pub fn new(highest: u64) -> Self {
        let highest = highest.max(2);
        // Enough sub-buckets that one step is at most one part in 10^digits
        // of the smallest value in a bucket's upper half: 2048 for three.
        let largest_single_unit = 2 * 10u64.pow(SIGNIFICANT_DIGITS);
        let sub_bucket_magnitude = 64 - (largest_single_unit - 1).leading_zeros();
        let sub_bucket_count = 1u64 << sub_bucket_magnitude;
        let half_magnitude = sub_bucket_magnitude - 1;
        // Bucket b covers values below sub_bucket_count << b.
        let mut buckets = 1u32;
        let mut untrackable = sub_bucket_count;
        while untrackable <= highest {
            if untrackable > u64::MAX / 2 {
                buckets += 1;
                break;
            }
            untrackable <<= 1;
            buckets += 1;
        }
        let len = ((buckets as usize) + 1) << half_magnitude;
        Histogram {
            counts: vec![0; len],
            highest,
            half_magnitude,
            sub_bucket_mask: sub_bucket_count - 1,
            total: 0,
            min: u64::MAX,
            max: 0,
            sum: 0,
            saturated: 0,
        }
    }

    /// The largest value this histogram tracks.
    pub fn highest(&self) -> u64 {
        self.highest
    }

    fn bucket_of(&self, value: u64) -> u32 {
        // The bucket is how far the value's top bit sits above the
        // sub-bucket range.
        let base = 64 - self.half_magnitude - 1;
        base - (value | self.sub_bucket_mask).leading_zeros()
    }

    fn index_of(&self, value: u64) -> usize {
        let bucket = self.bucket_of(value);
        let sub = value >> bucket;
        let half = 1u64 << self.half_magnitude;
        (((bucket as u64 + 1) << self.half_magnitude) + sub - half) as usize
    }

    /// The smallest value the counter at `index` holds, and the width of
    /// the range it holds.
    fn range_at(&self, index: usize) -> (u64, u64) {
        let half = 1usize << self.half_magnitude;
        let mut bucket = (index >> self.half_magnitude) as i64 - 1;
        let mut sub = (index & (half - 1)) + half;
        if bucket < 0 {
            sub -= half;
            bucket = 0;
        }
        ((sub as u64) << bucket, 1u64 << bucket)
    }

    /// Records one value. Zero is recorded as 1 and anything above
    /// [`highest`](Self::highest) as the highest.
    pub fn record(&mut self, value: u64) {
        self.record_n(value, 1);
    }

    /// Records `count` copies of one value.
    pub fn record_n(&mut self, value: u64, count: u64) {
        if count == 0 {
            return;
        }
        let mut value = value.max(1);
        if value > self.highest {
            value = self.highest;
            self.saturated += count;
        }
        let index = self.index_of(value);
        self.counts[index] += count;
        self.total += count;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.sum += value as u128 * count as u128;
    }

    /// Records a duration in nanoseconds.
    pub fn record_duration(&mut self, duration: Duration) {
        self.record(u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX));
    }

    /// Adds every count of `other`, which must track the same range.
    ///
    /// # Panics
    /// If the two track different ranges.
    pub fn merge(&mut self, other: &Histogram) {
        assert_eq!(
            self.highest, other.highest,
            "histograms track different ranges"
        );
        for (mine, theirs) in self.counts.iter_mut().zip(&other.counts) {
            *mine += theirs;
        }
        self.total += other.total;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.sum += other.sum;
        self.saturated += other.saturated;
    }

    /// How many values were recorded.
    pub fn len(&self) -> u64 {
        self.total
    }

    /// Whether nothing was recorded.
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// How many values were above the highest trackable value.
    pub fn saturated(&self) -> u64 {
        self.saturated
    }

    /// The smallest value recorded, exactly; 0 when empty.
    pub fn min(&self) -> u64 {
        if self.is_empty() { 0 } else { self.min }
    }

    /// The largest value recorded, exactly; 0 when empty.
    pub fn max(&self) -> u64 {
        self.max
    }

    /// The mean of the recorded values, exactly; 0 when empty.
    pub fn mean(&self) -> f64 {
        if self.is_empty() {
            0.0
        } else {
            self.sum as f64 / self.total as f64
        }
    }

    /// The value at quantile `q` (0 to 1): the smallest recorded value
    /// that at least `q` of the values are at or below, reported as the
    /// top of its counter's range and never above the exact maximum, so
    /// it is within one part in a thousand of the true value. 0 when
    /// empty.
    pub fn value_at_quantile(&self, q: f64) -> u64 {
        if self.is_empty() {
            return 0;
        }
        let q = if q.is_nan() { 0.0 } else { q.clamp(0.0, 1.0) };
        let rank = ((q * self.total as f64).ceil() as u64).clamp(1, self.total);
        let mut seen = 0u64;
        for (index, &count) in self.counts.iter().enumerate() {
            seen += count;
            if seen >= rank {
                let (low, width) = self.range_at(index);
                return (low + (width - 1)).min(self.max).max(self.min);
            }
        }
        self.max
    }

    /// The counts in ascending order of value, as (top of the counter's
    /// range, count), skipping empty counters.
    pub fn iter_recorded(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.counts
            .iter()
            .enumerate()
            .filter(|(_, count)| **count > 0)
            .map(|(index, &count)| {
                let (low, width) = self.range_at(index);
                (low + (width - 1), count)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact value at quantile q of sorted values, by the same rank rule.
    fn exact(sorted: &[u64], q: f64) -> u64 {
        let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
        sorted[rank - 1]
    }

    fn within_precision(reported: u64, truth: u64) -> bool {
        let truth = truth.max(1);
        reported >= truth && (reported - truth) as f64 <= truth as f64 / 1000.0
    }

    #[test]
    fn small_values_are_exact() {
        let mut h = Histogram::default();
        for v in 1..=2047 {
            h.record(v);
        }
        for v in [1u64, 2, 500, 1024, 2047] {
            let q = (v as f64 - 0.5) / 2047.0;
            assert_eq!(h.value_at_quantile(q), v);
        }
        assert_eq!(h.min(), 1);
        assert_eq!(h.max(), 2047);
        assert_eq!(h.len(), 2047);
        assert_eq!(h.mean(), 1024.0);
    }

    #[test]
    fn default_layout_size() {
        let h = Histogram::default();
        assert_eq!(h.counts.len(), 33 * 1024);
        assert_eq!(h.highest(), DEFAULT_HIGHEST);
    }

    #[test]
    fn every_index_round_trips_to_a_range_holding_its_values() {
        let h = Histogram::new(1 << 40);
        let mut value = 1u64;
        while value < (1 << 40) {
            let index = h.index_of(value);
            let (low, width) = h.range_at(index);
            assert!(
                low <= value && value < low + width,
                "value {value} index {index}"
            );
            assert!(width as f64 <= (low as f64 / 1000.0).max(1.0) * 2.0);
            value = value * 3 / 2 + 1;
        }
        // The highest value lands inside the counts.
        assert!(h.index_of(1 << 40) < h.counts.len());
    }

    #[test]
    fn quantiles_of_a_wide_spread_keep_three_digits() {
        // A deterministic spread from 1 ns to about 50 s.
        let mut values = Vec::new();
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let magnitude = x % 36;
            values.push((x >> 20) % (1u64 << magnitude) + 1);
        }
        let mut h = Histogram::default();
        for &v in &values {
            h.record(v);
        }
        values.sort_unstable();
        for q in [0.0, 0.1, 0.5, 0.9, 0.99, 0.999, 0.9999, 1.0] {
            let reported = h.value_at_quantile(q);
            let truth = exact(&values, q);
            assert!(
                within_precision(reported, truth),
                "q {q}: {reported} vs {truth}"
            );
        }
        assert_eq!(h.value_at_quantile(1.0), *values.last().unwrap());
        assert_eq!(h.min(), values[0]);
        let recorded: u64 = h.iter_recorded().map(|(_, c)| c).sum();
        assert_eq!(recorded, 20_000);
        let tops: Vec<u64> = h.iter_recorded().map(|(v, _)| v).collect();
        assert!(tops.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn a_rare_tail_shows_in_the_high_quantiles() {
        let mut h = Histogram::default();
        h.record_n(1_000_000, 9_990);
        h.record_n(100_000_000, 10);
        assert_eq!(h.value_at_quantile(0.5), 1_000_447);
        assert!(within_precision(h.value_at_quantile(0.998), 1_000_000));
        assert!(within_precision(h.value_at_quantile(0.9995), 100_000_000));
        assert_eq!(h.value_at_quantile(1.0), 100_000_000);
    }

    #[test]
    fn zero_and_oversized_values_are_clamped() {
        let mut h = Histogram::new(1000);
        h.record(0);
        h.record(5000);
        h.record_n(7, 0);
        assert_eq!(h.len(), 2);
        assert_eq!(h.min(), 1);
        assert_eq!(h.max(), 1000);
        assert_eq!(h.saturated(), 1);
        assert_eq!(h.value_at_quantile(f64::NAN), 1);
        assert_eq!(h.value_at_quantile(7.0), 1000);
        assert_eq!(Histogram::new(0).highest(), 2);
        let mut huge = Histogram::new(u64::MAX);
        huge.record(u64::MAX);
        assert_eq!(huge.value_at_quantile(1.0), u64::MAX);
        assert_eq!(huge.saturated(), 0);
    }

    #[test]
    fn an_empty_histogram_reports_zeros() {
        let h = Histogram::default();
        assert!(h.is_empty());
        assert_eq!(h.min(), 0);
        assert_eq!(h.max(), 0);
        assert_eq!(h.mean(), 0.0);
        assert_eq!(h.value_at_quantile(0.99), 0);
        assert_eq!(h.iter_recorded().count(), 0);
    }

    #[test]
    fn durations_record_in_nanoseconds() {
        let mut h = Histogram::default();
        h.record_duration(Duration::from_micros(250));
        h.record_duration(Duration::MAX);
        assert_eq!(h.min(), 250_000);
        assert_eq!(h.max(), DEFAULT_HIGHEST);
        assert_eq!(h.saturated(), 1);
    }

    #[test]
    fn merging_equals_recording_everything_in_one() {
        let mut a = Histogram::default();
        let mut b = Histogram::default();
        let mut all = Histogram::default();
        for v in 1..5000u64 {
            let v = v * v;
            (if v % 3 == 0 { &mut a } else { &mut b }).record(v);
            all.record(v);
        }
        a.merge(&b);
        assert_eq!(a, all);
        let mut empty = Histogram::default();
        empty.merge(&Histogram::default());
        assert!(empty.is_empty());
        assert_eq!(empty.min(), 0);
    }

    #[test]
    #[should_panic(expected = "different ranges")]
    fn merging_different_ranges_panics() {
        Histogram::new(1000).merge(&Histogram::new(2000));
    }
}
