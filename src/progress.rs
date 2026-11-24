use crate::LatencyStats;
use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

/// Read completed work during a single local run, including after cancellation.
#[derive(Default)]
pub struct RunProgress {
    state: OnceLock<(Instant, Arc<[WorkerProgress]>)>,
    partial: OnceLock<PartialProgress>,
}

struct PartialProgress {
    elapsed: Duration,
    percentiles: Option<(Duration, Duration)>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProgressSnapshot {
    pub elapsed: Duration,
    pub completed: usize,
    pub failed: usize,
}

impl RunProgress {
    /// The p95 and p99 of completed work, available after a cancelled run has joined its workers.
    pub fn partial_percentiles(&self) -> Option<(Duration, Duration)> {
        self.partial.get()?.percentiles
    }

    pub(crate) fn finish_interrupted(&self, elapsed: Duration, stats: Option<LatencyStats>) {
        let percentiles =
            stats.and_then(|stats| Some((stats.percentile(95)?, stats.percentile(99)?)));
        let _ = self.partial.set(PartialProgress {
            elapsed,
            percentiles,
        });
    }

    pub(crate) fn start(
        &self,
        started: Instant,
        workers: usize,
    ) -> Result<Arc<[WorkerProgress]>, &'static str> {
        let counters: Arc<[_]> = (0..workers).map(|_| WorkerProgress::default()).collect();
        self.state
            .set((started, Arc::clone(&counters)))
            .map_err(|_| "progress already belongs to a run")?;
        Ok(counters)
    }

    /// Live snapshots can lag concurrent completions. After workers join, counts are final.
    pub fn snapshot(&self) -> Option<ProgressSnapshot> {
        let (started, workers) = self.state.get()?;
        let mut completed = 0;
        let mut failed = 0;
        for worker in workers.iter() {
            let failures = worker.failed.load(Relaxed);
            completed += worker.succeeded.load(Relaxed) + failures;
            failed += failures;
        }
        Some(ProgressSnapshot {
            elapsed: self
                .partial
                .get()
                .map_or_else(|| started.elapsed(), |partial| partial.elapsed),
            completed,
            failed,
        })
    }
}

// Each worker publishes to its own cache line, without a read-modify-write.
#[repr(align(128))]
#[derive(Default)]
pub(crate) struct WorkerProgress {
    succeeded: AtomicUsize,
    failed: AtomicUsize,
}

impl WorkerProgress {
    pub(crate) fn record(&self, completed: usize, failures: usize, failed: bool) {
        if failed {
            self.failed.store(failures, Relaxed);
        } else {
            self.succeeded.store(completed - failures, Relaxed);
        }
    }
}
