//! Overflow-aware counters used by the local load generator.
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
pub(crate) struct AtomicCounter {
    count: AtomicUsize,
    overflowed: AtomicBool,
}

impl AtomicCounter {
    pub(crate) fn increment(&self) {
        if self.count.fetch_add(1, Ordering::Relaxed) == usize::MAX {
            self.overflowed.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn get(&self) -> Option<usize> {
        (!self.overflowed.load(Ordering::Relaxed)).then(|| self.count.load(Ordering::Relaxed))
    }
}

/// Continue collecting without panicking, but never report a wrapped sum as exact.
pub(crate) fn add_bytes(total: &mut u64, amount: u64, overflowed: &mut bool) {
    let (sum, overflow) = total.overflowing_add(amount);
    *total = sum;
    *overflowed |= overflow;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn byte_counter_detects_overflow_and_keeps_flag() {
        let mut total = u64::MAX - 1;
        let mut overflow = false;
        add_bytes(&mut total, 1, &mut overflow);
        assert!(!overflow);
        add_bytes(&mut total, 1, &mut overflow);
        assert!(overflow);
        add_bytes(&mut total, 1, &mut overflow);
        assert!(overflow);
    }
    #[test]
    fn atomic_counter_detects_overflow_and_keeps_flag() {
        let counter = AtomicCounter {
            count: AtomicUsize::new(usize::MAX - 1),
            overflowed: AtomicBool::new(false),
        };
        counter.increment();
        assert_eq!(counter.get(), Some(usize::MAX));
        counter.increment();
        counter.increment();
        assert_eq!(counter.get(), None);
    }
    #[test]
    fn concurrent_overflow_is_detected() {
        let counter = AtomicCounter {
            count: AtomicUsize::new(usize::MAX - 1),
            overflowed: AtomicBool::new(false),
        };
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| counter.increment());
            }
        });
        assert_eq!(counter.get(), None);
    }
}
