//! `newsvault ledger` - the coverage reader, written before the first collector run. Why `--at`
//! is required is architecture.md section 4's last row; the ledger row itself is section 3.2.
//! Neither is restated here.
//!
//! The reader comes first on the predecessor's evidence: its lake had no reader in code, and the
//! divergence between what it stored and what production used surfaced six months later as
//! 11,054 rows against a view holding 5,331.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Days, NaiveDate, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use vault_core::sqlite::read_only;

use crate::cmd_collect::Registered;
use crate::cmd_robots::ROBOTS_SOURCE;
use crate::cmd_roster::ROSTER_SOURCE;
use crate::store::{
    coverage_up_to, newest_visible_publication, require_current_version, unpublished_seq_ranges,
    SeqRange, WindowCoverage, COMMIT_LOG, REDIRECTS, RUNS_DIR,
};
use news_provider::{Closing, DeferReason, Verdict, CARRY_LIMIT};
use open_web_feed::Continuity;

pub const SCHEMA: &str = "newsvault.ledger/1";
pub const RUN_ENVELOPE_SCHEMA: &str = "newsvault.run/1";
/// The `refused` mark of a run file whose run another run's lease kept out (S7-D9).
pub const LEASE_HELD: &str = "lease_held";
/// Section 11.1 Pace and section 11.2's stop deadline: at most half a day, per host and per run.
pub const BUSY_BOUND: f64 = 0.5;
const SECONDS_A_DAY: f64 = 86_400.0;
const MICROS_A_SECOND: f64 = 1_000_000.0;
const IN_FLIGHT: &str = "in_flight";
const OUTCOME_OK: &str = "ok";
const OUTCOME_EMPTY: &str = "empty";
const OUTCOME_FAILED: &str = "failed";
const OUTCOME_INTERRUPTED: &str = "interrupted";
const OUTCOME_NOT_SENT: &str = "not_sent";

/// The `--check` answers, each the code of a finding: `coverage_incomplete` since 2026-11-13
/// (§8 row R20), the two busy bounds since 2026-11-18 (S7-D11). No asking-clock age: it needs a
/// number from the collect policy, whose first producer is a later commit.
const RUNS_BELOW_EXPECTED: &str = "runs_below_expected";
const HOLDS_A_FAILURE: &str = "holds_a_failure";
const ROWS_ZERO_AGAINST_VENDOR_ROWS: &str = "rows_committed_zero_against_vendor_rows";
const COVERAGE_INCOMPLETE: &str = "coverage_incomplete";
const WALL_TIME_OVER_BOUND: &str = "wall_time_over_bound";
const HOST_BUSY_OVER_BOUND: &str = "host_busy_over_bound";
/// S7-D8 (owner, 2026-11-17): a deferred document the carry closed after 24 h, never asked.
const DEFERRED_EXPIRED: &str = "deferred_expired";

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
    /// Per (source, target window) an index attempt read: how many accessions it planned to cover
    /// against how many distinct stories the store now holds. `covered < planned` is a gap a
    /// `vendor_archive` re-run closes (§8 row R20), and under `--check` it is a finding.
    pub coverage: Vec<WindowCoverage>,
    pub unreadable_runs: Vec<Unreadable>,
    pub attempts_without_a_run_envelope: Vec<Orphan>,
    pub unpublished_seq_ranges: Vec<SeqRange>,
    pub freshness: Freshness,
    /// The share of a day `wall_share` and each host's `share` are read against.
    pub busy_bound: f64,
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
    /// S7-D9: collect runs another run's lease kept out, left out of `runs_observed`.
    pub runs_lease_held: u32,
    /// S7-D11: the observed runs' summed wall time, and its share of 86,400 s against
    /// `busy_bound`; a run file from before 2026-11-18 adds nothing.
    pub wall_s: f64,
    pub wall_share: f64,
    /// Per host, the observed runs' summed paced waits plus request times, in host order.
    pub busy: Vec<HostBusyRow>,
    /// S7-D7: answers whose `Retry-After` the observed runs held to the 24 h ceiling.
    pub holds_capped: u32,
    pub per_source: Vec<SourceRow>,
    /// §8 row R22: per run, source and host of the day's runs, in that order.
    pub listing_latency: Vec<LatencyRow>,
}

/// One host's Document attempts in one run, each from its `received_at` back to the
/// `received_at` of the index attempt `attempts.listed_by` names (§8 row R22).
#[derive(Debug, Clone, Serialize)]
pub struct LatencyRow {
    pub run_id: String,
    pub source: String,
    /// `news_provider::host_of` of the attempt's payload url; null when it stored none.
    pub host: Option<String>,
    /// Listed by an index attempt of this run.
    pub fresh: Latency,
    /// Listed by an earlier run's index attempt, then deferred and carried (S7-D5).
    pub carried: Latency,
    /// Answered attempts naming no listing: a walk id, or one opened before user_version 15.
    pub unlinked: i64,
}

/// Nearest-rank percentiles, in seconds: the value at rank ⌈p × n / 100⌉ of the n sorted ones.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Latency {
    pub documents: i64,
    pub p50_s: Option<f64>,
    pub p95_s: Option<f64>,
    pub max_s: Option<f64>,
}

impl Latency {
    fn of(mut seconds: Vec<f64>) -> Self {
        seconds.sort_by(f64::total_cmp);
        let rank = |percent: usize| {
            let rank = (percent * seconds.len()).div_ceil(100).max(1);
            seconds.get(rank - 1).copied()
        };
        Self {
            documents: i64::try_from(seconds.len()).unwrap_or(i64::MAX),
            p50_s: rank(50),
            p95_s: rank(95),
            max_s: seconds.last().copied(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HostBusyRow {
    pub host: Option<String>,
    pub busy_s: f64,
    pub share: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceRow {
    pub source: String,
    pub attempts: u32,
    pub ok: u32,
    pub empty: u32,
    pub failed: u32,
    pub interrupted: u32,
    /// Attempts the send gate refused before the socket (section 3.2): opened, never sent, and
    /// no failure of the vendor's, so `--check` does not count them.
    pub not_sent: u32,
    pub in_flight: u32,
    pub opened_without_answered: Vec<String>,
    /// The ENOSPC and kill-mid-commit state: an attempt with an `Answered` row and no
    /// `Finalised` one. `items_returned` is not reported beside the ids because the row count
    /// arrives with the `Finalised` event, so in this state the store holds none.
    pub answered_without_finalised: Vec<String>,
    pub vendor_rows: i64,
    /// Section 11.1's `held`: vendor rows a live index left unfetched because the store already
    /// held their story. Subtracted from `vendor_rows` before the zero-rows finding.
    pub held: i64,
    /// Section 11.1: unusable rows the index row decided. Such a story is fetched again by every
    /// poll, so this counts one per fetch of such a story, the first fetch included. Not
    /// subtracted before the zero-rows finding.
    pub listing_unusable: i64,
    /// P10 (section 3.9): attempts whose body is stored cut at `body_limit`, from
    /// `payloads.truncated`. 0 for an attempt closed before user_version 6, which stored no
    /// over-limit body, so 0 there does not mean none was over.
    pub bodies_truncated: i64,
    /// Section 11.1: permanent document failures stored cut at or above the source's current body
    /// limit (`Registered::body_limit`), not 3xx: their stories are held until the limit is
    /// raised above the cut. 0 for a source with no registered limit.
    pub cuts_held_at_limit: i64,
    /// Section 11.1: permanent document failures stored cut below the current limit, closed more
    /// than `CARRY_LIMIT` before `--at`, still their story's latest attempt, and with no open
    /// `deferred` row of the story younger than `CARRY_LIMIT` at `--at` (whose carry would ask it):
    /// released, too old for `carry` to ask, and read again only if an index lists the story.
    pub cuts_released_unreachable: i64,
    /// Section 8 row R15, decision 10: `relisted` rows, one per live index attempt and held story
    /// listed under another minute than its kept item's, so k polls count k. 0 also before
    /// user_version 7 and for a listing R15 does not compare, so 0 does not mean no dispute.
    pub relisted_disputed: i64,
    /// Decision x: held stories live index attempts compared with a kept minute, equal or not,
    /// from `attempts.relisted_compared`; `relisted_disputed` of them disagreed. 0 also for an
    /// attempt closed before user_version 8.
    pub relisted_compared: i64,
    /// Owner decision 2026-11-17 (section 11.1): index rows not followed because the host's
    /// pinned robots.txt disallows their url, from `attempts.robots_disallowed`; never asked, so
    /// no attempt row. 0 also for an attempt closed before user_version 9.
    pub robots_disallowed: i64,
    /// Section 11.2: paged index polls by `attempts.continuity`. Every arm but `complete` left a
    /// stretch of the index not looked at; a poll that read no usable page is in none.
    pub continuity: ContinuityCounts,
    /// Section 11.2: those polls' page boundaries that shared no key, one count per
    /// `open_web_feed::Generation` from `attempts.boundary_*`; 0 before user_version 11.
    pub boundaries: BoundaryCounts,
    /// Section 11.2: walked polls' ids by answer, from `attempts.walk_*`; 0 before user_version 12.
    pub walk: WalkCounts,
    /// S7-D4: requests the source's runs did not ask, from `deferred`, one count per reason.
    pub deferred: DeferredCounts,
    /// S7-D5: `deferred` rows the source's runs closed, from `deferred_closed`, one count per
    /// `news_provider::Closing`; a row counts on the day of the run that closed it.
    pub carried: CarriedCounts,
    pub items_new: i64,
    pub rows_committed: i64,
}

/// One count per `open_web_feed::Continuity` arm.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct ContinuityCounts {
    pub complete: i64,
    pub boundary_no_shared_key: i64,
    pub no_previous_key: i64,
    pub first_poll: i64,
    pub next_page_unread: i64,
}

impl ContinuityCounts {
    const fn of(counts: [i64; 5]) -> Self {
        let [complete, boundary_no_shared_key, no_previous_key, first_poll, next_page_unread] =
            counts;
        Self {
            complete,
            boundary_no_shared_key,
            no_previous_key,
            first_poll,
            next_page_unread,
        }
    }

    const fn add(self, other: Self) -> Self {
        Self {
            complete: self.complete + other.complete,
            boundary_no_shared_key: self.boundary_no_shared_key + other.boundary_no_shared_key,
            no_previous_key: self.no_previous_key + other.no_previous_key,
            first_poll: self.first_poll + other.first_poll,
            next_page_unread: self.next_page_unread + other.next_page_unread,
        }
    }
}

/// Walked polls (a non-NULL `walk_unread`) by continuity, and their ids answered 410 (`gone`,
/// owner decision 2026-11-17), moved to another language and not fetched (`other_language`,
/// PG-D4) or left with no answer (`unread`), summed over polls.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct WalkCounts {
    pub complete: i64,
    pub first_poll: i64,
    pub id_unread: i64,
    pub range_unread: i64,
    pub gone: i64,
    pub other_language: i64,
    pub unread: i64,
}

impl WalkCounts {
    const fn add(self, other: Self) -> Self {
        Self {
            complete: self.complete + other.complete,
            first_poll: self.first_poll + other.first_poll,
            id_unread: self.id_unread + other.id_unread,
            range_unread: self.range_unread + other.range_unread,
            gone: self.gone + other.gone,
            other_language: self.other_language + other.other_language,
            unread: self.unread + other.unread,
        }
    }
}

/// One count per `news_provider::DeferReason`.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct DeferredCounts {
    pub run_deadline: i64,
    pub host_held: i64,
    pub follow_up_cap: i64,
    pub gate_refused: i64,
}

impl DeferredCounts {
    const fn add(self, other: Self) -> Self {
        Self {
            run_deadline: self.run_deadline + other.run_deadline,
            host_held: self.host_held + other.host_held,
            follow_up_cap: self.follow_up_cap + other.follow_up_cap,
            gate_refused: self.gate_refused + other.gate_refused,
        }
    }
}

/// One count per `news_provider::Closing` name.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct CarriedCounts {
    pub answered: i64,
    pub held: i64,
    pub expired: i64,
}

impl CarriedCounts {
    const fn add(self, other: Self) -> Self {
        Self {
            answered: self.answered + other.answered,
            held: self.held + other.held,
            expired: self.expired + other.expired,
        }
    }
}

/// One count per `open_web_feed::Generation` label.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct BoundaryCounts {
    pub same_generation: i64,
    pub older_generation: i64,
    pub newer_generation: i64,
    pub generation_unknown: i64,
}

impl BoundaryCounts {
    const fn add(self, other: Self) -> Self {
        Self {
            same_generation: self.same_generation + other.same_generation,
            older_generation: self.older_generation + other.older_generation,
            newer_generation: self.newer_generation + other.newer_generation,
            generation_unknown: self.generation_unknown + other.generation_unknown,
        }
    }
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
    /// The source a `collect` run asked, which `--source` filters busy time by; absent in a file
    /// written before 2026-11-18 and in a robots refresh's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub expected_runs: u32,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Absent in every file written before `robots_refresh` existed, so absent reads as `collect`
    /// and a `collect` envelope is written without it.
    #[serde(default, skip_serializing_if = "RunKind::is_collect")]
    pub kind: RunKind,
    /// S7-D9: `lease_held` on a run that never got the lease; such a run is not observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
    /// S7-D11: the run's wall time by the clock, start to finish; absent before 2026-11-18.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub busy: Vec<RunBusy>,
    /// S7-D7: each answer whose `Retry-After` was cut to the 24 h ceiling.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holds_capped: Vec<RunHold>,
}

/// One host's paced waits plus request times in one run (section 11.1 Pace); `host` is `None`
/// for urls no host keys.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RunBusy {
    pub host: Option<String>,
    pub busy_s: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RunHold {
    pub host: String,
    pub retry_after: String,
    pub held_until: DateTime<Utc>,
}

/// Which command wrote a run envelope. Only `collect` runs are due, counted and reported per day;
/// every kind is a known run for an attempt (architecture.md section 10, robots).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    #[default]
    Collect,
    RobotsRefresh,
    RosterFetch,
}

impl RunKind {
    #[must_use]
    pub const fn is_collect(&self) -> bool {
        matches!(self, Self::Collect)
    }
}

/// One attempt's state as the one GROUP BY returns it.
struct Grouped {
    run_id: String,
    source: String,
    state: String,
    attempts: u32,
    vendor_rows: i64,
    held: i64,
    listing_unusable: i64,
    bodies_truncated: i64,
    cuts_held_at_limit: i64,
    cuts_released_unreachable: i64,
    relisted_disputed: i64,
    relisted_compared: i64,
    robots_disallowed: i64,
    continuity: ContinuityCounts,
    boundaries: BoundaryCounts,
    walk: WalkCounts,
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
    let db = read_only(&log)?;
    require_current_version(&db)?;
    let at = stamp(ask.at);
    let (envelopes, unreadable_runs) = read_envelopes(&root.join(RUNS_DIR), ask.at);
    let grouped = grouped_attempts(&db, &at, &stamp(ask.at - CARRY_LIMIT))?;
    let in_flight = in_flight_attempts(&db, &at)?;
    let answers = answers_per_attempt(&db, &at)?;
    let deferred = deferred_per_run(&db, &at)?;
    let carried = carried_per_run(&db, &at)?;
    let latency = latency_per_run(&db, &at)?;
    let known_runs: BTreeSet<&str> = envelopes.iter().map(|run| run.run_id.as_str()).collect();
    let attempts_without_a_run_envelope = orphans(&db, &at, &known_runs)?;
    // Indexed by run once rather than scanned per day: a year at the measured cadence is about
    // 164,000 attempt rows, and a 30-day range would otherwise walk all of them 30 times.
    let grouped = by_run(&grouped, |row| &row.run_id);
    let in_flight = by_run(&in_flight, |row| &row.0);
    let answers_by_run = by_run(&answers, |row| &row.run_id);
    let deferred = by_run(&deferred, |row| &row.run_id);
    let carried = by_run(&carried, |row| &row.run_id);
    let latency = by_run(&latency, |row| &row.run_id);

    let mut days = Vec::new();
    let mut matched = 0;
    let mut findings = Vec::new();
    let mut date = ask.from;
    while date <= ask.to {
        let read = Read {
            grouped: &grouped,
            in_flight: &in_flight,
            answers: &answers_by_run,
            deferred: &deferred,
            carried: &carried,
            latency: &latency,
        };
        let mut row = day_row(date, &envelopes, &read, ask);
        matched += row.per_source.len();
        // The check runs over the whole day, before any cap: a cap that hides a finding is worse
        // than no cap (architecture.md section 8 row R16), which is the reason for `truncated`.
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
    // Coverage is per (source, target window) and not per run day, like the drift section: it is a
    // whole-store question up to the cutoff, independent of the --from/--to range. The finding is
    // added before the Check is built so --check surfaces it; a cap over the day rows never hides
    // it, because it is not a day row.
    let mut coverage = coverage_up_to(&db, &at)?;
    coverage.retain(|row| ask.wants(&row.source));
    for row in &coverage {
        if row.covered < row.planned {
            findings.push(Finding {
                code: COVERAGE_INCOMPLETE,
                run_date: row.target_window.clone(),
                source: Some(row.source.clone()),
                detail: format!("{} of {} accessions covered", row.covered, row.planned),
            });
        }
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
        coverage,
        unreadable_runs,
        attempts_without_a_run_envelope,
        unpublished_seq_ranges: unpublished_seq_ranges(&db)?,
        freshness: freshness(&db, &at, ask.at)?,
        busy_bound: BUSY_BOUND,
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
fn grouped_attempts(
    db: &Connection,
    at: &str,
    carried_since: &str,
) -> Result<Vec<Grouped>, LedgerReadError> {
    let [complete, boundary, no_previous, first, unread, id_unread, range_unread] =
        Continuity::ALL.map(Continuity::name);
    let limits = Registered::ALL
        .map(|entry| format!("('{}', {})", entry.source(), entry.body_limit()))
        .join(", ");
    let permanent = Verdict::PermanentFailure.name();
    let (low, high) = (REDIRECTS.start(), REDIRECTS.end());
    let cut_below = |compare: &str| {
        format!(
            "EXISTS (SELECT 1 FROM payloads p JOIN blobs b ON b.digest = p.raw_blob
                      JOIN limits l ON l.source = attempts.source
                     WHERE p.attempt_id = attempts.attempt_id AND p.truncated = 1
                       AND b.bytes {compare} l.body_limit)"
        )
    };
    let (held_cut, released_cut) = (cut_below(">="), cut_below("<"));
    let mut statement = db.prepare(&format!(
        "WITH limits (source, body_limit) AS (VALUES {limits})
         SELECT run_id, source, COALESCE(outcome, '{IN_FLIGHT}') AS state, COUNT(*),
                SUM(COALESCE(vendor_rows, 0)), SUM(COALESCE(items_new, 0)),
                SUM(CASE WHEN vendor_rows IS NOT NULL THEN COALESCE(held, 0) ELSE 0 END),
                SUM(CASE WHEN vendor_rows IS NOT NULL THEN COALESCE(listing_unusable, 0) ELSE 0 END),
                SUM(COALESCE((SELECT p.truncated FROM payloads p
                               WHERE p.attempt_id = attempts.attempt_id), 0)),
                SUM((SELECT COUNT(*) FROM relisted r WHERE r.attempt_id = attempts.attempt_id)),
                SUM(COALESCE(relisted_compared, 0)),
                SUM(COALESCE(robots_disallowed, 0)),
                SUM(continuity IS '{complete}' AND walk_unread IS NULL),
                SUM(continuity IS '{boundary}' AND walk_unread IS NULL),
                SUM(continuity IS '{no_previous}' AND walk_unread IS NULL),
                SUM(continuity IS '{first}' AND walk_unread IS NULL),
                SUM(continuity IS '{unread}' AND walk_unread IS NULL),
                SUM(COALESCE(boundary_same_generation, 0)),
                SUM(COALESCE(boundary_older_generation, 0)),
                SUM(COALESCE(boundary_newer_generation, 0)),
                SUM(COALESCE(boundary_generation_unknown, 0)),
                SUM(continuity IS '{complete}' AND walk_unread IS NOT NULL),
                SUM(continuity IS '{first}' AND walk_unread IS NOT NULL),
                SUM(continuity IS '{id_unread}' AND walk_unread IS NOT NULL),
                SUM(continuity IS '{range_unread}' AND walk_unread IS NOT NULL),
                SUM(COALESCE(walk_gone, 0)),
                SUM(COALESCE(walk_other_language, 0)),
                SUM(COALESCE(walk_unread, 0)),
                SUM(CASE WHEN story_id IS NOT NULL AND verdict = '{permanent}'
                          AND COALESCE(transport_status, 0) NOT BETWEEN {low} AND {high}
                          AND {held_cut} THEN 1 ELSE 0 END),
                SUM(CASE WHEN story_id IS NOT NULL AND verdict = '{permanent}'
                          AND closed_at <= ?2 AND {released_cut}
                          AND NOT EXISTS (SELECT 1 FROM attempts later
                                           WHERE later.source = attempts.source
                                             AND later.story_id = attempts.story_id
                                             AND later.rowid > attempts.rowid
                                             AND later.opened_at <= ?1)
                          AND COALESCE((SELECT MIN(d.deferred_at) FROM deferred d
                                         WHERE d.source = attempts.source
                                           AND d.story_id = attempts.story_id
                                           AND d.deferred_at <= ?1
                                           AND NOT EXISTS (SELECT 1 FROM deferred_closed c
                                                            WHERE c.deferred_id = d.deferred_id
                                                              AND c.closed_at <= ?1)), ?2) <= ?2
                     THEN 1 ELSE 0 END)
         FROM attempts WHERE opened_at <= ?1
         GROUP BY run_id, source, state
         ORDER BY run_id, source, state"
    ))?;
    let rows = statement
        .query_map([at, carried_since], |row| {
            Ok(Grouped {
                run_id: row.get(0)?,
                source: row.get(1)?,
                state: row.get(2)?,
                attempts: row.get(3)?,
                vendor_rows: row.get(4)?,
                items_new: row.get(5)?,
                held: row.get(6)?,
                listing_unusable: row.get(7)?,
                bodies_truncated: row.get(8)?,
                cuts_held_at_limit: row.get(28)?,
                cuts_released_unreachable: row.get(29)?,
                relisted_disputed: row.get(9)?,
                relisted_compared: row.get(10)?,
                robots_disallowed: row.get(11)?,
                continuity: ContinuityCounts::of([
                    row.get(12)?,
                    row.get(13)?,
                    row.get(14)?,
                    row.get(15)?,
                    row.get(16)?,
                ]),
                boundaries: BoundaryCounts {
                    same_generation: row.get(17)?,
                    older_generation: row.get(18)?,
                    newer_generation: row.get(19)?,
                    generation_unknown: row.get(20)?,
                },
                walk: WalkCounts {
                    complete: row.get(21)?,
                    first_poll: row.get(22)?,
                    id_unread: row.get(23)?,
                    range_unread: row.get(24)?,
                    gone: row.get(25)?,
                    other_language: row.get(26)?,
                    unread: row.get(27)?,
                },
            })
        })?
        .collect::<Result<Vec<Grouped>, _>>()?;
    Ok(rows)
}

/// One run's `deferred` rows of one source, counted per reason.
struct DeferredGroup {
    run_id: String,
    source: String,
    counts: DeferredCounts,
}

fn deferred_per_run(db: &Connection, at: &str) -> Result<Vec<DeferredGroup>, LedgerReadError> {
    let mut statement = db.prepare(
        "SELECT run_id, source, SUM(reason = ?2), SUM(reason = ?3), SUM(reason = ?4),
                SUM(reason = ?5)
           FROM deferred WHERE deferred_at <= ?1
          GROUP BY run_id, source ORDER BY run_id, source",
    )?;
    let reasons = params![
        at,
        DeferReason::RunDeadline.name(),
        DeferReason::HostHeld.name(),
        DeferReason::FollowUpCap.name(),
        DeferReason::GateRefused.name()
    ];
    let rows = statement
        .query_map(reasons, |row| {
            Ok(DeferredGroup {
                run_id: row.get(0)?,
                source: row.get(1)?,
                counts: DeferredCounts {
                    run_deadline: row.get(2)?,
                    host_held: row.get(3)?,
                    follow_up_cap: row.get(4)?,
                    gate_refused: row.get(5)?,
                },
            })
        })?
        .collect::<Result<Vec<DeferredGroup>, _>>()?;
    Ok(rows)
}

/// One run's `deferred_closed` rows of one source, counted per `Closing`.
struct CarriedGroup {
    run_id: String,
    source: String,
    counts: CarriedCounts,
}

fn carried_per_run(db: &Connection, at: &str) -> Result<Vec<CarriedGroup>, LedgerReadError> {
    let [answered, held, expired] = Closing::NAMES;
    let mut statement = db.prepare(
        "SELECT c.run_id, d.source, SUM(c.closed_as = ?2), SUM(c.closed_as = ?3),
                SUM(c.closed_as = ?4)
           FROM deferred_closed c JOIN deferred d ON d.deferred_id = c.deferred_id
          WHERE c.closed_at <= ?1
          GROUP BY c.run_id, d.source ORDER BY c.run_id, d.source",
    )?;
    let rows = statement
        .query_map(params![at, answered, held, expired], |row| {
            Ok(CarriedGroup {
                run_id: row.get(0)?,
                source: row.get(1)?,
                counts: CarriedCounts {
                    answered: row.get(2)?,
                    held: row.get(3)?,
                    expired: row.get(4)?,
                },
            })
        })?
        .collect::<Result<Vec<CarriedGroup>, _>>()?;
    Ok(rows)
}

fn latency_per_run(db: &Connection, at: &str) -> Result<Vec<LatencyRow>, LedgerReadError> {
    let mut statement = db.prepare(
        "SELECT d.run_id, d.source, p.url, d.received_at, l.received_at, l.run_id
           FROM attempts d
           LEFT JOIN payloads p ON p.attempt_id = d.attempt_id
           LEFT JOIN attempts l ON l.attempt_id = d.listed_by
          WHERE d.story_id IS NOT NULL AND d.received_at IS NOT NULL AND d.received_at <= ?1
          ORDER BY d.run_id, d.source, d.attempt_id",
    )?;
    type Spans = (Vec<f64>, Vec<f64>, i64);
    let mut grouped: BTreeMap<(String, String, Option<String>), Spans> = BTreeMap::new();
    let mut rows = statement.query([at])?;
    while let Some(row) = rows.next()? {
        let run: String = row.get(0)?;
        let url: Option<String> = row.get(2)?;
        let host = url
            .as_deref()
            .and_then(|url| news_provider::host_of(url).ok().flatten())
            .map(|host| host.as_str().to_owned());
        let spans = grouped.entry((run.clone(), row.get(1)?, host)).or_default();
        let fetched: String = row.get(3)?;
        let listed: Option<String> = row.get(4)?;
        let listing_run: Option<String> = row.get(5)?;
        match listed
            .as_deref()
            .and_then(|listed| seconds_between(listed, &fetched))
        {
            Some(seconds) if listing_run.as_deref() == Some(run.as_str()) => spans.0.push(seconds),
            Some(seconds) => spans.1.push(seconds),
            None => spans.2 += 1,
        }
    }
    Ok(grouped
        .into_iter()
        .map(
            |((run_id, source, host), (fresh, carried, unlinked))| LatencyRow {
                run_id,
                source,
                host,
                fresh: Latency::of(fresh),
                carried: Latency::of(carried),
                unlinked,
            },
        )
        .collect())
}

/// `to` less `from`, two stored stamps, in seconds; None when either does not parse.
fn seconds_between(from: &str, to: &str) -> Option<f64> {
    let from = DateTime::parse_from_rfc3339(from).ok()?;
    let to = DateTime::parse_from_rfc3339(to).ok()?;
    let micros = (to - from).num_microseconds()?;
    Some(micros as f64 / MICROS_A_SECOND)
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
    // A robots fetch asks a host, not a source, and a roster fetch asks once a day for no news,
    // so neither says anything about the collector's freshness.
    let newest_request: Option<String> = db.query_row(
        "SELECT MAX(requested_at) FROM attempts
          WHERE requested_at <= ?1 AND source <> ?2 AND source <> ?3",
        params![at, ROBOTS_SOURCE, ROSTER_SOURCE],
        |row| row.get(0),
    )?;
    // Both halves of the visibility question, which is the one `pit` exists for: a publication
    // value we had not read yet is not freshness.
    let newest_publication = newest_visible_publication(db, at)?;
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

/// Every store read a day row is built from, each indexed by run.
struct Read<'a> {
    grouped: &'a BTreeMap<&'a str, Vec<&'a Grouped>>,
    in_flight: &'a BTreeMap<&'a str, Vec<&'a InFlightRow>>,
    answers: &'a BTreeMap<&'a str, Vec<&'a Answered>>,
    deferred: &'a BTreeMap<&'a str, Vec<&'a DeferredGroup>>,
    carried: &'a BTreeMap<&'a str, Vec<&'a CarriedGroup>>,
    latency: &'a BTreeMap<&'a str, Vec<&'a LatencyRow>>,
}

fn day_row(date: NaiveDate, envelopes: &[Envelope], read: &Read, ask: &Ask) -> DayRow {
    let (held_out, today): (Vec<&Envelope>, Vec<&Envelope>) = envelopes
        .iter()
        .filter(|envelope| envelope.run_date == date && envelope.kind.is_collect())
        .partition(|envelope| envelope.refused.as_deref() == Some(LEASE_HELD));
    // Under `--source`, a run file naming another source or none adds no busy time (answer 6).
    let timed: Vec<&&Envelope> = today
        .iter()
        .filter(|envelope| ask.source.is_none() || envelope.source == ask.source)
        .collect();
    // `sum` of no f64 is -0.0, printed `-0.0` (measured 2026-11-18 on the golden ledger file).
    let wall_s = timed
        .iter()
        .filter_map(|envelope| envelope.wall_s)
        .fold(0.0, |total, run| total + run);
    let mut busy: BTreeMap<Option<String>, f64> = BTreeMap::new();
    for lane in timed.iter().flat_map(|envelope| &envelope.busy) {
        *busy.entry(lane.host.clone()).or_default() += lane.busy_s;
    }
    let holds_capped = timed
        .iter()
        .map(|envelope| envelope.holds_capped.len())
        .sum::<usize>();
    let runs: BTreeSet<&str> = today.iter().map(|run| run.run_id.as_str()).collect();
    let observed = u32::try_from(today.len()).unwrap_or(u32::MAX);
    // A run the lease kept out still states how many runs its day is due.
    let expected = today
        .iter()
        .chain(&held_out)
        .map(|envelope| envelope.expected_runs)
        .chain(ask.expected_runs)
        .max()
        .unwrap_or_default();
    let mut by_source: BTreeMap<String, SourceRow> = BTreeMap::new();
    for entry in of_the_day(read.grouped, &runs) {
        if !ask.wants(&entry.source) {
            continue;
        }
        let row = by_source
            .entry(entry.source.clone())
            .or_insert_with(|| SourceRow::named(&entry.source));
        row.attempts += entry.attempts;
        row.vendor_rows += entry.vendor_rows;
        row.held += entry.held;
        row.listing_unusable += entry.listing_unusable;
        row.bodies_truncated += entry.bodies_truncated;
        row.cuts_held_at_limit += entry.cuts_held_at_limit;
        row.cuts_released_unreachable += entry.cuts_released_unreachable;
        row.relisted_disputed += entry.relisted_disputed;
        row.relisted_compared += entry.relisted_compared;
        row.robots_disallowed += entry.robots_disallowed;
        row.continuity = row.continuity.add(entry.continuity);
        row.boundaries = row.boundaries.add(entry.boundaries);
        row.walk = row.walk.add(entry.walk);
        row.items_new += entry.items_new;
        match entry.state.as_str() {
            OUTCOME_OK => row.ok += entry.attempts,
            OUTCOME_EMPTY => row.empty += entry.attempts,
            OUTCOME_FAILED => row.failed += entry.attempts,
            OUTCOME_INTERRUPTED => row.interrupted += entry.attempts,
            OUTCOME_NOT_SENT => row.not_sent += entry.attempts,
            _ => row.in_flight += entry.attempts,
        }
    }
    for (_, source, attempt_id, answered) in of_the_day(read.in_flight, &runs) {
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
    for answer in of_the_day(read.answers, &runs) {
        if !ask.wants(&answer.source) {
            continue;
        }
        let row = by_source
            .entry(answer.source.clone())
            .or_insert_with(|| SourceRow::named(&answer.source));
        row.rows_committed += i64::try_from(answer.item_ids.len()).unwrap_or(i64::MAX);
    }
    for group in of_the_day(read.deferred, &runs) {
        if !ask.wants(&group.source) {
            continue;
        }
        let row = by_source
            .entry(group.source.clone())
            .or_insert_with(|| SourceRow::named(&group.source));
        row.deferred = row.deferred.add(group.counts);
    }
    for group in of_the_day(read.carried, &runs) {
        if !ask.wants(&group.source) {
            continue;
        }
        let row = by_source
            .entry(group.source.clone())
            .or_insert_with(|| SourceRow::named(&group.source));
        row.carried = row.carried.add(group.counts);
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
        runs_lease_held: u32::try_from(held_out.len()).unwrap_or(u32::MAX),
        wall_s,
        wall_share: wall_s / SECONDS_A_DAY,
        busy: busy
            .into_iter()
            .map(|(host, busy_s)| HostBusyRow {
                host,
                busy_s,
                share: busy_s / SECONDS_A_DAY,
            })
            .collect(),
        holds_capped: u32::try_from(holds_capped).unwrap_or(u32::MAX),
        per_source: by_source.into_values().collect(),
        listing_latency: of_the_day(read.latency, &runs)
            .into_iter()
            .filter(|row| ask.wants(&row.source))
            .cloned()
            .collect(),
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
            not_sent: 0,
            in_flight: 0,
            opened_without_answered: Vec::new(),
            answered_without_finalised: Vec::new(),
            vendor_rows: 0,
            held: 0,
            listing_unusable: 0,
            bodies_truncated: 0,
            cuts_held_at_limit: 0,
            cuts_released_unreachable: 0,
            relisted_disputed: 0,
            relisted_compared: 0,
            robots_disallowed: 0,
            continuity: ContinuityCounts::default(),
            boundaries: BoundaryCounts::default(),
            walk: WalkCounts::default(),
            deferred: DeferredCounts::default(),
            carried: CarriedCounts::default(),
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
    if day.wall_share > BUSY_BOUND {
        findings.push(Finding {
            code: WALL_TIME_OVER_BOUND,
            run_date: day.run_date.clone(),
            source: None,
            detail: format!("{} s of wall time, {} of a day", day.wall_s, day.wall_share),
        });
    }
    for lane in day.busy.iter().filter(|lane| lane.share > BUSY_BOUND) {
        findings.push(Finding {
            code: HOST_BUSY_OVER_BOUND,
            run_date: day.run_date.clone(),
            source: None,
            detail: format!(
                "host {} busy {} s, {} of a day",
                lane.host.as_deref().unwrap_or("-"),
                lane.busy_s,
                lane.share
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
        if row.carried.expired > 0 {
            findings.push(Finding {
                code: DEFERRED_EXPIRED,
                run_date: day.run_date.clone(),
                source: Some(row.source.clone()),
                detail: format!(
                    "{} deferred row(s) closed expired, never asked",
                    row.carried.expired
                ),
            });
        }
        if row.rows_committed == 0 && row.vendor_rows - row.held > 0 {
            findings.push(Finding {
                code: ROWS_ZERO_AGAINST_VENDOR_ROWS,
                run_date: day.run_date.clone(),
                source: Some(row.source.clone()),
                detail: format!(
                    "{} vendor rows, {} held, and no item row committed",
                    row.vendor_rows, row.held
                ),
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
        writeln!(
            out,
            "busy             lease_held {} wall_s {} wall_share {} bound {} holds_capped {}",
            day.runs_lease_held, day.wall_s, day.wall_share, report.busy_bound, day.holds_capped
        )?;
        for lane in &day.busy {
            writeln!(
                out,
                "busy_host        {} busy_s {} share {}",
                lane.host.as_deref().unwrap_or("-"),
                lane.busy_s,
                lane.share
            )?;
        }
        for row in &day.per_source {
            writeln!(
                out,
                "  {:<14} ok {} empty {} failed {} interrupted {} not_sent {} in-flight {} \
                 vendor_rows {} held {} listing_unusable {} bodies_truncated {} \
                 cuts_held_at_limit {} cuts_released_unreachable {} \
                 relisted_disputed {} relisted_compared {} robots_disallowed {} \
                 continuity_complete {} continuity_boundary_no_shared_key {} \
                 continuity_no_previous_key {} continuity_first_poll {} \
                 continuity_next_page_unread {} boundary_same_generation {} \
                 boundary_older_generation {} boundary_newer_generation {} \
                 boundary_generation_unknown {} walk_complete {} walk_first_poll {} \
                 walk_id_unread {} walk_range_unread {} walk_gone {} walk_other_language {} walk_unread {} \
                 deferred_run_deadline {} deferred_host_held {} deferred_follow_up_cap {} \
                 deferred_gate_refused {} \
                 carried_answered {} carried_held {} carried_expired {} \
                 items_new {} rows {}",
                row.source,
                row.ok,
                row.empty,
                row.failed,
                row.interrupted,
                row.not_sent,
                row.in_flight,
                row.vendor_rows,
                row.held,
                row.listing_unusable,
                row.bodies_truncated,
                row.cuts_held_at_limit,
                row.cuts_released_unreachable,
                row.relisted_disputed,
                row.relisted_compared,
                row.robots_disallowed,
                row.continuity.complete,
                row.continuity.boundary_no_shared_key,
                row.continuity.no_previous_key,
                row.continuity.first_poll,
                row.continuity.next_page_unread,
                row.boundaries.same_generation,
                row.boundaries.older_generation,
                row.boundaries.newer_generation,
                row.boundaries.generation_unknown,
                row.walk.complete,
                row.walk.first_poll,
                row.walk.id_unread,
                row.walk.range_unread,
                row.walk.gone,
                row.walk.other_language,
                row.walk.unread,
                row.deferred.run_deadline,
                row.deferred.host_held,
                row.deferred.follow_up_cap,
                row.deferred.gate_refused,
                row.carried.answered,
                row.carried.held,
                row.carried.expired,
                row.items_new,
                row.rows_committed
            )?;
        }
        for row in &day.listing_latency {
            let span = |latency: &Latency| {
                let seconds = |value: Option<f64>| value.map_or("-".to_owned(), |s| s.to_string());
                format!(
                    "{} p50_s {} p95_s {} max_s {}",
                    latency.documents,
                    seconds(latency.p50_s),
                    seconds(latency.p95_s),
                    seconds(latency.max_s)
                )
            };
            writeln!(
                out,
                "latency          {} {} {} fresh {} carried {} unlinked {}",
                row.run_id,
                row.source,
                row.host.as_deref().unwrap_or("-"),
                span(&row.fresh),
                span(&row.carried),
                row.unlinked
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
        "coverage         {} window(s) read, {} with a gap",
        report.coverage.len(),
        report
            .coverage
            .iter()
            .filter(|row| row.covered < row.planned)
            .count()
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

/// The version of [`render_prometheus`]'s exposition, carried as the `schema` label of
/// `newsvault_ledger_at_timestamp_seconds`, which the staleness alert selects on.
pub const PROMETHEUS_SCHEMA: &str = "newsvault.prom/1";
/// Every family [`render_prometheus`] writes, in its order: the list a deploy test holds the alert
/// rules to, so a family with no rule reading it, or a rule reading none, is red.
pub const PROMETHEUS_FAMILIES: [&str; 8] = [
    "newsvault_ledger_at_timestamp_seconds",
    "newsvault_ledger_complete",
    "newsvault_collect_last_asked_age_seconds",
    "newsvault_deferred_host_held",
    "newsvault_carried_expired",
    "newsvault_walk_range_unread_polls",
    "newsvault_holds_capped",
    "newsvault_check_findings",
];

/// A day's labels: `run_date`, which names one alert across the UTC midnight, and how many UTC days
/// before `at`'s it is, so a rule can tell today, still running, from yesterday, closed; a run
/// date that is not a date is `days_ago="unknown"`.
fn day_labels(at: DateTime<Utc>, run_date: &str) -> String {
    let ago = run_date.parse::<NaiveDate>().map_or_else(
        |_| "unknown".to_owned(),
        |day| (at.date_naive() - day).num_days().to_string(),
    );
    format!("run_date=\"{}\",days_ago=\"{ago}\"", label(run_date))
}

fn label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn family(
    out: &mut impl std::io::Write,
    name: &str,
    help: &str,
    samples: &[(String, String)],
) -> std::io::Result<()> {
    writeln!(out, "# HELP {name} {help}")?;
    writeln!(out, "# TYPE {name} gauge")?;
    for (labels, value) in samples {
        if labels.is_empty() {
            writeln!(out, "{name} {value}")?;
        } else {
            writeln!(out, "{name}{{{labels}}} {value}")?;
        }
    }
    Ok(())
}

/// The same report as node-exporter textfile metrics, one family per alert input in
/// `deploy/prometheus/newsvault.rules.yml`; architecture.md section 11.5 step 7 names each reader.
pub fn render_prometheus(
    report: &Report,
    at: DateTime<Utc>,
    out: &mut impl std::io::Write,
) -> std::io::Result<()> {
    let per_source = |count: &dyn Fn(&SourceRow) -> i64| -> Vec<(String, String)> {
        report
            .days
            .iter()
            .flat_map(|day| {
                day.per_source.iter().map(move |row| {
                    (
                        format!(
                            "source=\"{}\",{}",
                            label(&row.source),
                            day_labels(at, &day.run_date)
                        ),
                        count(row).to_string(),
                    )
                })
            })
            .collect()
    };
    let mut findings: BTreeMap<(&str, &str), u32> = BTreeMap::new();
    for finding in &report.check.findings {
        *findings
            .entry((finding.code, finding.run_date.as_str()))
            .or_default() += 1;
    }
    writeln!(out, "# {PROMETHEUS_SCHEMA}")?;
    family(
        out,
        "newsvault_ledger_at_timestamp_seconds",
        "The ledger read's --at as Unix seconds; the watchdog's last run.",
        &[(
            format!("schema=\"{PROMETHEUS_SCHEMA}\""),
            at.timestamp().to_string(),
        )],
    )?;
    family(
        out,
        "newsvault_ledger_complete",
        "1 when the read saw every run file and every attempt's run file, else 0 (exit 8).",
        &[(String::new(), u8::from(report.is_complete()).to_string())],
    )?;
    if let Some(age) = report.freshness.asking_clock.age_seconds {
        family(
            out,
            "newsvault_collect_last_asked_age_seconds",
            "Seconds from the newest vendor request any run sent to --at.",
            &[(String::new(), age.to_string())],
        )?;
    }
    family(
        out,
        "newsvault_deferred_host_held",
        "Requests a server's hold or slow-down kept out of a run (S7-D4 host_held).",
        &per_source(&|row| row.deferred.host_held),
    )?;
    family(
        out,
        "newsvault_carried_expired",
        "Deferred rows the carry closed expired after 24 h, never asked (S7-D5).",
        &per_source(&|row| row.carried.expired),
    )?;
    family(
        out,
        "newsvault_walk_range_unread_polls",
        "Walked polls that read no walk range (section 11.2 range_unread).",
        &per_source(&|row| row.walk.range_unread),
    )?;
    family(
        out,
        "newsvault_holds_capped",
        "Answers whose Retry-After was cut to the 24 h ceiling (S7-D7).",
        &report
            .days
            .iter()
            .map(|day| (day_labels(at, &day.run_date), day.holds_capped.to_string()))
            .collect::<Vec<_>>(),
    )?;
    family(
        out,
        "newsvault_check_findings",
        "ledger --check findings per code; a code with none has no sample.",
        &findings
            .into_iter()
            .map(|((code, run_date), count)| {
                (
                    format!("code=\"{}\",{}", label(code), day_labels(at, run_date)),
                    count.to_string(),
                )
            })
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        label, render_prometheus, CarriedCounts, Check, ClockAge, DayRow, DeferredCounts, Finding,
        Freshness, Latency, Report, SourceRow, WalkCounts, BUSY_BOUND, PROMETHEUS_FAMILIES, SCHEMA,
    };
    use chrono::{DateTime, Utc};

    const AT: &str = "2026-11-18T10:00:00Z";
    const SOURCE: &str = "open_web_wires";

    /// The text format's three escapes in a label value; an unescaped `"` ends the value early.
    #[test]
    fn a_label_value_escapes_its_quote_backslash_and_newline() {
        assert_eq!(label("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    /// §8 row R22's nearest rank ⌈p × n / 100⌉: of 20, p95 is the 19th and not the max; of 11,
    /// p95 is the 11th (⌈10.45⌉), where rounding would read the 10th.
    #[test]
    fn a_percentile_is_the_nearest_rank_rounded_up() {
        let ranked = |n: u32| Latency::of((1..=n).rev().map(f64::from).collect());
        let span = |documents: i64, p50: f64, p95: f64, max: f64| Latency {
            documents,
            p50_s: Some(p50),
            p95_s: Some(p95),
            max_s: Some(max),
        };
        assert_eq!(
            (ranked(20), ranked(11)),
            (span(20, 10.0, 19.0, 20.0), span(11, 6.0, 11.0, 11.0))
        );
    }

    fn day(run_date: &str, holds_capped: u32, host_held: i64, expired: i64, range: i64) -> DayRow {
        let mut row = SourceRow::named(SOURCE);
        row.deferred = DeferredCounts {
            run_deadline: 101,
            host_held,
            follow_up_cap: 103,
            gate_refused: 0,
        };
        row.carried = CarriedCounts {
            answered: 104,
            held: 105,
            expired,
        };
        row.walk = WalkCounts {
            range_unread: range,
            id_unread: 106,
            ..WalkCounts::default()
        };
        DayRow {
            run_date: run_date.to_owned(),
            runs_expected: 31,
            runs_observed: 7,
            runs_shortfall: 24,
            runs_over_expected: 0,
            runs_unfinished: Vec::new(),
            runs_lease_held: 0,
            wall_s: 0.0,
            wall_share: 0.0,
            busy: Vec::new(),
            holds_capped,
            per_source: vec![row],
            listing_latency: Vec::new(),
        }
    }

    fn finding(code: &'static str, run_date: &str) -> Finding {
        Finding {
            code,
            run_date: run_date.to_owned(),
            source: None,
            detail: String::new(),
        }
    }

    /// Every metric a value no other metric holds, so a family reading another's field is red.
    #[test]
    fn each_metric_carries_its_own_count_for_its_own_day() -> Result<(), Box<dyn std::error::Error>>
    {
        let at: DateTime<Utc> = AT.parse()?;
        let report = Report {
            schema: SCHEMA,
            at: AT.to_owned(),
            from: "2026-11-17".to_owned(),
            to: "2026-11-18".to_owned(),
            source: None,
            expected_runs: None,
            limit: None,
            days: vec![
                day("2026-11-17", 2, 3, 5, 7),
                day("2026-11-18", 11, 13, 17, 19),
            ],
            result_digest_drift: Vec::new(),
            coverage: Vec::new(),
            unreadable_runs: Vec::new(),
            attempts_without_a_run_envelope: Vec::new(),
            unpublished_seq_ranges: Vec::new(),
            freshness: Freshness {
                asking_clock: ClockAge {
                    newest: Some("2026-11-18T09:59:37Z".to_owned()),
                    age_seconds: Some(23),
                },
                publication_clock: ClockAge {
                    newest: None,
                    age_seconds: Some(29),
                },
            },
            busy_bound: BUSY_BOUND,
            matched: 2,
            returned: 2,
            truncated: false,
            check: Check {
                asked: true,
                findings: vec![
                    finding("runs_below_expected", "2026-11-17"),
                    finding("runs_below_expected", "2026-11-18"),
                    finding("host_busy_over_bound", "2026-11-18"),
                    finding("host_busy_over_bound", "2026-11-18"),
                    finding("coverage_incomplete", "2026-11"),
                ],
            },
        };
        let mut out = Vec::new();
        render_prometheus(&report, at, &mut out)?;
        let text = String::from_utf8(out)?;
        let families: Vec<&str> = text
            .lines()
            .filter_map(|line| line.strip_prefix("# TYPE "))
            .filter_map(|line| line.split_whitespace().next())
            .collect();
        assert_eq!(families, PROMETHEUS_FAMILIES);
        let samples: Vec<String> = text
            .lines()
            .filter(|line| !line.starts_with('#'))
            .map(str::to_owned)
            .collect();
        let day = |ago: u8| {
            let date = if ago == 0 { "2026-11-18" } else { "2026-11-17" };
            format!("source=\"{SOURCE}\",run_date=\"{date}\",days_ago=\"{ago}\"")
        };
        assert_eq!(
            samples,
            [
                "newsvault_ledger_at_timestamp_seconds{schema=\"newsvault.prom/1\"} 1794996000"
                    .to_owned(),
                "newsvault_ledger_complete 1".to_owned(),
                "newsvault_collect_last_asked_age_seconds 23".to_owned(),
                format!("newsvault_deferred_host_held{{{}}} 3", day(1)),
                format!("newsvault_deferred_host_held{{{}}} 13", day(0)),
                format!("newsvault_carried_expired{{{}}} 5", day(1)),
                format!("newsvault_carried_expired{{{}}} 17", day(0)),
                format!("newsvault_walk_range_unread_polls{{{}}} 7", day(1)),
                format!("newsvault_walk_range_unread_polls{{{}}} 19", day(0)),
                "newsvault_holds_capped{run_date=\"2026-11-17\",days_ago=\"1\"} 2".to_owned(),
                "newsvault_holds_capped{run_date=\"2026-11-18\",days_ago=\"0\"} 11".to_owned(),
                "newsvault_check_findings{code=\"coverage_incomplete\",run_date=\"2026-11\",\
                 days_ago=\"unknown\"} 1"
                    .to_owned(),
                "newsvault_check_findings{code=\"host_busy_over_bound\",run_date=\"2026-11-18\",\
                 days_ago=\"0\"} 2"
                    .to_owned(),
                "newsvault_check_findings{code=\"runs_below_expected\",run_date=\"2026-11-17\",\
                 days_ago=\"1\"} 1"
                    .to_owned(),
                "newsvault_check_findings{code=\"runs_below_expected\",run_date=\"2026-11-18\",\
                 days_ago=\"0\"} 1"
                    .to_owned(),
            ]
        );
        Ok(())
    }

    /// An incomplete read is a 0, and a store no request reached has no age sample at all, so
    /// the absence alert reads it instead of a made-up age.
    #[test]
    fn an_incomplete_read_is_zero_and_no_request_is_no_age_sample(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let at: DateTime<Utc> = AT.parse()?;
        let report = Report {
            schema: SCHEMA,
            at: AT.to_owned(),
            from: "2026-11-18".to_owned(),
            to: "2026-11-18".to_owned(),
            source: None,
            expected_runs: None,
            limit: None,
            days: Vec::new(),
            result_digest_drift: Vec::new(),
            coverage: Vec::new(),
            unreadable_runs: vec![super::Unreadable {
                path: "runs/x.json".to_owned(),
                code: "io_failed",
            }],
            attempts_without_a_run_envelope: Vec::new(),
            unpublished_seq_ranges: Vec::new(),
            freshness: Freshness {
                asking_clock: ClockAge {
                    newest: None,
                    age_seconds: None,
                },
                publication_clock: ClockAge {
                    newest: None,
                    age_seconds: None,
                },
            },
            busy_bound: BUSY_BOUND,
            matched: 0,
            returned: 0,
            truncated: false,
            check: Check {
                asked: true,
                findings: Vec::new(),
            },
        };
        let mut out = Vec::new();
        render_prometheus(&report, at, &mut out)?;
        let text = String::from_utf8(out)?;
        assert_eq!(
            (
                text.lines()
                    .any(|line| line == "newsvault_ledger_complete 0"),
                text.contains("newsvault_collect_last_asked_age_seconds"),
            ),
            (true, false)
        );
        Ok(())
    }
}
