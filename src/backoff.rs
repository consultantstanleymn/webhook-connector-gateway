//! Exponential backoff with equal jitter.
//!
//! The ceiling doubles with each attempt up to a cap. The delay is then
//! chosen between half the ceiling and the ceiling. Jitter spreads retries
//! so many failed events do not hit the downstream at the same moment.
//! The jitter source is a parameter, so the logic is pure and testable.

#[derive(Debug, Clone, Copy)]
pub struct BackoffPolicy {
    pub base_ms: u64,
    pub max_ms: u64,
}

impl BackoffPolicy {
    /// Upper bound for the delay after failed attempt `attempt` (1 based).
    pub fn ceiling_ms(&self, attempt: u32) -> u64 {
        let shift = attempt.saturating_sub(1).min(62);
        self.base_ms.saturating_mul(1u64 << shift).min(self.max_ms)
    }

    /// Delay in ms. `unit` is a random number in `[0, 1]`.
    pub fn delay_ms(&self, attempt: u32, unit: f64) -> u64 {
        let ceil = self.ceiling_ms(attempt);
        let half = ceil / 2;
        let unit = if unit.is_nan() {
            0.0
        } else {
            unit.clamp(0.0, 1.0)
        };
        half + (((ceil - half) as f64) * unit) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const P: BackoffPolicy = BackoffPolicy {
        base_ms: 500,
        max_ms: 60_000,
    };

    #[test]
    fn doubles_then_caps() {
        let c: Vec<u64> = (1..=9).map(|a| P.ceiling_ms(a)).collect();
        assert_eq!(
            c,
            vec![500, 1000, 2000, 4000, 8000, 16000, 32000, 60000, 60000]
        );
    }

    #[test]
    fn jitter_endpoints() {
        assert_eq!(P.delay_ms(3, 0.0), 1000);
        assert_eq!(P.delay_ms(3, 1.0), 2000);
    }

    proptest! {
        #[test]
        fn delay_within_bounds(base in 1u64..10_000, max in 1u64..1_000_000,
                               attempt in 0u32..u32::MAX, unit in -1.0f64..2.0) {
            let p = BackoffPolicy { base_ms: base, max_ms: max };
            let d = p.delay_ms(attempt, unit);
            let c = p.ceiling_ms(attempt);
            prop_assert!(d <= c);
            prop_assert!(d >= c / 2);
            prop_assert!(c <= max);
        }

        #[test]
        fn ceiling_never_decreases(base in 1u64..10_000, max in 1u64..1_000_000, a in 1u32..200) {
            let p = BackoffPolicy { base_ms: base, max_ms: max };
            prop_assert!(p.ceiling_ms(a + 1) >= p.ceiling_ms(a));
        }
    }
}
