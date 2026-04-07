// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

// https://aws.amazon.com/blogs/architecture/exponential-backoff-and-jitter/

use std::time::Duration;

use rand::thread_rng;
use rand::Rng;

pub const DEFAULT_REGION_BACKOFF: Backoff = Backoff::no_jitter_backoff(2, 500, 10);
pub const DEFAULT_STORE_BACKOFF: Backoff = Backoff::no_jitter_backoff(2, 1000, 10);
/// Lock resolution backoff for reads and writes encountering locks from
/// other transactions.  Uses a 20-second time budget (matching TiDB
/// client-go's `getMaxBackoff = 20000`) with exponential delays up to 3s.
///
/// The old config (2ms base, 500ms cap, 10 attempts = ~1.5s total) was
/// too aggressive — prewrite locks from concurrent 2PC could outlast the
/// budget, causing spurious `ResolveLockError` on plain SELECTs (#2156).
pub const OPTIMISTIC_BACKOFF: Backoff = Backoff::no_jitter_backoff_with_budget(2, 3000, 20_000);
pub const PESSIMISTIC_BACKOFF: Backoff = Backoff::no_jitter_backoff_with_budget(2, 3000, 20_000);

/// When a request is retried, we can backoff for some time to avoid saturating the network.
///
/// `Backoff` is an object which determines how long to wait for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backoff {
    kind: BackoffKind,
    current_attempts: u32,
    max_attempts: u32,
    base_delay_ms: u64,
    current_delay_ms: u64,
    max_delay_ms: u64,
    /// Cumulative sleep time across all retries (for time-budget mode).
    cumulative_delay_ms: u64,
    /// Total time budget in ms.  0 = disabled (attempt-count only).
    /// When > 0, `next_delay_duration` returns None once the budget is
    /// exhausted, and individual delays are clamped to the remaining budget.
    max_total_ms: u64,
}

impl Backoff {
    // Returns the delay period for next retry.  Returns None when:
    // - max_attempts is reached (count-based mode), OR
    // - max_total_ms budget is exhausted (time-budget mode).
    pub fn next_delay_duration(&mut self) -> Option<Duration> {
        if self.current_attempts >= self.max_attempts {
            return None;
        }
        // Time-budget check: if we've already consumed the entire budget,
        // stop retrying.
        if self.max_total_ms > 0 && self.cumulative_delay_ms >= self.max_total_ms {
            return None;
        }
        self.current_attempts += 1;

        let raw_delay_ms = match self.kind {
            BackoffKind::None => return None,
            BackoffKind::NoJitter => {
                let delay_ms = self.max_delay_ms.min(self.current_delay_ms);
                self.current_delay_ms <<= 1;
                delay_ms
            }
            BackoffKind::FullJitter => {
                let delay_ms = self.max_delay_ms.min(self.current_delay_ms);
                let mut rng = thread_rng();
                let delay_ms: u64 = rng.gen_range(0..delay_ms);
                self.current_delay_ms <<= 1;
                delay_ms
            }
            BackoffKind::EqualJitter => {
                let delay_ms = self.max_delay_ms.min(self.current_delay_ms);
                let half_delay_ms = delay_ms >> 1;
                let mut rng = thread_rng();
                let delay_ms: u64 = rng.gen_range(0..half_delay_ms) + half_delay_ms;
                self.current_delay_ms <<= 1;
                delay_ms
            }
            BackoffKind::DecorrelatedJitter => {
                let mut rng = thread_rng();
                let delay_ms: u64 = rng
                    .gen_range(0..self.current_delay_ms * 3 - self.base_delay_ms)
                    + self.base_delay_ms;
                let delay_ms = delay_ms.min(self.max_delay_ms);
                self.current_delay_ms = delay_ms;
                delay_ms
            }
        };

        // Clamp to remaining budget if time-budget mode is active.
        let delay_ms = if self.max_total_ms > 0 {
            let remaining = self.max_total_ms.saturating_sub(self.cumulative_delay_ms);
            let clamped = raw_delay_ms.min(remaining);
            self.cumulative_delay_ms += clamped;
            clamped
        } else {
            raw_delay_ms
        };

        Some(Duration::from_millis(delay_ms))
    }

    /// True if we should not backoff at all (usually indicates that we should not retry a request).
    pub fn is_none(&self) -> bool {
        self.kind == BackoffKind::None
    }

    /// Returns the number of attempts
    pub fn current_attempts(&self) -> u32 {
        self.current_attempts
    }

    /// Don't wait. Usually indicates that we should not retry a request.
    pub const fn no_backoff() -> Backoff {
        Backoff {
            kind: BackoffKind::None,
            current_attempts: 0,
            max_attempts: 0,
            base_delay_ms: 0,
            current_delay_ms: 0,
            max_delay_ms: 0,
            cumulative_delay_ms: 0,
            max_total_ms: 0,
        }
    }

    // Exponential backoff means that the retry delay should multiply a constant
    // after each attempt, up to a maximum value. After each attempt, the new retry
    // delay should be:
    //
    // new_delay = min(max_delay, base_delay * 2 ** attempts)
    pub const fn no_jitter_backoff(
        base_delay_ms: u64,
        max_delay_ms: u64,
        max_attempts: u32,
    ) -> Backoff {
        Backoff {
            kind: BackoffKind::NoJitter,
            current_attempts: 0,
            max_attempts,
            base_delay_ms,
            current_delay_ms: base_delay_ms,
            max_delay_ms,
            cumulative_delay_ms: 0,
            max_total_ms: 0,
        }
    }

    /// Exponential backoff (no jitter) bounded by a total time budget.
    ///
    /// Unlike `no_jitter_backoff` which uses a fixed attempt count, this
    /// variant tracks cumulative sleep time and terminates when the budget
    /// is exhausted.  Matches TiDB client-go's `Backoffer` architecture.
    pub const fn no_jitter_backoff_with_budget(
        base_delay_ms: u64,
        max_delay_ms: u64,
        max_total_ms: u64,
    ) -> Backoff {
        Backoff {
            kind: BackoffKind::NoJitter,
            current_attempts: 0,
            max_attempts: u32::MAX,
            base_delay_ms,
            current_delay_ms: base_delay_ms,
            max_delay_ms,
            cumulative_delay_ms: 0,
            max_total_ms,
        }
    }

    // Adds Jitter to the basic exponential backoff. Returns a random value between
    // zero and the calculated exponential backoff:
    //
    // temp = min(max_delay, base_delay * 2 ** attempts)
    // new_delay = random_between(0, temp)
    pub fn full_jitter_backoff(
        base_delay_ms: u64,
        max_delay_ms: u64,
        max_attempts: u32,
    ) -> Backoff {
        assert!(
            base_delay_ms > 0 && max_delay_ms > 0,
            "Both base_delay_ms and max_delay_ms must be positive"
        );

        Backoff {
            kind: BackoffKind::FullJitter,
            current_attempts: 0,
            max_attempts,
            base_delay_ms,
            current_delay_ms: base_delay_ms,
            max_delay_ms,
            cumulative_delay_ms: 0,
            max_total_ms: 0,
        }
    }

    // Equal Jitter limits the random value should be equal or greater than half of
    // the calculated exponential backoff:
    //
    // temp = min(max_delay, base_delay * 2 ** attempts)
    // new_delay = random_between(temp / 2, temp)
    pub fn equal_jitter_backoff(
        base_delay_ms: u64,
        max_delay_ms: u64,
        max_attempts: u32,
    ) -> Backoff {
        assert!(
            base_delay_ms > 1 && max_delay_ms > 1,
            "Both base_delay_ms and max_delay_ms must be greater than 1"
        );

        Backoff {
            kind: BackoffKind::EqualJitter,
            current_attempts: 0,
            max_attempts,
            base_delay_ms,
            current_delay_ms: base_delay_ms,
            max_delay_ms,
            cumulative_delay_ms: 0,
            max_total_ms: 0,
        }
    }

    // Decorrelated Jitter is always calculated with the previous backoff
    // (the initial value is base_delay):
    //
    // temp = random_between(base_delay, previous_delay * 3)
    // new_delay = min(max_delay, temp)
    pub fn decorrelated_jitter_backoff(
        base_delay_ms: u64,
        max_delay_ms: u64,
        max_attempts: u32,
    ) -> Backoff {
        assert!(base_delay_ms > 0, "base_delay_ms must be positive");

        Backoff {
            kind: BackoffKind::DecorrelatedJitter,
            current_attempts: 0,
            max_attempts,
            base_delay_ms,
            current_delay_ms: base_delay_ms,
            max_delay_ms,
            cumulative_delay_ms: 0,
            max_total_ms: 0,
        }
    }
}

/// The pattern for computing backoff times.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BackoffKind {
    None,
    NoJitter,
    FullJitter,
    EqualJitter,
    DecorrelatedJitter,
}

#[cfg(test)]
mod test {
    use std::convert::TryInto;

    use super::*;

    #[test]
    fn test_no_jitter_backoff() {
        // Tests for zero attempts.
        let mut backoff = Backoff::no_jitter_backoff(0, 0, 0);
        assert_eq!(backoff.next_delay_duration(), None);

        let mut backoff = Backoff::no_jitter_backoff(2, 7, 3);
        assert_eq!(
            backoff.next_delay_duration(),
            Some(Duration::from_millis(2))
        );
        assert_eq!(
            backoff.next_delay_duration(),
            Some(Duration::from_millis(4))
        );
        assert_eq!(
            backoff.next_delay_duration(),
            Some(Duration::from_millis(7))
        );
        assert_eq!(backoff.next_delay_duration(), None);
    }

    #[test]
    fn test_full_jitter_backoff() {
        let mut backoff = Backoff::full_jitter_backoff(2, 7, 3);
        assert!(backoff.next_delay_duration().unwrap() <= Duration::from_millis(2));
        assert!(backoff.next_delay_duration().unwrap() <= Duration::from_millis(4));
        assert!(backoff.next_delay_duration().unwrap() <= Duration::from_millis(7));
        assert_eq!(backoff.next_delay_duration(), None);
    }

    #[test]
    #[should_panic(expected = "Both base_delay_ms and max_delay_ms must be positive")]
    fn test_full_jitter_backoff_with_invalid_base_delay_ms() {
        Backoff::full_jitter_backoff(0, 7, 3);
    }

    #[test]
    #[should_panic(expected = "Both base_delay_ms and max_delay_ms must be positive")]
    fn test_full_jitter_backoff_with_invalid_max_delay_ms() {
        Backoff::full_jitter_backoff(2, 0, 3);
    }

    #[test]
    fn test_equal_jitter_backoff() {
        let mut backoff = Backoff::equal_jitter_backoff(2, 7, 3);

        let first_delay_dur = backoff.next_delay_duration().unwrap();
        assert!(first_delay_dur >= Duration::from_millis(1));
        assert!(first_delay_dur <= Duration::from_millis(2));

        let second_delay_dur = backoff.next_delay_duration().unwrap();
        assert!(second_delay_dur >= Duration::from_millis(2));
        assert!(second_delay_dur <= Duration::from_millis(4));

        let third_delay_dur = backoff.next_delay_duration().unwrap();
        assert!(third_delay_dur >= Duration::from_millis(3));
        assert!(third_delay_dur <= Duration::from_millis(6));

        assert_eq!(backoff.next_delay_duration(), None);
    }

    #[test]
    #[should_panic(expected = "Both base_delay_ms and max_delay_ms must be greater than 1")]
    fn test_equal_jitter_backoff_with_invalid_base_delay_ms() {
        Backoff::equal_jitter_backoff(1, 7, 3);
    }

    #[test]
    #[should_panic(expected = "Both base_delay_ms and max_delay_ms must be greater than 1")]
    fn test_equal_jitter_backoff_with_invalid_max_delay_ms() {
        Backoff::equal_jitter_backoff(2, 1, 3);
    }

    #[test]
    fn test_decorrelated_jitter_backoff() {
        let mut backoff = Backoff::decorrelated_jitter_backoff(2, 7, 3);

        let first_delay_dur = backoff.next_delay_duration().unwrap();
        assert!(first_delay_dur >= Duration::from_millis(2));
        assert!(first_delay_dur <= Duration::from_millis(6));

        let second_delay_dur = backoff.next_delay_duration().unwrap();
        assert!(second_delay_dur >= Duration::from_millis(2));
        let cap_ms = 7u64.min((first_delay_dur.as_millis() * 3).try_into().unwrap());
        assert!(second_delay_dur <= Duration::from_millis(cap_ms));

        let third_delay_dur = backoff.next_delay_duration().unwrap();
        assert!(third_delay_dur >= Duration::from_millis(2));
        let cap_ms = 7u64.min((second_delay_dur.as_millis() * 3).try_into().unwrap());
        assert!(second_delay_dur <= Duration::from_millis(cap_ms));

        assert_eq!(backoff.next_delay_duration(), None);
    }

    #[test]
    #[should_panic(expected = "base_delay_ms must be positive")]
    fn test_decorrelated_jitter_backoff_with_invalid_base_delay_ms() {
        Backoff::decorrelated_jitter_backoff(0, 7, 3);
    }

    #[test]
    fn test_no_jitter_backoff_with_budget() {
        // Budget of 10ms, base=2, cap=3000
        let mut backoff = Backoff::no_jitter_backoff_with_budget(2, 3000, 10);
        // Attempt 1: delay=2, cumulative=2
        assert_eq!(
            backoff.next_delay_duration(),
            Some(Duration::from_millis(2))
        );
        // Attempt 2: delay=4, cumulative=6
        assert_eq!(
            backoff.next_delay_duration(),
            Some(Duration::from_millis(4))
        );
        // Attempt 3: raw=8, but remaining=4, clamped to 4, cumulative=10
        assert_eq!(
            backoff.next_delay_duration(),
            Some(Duration::from_millis(4))
        );
        // Budget exhausted
        assert_eq!(backoff.next_delay_duration(), None);
    }

    #[test]
    fn test_budget_backoff_total_does_not_exceed_budget() {
        let mut backoff = Backoff::no_jitter_backoff_with_budget(2, 3000, 20_000);
        let mut total_ms = 0u64;
        let mut count = 0u32;
        while let Some(d) = backoff.next_delay_duration() {
            total_ms += d.as_millis() as u64;
            count += 1;
        }
        assert!(total_ms <= 20_000, "total {total_ms}ms exceeds budget");
        assert!(
            total_ms >= 19_000,
            "total {total_ms}ms too far below budget"
        );
        assert!(count > 10, "should use more than 10 attempts, got {count}");
    }

    #[test]
    fn test_budget_backoff_zero_budget_is_count_based() {
        // max_total_ms=0 means disabled — falls back to attempt count
        let mut backoff = Backoff::no_jitter_backoff(2, 500, 3);
        assert!(backoff.next_delay_duration().is_some());
        assert!(backoff.next_delay_duration().is_some());
        assert!(backoff.next_delay_duration().is_some());
        assert_eq!(backoff.next_delay_duration(), None);
    }
}
