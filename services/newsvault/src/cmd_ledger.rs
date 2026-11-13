//! `newsvault ledger` - the coverage reader, written before the first collector run. Which
//! fields it must carry and why `--at` is required are design.md section 4; the ledger row itself
//! is architecture.md section 3.2. Neither is restated here.
//!
//! The reader comes first on the predecessor's evidence: its lake had no reader in code, and the
//! divergence between what it stored and what production used surfaced six months later as
//! 11,054 rows against a view holding 5,331.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Days, NaiveDate, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::store::{unpublished_seq_ranges, SeqRange, COMMIT_LOG, RUNS_DIR};

pub const SCHEMA: &str = "newsvault.ledger/1";
pub const RUN_ENVELOPE_SCHEMA: &str = "newsvault.run/1";
const IN_FLIGHT: &str = "in_flight";
const OUTCOME_OK: &str = "ok";
const OUTCOME_EMPTY: &str = "empty";
const OUTCOME_FAILED: &str = "failed";
const OUTCOME_INTERRUPTED: &str = "interrupted";

/// The three `--check` answers, each the code of a finding. There is no fourth: the asking-clock
/// age needs a number from the collect policy, whose first producer is a later commit.
const RUNS_BELOW_EXPECTED: &str = "runs_below_expected";
const HOLDS_A_FAILURE: &str = "holds_a_failure";
const ROWS_ZERO_AGAINST_VENDOR_ROWS: &str = "rows_committed_zero_against_vendor_rows";

/// Codes from architecture.md section 5.1, used for a run envelope the reader could not take.
const INPUT_UNPARSEABLE: &str = "input_unparseable";
const INPUT_SCHEMA_MISMATCH: &str = "input_schema_mismatch";
const IO_FAILED: &str = "io_failed";

#[derive(Debug, thiserror::Error)]
pub enum LedgerReadError {
    #[error("no commit log under {0}")]
    StoreAbsent(PathBuf),
    #[error("--from {from} is after --to {to}")]
    RangeInverted { from: NaiveDate, to: NaiveDate },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
}

/// What a caller asked of the ledger. `at` has no default: a read with no cutoff is not a
/// point-in-time read, and every age in the answer is computed against this instant.
#[derive(Debug, Clone)]
pub struct Ask {
    pub at: DateTime<Utc>,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub source: Option<String>,
    /// How many runs a day was due, for a day whose envelopes are all missing - which is the
    /// stopped-collector case, and the one a day's own envelopes cannot speak for.
    pub expected_runs: Option<u32>,
    pub limit: Option<usize>,
    pub check: bool,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub at: String,
    pub from: String,
    pub to: String,
    pub source: Option<String>,
    pub expected_runs: Option<u32>,
    pub limit: Option<usize>,
    pub days: Vec<DayRow>,
    /// Per (source, target window), only where the attempts disagree: one group-by over rows the
    /// store already writes, which is what makes an unrecorded change of inputs visible.
    pub result_digest_drift: Vec<Drift>,
    pub unreadable_runs: Vec<Unreadable>,
    pub attempts_without_a_run_envelope: Vec<Orphan>,
    pub unpublished_seq_ranges: Vec<SeqRange>,
    pub freshness: Freshness,
    pub matched: usize,
    pub returned: usize,
    pub truncated: bool,
    pub check: Check,
}

#[derive(Debug, Serialize)]
pub struct DayRow {
    pub run_date: String,
    pub runs_expected: u32,
    pub runs_observed: u32,
    pub runs_shortfall: u32,
    /// Runs past what the schedule names. A retry produces this and it is not a defect: the
    /// envelope records an attempt to run, so two envelopes for a one-run day is one retry.
    pub runs_over_expected: u32,
    pub runs_unfinished: Vec<String>,
    pub per_source: Vec<SourceRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceRow {
    pub source: String,
    pub attempts: u32,
    pub ok: u32,
    pub empty: u32,
    pub failed: u32,
    pub interrupted: u32,
    pub in_flight: u32,
    pub opened_without_answered: Vec<String>,
    /// The ENOSPC and kill-mid-commit state: an attempt with an `Answered` row and no
    /// `Finalised` one. `items_returned` is not reported beside the ids because the row count
    /// arrives with the `Finalised` event, so in this state the store holds none.
    pub answered_without_finalised: Vec<String>,
    pub vendor_rows: i64,
    pub items_new: i64,
    pub rows_committed: i64,
}

#[derive(Debug, Serialize)]
pub struct Drift {
    pub source: String,
    pub target_window: String,
    pub distinct: usize,
    pub attempts: Vec<DriftAttempt>,
}

#[derive(Debug, Serialize)]
pub struct DriftAttempt {
    pub attempt_id: String,
    pub result_digest: String,
}

#[derive(Debug, Serialize)]
pub struct Unreadable {
    pub path: String,
    pub code: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Orphan {
    pub attempt_id: String,
    pub run_id: String,
    pub source: String,
}

#[derive(Debug, Serialize)]
pub struct Freshness {
    pub asking_clock: ClockAge,
    pub publication_clock: ClockAge,
}

/// One clock's newest value and its age against `Ask::at`. Two clocks and not one, because a
/// vendor that answers instantly with stale content moves one and not the other.
#[derive(Debug, Serialize)]
pub struct ClockAge {
    pub newest: Option<String>,
    pub age_seconds: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub asked: bool,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Serialize)]
pub struct Finding {
    pub code: &'static str,
    pub run_date: String,
    pub source: Option<String>,
    pub detail: String,
}

impl Report {
    /// Whether the reader could see everything the range covers. A run envelope it could not
    /// read, or an attempt whose run has no envelope, means it could not - and a `--check` over
    /// such a range cannot answer for it either way. The exit status that follows is main.rs's.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.unreadable_runs.is_empty() && self.attempts_without_a_run_envelope.is_empty()
    }

    /// Whether `--check`'s answer is no.
    #[must_use]
    pub fn answer_is_no(&self) -> bool {
        !self.check.findings.is_empty()
    }
}

/// One run envelope as it sits on disk, read here and written by `cmd_collect`: one struct, so
/// the writer and the reader cannot hold two versions of the file's shape. Why this reader landed
/// before that writer is the module header.
#[derive(Debug, Deserialize, Serialize)]
pub struct Envelope {
    pub schema: String,
    pub run_id: String,
    pub run_date: NaiveDate,
    pub expected_runs: u32,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// One attempt's state as the one GROUP BY returns it.
struct Grouped {
    run_id: String,
    source: String,
    state: String,
    attempts: u32,
    vendor_rows: i64,
    items_new: i64,
}

pub fn read(root: &Path, ask: &Ask) -> Result<Report, LedgerReadError> {
    if ask.from > ask.to {
        return Err(LedgerReadError::RangeInverted {
            from: ask.from,
            to: ask.to,
        });
    }
    let log = root.join(COMMIT_LOG);
    if !log.exists() {
        return Err(LedgerReadError::StoreAbsent(log));
    }
    let db = Connection::open_with_flags(&log, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let at = stamp(ask.at);
    let (envelopes, unreadable_runs) = read_envelopes(&root.join(RUNS_DIR), ask.at);
    let grouped = grouped_attempts(&db, &at)?;
    let in_flight = in_flight_attempts(&db, &at)?;
    let answers = answers_per_attempt(&db, &at)?;
    let known_runs: BTreeSet<&str> = envelopes.iter().map(|run| run.run_id.as_str()).collect();
    let attempts_without_a_run_envelope = orphans(&db, &at, &known_runs)?;
    // Indexed by run once rather than scanned per day: a year at the measured cadence is about
    // 164,000 attempt rows, and a 30-day range would otherwise walk all of them 30 times.
    let grouped = by_run(&grouped, |row| &row.run_id);
    let in_flight = by_run(&in_flight, |row| &row.0);
    let answers_by_run = by_run(&answers, |row| &row.run_id);

    let mut days = Vec::new();
    let mut matched = 0;
    let mut findings = Vec::new();
    let mut date = ask.from;
    while date <= ask.to {
        let mut row = day_row(date, &envelopes, &grouped, &in_flight, &answers_by_run, ask);
        matched += row.per_source.len();
        // The check runs over the whole day, before any cap: a cap that hides a finding is worse
        // than no cap, which is design.md section 4's reason for `truncated`.
        findings.extend(findings_for(&row));
        if let Some(limit) = ask.limit {
            row.per_source.truncate(limit);
        }
        days.push(row);
        date = match date.checked_add_days(Days::new(1)) {
            Some(next) => next,
            None => break,
        };
    }
    let returned = days.iter().map(|day| day.per_source.len()).sum();
    Ok(Report {
        schema: SCHEMA,
        at: at.clone(),
        from: ask.from.to_string(),
        to: ask.to.to_string(),
        source: ask.source.clone(),
        expected_runs: ask.expected_runs,
        limit: ask.limit,
        days,
        result_digest_drift: drift(&answers, ask.source.as_deref()),
        unreadable_runs,
        attempts_without_a_run_envelope,
        unpublished_seq_ranges: unpublished_seq_ranges(&db)?,
        freshness: freshness(&db, &at, ask.at)?,
        matched,
        returned,
        truncated: returned < matched,
        check: Check {
            asked: ask.check,
            findings: if ask.check { findings } else { Vec::new() },
        },
    })
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// Every envelope the directory holds, and a named code for each one that could not be read. A
/// strict reader that reports nothing because one file of thirty is truncated is disabled by the
/// very kill it exists to report.
fn read_envelopes(runs: &Path, at: DateTime<Utc>) -> (Vec<Envelope>, Vec<Unreadable>) {
    let mut taken = Vec::new();
    let mut unreadable = Vec::new();
    let mut paths: Vec<PathBuf> = match std::fs::read_dir(runs) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|kind| kind == "json"))
            .collect(),
        Err(_) => Vec::new(),
    };
    paths.sort();
    for path in paths {
        let named = format!(
            "{RUNS_DIR}/{}",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => {
                unreadable.push(Unreadable {
                    path: named,
                    code: IO_FAILED,
                });
                continue;
            }
        };
        match serde_json::from_str::<Envelope>(&text) {
            Ok(envelope) if envelope.schema != RUN_ENVELOPE_SCHEMA => unreadable.push(Unreadable {
                path: named,
                code: INPUT_SCHEMA_MISMATCH,
            }),
            Ok(envelope) => {
                if envelope.started_at <= at {
                    taken.push(envelope);
                }
            }
            Err(failure) => unreadable.push(Unreadable {
                path: named,
                code: code_of(&failure),
            }),
        }
    }
    (taken, unreadable)
}

/// A syntax or truncation failure is `input_unparseable`; a value of the wrong type or an absent
/// field is `input_schema_mismatch`. serde_json's own classification decides, so the two codes
/// cannot drift apart from what the parser actually refused.
fn code_of(failure: &serde_json::Error) -> &'static str {
    match failure.classify() {
        serde_json::error::Category::Data => INPUT_SCHEMA_MISMATCH,
        _ => INPUT_UNPARSEABLE,
    }
}

/// The one GROUP BY, across every target window. This is the question the colleague project
/// answers with three HTTP calls at decision time: "is this vendor down for every name, or is
/// this one name quiet?" - one aggregate over rows already on disk answers it for every name.
fn grouped_attempts(db: &Connection, at: &str) -> Result<Vec<Grouped>, LedgerReadError> {
    let mut statement = db.prepare(&format!(
        "SELECT run_id, source, COALESCE(outcome, '{IN_FLIGHT}') AS state, COUNT(*),
                SUM(COALESCE(vendor_rows, 0)), SUM(COALESCE(items_new, 0))
         FROM attempts WHERE opened_at <= ?1
         GROUP BY run_id, source, state
         ORDER BY run_id, source, state"
    ))?;
    let rows = statement
        .query_map([at], |row| {
            Ok(Grouped {
                run_id: row.get(0)?,
                source: row.get(1)?,
                state: row.get(2)?,
                attempts: row.get(3)?,
                vendor_rows: row.get(4)?,
                items_new: row.get(5)?,
            })
        })?
        .collect::<Result<Vec<Grouped>, _>>()?;
    Ok(rows)
}

/// `(run_id, source, attempt_id, answered)` for one attempt with no `Finalised` row.
type InFlightRow = (String, String, String, bool);
type InFlight = Vec<InFlightRow>;

fn in_flight_attempts(db: &Connection, at: &str) -> Result<InFlight, LedgerReadError> {
    let mut statement = db.prepare(
        "SELECT run_id, source, attempt_id, requested_at IS NOT NULL
         FROM attempts WHERE opened_at <= ?1 AND closed_at IS NULL
         ORDER BY attempt_id",
    )?;
    let rows = statement
        .query_map([at], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<Result<InFlight, _>>()?;
    Ok(rows)
}

/// What one closed attempt answered: the ids it committed, which is both the row count and the
/// input to its result digest.
struct Answered {
    attempt_id: String,
    run_id: String,
    source: String,
    target_window: String,
    item_ids: Vec<String>,
}

fn answers_per_attempt(db: &Connection, at: &str) -> Result<Vec<Answered>, LedgerReadError> {
    let mut statement = db.prepare(
        "SELECT a.attempt_id, a.run_id, a.source, a.target_window, i.item_id
         FROM attempts a LEFT JOIN items i
           ON i.attempt_id = a.attempt_id AND i.committed_at <= ?1
         WHERE a.opened_at <= ?1 AND a.closed_at IS NOT NULL
         ORDER BY a.attempt_id, i.item_id",
    )?;
    let mut answered: Vec<Answered> = Vec::new();
    let mut rows = statement.query([at])?;
    while let Some(row) = rows.next()? {
        let attempt_id: String = row.get(0)?;
        let item_id: Option<String> = row.get(4)?;
        match answered.last_mut() {
            Some(last) if last.attempt_id == attempt_id => last.item_ids.extend(item_id),
            _ => answered.push(Answered {
                attempt_id,
                run_id: row.get(1)?,
                source: row.get(2)?,
                target_window: row.get(3)?,
                item_ids: item_id.into_iter().collect(),
            }),
        }
    }
    Ok(answered)
}

fn orphans(
    db: &Connection,
    at: &str,
    known_runs: &BTreeSet<&str>,
) -> Result<Vec<Orphan>, LedgerReadError> {
    let mut statement = db.prepare(
        "SELECT attempt_id, run_id, source FROM attempts WHERE opened_at <= ?1 ORDER BY attempt_id",
    )?;
    let rows = statement
        .query_map([at], |row| {
            Ok(Orphan {
                attempt_id: row.get(0)?,
                run_id: row.get(1)?,
                source: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<Orphan>, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|orphan| !known_runs.contains(orphan.run_id.as_str()))
        .collect())
}

fn freshness(
    db: &Connection,
    at: &str,
    cutoff: DateTime<Utc>,
) -> Result<Freshness, LedgerReadError> {
    let newest_request: Option<String> = db.query_row(
        "SELECT MAX(requested_at) FROM attempts WHERE requested_at <= ?1",
        [at],
        |row| row.get(0),
    )?;
    // Both halves of the visibility question, which is the one `pit` exists for: a publication
    // value we had not read yet is not freshness.
    let newest_publication: Option<String> = db.query_row(
        "SELECT MAX(valid_at) FROM items WHERE valid_at <= ?1 AND observed_at <= ?1",
        [at],
        |row| row.get(0),
    )?;
    Ok(Freshness {
        asking_clock: age(newest_request, cutoff),
        publication_clock: age(newest_publication, cutoff),
    })
}

/// Against `cutoff` and never against the process clock, which is what makes two runs of one
/// fixture byte-identical.
fn age(newest: Option<String>, cutoff: DateTime<Utc>) -> ClockAge {
    let age_seconds = newest.as_deref().and_then(|value| {
        DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|at| (cutoff - at.with_timezone(&Utc)).num_seconds())
    });
    ClockAge {
        newest,
        age_seconds,
    }
}

/// The rows of one table, reachable by the run they belong to.
fn by_run<T>(rows: &[T], run_of: impl Fn(&T) -> &String) -> BTreeMap<&str, Vec<&T>> {
    let mut indexed: BTreeMap<&str, Vec<&T>> = BTreeMap::new();
    for row in rows {
        indexed.entry(run_of(row).as_str()).or_default().push(row);
    }
    indexed
}

fn of_the_day<'row, T>(
    indexed: &BTreeMap<&'row str, Vec<&'row T>>,
    runs: &BTreeSet<&str>,
) -> Vec<&'row T> {
    runs.iter()
        .filter_map(|run| indexed.get(run))
        .flatten()
        .copied()
        .collect()
}

fn day_row(
    date: NaiveDate,
    envelopes: &[Envelope],
    grouped: &BTreeMap<&str, Vec<&Grouped>>,
    in_flight: &BTreeMap<&str, Vec<&InFlightRow>>,
    answers: &BTreeMap<&str, Vec<&Answered>>,
    ask: &Ask,
) -> DayRow {
    let today: Vec<&Envelope> = envelopes
        .iter()
        .filter(|envelope| envelope.run_date == date)
        .collect();
    let runs: BTreeSet<&str> = today.iter().map(|run| run.run_id.as_str()).collect();
    let observed = u32::try_from(today.len()).unwrap_or(u32::MAX);
    let expected = today
        .iter()
        .map(|envelope| envelope.expected_runs)
        .chain(ask.expected_runs)
        .max()
        .unwrap_or_default();
    let mut by_source: BTreeMap<String, SourceRow> = BTreeMap::new();
    for entry in of_the_day(grouped, &runs) {
        if !ask.wants(&entry.source) {
            continue;
        }
        let row = by_source
            .entry(entry.source.clone())
            .or_insert_with(|| SourceRow::named(&entry.source));
        row.attempts += entry.attempts;
        row.vendor_rows += entry.vendor_rows;
        row.items_new += entry.items_new;
        match entry.state.as_str() {
            OUTCOME_OK => row.ok += entry.attempts,
            OUTCOME_EMPTY => row.empty += entry.attempts,
            OUTCOME_FAILED => row.failed += entry.attempts,
            OUTCOME_INTERRUPTED => row.interrupted += entry.attempts,
            _ => row.in_flight += entry.attempts,
        }
    }
    for (_, source, attempt_id, answered) in of_the_day(in_flight, &runs) {
        if !ask.wants(source) {
            continue;
        }
        let row = by_source
            .entry(source.clone())
            .or_insert_with(|| SourceRow::named(source));
        if *answered {
            row.answered_without_finalised.push(attempt_id.clone());
        } else {
            row.opened_without_answered.push(attempt_id.clone());
        }
    }
    for answer in of_the_day(answers, &runs) {
        if !ask.wants(&answer.source) {
            continue;
        }
        let row = by_source
            .entry(answer.source.clone())
            .or_insert_with(|| SourceRow::named(&answer.source));
        row.rows_committed += i64::try_from(answer.item_ids.len()).unwrap_or(i64::MAX);
    }
    DayRow {
        run_date: date.to_string(),
        runs_expected: expected,
        runs_observed: observed,
        runs_shortfall: expected.saturating_sub(observed),
        runs_over_expected: observed.saturating_sub(expected),
        runs_unfinished: today
            .iter()
            .filter(|envelope| envelope.finished_at.is_none())
            .map(|envelope| envelope.run_id.clone())
            .collect(),
        per_source: by_source.into_values().collect(),
    }
}

impl Ask {
    fn wants(&self, source: &str) -> bool {
        self.source.as_deref().is_none_or(|only| only == source)
    }
}

impl SourceRow {
    fn named(source: &str) -> Self {
        Self {
            source: source.to_owned(),
            attempts: 0,
            ok: 0,
            empty: 0,
            failed: 0,
            interrupted: 0,
            in_flight: 0,
            opened_without_answered: Vec::new(),
            answered_without_finalised: Vec::new(),
            vendor_rows: 0,
            items_new: 0,
            rows_committed: 0,
        }
    }
}

fn findings_for(day: &DayRow) -> Vec<Finding> {
    let mut findings = Vec::new();
    if day.runs_shortfall > 0 {
        findings.push(Finding {
            code: RUNS_BELOW_EXPECTED,
            run_date: day.run_date.clone(),
            source: None,
            detail: format!(
                "{} runs observed against {} expected",
                day.runs_observed, day.runs_expected
            ),
        });
    }
    for row in &day.per_source {
        if row.failed + row.interrupted > 0 {
            findings.push(Finding {
                code: HOLDS_A_FAILURE,
                run_date: day.run_date.clone(),
                source: Some(row.source.clone()),
                detail: format!("{} failed, {} interrupted", row.failed, row.interrupted),
            });
        }
        if row.rows_committed == 0 && row.vendor_rows > 0 {
            findings.push(Finding {
                code: ROWS_ZERO_AGAINST_VENDOR_ROWS,
                run_date: day.run_date.clone(),
                source: Some(row.source.clone()),
                detail: format!("{} vendor rows and no item row committed", row.vendor_rows),
            });
        }
    }
    findings
}

/// The digest of each closed attempt's returned id set, grouped per (source, target window), and
/// reported only where the attempts disagree.
fn drift(answers: &[Answered], only: Option<&str>) -> Vec<Drift> {
    let mut groups: BTreeMap<(String, String), Vec<DriftAttempt>> = BTreeMap::new();
    for answer in answers {
        if only.is_some_and(|source| source != answer.source) {
            continue;
        }
        let ids: Vec<&str> = answer.item_ids.iter().map(String::as_str).collect();
        let Ok(digest) = canon::digest::result_digest(&ids) else {
            continue;
        };
        groups
            .entry((answer.source.clone(), answer.target_window.clone()))
            .or_default()
            .push(DriftAttempt {
                attempt_id: answer.attempt_id.clone(),
                result_digest: digest.hex().to_owned(),
            });
    }
    groups
        .into_iter()
        .filter_map(|((source, target_window), attempts)| {
            let distinct: BTreeSet<&str> = attempts
                .iter()
                .map(|attempt| attempt.result_digest.as_str())
                .collect();
            (distinct.len() > 1).then_some(Drift {
                source,
                target_window,
                distinct: distinct.len(),
                attempts,
            })
        })
        .collect()
}

/// The human rendering of the same object the JSON carries, so neither can hold a fact the other
/// does not.
pub fn render_human(report: &Report, out: &mut impl std::io::Write) -> std::io::Result<()> {
    writeln!(out, "at               {}", report.at)?;
    writeln!(out, "range            {} .. {}", report.from, report.to)?;
    for day in &report.days {
        writeln!(
            out,
            "{}  runs {}/{} short {} over {} unfinished {}",
            day.run_date,
            day.runs_observed,
            day.runs_expected,
            day.runs_shortfall,
            day.runs_over_expected,
            day.runs_unfinished.len()
        )?;
        for row in &day.per_source {
            writeln!(
                out,
                "  {:<14} ok {} empty {} failed {} interrupted {} in-flight {} \
                 vendor_rows {} items_new {} rows {}",
                row.source,
                row.ok,
                row.empty,
                row.failed,
                row.interrupted,
                row.in_flight,
                row.vendor_rows,
                row.items_new,
                row.rows_committed
            )?;
        }
    }
    writeln!(
        out,
        "rows             matched {} returned {} truncated {}",
        report.matched, report.returned, report.truncated
    )?;
    writeln!(
        out,
        "asking clock     {} ({} s)",
        report
            .freshness
            .asking_clock
            .newest
            .as_deref()
            .unwrap_or("-"),
        report
            .freshness
            .asking_clock
            .age_seconds
            .unwrap_or_default()
    )?;
    writeln!(
        out,
        "publication clk  {} ({} s)",
        report
            .freshness
            .publication_clock
            .newest
            .as_deref()
            .unwrap_or("-"),
        report
            .freshness
            .publication_clock
            .age_seconds
            .unwrap_or_default()
    )?;
    writeln!(
        out,
        "digest drift     {} (source, window) pair(s)",
        report.result_digest_drift.len()
    )?;
    writeln!(
        out,
        "unpublished seq  {} span(s)",
        report.unpublished_seq_ranges.len()
    )?;
    for unreadable in &report.unreadable_runs {
        writeln!(
            out,
            "unreadable       {} {}",
            unreadable.path, unreadable.code
        )?;
    }
    for orphan in &report.attempts_without_a_run_envelope {
        writeln!(
            out,
            "no run envelope  {} {}",
            orphan.run_id, orphan.attempt_id
        )?;
    }
    for finding in &report.check.findings {
        writeln!(
            out,
            "check            {} {} {}",
            finding.run_date, finding.code, finding.detail
        )?;
    }
    Ok(())
}
