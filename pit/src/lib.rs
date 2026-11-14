//! The point-in-time contract, as types: **valid time** is when the world published an item and
//! **observation time** is when we could first have known it. The retrieval leak that forced the
//! split is memo section 1.2; how far the split reaches, and where it stops, are
//! `docs/design/architecture.md` section 8 row R2.
//! [`Visibility::visible_at`] is the single query and requires both instants.
//!
//! The two instants cannot be compared with each other, because they are different types:
//!
//! ```compile_fail
//! use pit::{ObservationTime, ValidTime};
//! # use chrono::{TimeZone, Utc};
//! let v = ValidTime::new(Utc.timestamp_opt(0, 0).single().unwrap());
//! let o = ObservationTime::new(Utc.timestamp_opt(1, 0).single().unwrap());
//! let _ = v < o; // mismatched types
//! ```
//!
//! **The positive control for the block above.** A `compile_fail` test passes whenever its snippet
//! fails to compile for *any* reason, so on its own it is a test of "this does not compile" rather
//! than of "these cannot be compared". This second doc test carries the same `use` line and the
//! legal form of the comparison, so a moved path or a broken import turns it RED while the
//! `compile_fail` one above would stay green. The measurements are section 8 row R2.
//!
//! ```
//! use pit::{ObservationTime, ValidTime};
//! # use chrono::{TimeZone, Utc};
//! let v = ValidTime::new(Utc.timestamp_opt(0, 0).single().unwrap());
//! let o = ObservationTime::new(Utc.timestamp_opt(1, 0).single().unwrap());
//! assert!(v.inner() < o.inner()); // the two instants compare only through `inner()`
//! ```

use chrono::{DateTime, Duration, Timelike, Utc};
use serde::{Deserialize, Serialize};

pub mod datedness;

/// When the world published an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ValidTime(DateTime<Utc>);

/// When we could first have known an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObservationTime(DateTime<Utc>);

/// A vendor's own claim about when IT saw an item. Never an [`ObservationTime`] on its own:
/// [`ObservationTime::floor_from_vendor`] is the only route from one to the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SeenTime(DateTime<Utc>);

/// The instant an analysis claims to stand at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Cutoff(DateTime<Utc>);

impl ValidTime {
    /// Truncated to a whole microsecond, because section 2's canonical form renders `%.6f` and two
    /// instants 1 ns apart would then digest the same while the stored column kept them apart.
    #[must_use]
    pub fn new(at: DateTime<Utc>) -> Self {
        Self(truncate_to_microsecond(at))
    }
    #[must_use]
    pub const fn inner(self) -> DateTime<Utc> {
        self.0
    }
}

impl SeenTime {
    /// Truncated like [`ValidTime::new`], so that the ordering check in
    /// [`Visibility::from_live`] compares two values at the precision the store keeps.
    #[must_use]
    pub fn new(at: DateTime<Utc>) -> Self {
        Self(truncate_to_microsecond(at))
    }
    #[must_use]
    pub const fn inner(self) -> DateTime<Utc> {
        self.0
    }
}

impl ObservationTime {
    /// Truncated like [`ValidTime::new`]. Truncating only one of the two instants made
    /// [`Visibility::from_live`] accept an observation 300 ns BEFORE its publication and report a
    /// positive lag for it - found by review 2026-11-13, before this crate had a second caller.
    /// An inversion smaller than one microsecond is now accepted as simultaneous, because the
    /// stored columns cannot tell the two instants apart; a whole microsecond is still refused.
    #[must_use]
    pub fn new(at: DateTime<Utc>) -> Self {
        Self(truncate_to_microsecond(at))
    }

    /// A vendor's claim about when IT saw an item can never move our observation earlier than the
    /// process that read it. `run_at` has no default, so the call does not compile without it.
    #[must_use]
    pub fn floor_from_vendor(seen: SeenTime, run_at: ObservationTime) -> Self {
        Self(seen.0.max(run_at.0))
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

    /// Redundant given both [`Visibility`] constructors, which refuse an observation before its
    /// publication, so `observed <= cutoff` already implies this. Kept because the claim the
    /// project makes is that the query asks both halves, and a reader of `visible_at` should see
    /// both halves asked rather than have to re-derive the implication.
    fn admits_valid(self, t: ValidTime) -> bool {
        t.0 <= self.0
    }

    pub(crate) fn admits_observation(self, t: ObservationTime) -> bool {
        t.0 <= self.0
    }
}

const NANOS_PER_MICRO: u32 = 1_000;

/// chrono carries a leap second's extra second in the nanosecond field, so the whole subsecond
/// field is truncated rather than its nanosecond part; `with_nanosecond` refuses only >= 2e9.
fn truncate_to_microsecond(at: DateTime<Utc>) -> DateTime<Utc> {
    at.with_nanosecond(at.timestamp_subsec_micros() * NANOS_PER_MICRO)
        .unwrap_or(at)
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
    #[error("valid time {valid} plus a lag of {seconds} s is past the last instant chrono holds")]
    LagOutOfRange { valid: DateTime<Utc>, seconds: i64 },
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
        let out_of_range = PitError::LagOutOfRange {
            valid: valid.0,
            seconds: lag.seconds,
        };
        let observed = Duration::try_seconds(lag.seconds)
            .and_then(|after| valid.0.checked_add_signed(after))
            .ok_or(out_of_range)?;
        Ok(Self {
            valid,
            observed: ObservationTime(observed),
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

    /// The two doc blocks in this file's header are section 8 row R2's whole enforcement point.
    const DOC_IMPORT: &str = "use pit::{ObservationTime, ValidTime};";

    /// One microsecond, in nanoseconds.
    const MICRO_NANOS: u32 = 1_000;

    fn utc(seconds: i64, nanos: u32) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, nanos)
            .single()
            .expect("a valid instant")
    }

    /// The guarded direction is a vendor claiming it saw an item BEFORE our run; the other
    /// direction is a vendor that saw it later, and that instant is the one that stands.
    /// Mutation 2026-11-13 replaced the maximum with `run_at` alone and nothing was red.
    #[test]
    fn a_vendor_that_saw_an_item_after_our_run_moves_our_observation_later() {
        let run_at = ObservationTime::new(utc(1_000, 0));
        let seen = SeenTime::new(utc(2_000, 0));
        assert_eq!(
            ObservationTime::floor_from_vendor(seen, run_at).inner(),
            seen.inner()
        );
    }

    #[test]
    fn an_observation_a_whole_microsecond_before_its_publication_is_still_refused() {
        let valid = ValidTime::new(utc(1_000, MICRO_NANOS));
        let observed = ObservationTime::new(utc(1_000, 0));
        assert_eq!(
            Visibility::from_live(valid, observed),
            Err(PitError::ObservedBeforeValid {
                valid: valid.inner(),
                observed: observed.inner(),
            })
        );
    }

    /// Both instants truncate, so a lag the stored columns cannot represent is reported as zero
    /// rather than as 300 ns. Truncating only the publication instant reported the 300 ns.
    #[test]
    fn a_lag_smaller_than_one_microsecond_is_reported_as_zero_and_not_as_nanoseconds() {
        let valid = ValidTime::new(utc(1_000, 0));
        let observed = ObservationTime::new(utc(1_000, 300));
        match Visibility::from_live(valid, observed) {
            Ok(v) => assert_eq!(v.lag(), Duration::zero()),
            Err(error) => assert_eq!(format!("{error:?}"), "an accepted visibility"),
        }
    }
    const COMPILE_FAIL: &str = "compile_fail";
    const COMPILE_FAIL_BLOCK: usize = 0;
    const POSITIVE_CONTROL_BLOCK: usize = 1;
    const VENDOR_SEEN_AT: &str = "2026-07-01T10:00:00Z";
    const THE_RUN_THAT_READ_IT: &str = "2026-07-02T00:34:00Z";
    const WHOLE_MICROSECOND: &str = "2026-07-01T12:00:00.000001Z";
    const ONE_NANOSECOND_LATER: &str = "2026-07-01T12:00:00.000001001Z";

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().unwrap()
    }

    fn instant(text: &str) -> DateTime<Utc> {
        match text.parse::<DateTime<Utc>>() {
            Ok(parsed) => parsed,
            Err(_) => DateTime::<Utc>::UNIX_EPOCH,
        }
    }

    /// The header's fenced blocks, in order. The fence is assembled rather than written out, so
    /// this function's own text does not split the file it reads.
    fn doc_blocks() -> Vec<String> {
        let fence = format!("//! {}", "```");
        include_str!("lib.rs")
            .split(&fence)
            .skip(1)
            .step_by(2)
            .map(str::to_owned)
            .collect()
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

    /// `i64::MAX / 2` is more seconds than a `TimeDelta` holds; `10^15` fits in one and still
    /// carries 2026 past year 262,143. Unchecked, both panic inside chrono 0.4.45.
    #[test]
    fn a_lag_past_the_last_representable_instant_is_refused_rather_than_panicking() {
        let valid = ValidTime::new(utc(1_762_810_335, 0));
        for seconds in [i64::MAX / 2, 1_000_000_000_000_000] {
            assert_eq!(
                Visibility::from_backfill(valid, &lag(seconds)),
                Err(PitError::LagOutOfRange {
                    valid: valid.inner(),
                    seconds,
                })
            );
        }
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

    #[test]
    fn a_vendor_seen_instant_cannot_move_our_observation_earlier_than_the_run_that_read_it() {
        let run_at = ObservationTime::new(instant(THE_RUN_THAT_READ_IT));
        let floored =
            ObservationTime::floor_from_vendor(SeenTime::new(instant(VENDOR_SEEN_AT)), run_at);
        assert_eq!(floored, run_at);
    }

    #[test]
    fn two_instants_one_nanosecond_apart_store_as_the_same_microsecond_they_digest_as() {
        let truncated = ValidTime::new(instant(ONE_NANOSECOND_LATER));
        assert_eq!(truncated, ValidTime::new(instant(WHOLE_MICROSECOND)));
        assert_eq!(truncated.inner(), instant(WHOLE_MICROSECOND));
    }

    /// A `compile_fail` block passes whenever its snippet stops compiling for ANY reason, so what
    /// these two pin is the import path: wrong in the control and the pair goes silently green.
    #[test]
    fn the_two_instants_still_cannot_be_compared_with_each_other_after_the_move() {
        let block = doc_blocks()
            .get(COMPILE_FAIL_BLOCK)
            .cloned()
            .unwrap_or_default();
        assert!(block.starts_with(COMPILE_FAIL), "{block}");
        assert!(block.contains(DOC_IMPORT), "{block}");
    }

    #[test]
    fn the_positive_control_for_the_compile_fail_block_imports_from_the_new_path() {
        let block = doc_blocks()
            .get(POSITIVE_CONTROL_BLOCK)
            .cloned()
            .unwrap_or_default();
        assert!(block.contains(DOC_IMPORT), "{block}");
    }
}
