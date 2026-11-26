//! `newsvault roster fetch` and `newsvault roster show`: the company roster, kept raw-first as
//! the exact bytes of each daily file with our observation time and parsed later (architecture.md
//! section 10, "The roster"). No item rows: a roster file is an L2 payload under an attempt row.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use canon::FileDigest;
use chrono::{DateTime, SecondsFormat, Utc};
use news_provider::robots::Robots;
use news_provider::{
    AttemptSlot, Baseline, Clock, Counts, Credentials, Exchange, Exit, Headers, LedgerError,
    Outcome, OutcomeKind, PaceRoot, Payload, Provenance, RawResponse, Request, RequestPhase, RunId,
    SourceId, Tally, TargetWindow, TransportError, TransportKind, TransportProfile, Verdict, Wire,
};
use pit::{Cutoff, ObservationTime, PitError, ValidTime, Visibility};
use rusqlite::Connection;
use sec_edgar::Caller;
use serde_json::{json, Value};
use vault_core::sqlite::read_only;

use crate::cmd_collect::{
    crawl_delay_over_interval, write_envelope, CollectRunError, RUN_ID_FORMAT,
};
use crate::cmd_ledger::{Envelope, RunKind, LEASE_HELD, RUN_ENVELOPE_SCHEMA};
use crate::cmd_robots::{self, or_abandon, sent, Asked, Fetched, Opened, RobotsError};
use crate::store::{require_current_version, Committer, RobotsAnswer, StoreError, COMMIT_LOG};

pub const SCHEMA: &str = "newsvault.roster/1";
pub const SHOW_SCHEMA: &str = "newsvault.roster_show/1";
/// The `attempts.source` of every roster file fetch. Kept out of the ledger's asking clock by
/// name, as `robots` is: a daily roster fetch says nothing about the news collector's freshness.
pub const ROSTER_SOURCE: &str = "roster";
const RUN_PREFIX: &str = "roster";
const OK: u16 = 200;
const FORBIDDEN: u16 = 403;
const REQUEST_TIMEOUT: u16 = 408;
const TOO_MANY_REQUESTS: u16 = 429;
const SERVER_ERRORS: std::ops::RangeInclusive<u16> = 500..=599;
/// The most a roster answer may hold: the 8 MiB the roster had while it took the news adapter's
/// profile whole, 2026-11-23 to 2026-11-25. Not fitted: the one file read 521,795 B on 2026-11-23.
const BODY_LIMIT: u64 = 8 * 1_024 * 1_024;
/// Why a file was not asked; each leaves no attempt row, because no call was made.
const ROBOTS_UNVERIFIED: &str = "robots_unverified";
const ROBOTS_REFUSED: &str = "robots_refused";
const ROBOTS_DISALLOWED: &str = "robots_disallowed";
/// SEC's own column names in `company_tickers_exchange.json`, read 2026-11-23.
const SEC_EXCHANGE_FIELDS: [&str; 4] = ["cik", "name", "ticker", "exchange"];
/// The valid time of a copy: the server's `Last-Modified`, else its `Date`.
const LAST_MODIFIED: &str = "last-modified";
const DATE: &str = "date";

/// One daily file. `rows` reads the vendor's row count, or `None` for a body of another shape: a
/// challenge page or a changed format is a failed fetch, never a roster.
#[derive(Debug, Clone, Copy)]
pub struct RosterFile {
    pub key: &'static str,
    pub host: &'static str,
    pub path: &'static str,
    rows: fn(&[u8]) -> Option<usize>,
}

impl RosterFile {
    #[must_use]
    pub fn url(&self) -> String {
        format!("https://{}{}", self.host, self.path)
    }
}

/// The files a fetch asks, in order. Nasdaq Trader's symbol files wait for the owner's reading of
/// its terms (section 10, "The roster"); SEC's content is free to reuse.
pub const FILES: [RosterFile; 1] = [RosterFile {
    key: "sec_company_tickers_exchange",
    host: sec_edgar::HOST,
    path: "/files/company_tickers_exchange.json",
    rows: sec_exchange_rows,
}];

/// `{"fields": [the four names], "data": [[...], ...]}`; the number of `data` rows.
fn sec_exchange_rows(body: &[u8]) -> Option<usize> {
    let parsed: Value = serde_json::from_slice(body).ok()?;
    let fields = parsed.get("fields")?.as_array()?;
    let named = fields
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<&str>>>()?;
    if named != SEC_EXCHANGE_FIELDS {
        return None;
    }
    Some(parsed.get("data")?.as_array()?.len())
}

#[derive(Debug, Clone)]
pub struct Ask {
    pub store: PathBuf,
    pub contact: String,
    pub dry_run: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum RosterError {
    /// `credential_contact_missing`: SEC refuses a User-Agent with no contact (`sec_edgar::CONTACT_CREDENTIAL`).
    #[error("no contact address for the User-Agent: set BELLWETHER_FEED_CONTACT_FILE")]
    ContactMissing,
    #[error("no commit log under {0}")]
    StoreAbsent(PathBuf),
    /// `input_unparseable`: a stored time this reader cannot place, never a hidden copy.
    #[error("attempt {attempt}: {why}")]
    StoredTime { attempt: String, why: String },
    #[error(transparent)]
    Collect(#[from] CollectRunError),
    #[error(transparent)]
    Robots(#[from] RobotsError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

impl RosterError {
    /// The section 5.1 exit: `input_absent` 4, `lease_held` and `gate_refused` 7; `credential_contact_missing`,
    /// `input_unparseable`, `input_schema_mismatch` and `io_failed` 2.
    #[must_use]
    pub const fn exit(&self) -> u8 {
        match self {
            Self::StoreAbsent(_) => 4,
            Self::Store(StoreError::LeaseHeld(_))
            | Self::Robots(RobotsError::Store(StoreError::LeaseHeld(_))) => 7,
            Self::Robots(RobotsError::GateRefused { .. }) => cmd_robots::GATE_REFUSED_EXIT,
            _ => 2,
        }
    }
}

#[derive(Debug)]
pub struct Answer {
    pub report: Value,
    pub exit: Exit,
}

impl Answer {
    /// Section 5.1's iff rule: the report on stdout for 0 and 8; for 7 it rides the error object.
    #[must_use]
    pub fn error_object(&self, argv: Vec<String>) -> Option<Value> {
        (!self.exit.carries_the_result_on_stdout()).then(|| {
            json!({
                "schema": "newsvault.error/1",
                "code": self.exit.error_code(),
                "message": "no roster file was answered",
                "retryable": self.exit.retryable(),
                "details": self.report,
                "suggestions": [argv],
            })
        })
    }
}

/// SEC's profile at the roster's row of SEC's interval table.
#[must_use]
pub fn transport() -> TransportProfile {
    sec_edgar::transport(Caller::Roster, BODY_LIMIT)
}

/// SEC's transport, its contact under SEC's credential name, and SEC's per-IP pace file: the
/// roster and `edgar_press` are one conversation with one vendor (section 8 row R19).
#[must_use]
pub fn wire_for(ask: &Ask, pace_root: &PaceRoot) -> Wire {
    Wire::new(
        transport(),
        Credentials::default().with(sec_edgar::CONTACT_CREDENTIAL, ask.contact.clone()),
        pace_root,
        &SourceId::new(sec_edgar::PER_IP_BUCKET),
    )
}

/// What one file's turn left.
struct FileRow {
    file: RosterFile,
    attempt_id: Option<String>,
    transport_status: Option<u16>,
    verdict: Option<Verdict>,
    outcome: Option<OutcomeKind>,
    rows: Option<usize>,
    raw_blob: Option<String>,
    last_modified: Option<String>,
    not_asked: Option<&'static str>,
}

impl FileRow {
    const fn planned(file: RosterFile) -> Self {
        Self {
            file,
            attempt_id: None,
            transport_status: None,
            verdict: None,
            outcome: None,
            rows: None,
            raw_blob: None,
            last_modified: None,
            not_asked: None,
        }
    }
}

struct RobotsRow {
    host: &'static str,
    fetched: Option<Fetched>,
}

/// Each host's robots.txt once, under its own `robots` attempt row, then each of its files the
/// answer allows, in [`FILES`] order. No retry and no startup sweep.
pub fn run(ask: &Ask, exchange: &dyn Exchange, clock: &dyn Clock) -> Result<Answer, RosterError> {
    if ask.contact.trim().is_empty() {
        return Err(RosterError::ContactMissing);
    }
    let started = clock.now();
    let run_id = format!("{RUN_PREFIX}-{}", started.format(RUN_ID_FORMAT));
    crate::store::refuse_an_unreadable_version(&ask.store)?;
    let hosts = hosts_of(&FILES);
    if ask.dry_run {
        let robots = hosts
            .iter()
            .map(|host| RobotsRow {
                host,
                fetched: None,
            })
            .collect::<Vec<RobotsRow>>();
        let files = FILES.map(FileRow::planned);
        return Ok(answer_of(ask, &run_id, started, &robots, &files));
    }
    let envelope = Envelope {
        schema: RUN_ENVELOPE_SCHEMA.to_owned(),
        run_id: run_id.clone(),
        run_date: started.date_naive(),
        source: None,
        expected_runs: 0,
        started_at: started,
        finished_at: None,
        kind: RunKind::RosterFetch,
        refused: None,
        wall_s: None,
        busy: Vec::new(),
        holds_capped: Vec::new(),
    };
    write_envelope(&ask.store, &envelope)?;
    let run = RunId::new(run_id.clone());
    let mut store = match Committer::open(&ask.store, &run) {
        Ok(store) => store,
        // Finished and marked as a refused collect's envelope is (S7-D9), never left unfinished.
        Err(StoreError::LeaseHeld(lock)) => {
            write_envelope(
                &ask.store,
                &Envelope {
                    finished_at: Some(clock.now()),
                    refused: Some(LEASE_HELD.to_owned()),
                    ..envelope
                },
            )?;
            return Err(StoreError::LeaseHeld(lock).into());
        }
        Err(other) => return Err(other.into()),
    };
    let fetched = fetch_each(&mut store, exchange, clock, &run, &hosts);
    // Finished and marked, as a run the lease kept out is: the first refusal ends the run, since
    // every later request would wait at the same per-IP gate.
    let refused = matches!(
        fetched,
        Err(RosterError::Robots(RobotsError::GateRefused { .. }))
    );
    write_envelope(
        &ask.store,
        &Envelope {
            finished_at: Some(clock.now()),
            refused: refused.then(|| cmd_robots::GATE_REFUSED.to_owned()),
            ..envelope
        },
    )?;
    let (robots, files) = fetched?;
    Ok(answer_of(ask, &run_id, started, &robots, &files))
}

/// Each host's robots.txt, then each of its files the answer allows.
fn fetch_each(
    store: &mut Committer,
    exchange: &dyn Exchange,
    clock: &dyn Clock,
    run: &RunId,
    hosts: &[&'static str],
) -> Result<(Vec<RobotsRow>, Vec<FileRow>), RosterError> {
    let interval = transport().budget.per_host;
    let mut ordinal: u32 = 0;
    let mut robots = Vec::with_capacity(hosts.len());
    let mut files = Vec::with_capacity(FILES.len());
    for &host in hosts {
        let request = cmd_robots::request_for(host, None);
        let asked = Asked {
            run,
            ordinal,
            host,
            request: &request,
            interval,
        };
        ordinal += 1;
        let fetched = cmd_robots::fetch(store, exchange, clock, &asked)?;
        for file in FILES.iter().filter(|file| file.host == host) {
            match gate(&fetched, file.path, interval) {
                Some(reason) => files.push(FileRow {
                    not_asked: Some(reason),
                    ..FileRow::planned(*file)
                }),
                None => {
                    files.push(fetch_file(store, exchange, clock, run, ordinal, *file)?);
                    ordinal += 1;
                }
            }
        }
        robots.push(RobotsRow {
            host,
            fetched: Some(fetched),
        });
    }
    Ok((robots, files))
}

/// Each host once, in the order [`FILES`] first names it.
fn hosts_of(files: &[RosterFile]) -> Vec<&'static str> {
    let mut hosts: Vec<&'static str> = Vec::new();
    for file in files {
        if !hosts.contains(&file.host) {
            hosts.push(file.host);
        }
    }
    hosts
}

/// `None` when the host's answered robots.txt allows `path` at our pace; else why it is not asked.
/// A failed fetch or a body that does not parse asks nothing: robots is honoured, never guessed.
fn gate(
    fetched: &Fetched,
    path: &str,
    interval: Option<std::time::Duration>,
) -> Option<&'static str> {
    match (&fetched.robots, &fetched.body) {
        (Some(RobotsAnswer::Absent), _) => None,
        (Some(RobotsAnswer::Present(_)), Some(body)) => match Robots::parse(body) {
            Err(_) => Some(ROBOTS_REFUSED),
            Ok(read) => {
                let slower = match (read.crawl_delay(), interval) {
                    (Some(delay), Some(interval)) => crawl_delay_over_interval(delay, interval),
                    _ => false,
                };
                if slower {
                    Some(ROBOTS_REFUSED)
                } else if read.allows(path) {
                    None
                } else {
                    Some(ROBOTS_DISALLOWED)
                }
            }
        },
        (Some(RobotsAnswer::Present(_)), None) | (None, _) => Some(ROBOTS_UNVERIFIED),
    }
}

fn request_of(file: RosterFile) -> Request {
    Request {
        source: SourceId::new(ROSTER_SOURCE),
        target_window: TargetWindow::new(file.key),
        phase: RequestPhase::Index,
        provenance: Provenance::LiveIngest,
        url: file.url(),
        headers: Vec::new(),
        params: Vec::new(),
        universe_version: FileDigest::of(&[]),
        listed_date: None,
        names_ticker: false,
        hosts_version: None,
        extract_version: None,
    }
}

fn fetch_file(
    store: &mut Committer,
    exchange: &dyn Exchange,
    clock: &dyn Clock,
    run: &RunId,
    ordinal: u32,
    file: RosterFile,
) -> Result<FileRow, RosterError> {
    let request = request_of(file);
    let opened_at = ObservationTime::new(clock.now());
    let mut slot = AttemptSlot::open(store, &request, None, run, ordinal, opened_at)?;
    let attempt_id = slot.attempt_id().hex().to_owned();
    let opened = Opened {
        attempt_id: &attempt_id,
        run,
        clock,
    };
    let permit = or_abandon::<_, _, RosterError>(slot.take_permit(), store, &opened)?;
    let answer = match exchange.send(permit, &request, clock) {
        Err(news_provider::SendFailure::Budget(why)) if why.is_a_gate_refusal() => {
            return Err(cmd_robots::unsent(slot, store, clock, why).into());
        }
        answered => or_abandon::<_, _, RosterError>(sent(answered), store, &opened)?,
    };
    let rows = match &answer {
        Ok(response) if response.status() == OK => (file.rows)(response.body()),
        _ => None,
    };
    let verdict = verdict_of(&answer, rows);
    // Every body is stored and linked, a 403 page and a cut body included (P10, section 3.9).
    let payload = Payload::stored(&request, &answer, |body| {
        store.put_blob(body).map(|digest| digest.hex().to_owned())
    });
    let payload = or_abandon::<_, _, RosterError>(payload, store, &opened)?;
    let outcome = Outcome::decide(
        &verdict,
        Counts {
            vendor_rows: rows.map_or(0, |rows| u32::try_from(rows).unwrap_or(u32::MAX)),
            listing_unusable: 0,
            robots_disallowed: 0,
        },
        &Baseline::from_trailing_ok(&[]),
    );
    let kind = outcome.kind();
    let closed = slot.close(store, outcome, &answer, ObservationTime::new(clock.now()))?;
    let at = closed.closed_at();
    store.record(closed, payload.as_ref(), &[], at)?;
    Ok(FileRow {
        attempt_id: Some(attempt_id),
        transport_status: match &answer {
            Ok(response) => Some(response.status()),
            Err(failed) => failed.status(),
        },
        verdict: Some(verdict),
        outcome: Some(kind),
        rows,
        raw_blob: payload.as_ref().map(|stored| stored.raw_blob.clone()),
        last_modified: payload
            .as_ref()
            .and_then(|stored| stored.headers.first(LAST_MODIFIED))
            .map(str::to_owned),
        ..FileRow::planned(file)
    })
}

/// 200 of the file's shape is usable, or empty with no rows; 200 of another shape (a challenge
/// page) and a body over the limit are permanent; 403, 408, 429, 5xx and a transport error are
/// transient, as `sec_edgar::body_of_200` reads SEC outside its `AccessDenied` arm; any other
/// status is permanent.
fn verdict_of(answer: &Result<RawResponse, TransportError>, rows: Option<usize>) -> Verdict {
    match answer {
        Ok(response) => match (response.status(), rows) {
            (OK, Some(0)) => Verdict::Empty,
            (OK, Some(_)) => Verdict::Usable,
            (OK, None) => Verdict::PermanentFailure,
            (FORBIDDEN | REQUEST_TIMEOUT | TOO_MANY_REQUESTS, _) => Verdict::TransientFailure,
            (status, _) if SERVER_ERRORS.contains(&status) => Verdict::TransientFailure,
            _ => Verdict::PermanentFailure,
        },
        Err(failed) if failed.kind() == TransportKind::BodyOverLimit => Verdict::PermanentFailure,
        Err(_) => Verdict::TransientFailure,
    }
}

/// An empty roster is the silent zero R6 forbids, so it counts as a failure, not as work done. A
/// failed robots fetch counts once for its host and its files add nothing more; a file a parsed
/// robots.txt keeps us from is a target not asked for a reason that is not clean work.
fn exit_of(robots: &[RobotsRow], files: &[FileRow]) -> Exit {
    let mut tally = Tally::default();
    let mut unverified: Vec<&str> = Vec::new();
    for row in robots {
        match row.fetched.as_ref().and_then(|fetched| fetched.verdict) {
            Some(Verdict::TransientFailure) => tally.transient += 1,
            Some(Verdict::PermanentFailure) => tally.permanent += 1,
            Some(Verdict::Usable | Verdict::Empty) | None => continue,
        }
        unverified.push(row.host);
    }
    for row in files {
        match (row.verdict, row.not_asked) {
            (Some(Verdict::Usable), _) => tally.committed += 1,
            (Some(Verdict::TransientFailure), _) => tally.transient += 1,
            (Some(Verdict::PermanentFailure | Verdict::Empty), _) => tally.permanent += 1,
            (None, Some(_)) if !unverified.contains(&row.file.host) => {
                tally.unclean_not_planned += 1;
            }
            (None, _) => {}
        }
    }
    tally.exit()
}

fn answer_of(
    ask: &Ask,
    run_id: &str,
    started: DateTime<Utc>,
    robots: &[RobotsRow],
    files: &[FileRow],
) -> Answer {
    let exit = if ask.dry_run {
        Exit::Ok
    } else {
        exit_of(robots, files)
    };
    let report = json!({
        "schema": SCHEMA,
        "run_id": run_id,
        "source": ROSTER_SOURCE,
        "at": started.to_rfc3339_opts(SecondsFormat::Micros, true),
        "dry_run": ask.dry_run,
        "robots": robots.iter().map(|row| {
            let fetched = row.fetched.as_ref();
            json!({
                "host": row.host,
                "attempt_id": fetched.and_then(|one| one.attempt_id.clone()),
                "transport_status": fetched.and_then(|one| one.transport_status),
                "answer": fetched.map(|one| one.robots.as_ref().map_or("failed", RobotsAnswer::name)),
                "parse_verdict": fetched.and_then(|one| one.parse_verdict),
            })
        }).collect::<Vec<Value>>(),
        "files": files.iter().map(|row| json!({
            "key": row.file.key,
            "url": row.file.url(),
            "attempt_id": row.attempt_id,
            "transport_status": row.transport_status,
            "outcome": row.outcome.map(OutcomeKind::name),
            "verdict": row.verdict.map(|verdict| verdict.name()),
            "rows": row.rows,
            "raw_blob": row.raw_blob,
            "last_modified": row.last_modified,
            "not_asked": row.not_asked,
        })).collect::<Vec<Value>>(),
        "changed": !ask.dry_run,
        "exit": {
            "code": exit.code(),
            "error_code": exit.error_code(),
            "retryable": exit.retryable(),
        },
    });
    Answer { report, exit }
}

/// The human rendering of the same object the JSON carries.
pub fn render_human(report: &Value, out: &mut impl std::io::Write) -> std::io::Result<()> {
    writeln!(out, "run              {}", text(report, "run_id"))?;
    let none = Vec::new();
    for row in report["robots"].as_array().unwrap_or(&none) {
        writeln!(
            out,
            "  robots {:<24} {:<8} parse_verdict {}",
            text(row, "host"),
            row["answer"].as_str().unwrap_or("planned"),
            row["parse_verdict"].as_str().unwrap_or("none"),
        )?;
    }
    for row in report["files"].as_array().unwrap_or(&none) {
        writeln!(
            out,
            "  file   {:<30} {:<18} rows {} not_asked {}",
            text(row, "key"),
            row["verdict"].as_str().unwrap_or("planned"),
            row["rows"],
            row["not_asked"].as_str().unwrap_or("none"),
        )?;
    }
    writeln!(out, "exit             {}", report["exit"]["code"])?;
    Ok(())
}

fn text<'a>(of: &'a Value, key: &str) -> &'a str {
    of[key].as_str().unwrap_or_default()
}

/// One stored roster attempt as `show` reads it.
struct Stored {
    attempt_id: String,
    run_id: String,
    key: String,
    opened_at: DateTime<Utc>,
    received_at: Option<DateTime<Utc>>,
    closed_at: Option<DateTime<Utc>>,
    outcome: Option<String>,
    verdict: Option<String>,
    vendor_rows: Option<i64>,
    url: Option<String>,
    raw_blob: Option<String>,
    bytes: Option<i64>,
    headers: Option<String>,
}

/// What `roster show --at` answers: per file, the newest usable copy visible at `at` by both of
/// its times, and the newest attempt opened by then, with its answer only once closed by then.
#[derive(Debug)]
pub struct Shown {
    pub report: Value,
    /// False when a file in [`FILES`] has no copy visible at `at`: the answer is no.
    pub every_file_visible: bool,
}

/// Read-only and lease-free. A copy's valid time is its `Last-Modified`, else its `Date`; its
/// observation time is `received_at`. `Visibility::visible_at` takes both.
pub fn show(root: &Path, at: DateTime<Utc>) -> Result<Shown, RosterError> {
    let log = root.join(COMMIT_LOG);
    if !log.exists() {
        return Err(RosterError::StoreAbsent(log));
    }
    let db = read_only(&log)?;
    require_current_version(&db)?;
    let cutoff = Cutoff::new(at);
    let mut by_key: BTreeMap<String, Vec<Stored>> = FILES
        .iter()
        .map(|file| (file.key.to_owned(), Vec::new()))
        .collect();
    for stored in stored_attempts(&db)? {
        if stored.opened_at <= at {
            by_key.entry(stored.key.clone()).or_default().push(stored);
        }
    }
    let mut every_file_visible = true;
    let mut rows = Vec::with_capacity(by_key.len());
    for (key, attempts) in &by_key {
        let mut current: Option<(&Stored, Visibility, &'static str)> = None;
        let mut refused = Vec::new();
        for stored in attempts {
            let received = stored.received_at.is_some_and(|received| received <= at);
            match copy_of(stored)? {
                Some(Copy::Dated(visibility, valid_from)) if visibility.visible_at(cutoff) => {
                    current = Some((stored, visibility, valid_from));
                }
                Some(Copy::ReceivedBeforeValid(why)) if received => {
                    refused.push(json!({"attempt_id": stored.attempt_id, "why": why}));
                }
                Some(Copy::Dated(..) | Copy::ReceivedBeforeValid(_)) | None => {}
            }
        }
        if current.is_none() && FILES.iter().any(|file| file.key == key) {
            every_file_visible = false;
        }
        let last = attempts.last();
        rows.push(json!({
            "key": key,
            "current": current.map(|(stored, visibility, valid_from)| json!({
                "attempt_id": stored.attempt_id,
                "run_id": stored.run_id,
                "url": stored.url,
                "universe_version": stored.raw_blob,
                "bytes": stored.bytes,
                "rows": stored.vendor_rows,
                "valid_at": stamp(visibility.valid().inner()),
                "valid_from": valid_from,
                "observed_at": stamp(visibility.observed().inner()),
            })),
            "last_attempt": last.map(|stored| {
                // An attempt's outcome is known at its close; before that it is in flight.
                let closed = stored.closed_at.is_some_and(|closed| closed <= at);
                json!({
                    "attempt_id": stored.attempt_id,
                    "opened_at": stamp(stored.opened_at),
                    "in_flight": !closed,
                    "outcome": stored.outcome.as_ref().filter(|_| closed),
                    "verdict": stored.verdict.as_ref().filter(|_| closed),
                })
            }),
            "attempts": attempts.len(),
            "refused": refused,
        }));
    }
    let report = json!({
        "schema": SHOW_SCHEMA,
        "at": stamp(at),
        "files": rows,
        "every_file_visible": every_file_visible,
    });
    Ok(Shown {
        report,
        every_file_visible,
    })
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn stored_attempts(db: &Connection) -> Result<Vec<Stored>, RosterError> {
    let mut statement = db.prepare(
        "SELECT a.attempt_id, a.run_id, a.target_window, a.opened_at, a.received_at, a.closed_at,
                a.outcome, a.verdict, a.vendor_rows, p.url, p.raw_blob, b.bytes, p.headers
           FROM attempts a
           LEFT JOIN payloads p ON p.attempt_id = a.attempt_id
           LEFT JOIN blobs b ON b.digest = p.raw_blob
          WHERE a.source = ?1
          ORDER BY a.target_window, a.opened_at, a.attempt_id",
    )?;
    let raw = statement
        .query_map([ROSTER_SOURCE], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                (
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ),
                (
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<i64>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                ),
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut stored = Vec::with_capacity(raw.len());
    for (attempt_id, run_id, key, opened, (received, closed), rest) in raw {
        let (outcome, verdict, vendor_rows, url, raw_blob, bytes, headers) = rest;
        let instant = |text: &str| {
            DateTime::parse_from_rfc3339(text)
                .map(|at| at.with_timezone(&Utc))
                .map_err(|why| RosterError::StoredTime {
                    attempt: attempt_id.clone(),
                    why: format!("{text:?}: {why}"),
                })
        };
        let opened_at = instant(&opened)?;
        let received_at = received.as_deref().map(instant).transpose()?;
        let closed_at = closed.as_deref().map(instant).transpose()?;
        stored.push(Stored {
            attempt_id,
            run_id,
            key,
            opened_at,
            received_at,
            closed_at,
            outcome,
            verdict,
            vendor_rows,
            url,
            raw_blob,
            bytes,
            headers,
        });
    }
    Ok(stored)
}

/// A usable copy as `show` places it.
enum Copy {
    /// Its two times, and the header its valid time came from.
    Dated(Visibility, &'static str),
    /// Its valid time is after we received it: a server clock ahead of ours. Listed under the
    /// file's `refused` and never current, so one skewed copy cannot hide the others.
    ReceivedBeforeValid(String),
}

/// `None` for an attempt that holds no usable copy. A usable copy whose stored headers or valid
/// time cannot be read is an error by name, never a hidden copy.
fn copy_of(stored: &Stored) -> Result<Option<Copy>, RosterError> {
    let refused = |why: String| RosterError::StoredTime {
        attempt: stored.attempt_id.clone(),
        why,
    };
    if stored.verdict.as_deref() != Some(Verdict::Usable.name()) {
        return Ok(None);
    }
    let (Some(received_at), Some(headers)) = (stored.received_at, stored.headers.as_deref()) else {
        return Ok(None);
    };
    let headers = Headers::from_json(headers).map_err(|why| refused(format!("headers: {why}")))?;
    let (named, value) = match (headers.first(LAST_MODIFIED), headers.first(DATE)) {
        (Some(value), _) => (LAST_MODIFIED, value),
        (None, Some(value)) => (DATE, value),
        (None, None) => return Err(refused("no last-modified and no date header".to_owned())),
    };
    let valid = DateTime::parse_from_rfc2822(value)
        .map_err(|why| refused(format!("{named} {value:?}: {why}")))?
        .with_timezone(&Utc);
    Ok(Some(
        match Visibility::from_live(ValidTime::new(valid), ObservationTime::new(received_at)) {
            Ok(visibility) => Copy::Dated(visibility, named),
            Err(why @ PitError::ObservedBeforeValid { .. }) => {
                Copy::ReceivedBeforeValid(why.to_string())
            }
            Err(why) => return Err(refused(why.to_string())),
        },
    ))
}

/// The human rendering of `show`'s object.
pub fn render_show_human(report: &Value, out: &mut impl std::io::Write) -> std::io::Result<()> {
    writeln!(out, "at               {}", text(report, "at"))?;
    let none = Vec::new();
    for row in report["files"].as_array().unwrap_or(&none) {
        let current = &row["current"];
        writeln!(
            out,
            "  {:<30} universe_version {} valid_at {} observed_at {}",
            text(row, "key"),
            current["universe_version"].as_str().unwrap_or("none"),
            current["valid_at"].as_str().unwrap_or("none"),
            current["observed_at"].as_str().unwrap_or("none"),
        )?;
    }
    Ok(())
}
