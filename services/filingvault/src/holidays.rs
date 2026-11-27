//! The holiday list `ledger check` reads: the federal holidays of 5 U.S.C. 6103(a) on the days
//! they are observed, for the years the file names (architecture.md section 12.2, "unlisted").
//! A day outside those years is an error, never "no holiday".

use std::collections::BTreeSet;
use std::path::Path;

use chrono::{Datelike, NaiveDate};
use serde_json::Value;

use crate::error::FilingsError;

pub const HOLIDAYS_SCHEMA: &str = "filingvault.holidays/1";
const KEYS: [&str; 4] = ["schema", "first_year", "last_year", "days"];
const DAY: &str = "%Y-%m-%d";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holidays {
    first_year: i32,
    last_year: i32,
    days: BTreeSet<NaiveDate>,
}

impl Holidays {
    /// Whether `day` is a listed holiday; `holidays_uncovered` for a day of a year the file does
    /// not cover, which is the reminder to extend it.
    pub fn holds(&self, day: NaiveDate) -> Result<bool, FilingsError> {
        if (self.first_year..=self.last_year).contains(&day.year()) {
            Ok(self.days.contains(&day))
        } else {
            Err(FilingsError::HolidaysUncovered {
                day,
                first_year: self.first_year,
                last_year: self.last_year,
            })
        }
    }
}

/// An absent file is `input_absent`; anything this reader cannot take whole, a day outside the
/// years named included, is `input_unparseable`.
pub fn read(path: &Path) -> Result<Holidays, FilingsError> {
    let text = match std::fs::read(path) {
        Ok(text) => text,
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => {
            return Err(FilingsError::Absent(path.to_path_buf()))
        }
        Err(why) => return Err(why.into()),
    };
    let refused = |why: String| FilingsError::Unparseable {
        what: path.display().to_string(),
        why,
    };
    let read: Value = serde_json::from_slice(&text).map_err(|why| refused(why.to_string()))?;
    let object = read
        .as_object()
        .ok_or_else(|| refused("not an object".to_owned()))?;
    if let Some(extra) = object.keys().find(|key| !KEYS.contains(&key.as_str())) {
        return Err(refused(format!("unknown key {extra}")));
    }
    if read["schema"] != HOLIDAYS_SCHEMA {
        return Err(refused(format!("schema is not {HOLIDAYS_SCHEMA}")));
    }
    let year = |key: &str| {
        read[key]
            .as_i64()
            .and_then(|year| i32::try_from(year).ok())
            .ok_or_else(|| refused(format!("{key} is not a year")))
    };
    let (first_year, last_year) = (year("first_year")?, year("last_year")?);
    if first_year > last_year {
        return Err(refused(format!("{first_year} is after {last_year}")));
    }
    let mut days = BTreeSet::new();
    for listed in read["days"]
        .as_array()
        .ok_or_else(|| refused("days is not a list".to_owned()))?
    {
        let day = listed
            .as_str()
            .and_then(|text| NaiveDate::parse_from_str(text, DAY).ok())
            .ok_or_else(|| refused(format!("{listed} is not a day")))?;
        if !(first_year..=last_year).contains(&day.year()) {
            return Err(refused(format!(
                "{day} is outside {first_year} to {last_year}"
            )));
        }
        days.insert(day);
    }
    Ok(Holidays {
        first_year,
        last_year,
        days,
    })
}

#[cfg(test)]
mod tests {
    use chrono::Weekday;

    use super::*;

    const COMMITTED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/rules/holidays.json");

    fn day(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, DAY).unwrap_or_default()
    }

    fn nth(year: i32, month: u32, weekday: Weekday, n: u8) -> Option<NaiveDate> {
        NaiveDate::from_weekday_of_month_opt(year, month, weekday, n)
    }

    /// 5 U.S.C. 6103(a), a Saturday observed on the Friday before and a Sunday on the Monday
    /// after (6103(b) and Executive Order 11582).
    fn of_year(year: i32) -> Vec<NaiveDate> {
        let observed = |month, on| {
            NaiveDate::from_ymd_opt(year, month, on).and_then(|day| match day.weekday() {
                Weekday::Sat => day.pred_opt(),
                Weekday::Sun => day.succ_opt(),
                _ => Some(day),
            })
        };
        let last_monday_of_may =
            nth(year, 5, Weekday::Mon, 5).or_else(|| nth(year, 5, Weekday::Mon, 4));
        [
            observed(1, 1),
            nth(year, 1, Weekday::Mon, 3),
            nth(year, 2, Weekday::Mon, 3),
            last_monday_of_may,
            observed(6, 19),
            observed(7, 4),
            nth(year, 9, Weekday::Mon, 1),
            nth(year, 10, Weekday::Mon, 2),
            observed(11, 11),
            nth(year, 11, Weekday::Thu, 4),
            observed(12, 25),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// The committed list is the statute's for every year it names, and holds the two weekdays
    /// SEC wrote no index on in 2026 Q4 to 11-12 (`probes/observation_lag/quarter_index.2026Q4.json`).
    /// Mutation 2026-11-27, 2027-12-31 dropped from the file: red here.
    #[test]
    fn the_committed_list_is_the_statutes_and_holds_the_measured_days() {
        let read = read(Path::new(COMMITTED));
        let built: BTreeSet<NaiveDate> = (2024..=2029)
            .flat_map(of_year)
            .filter(|day| (2025..=2028).contains(&day.year()))
            .collect();
        let held = |text: &str| read.as_ref().ok().map(|list| list.holds(day(text)).ok());
        assert_eq!(
            (
                read.as_ref().ok().map(|list| list.days.clone()),
                held("2026-10-12"),
                held("2026-11-11"),
                held("2027-12-31"),
            ),
            (
                Some(built),
                Some(Some(true)),
                Some(Some(true)),
                Some(Some(true))
            )
        );
    }

    /// A missing file, a file this reader cannot take, a listed day outside its years, and a day
    /// asked outside them each abort with a named code, never "no holiday" (R6). Mutations
    /// 2026-11-27, each red here: the year check of `holds` dropped; that of `read` dropped.
    #[test]
    fn a_missing_unreadable_or_uncovered_list_is_a_named_error() -> Result<(), std::io::Error> {
        let temp = tempfile::TempDir::new()?;
        let write = |name: &str, text: &str| -> Result<std::path::PathBuf, std::io::Error> {
            let path = temp.path().join(name);
            std::fs::write(&path, text)?;
            Ok(path)
        };
        let head =
            format!(r#""schema": "{HOLIDAYS_SCHEMA}", "first_year": 2026, "last_year": 2026"#);
        let outside = write(
            "outside.json",
            &format!(r#"{{{head}, "days": ["2027-01-01"]}}"#),
        )?;
        let unread = write(
            "unread.json",
            &format!(r#"{{{head}, "days": ["2026-1-x"]}}"#),
        )?;
        let good = write(
            "good.json",
            &format!(r#"{{{head}, "days": ["2026-01-01"]}}"#),
        )?;
        let code = |read: Result<Holidays, FilingsError>| read.err().map(|why| why.code().name());
        let asked = read(&good).map(|list| list.holds(day("2027-01-01")).map_err(|why| why.code()));
        assert_eq!(
            (
                code(read(&temp.path().join("absent.json"))),
                code(read(&unread)),
                code(read(&outside)),
                asked
                    .ok()
                    .map(|held| held.map_err(crate::error::ErrorCode::name)),
            ),
            (
                Some("input_absent"),
                Some("input_unparseable"),
                Some("input_unparseable"),
                Some(Err("holidays_uncovered"))
            )
        );
        Ok(())
    }
}
