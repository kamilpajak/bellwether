//! The one visibility predicate over the archive's STORED time columns: `newsvault items --at`,
//! `newsvault cut`, this crate's validator and, later, `derive` call it. Every arm goes through
//! `pit`, and no `ValidTime` is compared with an `ObservationTime`. Versioned by [`Rule`]: a file
//! is judged by the rule its own `snapshot.data/N` names (architecture.md section 3.0).

use chrono::{DateTime, Duration, NaiveDate, Utc};
use pit::datedness::{end_of_day, Datedness, Minute};
use pit::{Cutoff, ObservationTime, ValidTime, Visibility};

/// A version of the predicate. A change to any arm, or to the zone data a day closes in, is a new
/// variant and a new `snapshot.data/N`, never an edit of an existing arm: a file written under one
/// rule keeps validating under that rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// Instant and minute through `pit`; a day closes at the first instant of the next local day
    /// in its own `valid_tz`, which closes §8 row R13's 1 µs early admission.
    V1,
}

impl Rule {
    /// The rule `items --at` answers by and `cut` writes under.
    pub const CURRENT: Self = Self::V1;
    pub const ALL: [Self; 1] = [Self::V1];

    /// The `schema` key-value of a data file written under this rule.
    #[must_use]
    pub const fn data_schema(self) -> &'static str {
        match self {
            Self::V1 => "snapshot.data/1",
        }
    }

    /// The tzdb release a day's close was computed with when this rule was fixed: chrono-tz's
    /// `IANA_TZDB_VERSION`. A chrono-tz upgrade that changes it turns
    /// `the_current_rule_names_the_zone_data_this_build_carries` red.
    #[must_use]
    pub const fn tzdb(self) -> &'static str {
        match self {
            Self::V1 => "2025b",
        }
    }

    #[must_use]
    pub fn of_data_schema(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|rule| rule.data_schema() == name)
    }
}

/// The five stored strings the predicate reads, borrowed as the store or a file holds them.
#[derive(Debug, Clone, Copy)]
pub struct StoredTimes<'a> {
    pub valid_at: Option<&'a str>,
    pub valid_minute: Option<&'a str>,
    pub valid_day: Option<&'a str>,
    pub valid_tz: Option<&'a str>,
    pub observed_at: &'a str,
}

/// A stored value the predicate cannot answer for. Never mapped to "not visible": that silent
/// drop is the empty fallback §5 row 1 forbids.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TimeError {
    #[error("{column} holds {value:?}, which is not {expected}")]
    Unparseable {
        column: &'static str,
        value: String,
        expected: &'static str,
    },
    #[error("valid_tz {0:?} is not an IANA zone name")]
    ZoneUnknown(String),
    #[error("observed_at {observed} precedes the publication value {valid}")]
    ObservedBeforeValid { valid: String, observed: String },
    #[error("the publication columns hold {0}; exactly one of valid_at, valid_minute or valid_day with its valid_tz is the stored shape")]
    Shape(&'static str),
}

const INSTANT: &str = "an RFC 3339 instant";
const WHOLE_MINUTE: &str = "a whole minute";
const DATE: &str = "a YYYY-MM-DD date";

/// Whether the row is visible at `cutoff` under `rule`. `Ok(false)` for a row with no publication
/// value at all, which the items CHECK permits only beside `valid_refused` (§8 row R13).
pub fn visible_at(rule: Rule, row: &StoredTimes<'_>, cutoff: Cutoff) -> Result<bool, TimeError> {
    match rule {
        Rule::V1 => visible_v1(row, cutoff),
    }
}

fn visible_v1(row: &StoredTimes<'_>, cutoff: Cutoff) -> Result<bool, TimeError> {
    let observed = ObservationTime::new(instant("observed_at", row.observed_at)?);
    match (row.valid_at, row.valid_minute, row.valid_day, row.valid_tz) {
        (None, None, None, None) => Ok(false),
        (Some(at), None, None, None) => {
            let valid = ValidTime::new(instant("valid_at", at)?);
            Visibility::from_live(valid, observed)
                .map(|window| window.visible_at(cutoff))
                .map_err(|_| TimeError::ObservedBeforeValid {
                    valid: at.to_owned(),
                    observed: row.observed_at.to_owned(),
                })
        }
        (None, Some(text), None, None) => {
            let minute = Minute::new(instant("valid_minute", text)?).map_err(|_| {
                TimeError::Unparseable {
                    column: "valid_minute",
                    value: text.to_owned(),
                    expected: WHOLE_MINUTE,
                }
            })?;
            // `of_minute` answers `Undated` for an observation before the minute starts, which
            // would read as "never visible"; the store files such a row under `valid_refused`.
            match Datedness::of_minute(minute, observed) {
                Datedness::Undated { .. } => Err(TimeError::ObservedBeforeValid {
                    valid: text.to_owned(),
                    observed: row.observed_at.to_owned(),
                }),
                dated => Ok(dated.visible_at(cutoff)),
            }
        }
        (None, None, Some(text), Some(zone)) => {
            let day = text
                .parse::<NaiveDate>()
                .map_err(|_| TimeError::Unparseable {
                    column: "valid_day",
                    value: text.to_owned(),
                    expected: DATE,
                })?;
            let closes = day_closes(day, zone)?;
            Ok(cutoff.inner() >= closes && cutoff.inner() >= observed.inner())
        }
        (None, None, Some(_), None) => Err(TimeError::Shape("a valid_day with no valid_tz")),
        (None, None, None, Some(_)) => Err(TimeError::Shape("a valid_tz with no valid_day")),
        _ => Err(TimeError::Shape("more than one publication value")),
    }
}

/// The first instant of the next local day. `end_of_day` returns the day's LAST microsecond and is
/// left unchanged, because `edgar-press` dates backfill observations with it (§8 row R11).
fn day_closes(day: NaiveDate, zone: &str) -> Result<DateTime<Utc>, TimeError> {
    end_of_day(day, zone)
        .and_then(|ends| ends.checked_add_signed(Duration::microseconds(1)))
        .ok_or_else(|| TimeError::ZoneUnknown(zone.to_owned()))
}

fn instant(column: &'static str, text: &str) -> Result<DateTime<Utc>, TimeError> {
    text.parse().map_err(|_| TimeError::Unparseable {
        column,
        value: text.to_owned(),
        expected: INSTANT,
    })
}

#[cfg(test)]
mod tests {
    use super::{visible_at, Rule, StoredTimes, TimeError};
    use chrono::{DateTime, Utc};
    use pit::Cutoff;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const NEW_YORK: &str = "America/New_York";
    const A_DAY: &str = "2026-07-01";
    /// 23:59:59.999999 in New York on `A_DAY`, the microsecond `end_of_day` returns.
    const LAST_MICROSECOND_OF_THE_DAY: &str = "2026-07-02T03:59:59.999999Z";
    const NEXT_LOCAL_MIDNIGHT: &str = "2026-07-02T04:00:00Z";
    const OBSERVED_IN_THE_DAY: &str = "2026-07-01T14:00:00Z";
    const OBSERVED_THREE_DAYS_LATER: &str = "2026-07-04T00:00:00Z";
    const A_MINUTE: &str = "2026-11-14T19:00:00Z";
    const INSIDE_THE_MINUTE: &str = "2026-11-14T19:00:30Z";
    const BEFORE_THE_MINUTE: &str = "2026-11-14T18:59:59Z";
    const WHEN_THE_MINUTE_CLOSES: &str = "2026-11-14T19:01:00Z";
    const LAST_MICROSECOND_OF_THE_MINUTE: &str = "2026-11-14T19:00:59.999999Z";
    const AN_INSTANT: &str = "2026-07-01T13:30:00Z";
    const ITS_OBSERVATION: &str = "2026-07-01T13:31:00Z";
    const BETWEEN_THE_TWO: &str = "2026-07-01T13:30:30Z";

    fn cutoff(text: &str) -> Result<Cutoff, chrono::ParseError> {
        Ok(Cutoff::new(text.parse::<DateTime<Utc>>()?))
    }

    fn day(observed: &'static str) -> StoredTimes<'static> {
        StoredTimes {
            valid_at: None,
            valid_minute: None,
            valid_day: Some(A_DAY),
            valid_tz: Some(NEW_YORK),
            observed_at: observed,
        }
    }

    fn minute(observed: &'static str) -> StoredTimes<'static> {
        StoredTimes {
            valid_at: None,
            valid_minute: Some(A_MINUTE),
            valid_day: None,
            valid_tz: None,
            observed_at: observed,
        }
    }

    fn instant(observed: &'static str) -> StoredTimes<'static> {
        StoredTimes {
            valid_at: Some(AN_INSTANT),
            valid_minute: None,
            valid_day: None,
            valid_tz: None,
            observed_at: observed,
        }
    }

    fn answers(
        row: &StoredTimes<'_>,
        cutoffs: &[&str],
    ) -> Result<Vec<bool>, Box<dyn std::error::Error>> {
        cutoffs
            .iter()
            .map(|at| Ok(visible_at(Rule::V1, row, cutoff(at)?)?))
            .collect()
    }

    /// §8 row R13. Mutation 2026-11-22, `day_closes` returning `end_of_day` unchanged: red.
    #[test]
    fn a_day_row_is_invisible_at_the_last_microsecond_of_its_own_day() -> TestResult {
        assert_eq!(
            answers(
                &day(OBSERVED_IN_THE_DAY),
                &[LAST_MICROSECOND_OF_THE_DAY, NEXT_LOCAL_MIDNIGHT]
            )?,
            vec![false, true]
        );
        Ok(())
    }

    /// The observation half of the day arm. Mutation 2026-11-22, the `observed` clause dropped:
    /// red.
    #[test]
    fn a_row_visible_on_publication_but_not_on_observation_is_not_visible() -> TestResult {
        assert_eq!(
            answers(
                &day(OBSERVED_THREE_DAYS_LATER),
                &[NEXT_LOCAL_MIDNIGHT, OBSERVED_THREE_DAYS_LATER]
            )?,
            vec![false, true]
        );
        Ok(())
    }

    /// Mutation 2026-11-22, every `Err` of the predicate mapped to `Ok(false)`: red.
    #[test]
    fn an_unparseable_stored_time_is_an_error_not_invisible() -> TestResult {
        let garbled = StoredTimes {
            observed_at: "yesterday",
            ..instant(ITS_OBSERVATION)
        };
        let answer = visible_at(Rule::V1, &garbled, cutoff(ITS_OBSERVATION)?);
        assert!(
            matches!(
                answer,
                Err(TimeError::Unparseable {
                    column: "observed_at",
                    ..
                })
            ),
            "{answer:?}"
        );
        Ok(())
    }

    #[test]
    fn a_day_in_a_zone_no_tzdb_names_is_an_error_not_invisible() -> TestResult {
        let unknown = StoredTimes {
            valid_tz: Some("Mars/Phobos"),
            ..day(OBSERVED_IN_THE_DAY)
        };
        let answer = visible_at(Rule::V1, &unknown, cutoff(OBSERVED_THREE_DAYS_LATER)?);
        assert_eq!(
            answer,
            Err(TimeError::ZoneUnknown("Mars/Phobos".to_owned()))
        );
        Ok(())
    }

    /// The instant arm through `Visibility::from_live`, and the minute arm, where `of_minute`
    /// answers `Undated` rather than an error. Mutation 2026-11-22, the minute arm's `Undated`
    /// answered `Ok(false)`: red.
    #[test]
    fn an_observation_before_its_publication_is_an_error_in_the_instant_and_minute_arms(
    ) -> TestResult {
        let early_instant = StoredTimes {
            observed_at: "2026-07-01T13:29:00Z",
            ..instant(ITS_OBSERVATION)
        };
        let answers = [early_instant, minute(BEFORE_THE_MINUTE)]
            .map(|row| visible_at(Rule::V1, &row, Cutoff::new(DateTime::<Utc>::MAX_UTC)));
        assert!(
            answers
                .iter()
                .all(|answer| matches!(answer, Err(TimeError::ObservedBeforeValid { .. }))),
            "{answers:?}"
        );
        Ok(())
    }

    /// The case `Ok(false)` stays for: no publication value. The items CHECK (store.rs
    /// `MIGRATIONS[0]`, §8 row R13: exactly one of `valid_at`, `valid_day`, `valid_minute`,
    /// `valid_refused` is set) makes "none of the three and not refused" unstorable, so a row
    /// reaching this arm is a refused one. Mutation 2026-11-22, the arm answering `Shape`: red.
    #[test]
    fn a_refused_row_is_visible_at_no_cutoff_and_is_not_an_error() -> TestResult {
        let refused = StoredTimes {
            valid_at: None,
            ..instant(ITS_OBSERVATION)
        };
        assert_eq!(
            answers(&refused, &[ITS_OBSERVATION, NEXT_LOCAL_MIDNIGHT])?,
            vec![false, false]
        );
        Ok(())
    }

    #[test]
    fn a_row_holding_two_publication_values_or_a_day_without_its_zone_is_a_shape_error(
    ) -> TestResult {
        let two = StoredTimes {
            valid_minute: Some(A_MINUTE),
            ..instant(ITS_OBSERVATION)
        };
        let zoneless = StoredTimes {
            valid_tz: None,
            ..day(OBSERVED_IN_THE_DAY)
        };
        let answers = [two, zoneless]
            .map(|row| visible_at(Rule::V1, &row, Cutoff::new(DateTime::<Utc>::MAX_UTC)));
        assert!(
            answers
                .iter()
                .all(|answer| matches!(answer, Err(TimeError::Shape(_)))),
            "{answers:?}"
        );
        Ok(())
    }

    /// Rule V1's committed answers, one boundary per arm. An edit of any arm turns this red, which
    /// is the point: a changed rule is `Rule::V2` and `snapshot.data/2`, so files written under V1
    /// keep validating (architecture.md section 3.0, the rule-versioning bullet).
    #[test]
    fn rule_v1_answers_its_committed_cases() -> TestResult {
        let cases: [(StoredTimes<'static>, &str, bool); 8] = [
            (instant(ITS_OBSERVATION), BETWEEN_THE_TWO, false),
            (instant(ITS_OBSERVATION), ITS_OBSERVATION, true),
            (
                minute(INSIDE_THE_MINUTE),
                LAST_MICROSECOND_OF_THE_MINUTE,
                false,
            ),
            (minute(INSIDE_THE_MINUTE), WHEN_THE_MINUTE_CLOSES, true),
            (day(OBSERVED_IN_THE_DAY), LAST_MICROSECOND_OF_THE_DAY, false),
            (day(OBSERVED_IN_THE_DAY), NEXT_LOCAL_MIDNIGHT, true),
            (day(OBSERVED_THREE_DAYS_LATER), NEXT_LOCAL_MIDNIGHT, false),
            (
                day(OBSERVED_THREE_DAYS_LATER),
                OBSERVED_THREE_DAYS_LATER,
                true,
            ),
        ];
        for (row, at, expected) in cases {
            assert_eq!(
                visible_at(Rule::V1, &row, cutoff(at)?)?,
                expected,
                "{row:?} at {at}"
            );
        }
        Ok(())
    }

    #[test]
    fn the_current_rule_names_the_zone_data_this_build_carries() {
        assert_eq!(Rule::CURRENT.tzdb(), chrono_tz::IANA_TZDB_VERSION);
    }

    #[test]
    fn every_rule_is_found_again_by_its_own_data_schema_and_no_other_name_is() {
        for rule in Rule::ALL {
            assert_eq!(Rule::of_data_schema(rule.data_schema()), Some(rule));
        }
        assert_eq!(Rule::of_data_schema("snapshot.data/0"), None);
    }
}
