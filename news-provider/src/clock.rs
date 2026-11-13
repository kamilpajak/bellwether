//! The one clock this seam reads, injected, and the one wait it takes. `Utc::now` is called here
//! and nowhere else in the crate: `Wire::send` is handed a `&dyn Clock` and no other method takes
//! one, so a provider cannot read an instant even by accident. Why the trait is its own module
//! rather than an exemption written into a grep: `docs/design/design.md` section 2.2.

use std::time::Duration;

use chrono::{DateTime, Utc};

pub trait Clock {
    fn now(&self) -> DateTime<Utc>;
}

/// The process clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Who pays a pace wait, injected for the reason the clock is: a suite that waited in real time
/// would spend `Pace`'s interval on every send. `Wire::send` is the only caller.
pub trait Sleeper: std::fmt::Debug {
    fn sleep(&self, how_long: Duration);
}

/// The process sleeper.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&self, how_long: Duration) {
        std::thread::sleep(how_long);
    }
}

/// The test clock and this module's own test, in ONE `#[cfg(test)]` item: the CI size step counts
/// the lines before a file's first `#[cfg(test)]` as source and the rest as test, so a file
/// holding several markers measures wrong. The guard is the size step's own.
#[cfg(test)]
pub(crate) mod testing {
    use super::{Clock, Sleeper, SystemClock, ThreadSleeper};
    use chrono::{DateTime, Utc};
    use std::time::Duration;

    /// A clock handing out a stated sequence, so a test can say which read got which instant.
    /// Shared between this crate's test modules rather than written twice.
    #[derive(Debug)]
    pub(crate) struct StepClock {
        instants: Vec<DateTime<Utc>>,
        read: std::cell::Cell<usize>,
    }

    impl StepClock {
        pub(crate) fn over(instants: &[&str]) -> Self {
            Self {
                instants: instants
                    .iter()
                    .map(|text| text.parse().unwrap_or(DateTime::UNIX_EPOCH))
                    .collect(),
                read: std::cell::Cell::new(0),
            }
        }

        pub(crate) fn reads(&self) -> usize {
            self.read.get()
        }
    }

    impl Clock for StepClock {
        /// Past the end of the sequence it keeps answering, with the epoch: a test that reads more
        /// instants than it stated fails on the value rather than on a panic.
        fn now(&self) -> DateTime<Utc> {
            let index = self.read.get();
            self.read.set(index + 1);
            self.instants
                .get(index)
                .copied()
                .unwrap_or(DateTime::UNIX_EPOCH)
        }
    }

    /// Every wait a test's `Wire` was asked for, in order, so "it waited" is a value and not a
    /// stopwatch. Cloned into the `Wire` while the test keeps the handle to read.
    #[derive(Debug, Clone, Default)]
    pub(crate) struct WaitRecorder(std::rc::Rc<std::cell::RefCell<Vec<Duration>>>);

    impl WaitRecorder {
        pub(crate) fn waits(&self) -> Vec<Duration> {
            self.0.borrow().clone()
        }
    }

    impl Sleeper for WaitRecorder {
        fn sleep(&self, how_long: Duration) {
            self.0.borrow_mut().push(how_long);
        }
    }

    /// Any instant safely in the past of every run of this suite.
    const BEFORE_THIS_PROJECT_EXISTED: &str = "2026-01-01T00:00:00Z";
    /// Short enough that the suite pays it once, long enough to outlive scheduler noise.
    const A_WAIT_THIS_SUITE_CAN_AFFORD: Duration = Duration::from_millis(10);

    /// `SystemClock` is the only production `Clock` and nothing constructed it, so
    /// `now()` returning `DateTime::UNIX_EPOCH` left the whole suite green - measured 2026-11-13.
    #[test]
    fn the_process_clock_answers_with_the_present_and_not_with_the_epoch() {
        let floor: DateTime<Utc> = BEFORE_THIS_PROJECT_EXISTED
            .parse()
            .unwrap_or(DateTime::UNIX_EPOCH);
        assert!(SystemClock.now() > floor);
    }

    /// The same hole as the clock's, one layer over: every pace test injects a recorder, so a
    /// `ThreadSleeper::sleep` with an empty body would leave the whole suite green and every real
    /// collect unpaced.
    #[test]
    fn the_process_sleeper_actually_waits() {
        let started = std::time::Instant::now();
        ThreadSleeper.sleep(A_WAIT_THIS_SUITE_CAN_AFFORD);
        assert!(started.elapsed() >= A_WAIT_THIS_SUITE_CAN_AFFORD);
    }
}
