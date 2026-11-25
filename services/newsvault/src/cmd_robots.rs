//! `newsvault robots refresh` - one ledgered robots.txt fetch per hosts-file row, admitted and
//! candidate alike (architecture.md section 11.2). It writes attempt, `payloads` and `robots` rows,
//! blobs and a run envelope of kind `robots_refresh`, and never the hosts file: a changed pin is
//! an edit a person makes. How a robots attempt is labelled and kept out of the ledger is section 10.

use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::time::Duration;

use canon::FileDigest;
use chrono::{DateTime, Utc};
use news_provider::robots::{Reading, Robots, RobotsError as ParseError};
use news_provider::{
    AttemptId, AttemptSlot, Baseline, BudgetError, Clock, Counts, Credentials, Exchange, Exit,
    LedgerError, Outcome, OutcomeKind, Payload, Provenance, Provider, RawResponse, Request,
    RequestPhase, RunId, SendFailure, SourceId, Tally, TargetWindow, TransportError, TransportKind,
    Verdict, Wire,
};
use open_web_feed::{HostPin, OpenWebFeed};
use pit::ObservationTime;
use serde_json::{json, Value};

use crate::cmd_collect::{
    crawl_delay_over_interval, write_envelope, CollectRunError, Registered, RUN_ID_FORMAT,
};
use crate::cmd_ledger::{Envelope, RunKind, RUN_ENVELOPE_SCHEMA};
use crate::store::{Committer, RobotsAnswer, StoreError};

/// Major 3 since 2026-11-17: `parse_verdict` lost `refused_not_utf8` (architecture.md §10).
pub const SCHEMA: &str = "newsvault.robots/3";
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
pub fn parse_verdicts() -> [&'static str; 8] {
    [
        Reading::Rules.name(),
        Reading::NoRulesEmpty.name(),
        Reading::NoRulesNoGroup.name(),
        Reading::OrphanRulesIgnored.name(),
        NO_RULES_ABSENT,
        CRAWL_DELAY_OVER_INTERVAL,
        ParseError::HtmlPage.verdict(),
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
/// per-host slots a collect run does.
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
pub(crate) struct Fetched {
    pub(crate) attempt_id: Option<String>,
    pub(crate) transport_status: Option<u16>,
    pub(crate) verdict: Option<Verdict>,
    pub(crate) outcome: Option<OutcomeKind>,
    pub(crate) robots: Option<RobotsAnswer>,
    pub(crate) parse_verdict: Option<&'static str>,
    /// The answered body of a `present` row, which `roster fetch` reads its rules from.
    pub(crate) body: Option<Vec<u8>>,
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
                body: None,
            })
            .collect::<Vec<Fetched>>();
        return Ok(answer_of(ask, feed, &run_id, started, &rows, &planned));
    }
    let envelope = Envelope {
        schema: RUN_ENVELOPE_SCHEMA.to_owned(),
        run_id: run_id.clone(),
        run_date: started.date_naive(),
        source: None,
        expected_runs: 0,
        started_at: started,
        finished_at: None,
        kind: RunKind::RobotsRefresh,
        refused: None,
        wall_s: None,
        busy: Vec::new(),
        holds_capped: Vec::new(),
    };
    write_envelope(&ask.store, &envelope)?;
    let run = RunId::new(run_id.clone());
    let mut store = Committer::open(&ask.store, &run)?;
    let mut fetched = Vec::with_capacity(rows.len());
    let interval = feed.transport().budget.per_host;
    for (ordinal, row) in (0_u32..).zip(&rows) {
        let request = request_for(&row.host, Some(feed.hosts_version()));
        let asked = Asked {
            run: &run,
            ordinal,
            host: &row.host,
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

/// A robots fetch of `host`; `hosts_version` is `None` for a caller that reads no hosts file.
pub(crate) fn request_for(host: &str, hosts_version: Option<&FileDigest>) -> Request {
    Request {
        source: SourceId::new(ROBOTS_SOURCE),
        target_window: TargetWindow::new(host),
        phase: RequestPhase::Index,
        provenance: Provenance::LiveIngest,
        url: url_of(host),
        headers: Vec::new(),
        params: Vec::new(),
        universe_version: FileDigest::of(&[]),
        listed_date: None,
        names_ticker: false,
        hosts_version: hosts_version.cloned(),
        extract_version: None,
    }
}

fn url_of(host: &str) -> String {
    format!("https://{host}{ROBOTS_PATH}")
}

/// The attempt a run opened and has not closed, for [`or_abandon`].
pub(crate) struct Opened<'a> {
    pub(crate) attempt_id: &'a str,
    pub(crate) run: &'a RunId,
    pub(crate) clock: &'a dyn Clock,
}

/// A send's answer, or the error that sent nothing: a transport error is an answer to record.
pub(crate) fn sent(
    send: Result<RawResponse, SendFailure>,
) -> Result<Result<RawResponse, TransportError>, RobotsError> {
    match send {
        Ok(response) => Ok(Ok(response)),
        Err(SendFailure::Transport(failed)) => Ok(Err(failed)),
        Err(SendFailure::CredentialMissing(name)) => Err(RobotsError::Credential(name)),
        Err(SendFailure::Budget(why)) => Err(RobotsError::Budget(why)),
    }
}

/// An error between an attempt's open and its close closes it as abandoned first: left open, the
/// next collect's startup sweep closes it and counts it as that collect's interruption (exit 8).
pub(crate) fn or_abandon<T, E, R>(
    result: Result<T, E>,
    store: &mut Committer,
    opened: &Opened<'_>,
) -> Result<T, R>
where
    R: From<E> + From<RobotsError>,
{
    match result {
        Ok(value) => Ok(value),
        Err(why) => {
            let at = ObservationTime::new(opened.clock.now());
            let closed = news_provider::abandon(
                store,
                AttemptId::recorded(opened.attempt_id),
                at,
                opened.run,
            )
            .map_err(RobotsError::from)?;
            store
                .record(closed, None, &[], at)
                .map_err(RobotsError::from)?;
            Err(R::from(why))
        }
    }
}

/// One host's fetch as a run asks it; `interval` is the per-host pace the parse verdict compares a
/// Crawl-delay with.
pub(crate) struct Asked<'a> {
    pub(crate) run: &'a RunId,
    pub(crate) ordinal: u32,
    pub(crate) host: &'a str,
    pub(crate) request: &'a Request,
    pub(crate) interval: Option<Duration>,
}

pub(crate) fn fetch(
    store: &mut Committer,
    exchange: &dyn Exchange,
    clock: &dyn Clock,
    asked: &Asked<'_>,
) -> Result<Fetched, RobotsError> {
    let Asked {
        run,
        ordinal,
        host,
        request,
        interval,
    } = *asked;
    let opened_at = ObservationTime::new(clock.now());
    let mut slot = AttemptSlot::open(store, request, None, run, ordinal, opened_at)?;
    let attempt_id = slot.attempt_id().hex().to_owned();
    let opened = Opened {
        attempt_id: &attempt_id,
        run,
        clock,
    };
    let permit = or_abandon::<_, _, RobotsError>(slot.take_permit(), store, &opened)?;
    let answer = or_abandon::<_, _, RobotsError>(
        sent(exchange.send(permit, request, clock)),
        store,
        &opened,
    )?;
    let verdict = verdict_of(&answer);
    // Every body is stored and linked, a 403 page and a cut body included (P10, section 3.9).
    let payload = Payload::stored(request, &answer, |body| {
        store.put_blob(body).map(|digest| digest.hex().to_owned())
    });
    let payload = or_abandon::<_, _, RobotsError>(payload, store, &opened)?;
    let robots = match (&payload, verdict) {
        (Some(stored), Verdict::Usable) => {
            let read = store.blob_named(&stored.raw_blob);
            Some(RobotsAnswer::Present(or_abandon::<_, _, RobotsError>(
                read, store, &opened,
            )?))
        }
        (Some(_), Verdict::Empty) => Some(RobotsAnswer::Absent),
        _ => None,
    };
    let outcome = Outcome::decide(
        &verdict,
        Counts {
            vendor_rows: NO_VENDOR_ROWS,
            listing_unusable: 0,
            robots_disallowed: 0,
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
            store.record_robots(closed, host, answered, stored, at)?
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
        body: match (&robots, &answer) {
            (Some(RobotsAnswer::Present(_)), Ok(response)) => Some(response.body().to_vec()),
            _ => None,
        },
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
            "url": url_of(&row.host),
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
