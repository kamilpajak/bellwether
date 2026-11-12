//! The point-in-time contract, as types: **valid time** is when the world published an item and
//! **observation time** is when we could first have known it. The retrieval leak that forced the
//! split is memo section 1.2; how far the split reaches, and where it stops, are
//! `docs/design/architecture.md` section 8 row R2.
//! [`Visibility::visible_at`] is the single query and requires both instants.
//!
//! The two instants cannot be compared with each other, because they are different types:
//!
//! ```compile_fail
//! use newsvault::pit::{ObservationTime, ValidTime};
//! # use chrono::{TimeZone, Utc};
//! let v = ValidTime::new(Utc.timestamp_opt(0, 0).single().unwrap());
//! let o = ObservationTime::new(Utc.timestamp_opt(1, 0).single().unwrap());
//! let _ = v < o; // mismatched types
//! ```
//!
//! **The positive control for the block above.** A `compile_fail` test passes whenever its snippet
//! fails to compile for *any* reason, so on its own it is a test of "this does not compile" rather
//! than of "these cannot be compared". This second doc test carries the same `use` line and the
//! legal form of the comparison, so a rename, a moved path or a broken import turns it RED while
//! the `compile_fail` one above would stay green. Measured 2026-11-12 on a copy of this tree:
//! replacing the compared line with a call to a function that does not exist left the
//! `compile_fail` test reporting `ok`, and renaming the module left it reporting `ok` too while
//! this block failed with `error[E0432]: unresolved import`. A `compile_fail,E0308` annotation does
//! not close that hole — measured the same day, rustdoc reports `ok` for a snippet that fails with
//! a different error code than the one declared.
//!
//! ```
//! use newsvault::pit::{ObservationTime, ValidTime};
//! # use chrono::{TimeZone, Utc};
//! let v = ValidTime::new(Utc.timestamp_opt(0, 0).single().unwrap());
//! let o = ObservationTime::new(Utc.timestamp_opt(1, 0).single().unwrap());
//! assert!(v.inner() < o.inner()); // the two instants compare only through `inner()`
//! ```

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// When the world published an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ValidTime(DateTime<Utc>);

/// When we could first have known an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObservationTime(DateTime<Utc>);

/// The instant an analysis claims to stand at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Cutoff(DateTime<Utc>);

impl ValidTime {
    #[must_use]
    pub const fn new(at: DateTime<Utc>) -> Self {
        Self(at)
    }
    #[must_use]
    pub const fn inner(self) -> DateTime<Utc> {
        self.0
    }
}

impl ObservationTime {
    #[must_use]
    pub const fn new(at: DateTime<Utc>) -> Self {
        Self(at)
    }
    #[must_use]
    pub const fn inner(self) -> DateTime<Utc> {
        self.0
    }
}

impl Cutoff {
    #[must_use]
    pub const fn new(at: DateTime<Utc>) -> Self {
        Self(at)
    }
    #[must_use]
    pub const fn inner(self) -> DateTime<Utc> {
        self.0
    }

    fn admits_valid(self, t: ValidTime) -> bool {
        t.0 <= self.0
    }

    fn admits_observation(self, t: ObservationTime) -> bool {
        t.0 <= self.0
    }
}

/// How long after publication an item became visible to us, and the probe that established it.
/// [`Visibility::from_backfill`] is the only way to date a backfilled item and it takes one of
/// these, so the type cannot be skipped; that the measurement still can is section 8 row R11.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasuredLag {
    /// Identifier of the probe run that measured this lag.
    pub probe_id: String,
    /// How many documents the probe checked.
    pub probes: u32,
    /// How many of them turned out not to have been visible at the claimed instant.
    pub leaks: u32,
    /// The lag itself, in whole seconds.
    pub seconds: i64,
}

impl MeasuredLag {
    /// Fraction of probes that leaked, `None` with no probes. A non-zero rate is still usable;
    /// the rate travels with the lag so a panel can disclose it.
    #[must_use]
    pub fn leak_rate(&self) -> Option<f64> {
        if self.probes == 0 {
            return None;
        }
        Some(f64::from(self.leaks) / f64::from(self.probes))
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PitError {
    #[error(
        "observation time {observed} precedes valid time {valid}: nothing is seen before it exists"
    )]
    ObservedBeforeValid {
        valid: DateTime<Utc>,
        observed: DateTime<Utc>,
    },
    #[error("measured lag {0} s is negative")]
    NegativeLag(i64),
}

/// The pair of instants that decides whether an item may enter an analysis.
///
/// Deserialisation goes through [`WireWindow`] rather than straight onto the fields, so the file
/// path runs the same ordering check as [`Visibility::from_live`]: section 4 makes vault-to-derive a
/// FILE boundary and section 3.10.2 specifies a JSON stream carrying both instants, so serde is
/// that consumer's normal constructor and not an exception.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "WireWindow")]
pub struct Visibility {
    valid: ValidTime,
    observed: ObservationTime,
}

/// The two instants as they travel on the wire, with no claim about their order. It exists so that
/// [`Visibility`] has no deserialisation path that skips the check: a derived `Deserialize` over
/// the private fields accepted an item observed before it was published, with a negative
/// [`Visibility::lag`] - measured 2026-11-12 from outside the crate, and what section 5's fourth
/// row forbids.
#[derive(Deserialize)]
struct WireWindow {
    valid: ValidTime,
    observed: ObservationTime,
}

impl TryFrom<WireWindow> for Visibility {
    type Error = PitError;

    /// The same check as `from_live`, because the invariant is about the pair and not about how the
    /// pair was produced: a backfilled item's observation is its publication plus a non-negative
    /// lag, so it satisfies this too.
    fn try_from(wire: WireWindow) -> Result<Self, Self::Error> {
        Self::from_live(wire.valid, wire.observed)
    }
}

impl Visibility {
    /// An item recorded by a live ingest run: observation time is the run's instant.
    pub fn from_live(valid: ValidTime, run_at: ObservationTime) -> Result<Self, PitError> {
        if run_at.0 < valid.0 {
            return Err(PitError::ObservedBeforeValid {
                valid: valid.0,
                observed: run_at.0,
            });
        }
        Ok(Self {
            valid,
            observed: run_at,
        })
    }

    /// An item recovered from a vendor index after the fact: observation time is its publication
    /// time plus a lag somebody measured.
    pub fn from_backfill(valid: ValidTime, lag: &MeasuredLag) -> Result<Self, PitError> {
        if lag.seconds < 0 {
            return Err(PitError::NegativeLag(lag.seconds));
        }
        Ok(Self {
            valid,
            observed: ObservationTime(valid.0 + Duration::seconds(lag.seconds)),
        })
    }

    /// The only visibility query. Both instants must be at or before the cutoff.
    #[must_use]
    pub fn visible_at(&self, cutoff: Cutoff) -> bool {
        cutoff.admits_valid(self.valid) && cutoff.admits_observation(self.observed)
    }

    #[must_use]
    pub const fn valid(&self) -> ValidTime {
        self.valid
    }

    #[must_use]
    pub const fn observed(&self) -> ObservationTime {
        self.observed
    }

    #[must_use]
    pub fn lag(&self) -> Duration {
        self.observed.0 - self.valid.0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().unwrap()
    }

    fn lag(seconds: i64) -> MeasuredLag {
        MeasuredLag {
            probe_id: "pplx-2026-11-11".to_owned(),
            probes: 69,
            leaks: 0,
            seconds,
        }
    }

    #[test]
    fn live_item_is_visible_once_both_instants_have_passed() {
        let v =
            Visibility::from_live(ValidTime::new(at(100)), ObservationTime::new(at(300))).unwrap();
        assert!(!v.visible_at(Cutoff::new(at(99))), "before publication");
        assert!(
            !v.visible_at(Cutoff::new(at(200))),
            "published but not yet observed by us"
        );
        assert!(v.visible_at(Cutoff::new(at(300))), "at the observation");
        assert!(v.visible_at(Cutoff::new(at(301))));
    }

    #[test]
    fn publication_alone_does_not_make_an_item_visible() {
        // The 60.2% leak as a test: the publication filter alone admits an item we could not have
        // read yet.
        let v =
            Visibility::from_live(ValidTime::new(at(0)), ObservationTime::new(at(86_400))).unwrap();
        let cutoff = Cutoff::new(at(3_600));
        assert!(cutoff.admits_valid(v.valid()), "publication test passes");
        assert!(!v.visible_at(cutoff), "and the item is still not visible");
    }

    #[test]
    fn observation_cannot_precede_publication() {
        let err = Visibility::from_live(ValidTime::new(at(500)), ObservationTime::new(at(400)))
            .unwrap_err();
        assert_eq!(
            err,
            PitError::ObservedBeforeValid {
                valid: at(500),
                observed: at(400)
            }
        );
    }

    #[test]
    fn backfilled_item_is_not_visible_on_its_publication_day() {
        let two_days = 2 * 24 * 3_600;
        let v = Visibility::from_backfill(ValidTime::new(at(0)), &lag(two_days)).unwrap();
        assert!(
            !v.visible_at(Cutoff::new(at(3_600))),
            "a backfilled item must not claim same-day visibility"
        );
        assert!(v.visible_at(Cutoff::new(at(two_days))));
        assert_eq!(v.lag(), Duration::seconds(two_days));
    }

    /// The file path is a constructor too. Before the `try_from` attribute the derived
    /// `Deserialize` wrote straight onto the private fields, so this pair arrived with a negative
    /// lag while `from_live` refused it.
    #[test]
    fn deserialising_an_item_observed_before_it_was_published_is_refused() {
        let backwards = r#"{"valid":"2026-07-01T12:00:00Z","observed":"2026-06-01T00:00:00Z"}"#;
        let why = serde_json::from_str::<Visibility>(backwards).unwrap_err();
        assert!(
            why.to_string().contains("nothing is seen before it exists"),
            "{why}"
        );
    }

    /// The other half of the same check: a well-ordered pair still deserialises, and to the same
    /// value the constructor gives.
    #[test]
    fn deserialising_a_well_ordered_pair_gives_what_the_constructor_gives() {
        let forwards = r#"{"valid":"2026-06-01T00:00:00Z","observed":"2026-07-01T12:00:00Z"}"#;
        let read: Visibility = serde_json::from_str(forwards).unwrap();
        let built = Visibility::from_live(
            ValidTime::new("2026-06-01T00:00:00Z".parse().unwrap()),
            ObservationTime::new("2026-07-01T12:00:00Z".parse().unwrap()),
        )
        .unwrap();
        assert_eq!(read, built);
        assert!(read.lag() > Duration::zero());
    }

    #[test]
    fn negative_lag_is_rejected() {
        let err = Visibility::from_backfill(ValidTime::new(at(0)), &lag(-1)).unwrap_err();
        assert_eq!(err, PitError::NegativeLag(-1));
    }

    #[test]
    fn leak_rate_is_reported_and_is_none_without_probes() {
        assert_eq!(lag(0).leak_rate(), Some(0.0));
        let mut l = lag(0);
        l.leaks = 42;
        l.probes = 69;
        assert!((l.leak_rate().unwrap() - 42.0 / 69.0).abs() < 1e-12);
        l.probes = 0;
        assert_eq!(l.leak_rate(), None);
    }
}
