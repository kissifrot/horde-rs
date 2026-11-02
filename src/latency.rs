use hdrhistogram::Histogram;
use std::time::Duration;

/// Fixed-range histogram; percentile values are upper bounds on the recorded values.
#[derive(Clone, Debug)]
pub struct BoundedLatency {
    histogram: Histogram<u64>,
    min: Duration,
    max: Duration,
    total_seconds: f64,
}

impl BoundedLatency {
    pub(crate) fn new() -> Self {
        Self {
            histogram: Histogram::new_with_bounds(1, u64::MAX, 3)
                .expect("valid histogram configuration"),
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
