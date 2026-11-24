use hdrhistogram::Histogram;
use std::time::Duration;

/// Bounded-memory histogram; percentile values are upper bounds on the recorded values.
#[derive(Clone, Debug)]
pub struct BoundedLatency {
    histogram: Histogram<u64>,
    min: Duration,
    max: Duration,
    total_seconds: f64,
}

impl BoundedLatency {
    #[cfg(feature = "distributed-offline")]
    pub(crate) fn histogram(&self) -> &Histogram<u64> {
        &self.histogram
    }

    #[cfg(feature = "distributed-offline")]
    pub(crate) fn total_seconds(&self) -> f64 {
        self.total_seconds
    }

    #[cfg(feature = "distributed-offline")]
    pub(crate) fn from_validated(
        histogram: Histogram<u64>,
        min: Duration,
        max: Duration,
        total_seconds: f64,
    ) -> Self {
        Self {
            histogram,
            min,
            max,
            total_seconds,
        }
    }

    #[cfg(feature = "distributed-offline")]
    pub(crate) fn merge_checked(&mut self, other: &Self) -> Result<(), &'static str> {
        let total_count = self
            .len()
            .checked_add(other.len())
            .ok_or("histogram count overflow")?;
        for entry in other.histogram.iter_recorded() {
            self.histogram
                .count_at(entry.value_iterated_to())
                .checked_add(entry.count_at_value())
                .ok_or("histogram bucket overflow")?;
        }
        let total_seconds = self.total_seconds + other.total_seconds;
        if !total_seconds.is_finite()
            || (total_count > 0 && !(total_seconds / total_count as f64 * 1000.0).is_finite())
        {
            return Err("histogram sum overflow");
        }
        self.merge(other)
    }

    pub(crate) fn new() -> Self {
        Self::with_histogram(
            Histogram::new_with_bounds(1, u64::MAX, 3).expect("valid histogram configuration"),
        )
    }

    /// Grow the same three-digit buckets as needed instead of reserving the full range up front.
    pub(crate) fn new_growing() -> Self {
        Self::with_histogram(Histogram::new(3).expect("valid histogram configuration"))
    }

    fn with_histogram(histogram: Histogram<u64>) -> Self {
        Self {
            histogram,
            min: Duration::MAX,
            max: Duration::ZERO,
            total_seconds: 0.0,
        }
    }

    pub(crate) fn record(&mut self, latency: Duration) -> Result<(), &'static str> {
        let nanos = u64::try_from(latency.as_nanos())
            .map_err(|_| "latency exceeds bounded histogram range")?;
        self.histogram
            .record(nanos)
            .map_err(|_| "latency histogram overflow")?;
        self.min = self.min.min(latency);
        self.max = self.max.max(latency);
        self.total_seconds += latency.as_secs_f64();
        Ok(())
    }

    pub(crate) fn merge(&mut self, other: &Self) -> Result<(), &'static str> {
        self.histogram
            .add(&other.histogram)
            .map_err(|_| "latency histogram overflow")?;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.total_seconds += other.total_seconds;
        Ok(())
    }

    pub(crate) fn len(&self) -> u64 {
        self.histogram.len()
    }
    pub(crate) fn min(&self) -> Duration {
        self.min
    }
    pub(crate) fn max(&self) -> Duration {
        self.max
    }
    pub(crate) fn mean_ms(&self) -> f64 {
        self.total_seconds * 1000.0 / self.len() as f64
    }

    pub(crate) fn rank(&self, rank: u64) -> Duration {
        let mut count = 0u64;
        for entry in self.histogram.iter_recorded() {
            count += entry.count_at_value();
            if count >= rank {
                let value = entry.value_iterated_to();
                let max_nanos =
                    u64::try_from(self.max.as_nanos()).expect("recorded histogram value");
                if self.histogram.lowest_equivalent(value)
                    == self.histogram.lowest_equivalent(max_nanos)
                {
                    return self.max;
                }
                return Duration::from_nanos(self.histogram.highest_equivalent(value))
                    .min(self.max);
            }
        }
        unreachable!("rank is within histogram sample count")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn growing_histograms_match_fixed_buckets_across_recording_and_merge() {
        let mut growing = BoundedLatency::new_growing();
        let mut fixed = BoundedLatency::new();
        for nanos in [0, 35, 87_654, 1_234_567] {
            let duration = Duration::from_nanos(nanos);
            growing.record(duration).unwrap();
            fixed.record(duration).unwrap();
        }
        assert!(growing.histogram.distinct_values() < fixed.histogram.distinct_values());

        let mut growing_long = BoundedLatency::new_growing();
        let mut fixed_long = BoundedLatency::new();
        for nanos in [35, 1_234_568, 999_999_999, 1 << 62] {
            let duration = Duration::from_nanos(nanos);
            growing_long.record(duration).unwrap();
            fixed_long.record(duration).unwrap();
        }

        fixed.merge(&fixed_long).unwrap();
        for (destination, source) in [(&growing, &growing_long), (&growing_long, &growing)] {
            let mut destination = destination.clone();
            destination.merge(source).unwrap();
            assert_eq!(destination.len(), fixed.len());
            assert_eq!(destination.min(), fixed.min());
            assert_eq!(destination.max(), fixed.max());
            for rank in 1..=fixed.len() {
                assert_eq!(destination.rank(rank), fixed.rank(rank), "rank {rank}");
            }
        }

        let largest = Duration::from_nanos(u64::MAX);
        for histogram in [&mut growing_long, &mut fixed_long] {
            histogram.record(largest).unwrap();
            assert_eq!(histogram.rank(histogram.len()), largest);
        }
    }
}
