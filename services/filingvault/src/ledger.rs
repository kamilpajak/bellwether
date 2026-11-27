//! `filingvault ledger check --at`: which index days were asked, as of an instant (architecture.md
//! section 12.2, "What complete means"). Read-only and lease-free. Exit 1 is "the answer is no":
//! a forward day is not asked, or no collect run had started by `--at`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Days, NaiveDate, Utc};
use rusqlite::Connection;
use sec_edgar::listed_days;
use serde_json::{json, Value};
use vault_core::blobs::BlobStore;

use crate::collect::instant;
use crate::discover::{day_states, DayState, BODY_LIMIT};
use crate::error::FilingsError;
use crate::rules::Rules;
use crate::store;

pub const LEDGER_SCHEMA: &str = "filingvault.ledger/1";
/// A forward day is owed once this many days have passed after it (section 12.2's metric).
pub const FORWARD_GRACE_DAYS: u64 = 3;
pub const ANSWER_IS_NO: u8 = 1;

#[derive(Debug, Clone)]
pub struct Asked {
    pub at: DateTime<Utc>,
    pub store: PathBuf,
    pub rules: Rules,
}

#[derive(Debug)]
pub struct Checked {
    pub report: Value,
    pub exit: u8,
}

/// Per day: the listings rows first listed by an attempt closed by `at`.
#[derive(Debug, Default)]
struct Rows {
    listed: u32,
    by_form: BTreeMap<String, u32>,
    late: u32,
    listed_later: u32,
}

pub fn check(asked: &Asked, now: DateTime<Utc>) -> Result<Checked, FilingsError> {
    if asked.at > now {
        return Err(FilingsError::Usage(format!(
            "--at {} is after now, {}",
            store::stamp(asked.at),
            store::stamp(now)
        )));
    }
    let db = store::open_read(&asked.store)?;
    let blobs = BlobStore::new(asked.store.join(store::BLOBS), store::WRITE_POLICY);
    let until = store::stamp(asked.at);
    let at_date = asked.at.date_naive();
    let forward_from = match store::collect_starts(&db, Some(&until))?.first() {
        Some(started) => Some(instant(started)?.date_naive()),
        None => None,
    };
    let mut listed: BTreeMap<NaiveDate, String> = BTreeMap::new();
    for (label, blob, _) in store::held_listings(&db, Some(&until))? {
        let days = listed_days(&store::read_blob(&blobs, &blob)?).map_err(|why| {
            FilingsError::Unparseable {
                what: format!("the stored listing of {label}"),
                why: why.to_string(),
            }
        })?;
        for day in days.into_iter().filter(|day| *day < at_date) {
            listed.insert(day, label.clone());
        }
    }
    let attempts = store::index_attempts(&db, Some(&until))?;
    let states = day_states(&attempts, &asked.rules.forms, BODY_LIMIT);
    let rows = rows_by_day(&db, &until, &attempts)?;
    let owed = |day: NaiveDate| {
        forward_from
            .is_some_and(|from| day >= from && day + Days::new(FORWARD_GRACE_DAYS) < at_date)
    };
    let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
    let mut forward_unasked = 0_u32;
    let mut day_rows = Vec::with_capacity(listed.len());
    for (day, quarter) in &listed {
        let state = states.get(day).copied().unwrap_or(DayState::Unasked);
        *counts.entry(state.name()).or_default() += 1;
        let forward = owed(*day);
        if forward && state != DayState::Asked {
            forward_unasked += 1;
        }
        let of_day: Vec<&store::IndexAttempt> = attempts
            .iter()
            .filter(|attempt| attempt.day == *day)
            .collect();
        let last = of_day.last();
        let none = Rows::default();
        let held = rows.get(day).unwrap_or(&none);
        day_rows.push(json!({
            "index_day": store::day_text(*day),
            "quarter": quarter,
            "state": state.name(),
            "forward": forward,
            "attempts": of_day.len(),
            "last_attempt": last.map(|attempt| json!({
                "outcome": attempt.outcome,
                "verdict": attempt.verdict,
                "status": attempt.status,
            })),
            "last_modified": match last {
                Some(attempt) => last_modified(attempt.headers.as_deref())?,
                None => None,
            },
            "listed_rows": held.listed,
            "by_form": held.by_form,
            "late": held.late,
            "listed_later": held.listed_later,
        }));
    }
    let by_state: BTreeMap<&str, u32> = DayState::ALL
        .map(|state| (state.name(), counts.get(state.name()).copied().unwrap_or(0)))
        .into_iter()
        .collect();
    let (raw, stored): (Option<i64>, Option<i64>) = db.query_row(
        "SELECT sum(bytes), sum(stored_bytes) FROM blobs
          WHERE digest IN (SELECT raw_blob FROM attempts WHERE closed_at <= ?1)",
        [&until],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let last_run = store::last_run(&db, &until)?;
    let exit = if forward_from.is_none() || forward_unasked > 0 {
        ANSWER_IS_NO
    } else {
        0
    };
    let report = json!({
        "schema": LEDGER_SCHEMA,
        "at": until,
        "forms": asked.rules.forms,
        "forward_from": forward_from.map(store::day_text),
        "days": by_state,
        "forward_unasked": forward_unasked,
        "listed_rows": rows.values().map(|held| held.listed).sum::<u32>(),
        "per_day": day_rows,
        "blob_raw_bytes": raw.unwrap_or(0),
        "blob_stored_bytes": stored.unwrap_or(0),
        "last_run": last_run,
        "exit": exit,
    });
    Ok(Checked { report, exit })
}

/// The listings rows first listed by an attempt closed by `until`, counted per day; a row is
/// `listed_later` when its first attempt came after the day's first usable one.
fn rows_by_day(
    db: &Connection,
    until: &str,
    attempts: &[store::IndexAttempt],
) -> Result<BTreeMap<NaiveDate, Rows>, FilingsError> {
    let mut first_usable: BTreeMap<NaiveDate, i64> = BTreeMap::new();
    for attempt in attempts
        .iter()
        .filter(|attempt| attempt.verdict.as_deref() == Some("usable"))
    {
        first_usable.entry(attempt.day).or_insert(attempt.seq);
    }
    let mut statement = db.prepare(
        "SELECT l.index_day, l.form, l.filed_day, l.first_seq
           FROM listings l JOIN attempts a ON a.seq = l.first_seq
          WHERE a.closed_at <= ?1",
    )?;
    let found = statement
        .query_map([until], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut by_day: BTreeMap<NaiveDate, Rows> = BTreeMap::new();
    for (day, form, filed, first_seq) in found {
        let day = store::day_of(&day)?;
        let held = by_day.entry(day).or_default();
        held.listed += 1;
        *held.by_form.entry(form).or_default() += 1;
        if store::day_of(&filed)? != day {
            held.late += 1;
        }
        if first_usable
            .get(&day)
            .is_some_and(|first| first_seq > *first)
        {
            held.listed_later += 1;
        }
    }
    Ok(by_day)
}

/// The index's `Last-Modified`: the instant SEC wrote it, section 12.3's floor.
fn last_modified(headers: Option<&str>) -> Result<Option<String>, FilingsError> {
    let Some(headers) = headers else {
        return Ok(None);
    };
    let headers =
        news_provider::Headers::from_json(headers).map_err(|why| FilingsError::Unparseable {
            what: "stored response headers".to_owned(),
            why: why.to_string(),
        })?;
    Ok(headers.first("last-modified").map(str::to_owned))
}
