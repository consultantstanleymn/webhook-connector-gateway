//! Token bucket rate limiter, one bucket per tenant.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct Limit {
    /// Burst size.
    pub capacity: f64,
    /// Tokens added each second.
    pub refill_per_sec: f64,
}

#[derive(Debug)]
pub struct TokenBucket {
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(limit: Limit, now: Instant) -> Self {
        Self {
            tokens: limit.capacity,
            last: now,
        }
    }

    /// Take one token. On failure, return how long to wait.
    pub fn try_acquire(&mut self, limit: Limit, now: Instant) -> Result<(), Duration> {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * limit.refill_per_sec).min(limit.capacity);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            let missing = 1.0 - self.tokens;
            let secs = if limit.refill_per_sec > 0.0 {
                missing / limit.refill_per_sec
            } else {
                3600.0
            };
            Err(Duration::from_secs_f64(secs))
        }
    }
}

#[derive(Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, TokenBucket>>,
}

impl RateLimiter {
    pub fn check(&self, tenant: &str, limit: Limit, now: Instant) -> Result<(), Duration> {
        // The lock is short and never held across an await point.
        let mut map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(tenant.to_string())
            .or_insert_with(|| TokenBucket::new(limit, now))
            .try_acquire(limit, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: Limit = Limit {
        capacity: 3.0,
        refill_per_sec: 2.0,
    };

    #[test]
    fn burst_then_block_then_refill() {
        let t0 = Instant::now();
        let rl = RateLimiter::default();
        for _ in 0..3 {
            assert!(rl.check("a", L, t0).is_ok());
        }
        let wait = rl.check("a", L, t0).unwrap_err();
        assert!(wait <= Duration::from_millis(500) && wait > Duration::ZERO);
        assert!(rl.check("a", L, t0 + Duration::from_millis(600)).is_ok());
    }

    #[test]
    fn tenants_are_independent() {
        let t0 = Instant::now();
        let rl = RateLimiter::default();
        for _ in 0..3 {
            rl.check("a", L, t0).unwrap();
        }
        assert!(rl.check("a", L, t0).is_err());
        assert!(rl.check("b", L, t0).is_ok());
    }

    #[test]
    fn never_exceeds_capacity() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(L, t0);
        let later = t0 + Duration::from_secs(3600);
        let mut n = 0;
        while b.try_acquire(L, later).is_ok() {
            n += 1;
        }
        assert_eq!(n, 3);
    }
}
