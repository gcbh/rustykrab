use rand::Rng;
use std::time::Duration;

/// Exponential backoff delay for retry `attempt` (1-based), with ±25%
/// jitter so concurrent clients retrying a struggling server don't all
/// hit it again at the same instant (thundering herd).
pub(crate) fn retry_delay(base: Duration, attempt: u32) -> Duration {
    let exp = base.saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1)));
    let jitter = rand::rng().random_range(0.75..=1.25);
    exp.mul_f64(jitter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_is_exponential_with_bounded_jitter() {
        let base = Duration::from_secs(1);
        for attempt in 1..=4 {
            let expected = base * 2u32.pow(attempt - 1);
            for _ in 0..100 {
                let d = retry_delay(base, attempt);
                assert!(
                    d >= expected.mul_f64(0.75),
                    "attempt {attempt}: {d:?} below jitter floor"
                );
                assert!(
                    d <= expected.mul_f64(1.25),
                    "attempt {attempt}: {d:?} above jitter ceiling"
                );
            }
        }
    }
}
