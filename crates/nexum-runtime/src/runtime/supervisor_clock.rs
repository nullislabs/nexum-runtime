//! The supervisor's own `now()` source. Guest-visible time is
//! [`WasiClockOverride`](crate::supervisor::WasiClockOverride), a separate
//! seam: neither drives the other.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Host-backed by default. [`manual`](Self::manual) pins the instant so a
/// test drives poison-window and restart-backoff timing with
/// [`advance`](Self::advance) instead of real sleeps. Clones share one
/// timeline.
#[derive(Clone, Debug, Default)]
pub struct SupervisorClock(Option<Arc<Mutex<Instant>>>);

impl SupervisorClock {
    /// Pinned at the construction instant; only [`advance`](Self::advance)
    /// moves it.
    pub fn manual() -> Self {
        Self(Some(Arc::new(Mutex::new(Instant::now()))))
    }

    pub fn now(&self) -> Instant {
        match &self.0 {
            None => Instant::now(),
            Some(pinned) => *pinned.lock().unwrap_or_else(PoisonError::into_inner),
        }
    }

    /// Panics on the host-backed default: a test advancing a clock it does
    /// not control would otherwise wait real wall-clock time.
    pub fn advance(&self, by: Duration) {
        let pinned = self
            .0
            .as_ref()
            .expect("advance on a host-backed SupervisorClock: build it with manual()");
        let mut now = pinned.lock().unwrap_or_else(PoisonError::into_inner);
        *now = now
            .checked_add(by)
            .expect("SupervisorClock advanced past the Instant range");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_host_backed() {
        assert!(SupervisorClock::default().0.is_none());
    }

    #[test]
    fn manual_is_pinned_until_advanced() {
        let clock = SupervisorClock::manual();
        let t0 = clock.now();
        assert_eq!(clock.now(), t0);
        clock.advance(Duration::from_secs(7));
        assert_eq!(clock.now(), t0 + Duration::from_secs(7));
    }

    #[test]
    fn clones_share_one_timeline() {
        let a = SupervisorClock::manual();
        let b = a.clone();
        a.advance(Duration::from_millis(250));
        assert_eq!(b.now(), a.now());
    }

    #[test]
    #[should_panic(expected = "host-backed SupervisorClock")]
    fn advance_on_the_host_clock_panics() {
        SupervisorClock::default().advance(Duration::from_secs(1));
    }
}
