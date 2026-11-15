//! `newsvault items --at <t>` - the point-in-time read over the archive's own rows. `--at` has no
//! default for the reason `ledger`'s has none: a read with no cutoff is not a point-in-time read.
//!
//! `--per-story latest` defaults OFF. A default that silently collapses rows is R1's defect moved
//! from the archive into the view, and the inflation it would hide is measured: 157 ids appear in
//! more than one partition of the predecessor's lake, all 157 carrying a publication time they did
//! not have before, 248 extra appearances.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, Utc};
use pit::datedness::{end_of_day, Datedness, Minute};
use pit::{Cutoff, ObservationTime, ValidTime, Visibility};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Map, Value};

use crate::store::{require_current_version, StoreError, COMMIT_LOG, VALID_DATE};

pub const SCHEMA: &str = "newsvault.items/1";

/// Every field an item row can be reported under, which is also what `--fields` may name. One
/// list, so the SQL, the envelope and the help cannot drift apart.
pub const FIELDS: [&str; 16] = [
    "seq",
    "item_id",
    "story_id",
    "doc_id",
    "attempt_id",
    "source",
    "valid_at",
    "valid_day",
    "valid_tz",
    "valid_minute",
    "valid_refused",
    "valid_precision",
    "observed_at",
    "valid_disputed",
    "earliest_valid_at",
    "valid_date",
];

#[derive(Debug, thiserror::Error)]
pub enum ItemsError {
    #[error("no commit log under {0}")]
    StoreAbsent(PathBuf),
    #[error("--from {from} is after --to {to}")]
    RangeInverted { from: NaiveDate, to: NaiveDate },
    #[error("--fields names {0}, which is not a field of this output")]
    UnknownField(String),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Which rows of one story a read returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerStory {
    All,
    Latest,
}

impl PerStory {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Latest => "latest",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Ask {
    pub at: DateTime<Utc>,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    pub source: Option<String>,
    pub limit: Option<usize>,
    pub fields: Option<Vec<String>>,
    pub per_story: PerStory,
}

#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub at: String,
    pub per_story: &'static str,
    /// Visible ROWS and visible STORIES, always both: one is the archive's count and the other is
    /// the document count, and a reader given one number cannot tell a re-stamp from a new story.
    pub rows_visible: usize,
    pub stories_visible: usize,
    pub matched: usize,
    pub returned: usize,
    pub truncated: bool,
    pub items: Vec<Value>,
}

/// One item row as the archive stores it. Every instant stays the stored string, so a value an
/// `item_id` digests is never re-rendered by this reader.
#[derive(Debug, Clone)]
struct Row {
    seq: i64,
    item_id: String,
    story_id: String,
    doc_id: Option<String>,
    attempt_id: String,
    source: String,
    valid_at: Option<String>,
    valid_day: Option<String>,
    valid_tz: Option<String>,
    valid_minute: Option<String>,
    valid_refused: Option<String>,
    observed_at: String,
    valid_disputed: bool,
    earliest_valid_at: Option<String>,
    valid_date: String,
}

pub fn read(root: &Path, ask: &Ask) -> Result<Report, ItemsError> {
    if let (Some(from), Some(to)) = (ask.from, ask.to) {
        if from > to {
            return Err(ItemsError::RangeInverted { from, to });
        }
    }
    let fields = chosen(ask.fields.as_deref())?;
    let log = root.join(COMMIT_LOG);
    if !log.exists() {
        return Err(ItemsError::StoreAbsent(log));
    }
    let db = Connection::open_with_flags(&log, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    require_current_version(&db)?;
    let cutoff = Cutoff::new(ask.at);
    let mut visible: Vec<Row> = rows(&db)?
        .into_iter()
        .filter(|row| in_range(row, ask))
        .filter(|row| visible_at(row, cutoff))
        .collect();
    let rows_visible = visible.len();
    let stories_visible = visible
        .iter()
        .map(|row| row.story_id.as_str())
        .collect::<BTreeSet<&str>>()
        .len();
    if ask.per_story == PerStory::Latest {
        visible = latest_per_story(visible);
    }
    let matched = visible.len();
    if let Some(limit) = ask.limit {
        visible.truncate(limit);
    }
    let returned = visible.len();
    Ok(Report {
        schema: SCHEMA,
        at: ask.at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        per_story: ask.per_story.name(),
        rows_visible,
        stories_visible,
        matched,
        returned,
        truncated: returned < matched,
        items: visible.iter().map(|row| reported(row, &fields)).collect(),
    })
}

fn chosen(asked: Option<&[String]>) -> Result<Vec<&'static str>, ItemsError> {
    let Some(asked) = asked else {
        return Ok(FIELDS.to_vec());
    };
    let mut chosen = Vec::new();
    for name in asked {
        match FIELDS.iter().find(|field| *field == name) {
            Some(field) => chosen.push(*field),
            None => return Err(ItemsError::UnknownField(name.clone())),
        }
    }
    Ok(chosen)
}

/// Ordered by `seq` and by nothing else, which is the only ordering authority
/// (architecture.md section 2).
fn rows(db: &Connection) -> Result<Vec<Row>, ItemsError> {
    let mut statement = db.prepare(&format!(
        "SELECT i.seq, i.item_id, i.story_id, i.doc_id, i.attempt_id, a.source, i.valid_at,
                i.valid_day, i.valid_tz, i.valid_minute, i.valid_refused, i.observed_at,
                i.valid_disputed, i.earliest_valid_at, {VALID_DATE}
         FROM items i JOIN attempts a ON a.attempt_id = i.attempt_id
         ORDER BY i.seq"
    ))?;
    let read = statement
        .query_map([], |row| {
            Ok(Row {
                seq: row.get(0)?,
                item_id: row.get(1)?,
                story_id: row.get(2)?,
                doc_id: row.get(3)?,
                attempt_id: row.get(4)?,
                source: row.get(5)?,
                valid_at: row.get(6)?,
                valid_day: row.get(7)?,
                valid_tz: row.get(8)?,
                valid_minute: row.get(9)?,
                valid_refused: row.get(10)?,
                observed_at: row.get(11)?,
                valid_disputed: row.get(12)?,
                earliest_valid_at: row.get(13)?,
                valid_date: row.get(14)?,
            })
        })?
        .collect::<Result<Vec<Row>, _>>()?;
    Ok(read)
}

fn in_range(row: &Row, ask: &Ask) -> bool {
    let after = ask
        .from
        .is_none_or(|from| row.valid_date >= from.to_string());
    let before = ask.to.is_none_or(|to| row.valid_date <= to.to_string());
    let sourced = ask
        .source
        .as_ref()
        .is_none_or(|source| &row.source == source);
    after && before && sourced
}

/// Every predicate comes from `pit`: an instant row through `Visibility`, a minute row through
/// `Datedness::of_minute`, a day row through the end of that day in its OWN calendar. A day bounded by UTC midnight is 19:00 in New York and admits
/// the afternoon, which is the 13 h of lookahead the two day columns exist to remove. The zone is
/// a stored string rather than the `&'static str` `Datedness::DayDated` carries, so this reaches
/// `end_of_day` directly instead of rebuilding that value.
fn visible_at(row: &Row, cutoff: Cutoff) -> bool {
    if let Some(minute) = &row.valid_minute {
        let (Some(minute), Some(observed)) = (
            instant(minute).and_then(|starts| Minute::new(starts).ok()),
            instant(&row.observed_at),
        ) else {
            return false;
        };
        return Datedness::of_minute(minute, ObservationTime::new(observed)).visible_at(cutoff);
    }
    if let (Some(day), Some(zone)) = (&row.valid_day, &row.valid_tz) {
        let (Ok(day), Some(observed)) = (day.parse::<NaiveDate>(), instant(&row.observed_at))
        else {
            return false;
        };
        return match end_of_day(day, zone) {
            Some(closes) => cutoff.inner() >= closes && cutoff.inner() >= observed,
            None => false,
        };
    }
    let (Some(valid), Some(observed)) = (
        row.valid_at.as_deref().and_then(instant),
        instant(&row.observed_at),
    ) else {
        // A refused publication value is visible at no cutoff: architecture.md section 5 row 4.
        return false;
    };
    match Visibility::from_live(ValidTime::new(valid), ObservationTime::new(observed)) {
        Ok(window) => window.visible_at(cutoff),
        Err(_) => false,
    }
}

fn instant(text: &str) -> Option<DateTime<Utc>> {
    text.parse().ok()
}

/// The highest `seq` among the visible rows sharing a `story_id`, which keeps the ordering free of
/// any time field.
fn latest_per_story(rows: Vec<Row>) -> Vec<Row> {
    let mut kept: BTreeMap<String, Row> = BTreeMap::new();
    for row in rows {
        match kept.get(&row.story_id) {
            Some(standing) if standing.seq >= row.seq => {}
            _ => {
                kept.insert(row.story_id.clone(), row);
            }
        }
    }
    let mut kept: Vec<Row> = kept.into_values().collect();
    kept.sort_by_key(|row| row.seq);
    kept
}

/// The precision the stored columns state, derived from which column holds the value rather than
/// carried beside it, so the two cannot disagree.
fn precision(row: &Row) -> &'static str {
    match (&row.valid_at, &row.valid_minute, &row.valid_day) {
        (Some(_), _, _) => "instant",
        (None, Some(_), _) => "minute",
        (None, None, Some(_)) => "day",
        (None, None, None) => "unknown",
    }
}

fn reported(row: &Row, fields: &[&'static str]) -> Value {
    let mut whole: Map<String, Value> = Map::new();
    whole.insert("seq".to_owned(), json!(row.seq));
    whole.insert("item_id".to_owned(), json!(row.item_id));
    whole.insert("story_id".to_owned(), json!(row.story_id));
    whole.insert("doc_id".to_owned(), json!(row.doc_id));
    whole.insert("attempt_id".to_owned(), json!(row.attempt_id));
    whole.insert("source".to_owned(), json!(row.source));
    whole.insert("valid_at".to_owned(), json!(row.valid_at));
    whole.insert("valid_day".to_owned(), json!(row.valid_day));
    whole.insert("valid_tz".to_owned(), json!(row.valid_tz));
    whole.insert("valid_minute".to_owned(), json!(row.valid_minute));
    whole.insert("valid_refused".to_owned(), json!(row.valid_refused));
    whole.insert("valid_precision".to_owned(), json!(precision(row)));
    whole.insert("observed_at".to_owned(), json!(row.observed_at));
    whole.insert("valid_disputed".to_owned(), json!(row.valid_disputed));
    whole.insert("earliest_valid_at".to_owned(), json!(row.earliest_valid_at));
    whole.insert("valid_date".to_owned(), json!(row.valid_date));
    let mut picked = Map::new();
    for field in fields {
        if let Some(value) = whole.get(*field) {
            picked.insert((*field).to_owned(), value.clone());
        }
    }
    Value::Object(picked)
}

/// The human rendering of the same object the JSON carries.
pub fn render_human(report: &Report, out: &mut impl std::io::Write) -> std::io::Result<()> {
    writeln!(out, "at               {}", report.at)?;
    writeln!(out, "per story        {}", report.per_story)?;
    for item in &report.items {
        writeln!(
            out,
            "  {:>6} {} {} {}",
            item["seq"],
            item["valid_precision"].as_str().unwrap_or_default(),
            item["valid_at"]
                .as_str()
                .or_else(|| item["valid_minute"].as_str())
                .or_else(|| item["valid_day"].as_str())
                .unwrap_or("-"),
            item["item_id"].as_str().unwrap_or_default(),
        )?;
    }
    writeln!(out, "rows visible     {}", report.rows_visible)?;
    writeln!(out, "stories visible  {}", report.stories_visible)?;
    writeln!(
        out,
        "matched/returned {}/{} truncated {}",
        report.matched, report.returned, report.truncated
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use canon::Stamp;
    use chrono::{DateTime, Utc};
    use pit::datedness::{Minute, Published};
    use tempfile::TempDir;

    use super::{read, Ask, PerStory};
    use crate::publish::tests::{commit_valued, open};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const RUN: &str = "2026-11-14T19:05:00Z";
    /// GNW's RSS `pubDate` of 2026-11-14, `Sat, 14 Nov 2026 19:00 GMT`, read inside that minute.
    const A_PUBLICATION_MINUTE: &str = "2026-11-14T19:00:00Z";
    const READ_INSIDE_IT: &str = "2026-11-14T19:00:30Z";
    const ONE_MICROSECOND_BEFORE_IT_CLOSES: &str = "2026-11-14T19:00:59.999998Z";
    const THE_LAST_MICROSECOND_OF_IT: &str = "2026-11-14T19:00:59.999999Z";

    fn ask(at: &str) -> Result<Ask, chrono::ParseError> {
        Ok(Ask {
            at: at.parse()?,
            from: None,
            to: None,
            source: None,
            limit: None,
            fields: None,
            per_story: PerStory::All,
        })
    }

    #[test]
    fn a_minute_dated_item_is_invisible_until_its_minute_has_closed() -> TestResult {
        let root = TempDir::new()?;
        let mut store = open(&root, RUN)?;
        let minute = Minute::new(A_PUBLICATION_MINUTE.parse::<DateTime<Utc>>()?)?;
        commit_valued(
            &mut store,
            "3403075",
            Published::AtMinute(minute),
            Stamp::Minute(minute.start()),
            READ_INSIDE_IT,
        )?;
        drop(store);
        let mut precisions = Vec::new();
        for cutoff in [
            READ_INSIDE_IT,
            ONE_MICROSECOND_BEFORE_IT_CLOSES,
            THE_LAST_MICROSECOND_OF_IT,
        ] {
            let report = read(root.path(), &ask(cutoff)?)?;
            precisions.push(
                report
                    .items
                    .iter()
                    .filter_map(|item| item["valid_precision"].as_str().map(str::to_owned))
                    .collect::<Vec<String>>(),
            );
        }
        assert_eq!(
            precisions,
            vec![Vec::new(), Vec::new(), vec!["minute".to_owned()]]
        );
        Ok(())
    }
}
