//! `newsvault robots refresh` - one ledgered robots.txt fetch per hosts-file row, admitted and
//! candidate alike (architecture.md section 11.2). It writes attempt, `payloads` and `robots` rows,
//! blobs and a run envelope of kind `robots_refresh`, and never the hosts file: a changed pin is
//! an edit a person makes. How a robots attempt is labelled and kept out of the ledger is section 10.

use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::time::Duration;

use canon::FileDigest;
use chrono::{DateTime, Utc};
use news_provider::{
    AttemptSlot, Baseline, BudgetError, Clock, Counts, Credentials, Exchange, Exit, LedgerError,
    Outcome, OutcomeKind, Payload, Provenance, Provider, RawResponse, Request, RequestPhase, RunId,
    SendFailure, SourceId, Tally, TargetWindow, TransportError, TransportKind, Verdict, Wire,
};
use open_web_feed::{HostPin, OpenWebFeed};
use pit::ObservationTime;
use serde_json::{json, Value};

use crate::cmd_collect::{
    crawl_delay_over_interval, write_envelope, CollectRunError, Registered, RUN_ID_FORMAT,
};
use crate::cmd_ledger::{Envelope, RunKind, RUN_ENVELOPE_SCHEMA};
use crate::robots::{Reading, Robots, RobotsError as ParseError};
use crate::store::{Committer, RobotsAnswer, StoreError};

pub const SCHEMA: &str = "newsvault.robots/1";
/// The `attempts.source` of every robots fetch: robots is a fact about a host, not about one
/// source, and no registered source carries this name.
pub const ROBOTS_SOURCE: &str = "robots";
const RUN_PREFIX: &str = "robots";
const ROBOTS_PATH: &str = "/robots.txt";
const OK: u16 = 200;
const NOT_FOUND: u16 = 404;
const TOO_MANY_REQUESTS: u16 = 429;
const SERVER_ERRORS: RangeInclusive<u16> = 500..=599;
/// A robots body is no vendor row, so every robots attempt is decided against zero of them.
const NO_VENDOR_ROWS: u32 = 0;
const NO_RULES_ABSENT: &str = "no_rules_absent";
const CRAWL_DELAY_OVER_INTERVAL: &str = "crawl_delay_over_interval";
/// Every `parse_verdict` a report row can carry besides `null`, as `newsvault schema` lists them.
#[must_use]
pub fn parse_verdicts() -> [&'static str; 9] {
    [
        Reading::Rules.name(),
        Reading::NoRulesEmpty.name(),
        Reading::OrphanRulesAppliedToStar.name(),
        NO_RULES_ABSENT,
        CRAWL_DELAY_OVER_INTERVAL,
        ParseError::NotUtf8.verdict(),
        ParseError::HtmlPage.verdict(),
        ParseError::NoUserAgentLine.verdict(),
        ParseError::CrawlDelayNotANumber(String::new()).verdict(),
    ]
}

#[derive(Debug, Clone)]
pub struct Ask {
    pub source: String,
    pub hosts: PathBuf,
    pub rules: PathBuf,
    pub store: PathBuf,
    pub contact: String,
    pub dry_run: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum RobotsError {
    #[error("robots refresh reads a hosts file, and {0} takes none")]
    NoHostsFile(&'static str),
    /// An unregistered source, and a run envelope the filesystem refused.
    #[error(transparent)]
    Collect(#[from] CollectRunError),
    #[error(transparent)]
    OpenWebRefused(#[from] open_web_feed::Refusal),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("no credential named {0} is held")]
    Credential(String),
    #[error(transparent)]
    Budget(#[from] BudgetError),
}

impl RobotsError {
    /// `invalid_argument` (section 5.1), which `main` writes as `usage error:`.
    #[must_use]
    pub const fn is_usage(&self) -> bool {
        match self {
            Self::NoHostsFile(_) => true,
            Self::Collect(failure) => failure.is_usage(),
            Self::OpenWebRefused(_)
            | Self::Store(_)
            | Self::Ledger(_)
            | Self::Credential(_)
            | Self::Budget(_) => false,
        }
    }
}

/// The exit a refresh takes. `PinDiffers` is decision D1's "the answer is no": every row was
/// answered and an admitted row's pin is not what came back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RobotsExit {
    Ok,
    PinDiffers,
    Failed(Exit),
}

impl RobotsExit {
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::PinDiffers => 1,
            Self::Failed(exit) => exit.code(),
        }
    }

    #[must_use]
    pub const fn error_code(self) -> Option<&'static str> {
        match self {
            Self::Ok | Self::PinDiffers => None,
            Self::Failed(exit) => exit.error_code(),
        }
    }

    #[must_use]
    pub const fn retryable(self) -> bool {
        match self {
            Self::Ok | Self::PinDiffers => false,
            Self::Failed(exit) => exit.retryable(),
        }
    }

    /// Section 5.1's iff rule: stdout carries the report for exits 0, 1 and 8 only.
    #[must_use]
    pub const fn carries_the_result_on_stdout(self) -> bool {
        match self {
            Self::Ok | Self::PinDiffers => true,
            Self::Failed(exit) => exit.carries_the_result_on_stdout(),
        }
    }
}

#[derive(Debug)]
pub struct Answer {
    pub report: Value,
    pub exit: RobotsExit,
}

/// Which stream a refresh writes, and what: section 5.1's iff rule, decided here so a test can
/// reach the exits a stub produces and the binary cannot without the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Emitted {
    /// The report, for exits 0, 1 and 8.
    Stdout(Value),
    /// A `newsvault.error/1` object carrying the report; `argv` is the suggested rerun.
    Stderr(Value),
}

impl Answer {
    #[must_use]
    pub fn emitted(&self, argv: Vec<String>) -> Emitted {
        if self.exit.carries_the_result_on_stdout() {
            return Emitted::Stdout(self.report.clone());
        }
        Emitted::Stderr(json!({
            "schema": "newsvault.error/1",
            "code": self.exit.error_code(),
            "message": "no robots.txt answered",
            "retryable": self.exit.retryable(),
            "details": self.report,
            "suggestions": [argv],
        }))
    }
}

/// The one source a refresh reads the hosts file of; `edgar_press` has none.
pub fn registered(source: &str) -> Result<Registered, RobotsError> {
    match Registered::chosen(&[source.to_owned()])? {
        Registered::OpenWebWires => Ok(Registered::OpenWebWires),
        other @ Registered::EdgarPress => Err(RobotsError::NoHostsFile(other.source())),
    }
}

/// The hosts file through the collector's own loader, so one set of refusals holds for both.
pub fn open(ask: &Ask) -> Result<OpenWebFeed, RobotsError> {
    registered(&ask.source)?;
    Ok(OpenWebFeed::open(&ask.hosts, &ask.rules, &ask.contact)?)
}

/// The one place a refresh builds its `Wire`, for `main.rs` and the loopback test alike: the
/// collector's open-web profile, credential and pace files, so a robots fetch waits for the same
/// per-IP and per-host slots a collect run does.
#[must_use]
pub fn wire_for(ask: &Ask, feed: &OpenWebFeed) -> Wire {
    Wire::new(
        feed.transport(),
        Credentials::default().with(Registered::OpenWebWires.credential(), ask.contact.clone()),
        &ask.store,
        &feed.source(),
    )
}

/// What one row's fetch left: `None` fields on a dry run, which asks nothing.
struct Fetched {
    attempt_id: Option<String>,
    transport_status: Option<u16>,
    verdict: Option<Verdict>,
    outcome: Option<OutcomeKind>,
    robots: Option<RobotsAnswer>,
    parse_verdict: Option<&'static str>,
}

/// Asks each row's robots.txt once, in hosts-file order. No retry and no startup sweep.
pub fn run(
    ask: &Ask,
    feed: &OpenWebFeed,
    exchange: &dyn Exchange,
    clock: &dyn Clock,
) -> Result<Answer, RobotsError> {
    let started = clock.now();
    let run_id = format!("{RUN_PREFIX}-{}", started.format(RUN_ID_FORMAT));
    let rows = feed.host_pins();
    // Read-only, so before the dry-run answer too: a store this build cannot open is left as
    // found, and a dry run says so rather than exiting 0.
    crate::store::refuse_an_unreadable_version(&ask.store)?;
    if ask.dry_run {
        let planned = rows
            .iter()
            .map(|_| Fetched {
                attempt_id: None,
                transport_status: None,
                verdict: None,
                outcome: None,
                robots: None,
                parse_verdict: None,
            })
            .collect::<Vec<Fetched>>();
        return Ok(answer_of(ask, feed, &run_id, started, &rows, &planned));
    }
    let envelope = Envelope {
        schema: RUN_ENVELOPE_SCHEMA.to_owned(),
        run_id: run_id.clone(),
        run_date: started.date_naive(),
        expected_runs: 0,
        started_at: started,
        finished_at: None,
        kind: RunKind::RobotsRefresh,
    };
    write_envelope(&ask.store, &envelope)?;
    let run = RunId::new(run_id.clone());
    let mut store = Committer::open(&ask.store, &run)?;
    let mut fetched = Vec::with_capacity(rows.len());
    let interval = feed.transport().budget.per_host;
    for (ordinal, row) in (0_u32..).zip(&rows) {
        let request = request_for(row, feed.hosts_version());
        let asked = Asked {
            run: &run,
            ordinal,
            row,
            request: &request,
            interval,
        };
        fetched.push(fetch(&mut store, exchange, clock, &asked)?);
    }
    write_envelope(
        &ask.store,
        &Envelope {
            finished_at: Some(clock.now()),
            ..envelope
        },
    )?;
    Ok(answer_of(ask, feed, &run_id, started, &rows, &fetched))
}

fn request_for(row: &HostPin, hosts_version: &FileDigest) -> Request {
    Request {
        source: SourceId::new(ROBOTS_SOURCE),
        target_window: TargetWindow::new(&row.host),
        phase: RequestPhase::Index,
        provenance: Provenance::LiveIngest,
        url: url_of(row),
        headers: Vec::new(),
        params: Vec::new(),
        universe_version: FileDigest::of(&[]),
        listed_date: None,
        hosts_version: Some(hosts_version.clone()),
        extract_version: None,
    }
}

fn url_of(row: &HostPin) -> String {
    format!("https://{}{ROBOTS_PATH}", row.host)
}

/// One row's fetch as `run` asks it; `interval` is the per-host pace the parse verdict compares a
/// Crawl-delay with.
struct Asked<'a> {
    run: &'a RunId,
    ordinal: u32,
    row: &'a HostPin,
    request: &'a Request,
    interval: Option<Duration>,
}

fn fetch(
    store: &mut Committer,
    exchange: &dyn Exchange,
    clock: &dyn Clock,
    asked: &Asked<'_>,
) -> Result<Fetched, RobotsError> {
    let Asked {
        run,
        ordinal,
        row,
        request,
        interval,
    } = *asked;
    let opened_at = ObservationTime::new(clock.now());
    let mut slot = AttemptSlot::open(store, request, None, run, ordinal, opened_at)?;
    let attempt_id = slot.attempt_id().hex().to_owned();
    let permit = slot.take_permit()?;
    let answer = match exchange.send(permit, request, clock) {
        Ok(response) => Ok(response),
        Err(SendFailure::Transport(failed)) => Err(failed),
        Err(SendFailure::CredentialMissing(name)) => return Err(RobotsError::Credential(name)),
        Err(SendFailure::Budget(why)) => return Err(RobotsError::Budget(why)),
    };
    let verdict = verdict_of(&answer);
    // Every body is stored and linked, a 403 page and a cut body included (P10, section 3.9).
    let payload = Payload::stored(request, &answer, |body| {
        store.put_blob(body).map(|digest| digest.hex().to_owned())
    })?;
    let robots = match (&payload, verdict) {
        (Some(stored), Verdict::Usable) => {
            Some(RobotsAnswer::Present(store.blob_named(&stored.raw_blob)?))
        }
        (Some(_), Verdict::Empty) => Some(RobotsAnswer::Absent),
        _ => None,
    };
    let outcome = Outcome::decide(
        &verdict,
        Counts {
            vendor_rows: NO_VENDOR_ROWS,
            listing_unusable: 0,
        },
        &Baseline::from_trailing_ok(&[]),
    );
    let kind = outcome.kind();
    let parse_verdict = match (&robots, &answer) {
        (Some(answered), Ok(response)) => {
            Some(parse_verdict_of(answered, response.body(), interval))
        }
        _ => None,
    };
    let closed = slot.close(store, outcome, &answer, ObservationTime::new(clock.now()))?;
    let at = closed.closed_at();
    match (&robots, &payload) {
        (Some(answered), Some(stored)) => {
            store.record_robots(closed, &row.host, answered, stored, at)?
        }
        (_, stored) => store.record(closed, stored.as_ref(), &[], at)?,
    };
    Ok(Fetched {
        attempt_id: Some(attempt_id),
        transport_status: match &answer {
            Ok(response) => Some(response.status()),
            Err(failed) => failed.status(),
        },
        verdict: Some(verdict),
        outcome: Some(kind),
        robots,
        parse_verdict,
    })
}

/// Owner decision 3d: what `collect` would make of this answer, by the same parser and the same
/// Crawl-delay rule.
fn parse_verdict_of(
    robots: &RobotsAnswer,
    body: &[u8],
    interval: Option<Duration>,
) -> &'static str {
    if matches!(robots, RobotsAnswer::Absent) {
        return NO_RULES_ABSENT;
    }
    match Robots::parse(body) {
        Err(why) => why.verdict(),
        Ok(read) => match (read.crawl_delay(), interval) {
            (Some(delay), Some(interval)) if crawl_delay_over_interval(delay, interval) => {
                CRAWL_DELAY_OVER_INTERVAL
            }
            _ => read.reading().name(),
        },
    }
}

/// 200 is present, 404 is absent (section 11.1); 429, a 5xx and a transport error are transient;
/// every other status, 403, 408 and 410 included, and a body over the limit are permanent.
fn verdict_of(answer: &Result<RawResponse, TransportError>) -> Verdict {
    match answer {
        Ok(response) => match response.status() {
            OK => Verdict::Usable,
            NOT_FOUND => Verdict::Empty,
            TOO_MANY_REQUESTS => Verdict::TransientFailure,
            status if SERVER_ERRORS.contains(&status) => Verdict::TransientFailure,
            _ => Verdict::PermanentFailure,
        },
        Err(failed) if failed.kind() == TransportKind::BodyOverLimit => Verdict::PermanentFailure,
        Err(_) => Verdict::TransientFailure,
    }
}

/// The digest the answer would be pinned by, in the hosts file's own spelling.
pub(crate) fn fetched_pin(robots: &RobotsAnswer) -> String {
    match robots {
        RobotsAnswer::Present(digest) => digest.hex().to_owned(),
        RobotsAnswer::Absent => open_web_feed::RobotsPin::Absent.text().to_owned(),
    }
}

fn matches_pin(row: &HostPin, fetched: &Fetched) -> Option<bool> {
    let pinned = row.robots.as_ref()?;
    let robots = fetched.robots.as_ref()?;
    Some(pinned.text() == fetched_pin(robots))
}

fn exit_of(rows: &[HostPin], fetched: &[Fetched]) -> RobotsExit {
    let mut tally = Tally::default();
    for one in fetched {
        match one.verdict {
            Some(Verdict::TransientFailure) => tally.transient += 1,
            Some(Verdict::PermanentFailure) => tally.permanent += 1,
            Some(Verdict::Usable | Verdict::Empty) => tally.committed += 1,
            None => {}
        }
    }
    let differs = rows
        .iter()
        .zip(fetched)
        .any(|(row, one)| row.admitted && matches_pin(row, one) == Some(false));
    match tally.exit() {
        Exit::Ok if differs => RobotsExit::PinDiffers,
        Exit::Ok => RobotsExit::Ok,
        failed => RobotsExit::Failed(failed),
    }
}

fn answer_of(
    ask: &Ask,
    feed: &OpenWebFeed,
    run_id: &str,
    started: DateTime<Utc>,
    rows: &[HostPin],
    fetched: &[Fetched],
) -> Answer {
    let exit = exit_of(rows, fetched);
    let report = json!({
        "schema": SCHEMA,
        "run_id": run_id,
        "source": open_web_feed::SOURCE,
        "hosts_version": feed.hosts_version().hex(),
        "at": started.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        "dry_run": ask.dry_run,
        "rows": rows.iter().zip(fetched).map(|(row, one)| json!({
            "host": row.host,
            "status": if row.admitted { "admitted" } else { "candidate" },
            "url": url_of(row),
            "attempt_id": one.attempt_id,
            "transport_status": one.transport_status,
            "outcome": one.outcome.map(OutcomeKind::name),
            "answer": match (&one.robots, one.verdict) {
                (Some(robots), _) => Some(robots.name()),
                (None, Some(_)) => Some("failed"),
                (None, None) => None,
            },
            "fetched_sha256": one.robots.as_ref().and_then(RobotsAnswer::blob).map(|blob| blob.hex()),
            "pinned": row.robots.as_ref().map(open_web_feed::RobotsPin::text),
            "matches_pin": matches_pin(row, one),
            "parse_verdict": one.parse_verdict,
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
    for row in report["rows"].as_array().unwrap_or(&none) {
        writeln!(
            out,
            "  {:<28} {:<9} {:<7} matches_pin {} parse_verdict {}",
            text(row, "host"),
            text(row, "status"),
            row["answer"].as_str().unwrap_or("planned"),
            row["matches_pin"],
            row["parse_verdict"].as_str().unwrap_or("none"),
        )?;
    }
    writeln!(out, "exit             {}", report["exit"]["code"])?;
    Ok(())
}

fn text<'a>(of: &'a Value, key: &str) -> &'a str {
    of[key].as_str().unwrap_or_default()
}
