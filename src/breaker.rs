//! Circuit breaker for a downstream endpoint.
//!
//! Closed: calls pass. After `threshold` failures in a row it opens.
//! Open: calls are refused until the cooldown ends.
//! Half open: one probe call passes. Success closes it, failure re-opens it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Closed { failures: u32 },
    Open { until: Instant },
    HalfOpen { probing: bool },
}

#[derive(Debug)]
pub struct CircuitBreaker {
    threshold: u32,
    cooldown: Duration,
    state: Mutex<State>,
}

impl CircuitBreaker {
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            threshold: threshold.max(1),
            cooldown,
            state: Mutex::new(State::Closed { failures: 0 }),
        }
    }

    /// Ask to make a call. `Err(wait)` means the circuit refuses it.
    pub fn acquire(&self, now: Instant) -> Result<(), Duration> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match *s {
            State::Closed { .. } => Ok(()),
            State::Open { until } if now >= until => {
                *s = State::HalfOpen { probing: true };
                Ok(())
            }
            State::Open { until } => Err(until - now),
            State::HalfOpen { probing: false } => {
                *s = State::HalfOpen { probing: true };
                Ok(())
            }
            State::HalfOpen { probing: true } => Err(Duration::from_millis(500)),
        }
    }

    pub fn on_success(&self) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = State::Closed { failures: 0 };
    }

    pub fn on_failure(&self, now: Instant) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *s = match *s {
            State::Closed { failures } if failures + 1 >= self.threshold => State::Open {
                until: now + self.cooldown,
            },
            State::Closed { failures } => State::Closed {
                failures: failures + 1,
            },
            State::HalfOpen { .. } | State::Open { .. } => State::Open {
                until: now + self.cooldown,
            },
        };
    }

    pub fn is_open(&self) -> bool {
        !matches!(
            *self.state.lock().unwrap_or_else(|e| e.into_inner()),
            State::Closed { .. }
        )
    }
}

/// One breaker per tenant, created on first use.
pub struct Breakers {
    threshold: u32,
    cooldown: Duration,
    map: Mutex<HashMap<String, Arc<CircuitBreaker>>>,
}

impl Breakers {
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            threshold,
            cooldown,
            map: Mutex::new(HashMap::new()),
        }
    }

    pub fn for_tenant(&self, tenant: &str) -> Arc<CircuitBreaker> {
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        m.entry(tenant.to_string())
            .or_insert_with(|| Arc::new(CircuitBreaker::new(self.threshold, self.cooldown)))
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_after_threshold_and_recovers() {
        let t0 = Instant::now();
        let b = CircuitBreaker::new(3, Duration::from_secs(10));
        for _ in 0..3 {
            assert!(b.acquire(t0).is_ok());
            b.on_failure(t0);
        }
        assert!(b.is_open());
        assert!(b.acquire(t0 + Duration::from_secs(5)).is_err());
        // cooldown over: one probe allowed, a second caller is refused
        let t1 = t0 + Duration::from_secs(11);
        assert!(b.acquire(t1).is_ok());
        assert!(b.acquire(t1).is_err());
        b.on_success();
        assert!(!b.is_open());
        assert!(b.acquire(t1).is_ok());
    }

    #[test]
    fn failed_probe_reopens() {
        let t0 = Instant::now();
        let b = CircuitBreaker::new(1, Duration::from_secs(10));
        b.on_failure(t0);
        let t1 = t0 + Duration::from_secs(11);
        assert!(b.acquire(t1).is_ok());
        b.on_failure(t1);
        assert!(b.acquire(t1 + Duration::from_secs(1)).is_err());
    }

    #[test]
    fn success_resets_failure_count() {
        let t0 = Instant::now();
        let b = CircuitBreaker::new(2, Duration::from_secs(10));
        b.on_failure(t0);
        b.on_success();
        b.on_failure(t0);
        assert!(!b.is_open());
    }
}
