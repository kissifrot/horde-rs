//! Shared admission clock for request rates that need sub-millisecond pacing.
use std::{
    sync::atomic::{AtomicU64, Ordering::Relaxed},
    time::{Duration, Instant},
};

// Small runs have fewer workers competing for timer wakeups. Taper their
// extra headroom back to the measured 500 starts/s at eight local workers,
// so a standalone run or two equal Grunts keep the same cutoff at 16 total.
fn timer_starts_per_worker(workers: usize) -> usize {
    match workers {
        0..=2 => 600,
        3..=4 => 550,
        5..=7 => 525,
        _ => 500,
    }
}

pub(crate) struct FastPacer {
    start: Instant,
    interval_ns: u64,
    next_ns: AtomicU64,
}

impl FastPacer {
    pub(crate) fn new(rate: u32, workers: usize, start: Instant) -> Option<Self> {
        let timer_capacity = workers.saturating_mul(timer_starts_per_worker(workers));
        if rate <= 1_000 || rate as usize <= timer_capacity {
            return None;
        }
        let interval_ns = 1_000_000_000u64.div_ceil(u64::from(rate));
        Some(Self {
            start,
            interval_ns,
            next_ns: AtomicU64::new(0),
        })
    }

    fn try_admit(&self, now_ns: u64) -> bool {
        let next_ns = self.next_ns.load(Relaxed);
        now_ns >= next_ns
            && self
                .next_ns
                .compare_exchange(
                    next_ns,
                    now_ns.saturating_add(self.interval_ns),
                    Relaxed,
                    Relaxed,
                )
                .is_ok()
    }

    /// Admit one worker at a time without banking missed slots. Yielding keeps
    /// this wait cancellable and gives competing I/O a chance to run.
    pub(crate) async fn wait(&self, duration: Option<Duration>) -> bool {
        loop {
            let elapsed = self.start.elapsed();
            if duration.is_some_and(|limit| elapsed >= limit) {
                return false;
            }
            let now_ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
            if self.try_admit(now_ns) {
                return true;
            }
            tokio::task::yield_now().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admissions_are_spaced_and_do_not_accumulate_after_a_pause() {
        let pacer = FastPacer::new(300_000, 16, Instant::now()).unwrap();
        assert_eq!(pacer.interval_ns, 3_334);
        assert!(pacer.try_admit(0));
        assert!(!pacer.try_admit(0));
        assert!(!pacer.try_admit(3_333));
        assert!(pacer.try_admit(3_334));

        assert!(pacer.try_admit(100_000));
        assert!(!pacer.try_admit(100_000));
        assert!(!pacer.try_admit(103_333));
        assert!(pacer.try_admit(103_334));
    }

    #[test]
    fn selects_the_timer_for_moderate_rates_at_each_worker_count() {
        let start = Instant::now();
        for (workers, slowest_fast_rate) in [
            (1, 1_001),
            (2, 1_201),
            (4, 2_201),
            (6, 3_151),
            (8, 4_001),
            (16, 8_001),
        ] {
            assert!(FastPacer::new(slowest_fast_rate - 1, workers, start).is_none());
            assert!(FastPacer::new(slowest_fast_rate, workers, start).is_some());
        }
    }

    #[tokio::test]
    async fn fast_wait_yields_and_can_be_cancelled() {
        let pacer = FastPacer::new(4_000, 4, Instant::now()).unwrap();
        pacer.next_ns.store(u64::MAX, Relaxed);
        let cancel = crate::Cancellation::new();
        let mut cancelled = cancel.subscribe();
        let wait = pacer.wait(None);
        tokio::pin!(wait);
        assert!(futures_util::poll!(&mut wait).is_pending());

        cancel.cancel();
        tokio::select! {
            biased;
            _ = crate::wait_for_cancel(&mut cancelled) => {}
            _ = &mut wait => panic!("cancelled worker should not be admitted"),
        }
        assert!(!pacer.wait(Some(Duration::ZERO)).await);
    }
}
