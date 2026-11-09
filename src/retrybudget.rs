//! Retry budget. Retries may not pass a share of first attempts in a sliding window.
//!
//! When a downstream fails, every event retries. The retries can double the load on a
//! system that is already hurt. The budget caps retries at `max(min, ratio * first attempts)`
//! in the window. A refused retry waits for the longest backoff instead.

use std::collections::VecDeque;

const BUCKETS: i64 = 10;

#[derive(Debug, Clone)]
pub struct RetryBudget {
    ratio: f64,
    min_retries: u32,
    window_ms: i64,
    /// (bucket start in ms, first attempts, retries)
    buckets: VecDeque<(i64, u32, u32)>,
}

impl RetryBudget {
    pub fn new(ratio: f64, min_retries: u32, window_ms: i64) -> Self {
        Self {
            ratio,
            min_retries,
            window_ms: window_ms.max(BUCKETS),
            buckets: VecDeque::new(),
        }
    }

    fn expire(&mut self, now_ms: i64) {
        while let Some(front) = self.buckets.front() {
            if front.0 + self.window_ms <= now_ms {
                self.buckets.pop_front();
            } else {
                break;
            }
        }
    }

    fn current(&mut self, now_ms: i64) -> &mut (i64, u32, u32) {
        let width = (self.window_ms / BUCKETS).max(1);
        let start = now_ms - now_ms.rem_euclid(width);
        if self.buckets.back().map(|b| b.0) != Some(start) {
            self.buckets.push_back((start, 0, 0));
        }
        self.buckets.back_mut().expect("bucket exists")
    }

    /// Count one first attempt.
    pub fn record_first(&mut self, now_ms: i64) {
        self.expire(now_ms);
        self.current(now_ms).1 += 1;
    }

    /// Ask for one retry. True counts it against the budget.
    pub fn allow_retry(&mut self, now_ms: i64) -> bool {
        self.expire(now_ms);
        let firsts: u32 = self.buckets.iter().map(|b| b.1).sum();
        let retries: u32 = self.buckets.iter().map(|b| b.2).sum();
        let allowed = self
            .min_retries
            .max((self.ratio * f64::from(firsts)) as u32);
        if retries < allowed {
            self.current(now_ms).2 += 1;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn minimum_retries_are_allowed_without_first_attempts() {
        let mut b = RetryBudget::new(0.2, 3, 60_000);
        let ok: Vec<bool> = (0..5).map(|_| b.allow_retry(1000)).collect();
        assert_eq!(ok, vec![true, true, true, false, false]);
    }

    #[test]
    fn ratio_grows_the_allowance() {
        let mut b = RetryBudget::new(0.5, 0, 60_000);
        for _ in 0..10 {
            b.record_first(1000);
        }
        let allowed = (0..20).filter(|_| b.allow_retry(1000)).count();
        assert_eq!(allowed, 5);
    }

    #[test]
    fn old_counts_leave_the_window() {
        let mut b = RetryBudget::new(0.0, 1, 1000);
        assert!(b.allow_retry(0));
        assert!(!b.allow_retry(500));
        assert!(b.allow_retry(2000), "the first retry left the window");
    }

    proptest! {
        /// Inside one bucket, allowed retries never pass max(min, ratio * firsts).
        #[test]
        fn retries_never_exceed_the_cap(
            ratio in 0.0f64..1.0,
            min in 0u32..10,
            ops in proptest::collection::vec(any::<bool>(), 0..200),
        ) {
            let mut b = RetryBudget::new(ratio, min, 60_000);
            let (mut firsts, mut allowed) = (0u32, 0u32);
            for is_first in ops {
                if is_first {
                    b.record_first(1000);
                    firsts += 1;
                } else if b.allow_retry(1000) {
                    allowed += 1;
                }
            }
            let cap = min.max((ratio * f64::from(firsts)) as u32);
            prop_assert!(allowed <= cap);
        }
    }
}
