//! The precision-aware visibility arm. `docs/design/architecture.md` section 3.1's
//! `valid_precision` row says a `day`-precision time must not be assumed into a window, and
//! [`Datedness`] is where it stops being assumed: a date is carried as a date and never folded
//! into a fabricated midnight instant.

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::{Cutoff, ObservationTime, ValidTime, Visibility};

/// The publication value of an item. Comparable and printable, and **not** a [`ValidTime`], so it
/// cannot reach a cutoff: `Cutoff::admits_valid` takes a `ValidTime` and this is not one. That is
/// what stops a date being compared against an instant somewhere downstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Published {
    At(ValidTime),
    /// A date with no hour, in the calendar `tz` names.
    On {
        day: NaiveDate,
        tz: &'static str,
    },
    Unknown,
}

impl Published {
    /// The earliest instant this value could mean, for ORDERING and DISPLAY only. A date's bound
    /// is the first instant of that day in its own calendar, which is not a publication instant.
    #[must_use]
    pub fn lower_bound(self) -> Option<DateTime<Utc>> {
        match self {
            Self::At(valid) => Some(valid.inner()),
            Self::On { day, tz } => {
                let zone: Tz = tz.parse().ok()?;
                let starts = zone
                    .from_local_datetime(&day.and_time(NaiveTime::MIN))
                    .earliest()?;
                Some(starts.with_timezone(&Utc))
            }
            Self::Unknown => None,
        }
    }
}

impl std::fmt::Display for Published {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::At(valid) => write!(f, "{}", valid.inner()),
            Self::On { day, tz } => write!(f, "{day} ({tz} day)"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

/// What is known about when an item was published, which decides which predicate applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Datedness {
    /// Both instants known. Reuses [`Visibility`] unchanged.
    Dated(Visibility),
    /// A publication DATE with no hour, in the calendar `tz` names.
    DayDated {
        day: NaiveDate,
        tz: &'static str,
        observed: ObservationTime,
    },
    /// No publication time at all.
    Undated { observed: ObservationTime },
}

impl Datedness {
    /// The only visibility query for an item whose publication time may be a date.
    #[must_use]
    pub fn visible_at(&self, cutoff: Cutoff) -> bool {
        match self {
            Self::Dated(v) => v.visible_at(cutoff),
            // The end of the day in the WINDOW'S OWN calendar, not UTC: a day bounded by UTC
            // midnight is 19:00 New York and admits the afternoon. An unresolvable zone leaves
            // the item invisible rather than falling back to UTC.
            Self::DayDated { day, tz, observed } => match end_of_day(*day, tz) {
                Some(ends) => cutoff.inner() >= ends && cutoff.admits_observation(*observed),
                None => false,
            },
            Self::Undated { .. } => false,
        }
    }

    /// The publication value, for ordering, display and the disagreement test. Never for a cutoff.
    #[must_use]
    pub fn published(&self) -> Published {
        match self {
            Self::Dated(v) => Published::At(v.valid()),
            Self::DayDated { day, tz, .. } => Published::On { day: *day, tz },
            Self::Undated { .. } => Published::Unknown,
        }
    }

    #[must_use]
    pub fn observed(&self) -> ObservationTime {
        match self {
            Self::Dated(v) => v.observed(),
            Self::DayDated { observed, .. } | Self::Undated { observed } => *observed,
        }
    }
}

/// The last microsecond of `day` in the calendar `tz` names, or `None` when `tz` is not an IANA
/// zone name. When the next local midnight falls in a daylight-saving gap, the day ends where the
/// gap starts, which is the instant of the first local time after it.
#[must_use]
pub fn end_of_day(day: NaiveDate, tz: &str) -> Option<DateTime<Utc>> {
    let zone: Tz = tz.parse().ok()?;
    let midnight = day.succ_opt()?.and_time(NaiveTime::MIN);
    // `.latest()` of an ambiguous local midnight, never `.earliest()`: the later instant is the
    // later bound, and a later bound cannot admit the item sooner.
    let starts = match zone.from_local_datetime(&midnight).latest() {
        Some(starts) => starts,
        None => first_local_time_after_a_gap(zone, midnight)?,
    };
    Some(starts.with_timezone(&Utc) - Duration::microseconds(1))
}

/// Pacific/Apia skipped the whole of 2011-12-30, so a gap can be a day long.
const LONGEST_GAP_MINUTES: i64 = 2 * 24 * 60;
const SECONDS_PER_MINUTE: i64 = 60;

/// The first local time at or after `inside` that exists, found to the second: tzdb transitions
/// fall on whole seconds, and the gap is one contiguous run of local times that do not exist.
fn first_local_time_after_a_gap(zone: Tz, inside: NaiveDateTime) -> Option<DateTime<Tz>> {
    let exists = |local: &NaiveDateTime| zone.from_local_datetime(local).earliest();
    let minute = (1..=LONGEST_GAP_MINUTES)
        .map(|minutes| inside + Duration::minutes(minutes))
        .find(|local| exists(local).is_some())?;
    let first = (1..SECONDS_PER_MINUTE)
        .map(|seconds| minute - Duration::seconds(seconds))
        .take_while(|local| exists(local).is_some())
        .last()
        .unwrap_or(minute);
    exists(&first)
}

#[cfg(test)]
mod tests {
    use super::{end_of_day, Datedness};
    use crate::{Cutoff, ObservationTime, ValidTime, Visibility};
    use chrono::{DateTime, NaiveDate, Utc};

    const NEW_YORK: &str = "America/New_York";
    const NOT_A_ZONE: &str = "Mars/Phobos";
    const UTC: &str = "UTC";
    const PUBLICATION_DAY: &str = "2026-07-01";
    /// 18:00 in New York on `PUBLICATION_DAY`, so a day bounded by UTC midnight would admit it.
    const INSIDE_THE_DAY_IN_NEW_YORK: &str = "2026-07-01T23:00:00Z";
    const EARLY_ON_THE_PUBLICATION_DAY: &str = "2026-07-01T02:00:00Z";
    /// 23:59:59.999999 in New York on `PUBLICATION_DAY`.
    const END_OF_THE_DAY_IN_NEW_YORK: &str = "2026-07-02T03:59:59.999999Z";
    const OBSERVED_AT: &str = "2026-07-01T01:00:00Z";
    /// A backfill: the date is 2026-07-01 and we did not read the row until three days later.
    const OBSERVED_THREE_DAYS_LATER: &str = "2026-07-04T00:00:00Z";
    const END_OF_THE_DAY_IN_UTC: &str = "2026-07-01T23:59:59.999999Z";
    /// Past the end of the UTC day and four hours short of the end of the New York one.
    const UTC_MIDNIGHT_AFTER_THE_DAY: &str = "2026-07-02T00:00:00Z";
    const HAVANA: &str = "America/Havana";
    const DAY_BEFORE_HAVANA_FALLS_BACK: &str = "2026-10-31";
    /// One microsecond before 05:00Z; the earlier midnight, 04:00Z, would end it an hour sooner.
    const END_OF_THAT_DAY_AT_THE_LATER_MIDNIGHT: &str = "2026-11-01T04:59:59.999999Z";
    /// Havana springs forward at its 2026-03-08 midnight, so the day before has no local midnight
    /// to end at; the gap starts at 00:00 CST, which is 05:00Z.
    const DAY_BEFORE_HAVANA_SPRINGS_FORWARD: &str = "2026-03-07";
    const END_OF_THAT_DAY_WHERE_THE_GAP_STARTS: &str = "2026-03-08T04:59:59.999999Z";
    const EVERY_CUTOFF: [&str; 3] = [
        "1970-01-01T00:00:00Z",
        "2026-07-09T00:00:00Z",
        "2262-04-11T00:00:00Z",
    ];

    fn at(text: &str) -> DateTime<Utc> {
        match text.parse::<DateTime<Utc>>() {
            Ok(instant) => instant,
            Err(_) => DateTime::<Utc>::UNIX_EPOCH,
        }
    }

    fn day(text: &str) -> NaiveDate {
        match text.parse::<NaiveDate>() {
            Ok(date) => date,
            Err(_) => NaiveDate::MIN,
        }
    }

    fn day_dated_observed_at(tz: &'static str, observed: &str) -> Datedness {
        Datedness::DayDated {
            day: day(PUBLICATION_DAY),
            tz,
            observed: ObservationTime::new(at(observed)),
        }
    }

    fn day_dated(tz: &'static str) -> Datedness {
        Datedness::DayDated {
            day: day(PUBLICATION_DAY),
            tz,
            observed: ObservationTime::new(at(OBSERVED_AT)),
        }
    }

    #[test]
    fn a_day_precision_item_is_not_visible_before_the_end_of_its_own_day() {
        assert!(!day_dated(NEW_YORK).visible_at(Cutoff::new(at(EARLY_ON_THE_PUBLICATION_DAY))));
    }

    #[test]
    fn a_day_precision_items_day_ends_in_the_windows_calendar_and_not_in_utc() {
        let item = day_dated(NEW_YORK);
        assert!(
            !item.visible_at(Cutoff::new(at(INSIDE_THE_DAY_IN_NEW_YORK))),
            "a UTC-midnight bound would admit 18:00 New York"
        );
        assert!(item.visible_at(Cutoff::new(at(END_OF_THE_DAY_IN_NEW_YORK))));
    }

    /// The half-question - "was it published before T?" - is the leak this repository exists to
    /// close: it admitted documents not visible at the claimed instant in 60.2% of 69 retrieval
    /// probes while the full question leaked in none. The day arm must ask both halves, and
    /// mutation 2026-11-13 showed that dropping the observation half turned nothing red.
    #[test]
    fn a_day_precision_item_observed_after_its_day_ended_is_not_visible_when_the_day_ends() {
        let item = day_dated_observed_at(UTC, OBSERVED_THREE_DAYS_LATER);
        assert!(!item.visible_at(Cutoff::new(at(END_OF_THE_DAY_IN_UTC))));
    }

    #[test]
    fn a_day_precision_item_observed_after_its_day_ended_is_visible_once_we_had_read_it() {
        let item = day_dated_observed_at(UTC, OBSERVED_THREE_DAYS_LATER);
        assert!(item.visible_at(Cutoff::new(at(OBSERVED_THREE_DAYS_LATER))));
    }

    /// The companion the calendar test needed: a cutoff PAST the end of the UTC day and short of
    /// the end of the New York one. Mutation 2026-11-13 forced the zone to UTC and the other test
    /// stayed green, because its cutoff sits inside both days.
    #[test]
    fn a_new_york_dated_item_is_not_visible_when_only_the_utc_day_has_ended() {
        let item = day_dated(NEW_YORK);
        assert!(!item.visible_at(Cutoff::new(at(UTC_MIDNIGHT_AFTER_THE_DAY))));
    }

    /// Mutation 2026-11-13 replaced this arm with `true` and nothing was red: the full-precision
    /// case had no test of its own in this module.
    #[test]
    fn a_fully_dated_item_answers_through_visibility_and_is_invisible_before_its_instants() {
        let dated = match Visibility::from_live(
            ValidTime::new(at(EARLY_ON_THE_PUBLICATION_DAY)),
            ObservationTime::new(at(INSIDE_THE_DAY_IN_NEW_YORK)),
        ) {
            Ok(v) => Datedness::Dated(v),
            Err(error) => {
                assert_eq!(format!("{error:?}"), "an accepted visibility");
                return;
            }
        };
        assert!(!dated.visible_at(Cutoff::new(at(EARLY_ON_THE_PUBLICATION_DAY))));
        assert!(dated.visible_at(Cutoff::new(at(INSIDE_THE_DAY_IN_NEW_YORK))));
    }

    #[test]
    fn an_undated_item_is_visible_at_no_cutoff_whatever_the_cutoff_is() {
        let item = Datedness::Undated {
            observed: ObservationTime::new(at(OBSERVED_AT)),
        };
        for cutoff in EVERY_CUTOFF {
            assert!(!item.visible_at(Cutoff::new(at(cutoff))), "{cutoff}");
        }
    }

    /// Section 8 row R11, the ambiguous midnight.
    #[test]
    fn a_day_ending_at_an_ambiguous_midnight_ends_at_the_later_of_its_two_instants() {
        assert_eq!(
            end_of_day(day(DAY_BEFORE_HAVANA_FALLS_BACK), HAVANA),
            Some(at(END_OF_THAT_DAY_AT_THE_LATER_MIDNIGHT))
        );
    }

    /// Section 8 row R11, the midnight inside a gap.
    #[test]
    fn a_day_whose_next_midnight_falls_in_a_gap_ends_where_the_gap_starts() {
        assert_eq!(
            end_of_day(day(DAY_BEFORE_HAVANA_SPRINGS_FORWARD), HAVANA),
            Some(at(END_OF_THAT_DAY_WHERE_THE_GAP_STARTS))
        );
    }

    /// `tz` is a public field, so an unresolvable zone name is reachable from any caller.
    #[test]
    fn a_day_dated_item_whose_zone_is_not_a_known_name_is_visible_at_no_cutoff() {
        assert_eq!(end_of_day(day(PUBLICATION_DAY), NOT_A_ZONE), None);
        for cutoff in EVERY_CUTOFF {
            assert!(
                !day_dated(NOT_A_ZONE).visible_at(Cutoff::new(at(cutoff))),
                "{cutoff}"
            );
        }
    }
}
