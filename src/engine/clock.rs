use std::time::Duration;

use thiserror::Error;

use super::ntp::{sample_any_offset, unix_millis, NTP_HOSTS};

/// Above this the clock is not trustworthy enough to fire on. Refused in either
/// direction: firing early is refused by the contract, and firing late loses the
/// slot to whoever did not.
pub const MAX_DRIFT_MS: i64 = 250;

// `sleep_until` and its constants land in P5, when `mint` has a deadline to wait
// for. They are here now because the timing rules they encode were measured, and
// re-deriving them later is how a port loses the thing it was ported for.
#[allow(dead_code)]
/// How long the last stretch is spent spinning rather than sleeping.
///
/// Measured in the TypeScript build: with a 30 ms window the spin still exits
/// within four microseconds at p95, while a two second window burned a whole
/// core and blocked everything else for two seconds. Fifty is generous.
const SPIN_WINDOW_MS: i64 = 50;

/// The longest single sleep. Short enough that one late wake is corrected on the
/// next pass rather than absorbed, long enough that waiting an hour for a drop
/// is not fourteen thousand wakeups.
#[allow(dead_code)]
const SLEEP_STEP_MS: i64 = 250;

#[derive(Debug, Error)]
pub enum ClockError {
    #[error("no time source could be reached, so the clock cannot be trusted")]
    Unreachable,
    #[error("clock drift is {0} ms, over the {MAX_DRIFT_MS} ms limit")]
    Drift(i64),
}

#[derive(Debug)]
pub struct Clock {
    offset_ms: i64,
    healthy: bool,
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            offset_ms: 0,
            healthy: false,
        }
    }

    /// A clock with a known offset, for tests and for a caller that has already
    /// measured one.
    #[allow(dead_code)]
    #[must_use]
    pub const fn with_offset(offset_ms: i64) -> Self {
        Self {
            offset_ms,
            healthy: true,
        }
    }

    /// Deliberately swallows a failure rather than returning it. A time source
    /// that cannot be reached is not a crash, it is a refusal to fire, and
    /// `assert_usable` is what reports it at the moment it matters.
    pub async fn sync(&mut self) {
        match sample_any_offset(&NTP_HOSTS, Duration::from_secs(2)).await {
            Ok(offset) => {
                self.offset_ms = offset;
                self.healthy = true;
            }
            Err(_) => self.healthy = false,
        }
    }

    #[must_use]
    pub const fn drift_ms(&self) -> i64 {
        self.offset_ms
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn now_ms(&self) -> i64 {
        unix_millis() + self.offset_ms
    }

    pub const fn assert_usable(&self) -> Result<(), ClockError> {
        if !self.healthy {
            return Err(ClockError::Unreachable);
        }
        if self.offset_ms.abs() > MAX_DRIFT_MS {
            return Err(ClockError::Drift(self.offset_ms));
        }
        Ok(())
    }

    /// Waits until the given Unix millisecond, correcting for drift.
    ///
    /// Wakes repeatedly rather than once. A single sleep that overshoots its
    /// window misses the deadline with nothing to catch it; re-checking every
    /// step turns overshoot into something corrected rather than absorbed. The
    /// last stretch is a spin, because an async timer resolves to milliseconds
    /// and a millisecond is ten blocks on this chain.
    #[allow(dead_code)]
    pub async fn sleep_until(&self, unix_ms: i64) {
        loop {
            let remaining = unix_ms - self.now_ms();
            if remaining <= SPIN_WINDOW_MS {
                break;
            }
            let step = remaining.saturating_sub(SPIN_WINDOW_MS).min(SLEEP_STEP_MS);
            tokio::time::sleep(Duration::from_millis(u64::try_from(step).unwrap_or(0))).await;
        }
        while self.now_ms() < unix_ms {
            std::hint::spin_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unsynced_clock_refuses_rather_than_guessing() {
        assert!(matches!(
            Clock::new().assert_usable(),
            Err(ClockError::Unreachable)
        ));
    }

    #[test]
    fn a_clock_inside_the_limit_is_usable() {
        assert!(Clock::with_offset(120).assert_usable().is_ok());
        assert!(Clock::with_offset(-120).assert_usable().is_ok());
    }

    /// Refused in both directions. Being ahead is not safer than being behind:
    /// the contract refuses an early mint and the drop is lost either way.
    #[test]
    fn drift_is_refused_in_either_direction() {
        assert!(matches!(
            Clock::with_offset(300).assert_usable(),
            Err(ClockError::Drift(300))
        ));
        assert!(matches!(
            Clock::with_offset(-300).assert_usable(),
            Err(ClockError::Drift(-300))
        ));
    }

    #[test]
    fn the_offset_moves_the_clock() {
        let ahead = Clock::with_offset(5_000);
        let delta = ahead.now_ms() - super::super::ntp::unix_millis();
        assert!(
            (4_990..=5_010).contains(&delta),
            "offset not applied: {delta}"
        );
    }

    #[tokio::test]
    async fn sleeping_until_a_past_moment_returns_at_once() {
        let clock = Clock::with_offset(0);
        let started = std::time::Instant::now();
        clock.sleep_until(clock.now_ms() - 1_000).await;
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn sleeping_lands_on_the_deadline_rather_than_near_it() {
        let clock = Clock::with_offset(0);
        let target = clock.now_ms() + 120;
        clock.sleep_until(target).await;
        let overshoot = clock.now_ms() - target;
        // The spin is what buys this. A bare timer would routinely be several
        // milliseconds late, which is tens of blocks on this chain.
        assert!(
            (0..=5).contains(&overshoot),
            "landed {overshoot} ms off the deadline"
        );
    }
}
