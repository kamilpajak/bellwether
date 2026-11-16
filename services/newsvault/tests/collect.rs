//! `newsvault collect` and `newsvault items`, driven over the committed EDGAR fixtures and the
//! real provider. No test here opens a socket: `Exchange` is the seam and this file's
//! implementation of it answers out of `providers/edgar-press/tests/fixtures/`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{DateTime, NaiveDate, Utc};
use news_provider::{
    Archive, ArchiveError, AttemptEvent, AttemptReceipt, AttemptSlot, Clock, CollectedItem,
    Committed, CredentialRef, Exchange, Exit, HeaderValue, LedgerError, Provenance, RawResponse,
    Request, RequestPhase, RunId, SendFailure, SendPermit, Sink, SourceId, TargetWindow,
};
use news_provider::{CollectError, PlanRefusal, Run};
use newsvault::cmd_collect::{self, Ask};
use newsvault::cmd_items::{self, PerStory};
use newsvault::cmd_ledger;
use newsvault::store::{Committer, COMMIT_LOG};
use pit::ObservationTime;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tempfile::TempDir;

mod seed;

type TestResult = Result<(), Box<dyn Error>>;

/// Inside the day the fixtures name, so every window the tests ask for is a live ingest and no
/// probed observation lag is needed (decision D5).
const AT: &str = "2026-11-10T23:00:00Z";
/// The one session day the quarter listing names and the daily-index fixture exists for.
const A_LISTED_SESSION: &str = "2026-11-10";
/// A session the calendar names and the listing does not: not published yet.
const AN_UNLISTED_SESSION: &str = "2026-11-12";
/// A Saturday, which the calendar closes.
const A_CLOSED_DAY: &str = "2026-11-07";
/// Past the last microsecond of 2026-11-10 in New York, which is 2026-11-11T04:59:59.999999Z.
const AFTER_THE_NEW_YORK_DAY_CLOSED: &str = "2026-11-11T05:00:00Z";
/// A range naming no day at all, which is the unclean planning reason that ships.
const AN_INVERTED_RANGE: &str = "2026-11-12..2026-11-09";
const SOURCE: &str = "edgar_press";
const CONTACT: &str = "bellwether collector contact@example.test";
/// The address half of `CONTACT`, which is the part that is a person.
const CONTACT_ADDRESS: &str = "contact@example.test";
const SCHEDULE: &str = "nightly";
const EXPECTED_RUNS: u32 = 1;

const INDEX_URL: &str =
    "https://www.sec.gov/Archives/edgar/daily-index/2026/QTR4/form.20261110.idx";
const APPLE_URL: &str = "https://www.sec.gov/Archives/edgar/data/320193/0000320193-26-000071.txt";
const MICROSOFT_URL: &str =
    "https://www.sec.gov/Archives/edgar/data/789019/0000789019-26-000045.txt";

const OK: u16 = 200;
const FORBIDDEN: u16 = 403;
/// A server error, which `edgar-press::classify` reads as transient from the status alone (the body
/// is not consulted for a 5xx), so any body fixture states it.
const SERVICE_UNAVAILABLE: u16 = 503;

const EXIT_OK: i32 = 0;
const EXIT_USAGE: i32 = 2;
const EXIT_NOT_FOUND: i32 = 4;
const EXIT_LEASE_HELD: i32 = 7;
const EXIT_INCOMPLETE: i32 = 8;
/// One above `store::SCHEMA_VERSION`, which this build must refuse rather than write into.
const A_NEWER_USER_VERSION: i32 = newsvault::store::SCHEMA_VERSION + 1;

/// The index names four filings, two of them in universe under an in-list form type.
const VENDOR_ROWS_IN_THE_INDEX: u32 = 4;
const IN_UNIVERSE_FILINGS: u32 = 2;
/// One index attempt plus one attempt per follow-up.
const ATTEMPTS_PER_RUN: usize = 3;
/// The index payload, linked by its `payloads` row (P10), the two filings' raw payloads and the
/// two bodies extracted out of them. 4 before user_version 6, when the index payload was an
/// orphan no row named.
const BLOBS_INDEXED_PER_GOOD_DAY: u64 = 5;

fn provider_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../providers/edgar-press")
}

fn fixture(name: &str) -> PathBuf {
    provider_dir().join("tests/fixtures").join(name)
}

/// The production rules file, not a copy of it: one fact, one home.
fn rules() -> PathBuf {
    provider_dir().join("rules/edgar-press.rules.json")
}

fn at() -> DateTime<Utc> {
    AT.parse().unwrap_or(DateTime::UNIX_EPOCH)
}

fn ask(store: &Path, window: &str) -> Ask {
    Ask {
        sources: vec![SOURCE.to_owned()],
        window: window.to_owned(),
        schedule: Some(SCHEDULE.to_owned()),
        expected_runs: EXPECTED_RUNS,
        store: store.to_path_buf(),
        calendar: Some(fixture("calendar.json")),
        listing: Some(fixture("quarter_index.json")),
        roster: Some(fixture("roster.json")),
        hosts: None,
        rules: rules(),
        lag_policy: None,
        contact: CONTACT.to_owned(),
        at: at(),
        dry_run: false,
    }
}

/// One instant for every read, so two runs of one fixture differ only where the archive does.
struct Fixed;

impl Clock for Fixed {
    fn now(&self) -> DateTime<Utc> {
        at()
    }
}

/// Answers out of committed bytes, and records every url it was asked for.
struct Payloads {
    answers: BTreeMap<String, (u16, Vec<u8>)>,
    asked: RefCell<Vec<String>>,
}

impl Payloads {
    fn of(answers: &[(&str, u16, &str)]) -> Result<Self, Box<dyn Error>> {
        let mut held = BTreeMap::new();
        for (url, status, name) in answers {
            held.insert((*url).to_owned(), (*status, std::fs::read(fixture(name))?));
        }
        Ok(Self {
            answers: held,
            asked: RefCell::new(Vec::new()),
        })
    }

    /// The whole day as it really answers: an index and its two in-universe filings.
    fn a_good_day() -> Result<Self, Box<dyn Error>> {
        Self::of(&[
            (INDEX_URL, OK, "form.20261110.idx"),
            (APPLE_URL, OK, "filing_8k_apple.txt"),
            (MICROSOFT_URL, OK, "filing_8k_microsoft_no_acceptance.txt"),
        ])
    }

    fn sends(&self) -> usize {
        self.asked.borrow().len()
    }
}

impl Exchange for Payloads {
    fn send(
        &self,
        permit: SendPermit,
        request: &Request,
        clock: &dyn Clock,
    ) -> Result<RawResponse, SendFailure> {
        self.asked.borrow_mut().push(request.url.clone());
        let attempt = permit.attempt_id().clone();
        let requested_at = ObservationTime::new(clock.now());
        let received_at = ObservationTime::new(clock.now());
        match self.answers.get(&request.url) {
            Some((status, body)) => Ok(RawResponse::new(
                attempt,
                *status,
                body.clone(),
                requested_at,
                received_at,
            )),
            None => Err(SendFailure::Transport(news_provider::TransportError::new(
                attempt,
                requested_at,
                received_at,
                format!("no fixture answers {}", request.url),
            ))),
        }
    }
}

/// Answers from a per-url SEQUENCE, so one url can fail on its first call and succeed on the next,
/// which a static `Payloads` cannot say. Past the end of a url's sequence it repeats the last
/// answer, so an always-failing url is one entry long. Records every url it was asked for, like
/// `Payloads`, so a retry is visible as a second ask.
struct Flaky {
    answers: BTreeMap<String, Vec<(u16, Vec<u8>)>>,
    calls: RefCell<BTreeMap<String, usize>>,
    asked: RefCell<Vec<String>>,
}

impl Flaky {
    fn of(answers: &[(&str, &[(u16, &str)])]) -> Result<Self, Box<dyn Error>> {
        let mut held = BTreeMap::new();
        for (url, sequence) in answers {
            let mut responses = Vec::new();
            for (status, name) in *sequence {
                responses.push((*status, std::fs::read(fixture(name))?));
            }
            held.insert((*url).to_owned(), responses);
        }
        Ok(Self {
            answers: held,
            calls: RefCell::new(BTreeMap::new()),
            asked: RefCell::new(Vec::new()),
        })
    }
}

impl Exchange for Flaky {
    fn send(
        &self,
        permit: SendPermit,
        request: &Request,
        clock: &dyn Clock,
    ) -> Result<RawResponse, SendFailure> {
        self.asked.borrow_mut().push(request.url.clone());
        let attempt = permit.attempt_id().clone();
        let requested_at = ObservationTime::new(clock.now());
        let received_at = ObservationTime::new(clock.now());
        let nth = {
            let mut calls = self.calls.borrow_mut();
            let seen = calls.entry(request.url.clone()).or_insert(0);
            let nth = *seen;
            *seen += 1;
            nth
        };
        let answer = self
            .answers
            .get(&request.url)
            .and_then(|sequence| sequence.get(nth.min(sequence.len().saturating_sub(1))));
        match answer {
            Some((status, body)) => Ok(RawResponse::new(
                attempt,
                *status,
                body.clone(),
                requested_at,
                received_at,
            )),
            None => Err(SendFailure::Transport(news_provider::TransportError::new(
                attempt,
                requested_at,
                received_at,
                format!("no fixture answers {}", request.url),
            ))),
        }
    }
}

/// A real archive whose `commit` refuses one attempt, which is the only way to reach the
/// `commit_failed` bucket of the exit table without corrupting a store.
struct Refusing {
    store: Committer,
    refuse_commit: usize,
    commits: RefCell<usize>,
}

impl Sink for Refusing {
    fn write(&mut self, event: &AttemptEvent) -> Result<(), LedgerError> {
        self.store.write(event)
    }
}

impl Archive for Refusing {
    fn put_blob(&self, payload: &[u8]) -> Result<String, ArchiveError> {
        Archive::put_blob(&self.store, payload)
    }

    fn commit(
        &mut self,
        closed: AttemptReceipt,
        items: &[CollectedItem],
        payload: Option<&news_provider::Payload>,
        observed: ObservationTime,
    ) -> Result<Committed, ArchiveError> {
        let nth = *self.commits.borrow();
        *self.commits.borrow_mut() = nth + 1;
        if nth == self.refuse_commit {
            return Err(ArchiveError {
                act: "commit",
                detail: format!("refused attempt {}", closed.attempt_id().hex()),
            });
        }
        Archive::commit(&mut self.store, closed, items, payload, observed)
    }

    fn ok_vendor_rows(&self, source: &SourceId) -> Result<Vec<u32>, ArchiveError> {
        Archive::ok_vendor_rows(&self.store, source)
    }

    fn attempts_in_flight(&self) -> Result<Vec<String>, ArchiveError> {
        Archive::attempts_in_flight(&self.store)
    }

    fn covered_story_ids(
        &self,
        source: &SourceId,
        target_window: &TargetWindow,
    ) -> Result<std::collections::BTreeSet<String>, ArchiveError> {
        Archive::covered_story_ids(&self.store, source, target_window)
    }

    fn held_story_ids(
        &self,
        source: &SourceId,
    ) -> Result<std::collections::BTreeSet<String>, ArchiveError> {
        Archive::held_story_ids(&self.store, source)
    }

    fn record_planned_coverage(
        &mut self,
        attempt_id: &str,
        matched: u32,
    ) -> Result<(), ArchiveError> {
        Archive::record_planned_coverage(&mut self.store, attempt_id, matched)
    }

    fn record_held(&mut self, attempt_id: &str, held: u32) -> Result<(), ArchiveError> {
        Archive::record_held(&mut self.store, attempt_id, held)
    }
}

/// One run through the library, with the exchange the caller chose (static `Payloads` or a
/// sequenced `Flaky`).
fn collected(
    store: &Path,
    window: &str,
    exchange: &dyn Exchange,
) -> Result<cmd_collect::Answer, Box<dyn Error>> {
    let ask = ask(store, window);
    let provider = cmd_collect::open_provider(&ask)?;
    Ok(cmd_collect::run(&ask, &provider, exchange, &Fixed)?)
}

/// A store holding one good day, collected `runs` times. The lease is released before it returns.
fn a_collected_store(runs: usize) -> Result<TempDir, Box<dyn Error>> {
    let root = TempDir::new()?;
    for run in 0..runs {
        let mut ask = ask(root.path(), A_LISTED_SESSION);
        ask.schedule = Some(format!("{SCHEDULE}-{run}"));
        let provider = cmd_collect::open_provider(&ask)?;
        let answer = cmd_collect::run(&ask, &provider, &Payloads::a_good_day()?, &Fixed)?;
        assert_eq!(answer.exit, Exit::Ok, "{:#}", answer.envelope);
    }
    Ok(root)
}

struct Answered {
    code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs the binary with stdin closed, which is the shape a scheduler gives it.
fn binary(args: &[&str]) -> Result<Answered, Box<dyn Error>> {
    binary_env(
        args,
        &[
            ("BELLWETHER_SEC_CONTACT", CONTACT),
            ("BELLWETHER_FEED_CONTACT", CONTACT),
        ],
    )
}

/// `binary` with exactly the contact variables named, and any other variable a test sets.
fn binary_env(args: &[&str], env: &[(&str, &str)]) -> Result<Answered, Box<dyn Error>> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_newsvault"));
    command
        .env_remove("BELLWETHER_SEC_CONTACT")
        .env_remove("BELLWETHER_FEED_CONTACT");
    for (name, value) in env {
        command.env(name, value);
    }
    let mut child = command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    drop(child.stdin.take());
    let output = child.wait_with_output()?;
    Ok(Answered {
        code: output.status.code().unwrap_or(-1),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn collect_args(store: &Path, window: &str, extra: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = ["collect", "--source", SOURCE, "--window", window]
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect();
    for (flag, value) in [
        ("--store", store.to_string_lossy().into_owned()),
        ("--calendar", path(&fixture("calendar.json"))),
        ("--listing", path(&fixture("quarter_index.json"))),
        ("--roster", path(&fixture("roster.json"))),
        ("--rules", path(&rules())),
        ("--at", AT.to_owned()),
        ("--format", "json".to_owned()),
    ] {
        args.push(flag.to_owned());
        args.push(value);
    }
    args.extend(extra.iter().map(|arg| (*arg).to_owned()));
    args
}

fn path(of: &Path) -> String {
    of.to_string_lossy().into_owned()
}

fn run_collect(store: &Path, window: &str, extra: &[&str]) -> Result<Answered, Box<dyn Error>> {
    let args = collect_args(store, window, extra);
    binary(&args.iter().map(String::as_str).collect::<Vec<&str>>())
}

/// Every path under a root with its bytes, so "byte-identical" is one equality.
fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut found = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                found.insert(path.to_string_lossy().into_owned(), bytes);
            }
        }
    }
    found
}

#[test]
fn a_dry_run_leaves_the_store_byte_identical_and_calls_nothing() -> TestResult {
    let root = a_collected_store(1)?;
    let before = tree(root.path());
    let mut dry = ask(root.path(), A_LISTED_SESSION);
    dry.dry_run = true;
    let provider = cmd_collect::open_provider(&dry)?;
    let exchange = Payloads::a_good_day()?;
    let answer = cmd_collect::run(&dry, &provider, &exchange, &Fixed)?;
    assert_eq!(
        (
            exchange.sends(),
            answer.exit,
            answer.envelope["changed"].as_bool()
        ),
        (0, Exit::Ok, Some(false))
    );
    assert_eq!(
        answer.envelope["plan"]["requests"].as_u64(),
        Some(1),
        "{:#}",
        answer.envelope
    );
    assert_eq!(tree(root.path()), before);
    Ok(())
}

#[test]
fn stdout_parses_as_exactly_one_json_value_and_the_parse_consumes_the_stream() -> TestResult {
    let root = TempDir::new()?;
    let answered = run_collect(root.path(), A_CLOSED_DAY, &["--dry-run"])?;
    let mut values = serde_json::Deserializer::from_slice(&answered.stdout).into_iter::<Value>();
    let first = values.next().transpose()?;
    assert!(first.is_some(), "{answered:?}", answered = answered.code);
    assert!(values.next().is_none());
    Ok(())
}

#[test]
fn with_stdin_not_a_tty_the_binary_never_reads_dev_tty_and_never_blocks() -> TestResult {
    let root = TempDir::new()?;
    let answered = run_collect(root.path(), A_CLOSED_DAY, &["--non-interactive"])?;
    assert_eq!(answered.code, EXIT_OK);
    // The other half of the claim: no command here has a prompt to fall back on. The three
    // spellings a read would have to use, not the word, so a doc comment saying "reads stdin"
    // does not pass for an implementation that does.
    const NO_PROMPT: [&str; 4] = ["/dev/tty", "io::stdin", "read_line", "Stdin"];
    let sources = [
        include_str!("../src/cmd_collect.rs"),
        include_str!("../src/cmd_items.rs"),
        include_str!("../src/main.rs"),
    ];
    for source in sources {
        for spelling in NO_PROMPT {
            assert!(!source.contains(spelling), "a command holds {spelling}");
        }
    }
    Ok(())
}

#[test]
fn the_run_envelope_exists_on_disk_before_the_first_send_and_before_the_lease() -> TestResult {
    let root = TempDir::new()?;
    let held = Committer::open(root.path(), &RunId::new("the-holder"))?;
    let exchange = Payloads::a_good_day()?;
    let ask = ask(root.path(), A_LISTED_SESSION);
    let provider = cmd_collect::open_provider(&ask)?;
    let blocked = cmd_collect::run(&ask, &provider, &exchange, &Fixed);
    assert!(blocked.is_err(), "the lease was not held");
    assert_eq!(exchange.sends(), 0);
    let envelopes = tree(&root.path().join("runs"));
    let written: Vec<Value> = envelopes
        .values()
        .filter_map(|bytes| serde_json::from_slice(bytes).ok())
        .collect();
    assert_eq!(written.len(), 1, "{envelopes:?}");
    assert_eq!(
        (
            written[0]["schema"].as_str(),
            written[0]["finished_at"].is_null()
        ),
        (Some("newsvault.run/1"), true)
    );
    drop(held);
    Ok(())
}

#[test]
fn a_held_lease_exits_seven_and_leaves_a_run_record_rather_than_a_hole() -> TestResult {
    let root = TempDir::new()?;
    let held = Committer::open(root.path(), &RunId::new("the-holder"))?;
    let answered = run_collect(root.path(), A_LISTED_SESSION, &[])?;
    assert_eq!((answered.code, answered.stdout.len()), (EXIT_LEASE_HELD, 0));
    assert_eq!(tree(&root.path().join("runs")).len(), 1);
    drop(held);
    Ok(())
}

#[test]
fn every_target_erring_transiently_with_nothing_committed_exits_seven() -> TestResult {
    let root = TempDir::new()?;
    // No fixture answers the index url, so the conversation itself fails.
    let answer = collected(root.path(), A_LISTED_SESSION, &Payloads::of(&[])?)?;
    assert_eq!(
        (
            answer.exit,
            answer.exit.code(),
            answer.exit.retryable(),
            answer.exit.carries_the_result_on_stdout()
        ),
        (Exit::VendorUnreachable, 7, true, false)
    );
    Ok(())
}

#[test]
fn every_target_failing_permanently_with_nothing_committed_exits_eight_and_is_not_retryable(
) -> TestResult {
    let root = TempDir::new()?;
    let refused = Payloads::of(&[(INDEX_URL, FORBIDDEN, "error_403_access_denied.xml")])?;
    let answer = collected(root.path(), A_LISTED_SESSION, &refused)?;
    assert_eq!(
        (answer.exit, answer.exit.code(), answer.exit.retryable()),
        (Exit::VendorRefusedAll, EXIT_INCOMPLETE as u8, false)
    );
    Ok(())
}

#[test]
fn a_mixed_run_exits_eight_with_the_envelope_still_on_stdout() -> TestResult {
    let root = TempDir::new()?;
    let mixed = Payloads::of(&[
        (INDEX_URL, OK, "form.20261110.idx"),
        (APPLE_URL, OK, "filing_8k_apple.txt"),
        (MICROSOFT_URL, FORBIDDEN, "error_403_traffic.html"),
    ])?;
    let answer = collected(root.path(), A_LISTED_SESSION, &mixed)?;
    assert_eq!(
        (
            answer.exit,
            answer.exit.carries_the_result_on_stdout(),
            answer.envelope["items_committed"].as_u64()
        ),
        (Exit::PartialCollect, true, Some(1))
    );
    Ok(())
}

#[test]
fn a_commit_the_store_refused_exits_eight_and_names_the_attempt() -> TestResult {
    let root = TempDir::new()?;
    let ask = ask(root.path(), A_LISTED_SESSION);
    let provider = cmd_collect::open_provider(&ask)?;
    let mut archive = Refusing {
        store: Committer::open(root.path(), &RunId::new("refusing"))?,
        refuse_commit: 1,
        commits: RefCell::new(0),
    };
    let run = RunId::new("refusing");
    let driven = cmd_collect::drive(
        &ask,
        provider.provider.as_ref(),
        &mut archive,
        &Payloads::a_good_day()?,
        &Fixed,
        &run,
    )?;
    let collected = driven.collected;
    let named: Vec<&str> = collected
        .attempts
        .iter()
        .filter_map(|attempt| attempt.commit_refused.as_deref())
        .collect();
    assert_eq!(collected.exit(), Exit::CommitFailed);
    assert_eq!(named.len(), 1, "{:?}", collected.attempts);
    assert!(named[0].contains("refused attempt"), "{named:?}");
    Ok(())
}

/// An archive that records every call made to it, so "refused before anything" is a count.
#[derive(Default)]
struct Untouched {
    calls: RefCell<Vec<&'static str>>,
}

impl Untouched {
    fn touched(&self, call: &'static str) -> ArchiveError {
        self.calls.borrow_mut().push(call);
        ArchiveError {
            act: call,
            detail: "this archive was not to be touched".to_owned(),
        }
    }
}

impl Sink for Untouched {
    fn write(&mut self, _event: &AttemptEvent) -> Result<(), LedgerError> {
        self.calls.borrow_mut().push("write");
        Ok(())
    }
}

impl Archive for Untouched {
    fn put_blob(&self, _payload: &[u8]) -> Result<String, ArchiveError> {
        Err(self.touched("put_blob"))
    }

    fn commit(
        &mut self,
        _closed: AttemptReceipt,
        _items: &[CollectedItem],
        _payload: Option<&news_provider::Payload>,
        _observed: ObservationTime,
    ) -> Result<Committed, ArchiveError> {
        Err(self.touched("commit"))
    }

    fn ok_vendor_rows(&self, _source: &SourceId) -> Result<Vec<u32>, ArchiveError> {
        Err(self.touched("ok_vendor_rows"))
    }

    fn attempts_in_flight(&self) -> Result<Vec<String>, ArchiveError> {
        Err(self.touched("attempts_in_flight"))
    }

    fn covered_story_ids(
        &self,
        _source: &SourceId,
        _target_window: &TargetWindow,
    ) -> Result<std::collections::BTreeSet<String>, ArchiveError> {
        Err(self.touched("covered_story_ids"))
    }

    fn held_story_ids(
        &self,
        _source: &SourceId,
    ) -> Result<std::collections::BTreeSet<String>, ArchiveError> {
        Err(self.touched("held_story_ids"))
    }

    fn record_planned_coverage(
        &mut self,
        _attempt_id: &str,
        _matched: u32,
    ) -> Result<(), ArchiveError> {
        Err(self.touched("record_planned_coverage"))
    }

    fn record_held(&mut self, _attempt_id: &str, _held: u32) -> Result<(), ArchiveError> {
        Err(self.touched("record_held"))
    }
}

/// Architecture.md section 11.2: a run whose inputs do not match the provider's declaration is
/// refused by the driver before the startup sweep reads the archive and before any send.
#[test]
fn a_session_bound_run_with_no_calendar_is_refused_before_any_request() -> TestResult {
    let root = TempDir::new()?;
    let ask = ask(root.path(), A_LISTED_SESSION);
    let opened = cmd_collect::open_provider(&ask)?;
    let listing = edgar_press::read_listing(&std::fs::read(
        ask.listing
            .as_deref()
            .ok_or("an edgar ask names a listing")?,
    )?)?;
    let exchange = Payloads::a_good_day()?;
    let mut archive = Untouched::default();
    let refused = news_provider::collect(
        opened.provider.as_ref(),
        &mut archive,
        &exchange,
        &Fixed,
        &Run {
            run: &RunId::new("run-no-calendar"),
            target: &TargetWindow::new(A_LISTED_SESSION),
            calendar: None,
            listing: Some(&listing),
            at: ObservationTime::new(at()),
        },
    );
    let named = matches!(
        refused,
        Err(CollectError::Planning(PlanRefusal::InputsDoNotMatchCadence { why, .. }))
            if why == "has no calendar"
    );
    assert!(named, "{refused:?}");
    assert_eq!(
        (exchange.sends(), archive.calls.borrow().clone()),
        (0, Vec::<&str>::new())
    );
    Ok(())
}

#[test]
fn a_run_with_every_target_not_planned_for_a_closed_market_exits_zero() -> TestResult {
    let root = TempDir::new()?;
    let answered = run_collect(root.path(), A_CLOSED_DAY, &[])?;
    let envelope: Value = serde_json::from_slice(&answered.stdout)?;
    assert_eq!(answered.code, EXIT_OK);
    assert_eq!(
        (
            envelope["plan"]["requests"].as_u64(),
            envelope["plan"]["not_planned"][0]["reason"].as_str()
        ),
        (Some(0), Some("calendar_closed"))
    );
    Ok(())
}

/// The design calls this bucket "skipped for budget". No budget reserves anything in the tree, so
/// the unclean planning reason that ships is a window naming no day at all.
#[test]
fn a_run_whose_target_window_names_no_day_exits_eight_rather_than_zero() -> TestResult {
    let root = TempDir::new()?;
    let answered = run_collect(root.path(), AN_INVERTED_RANGE, &[])?;
    let envelope: Value = serde_json::from_slice(&answered.stdout)?;
    assert_eq!(
        (
            answered.code,
            envelope["plan"]["not_planned"][0]["reason"].as_str(),
            envelope["plan"]["not_planned"][0]["clean"].as_bool()
        ),
        (EXIT_INCOMPLETE, Some("plan_empty"), Some(false))
    );
    Ok(())
}

#[test]
fn per_source_carries_an_entry_for_every_planned_source_including_zeros() -> TestResult {
    let root = TempDir::new()?;
    let answered = run_collect(root.path(), AN_UNLISTED_SESSION, &[])?;
    let envelope: Value = serde_json::from_slice(&answered.stdout)?;
    let per_source = &envelope["per_source"][0];
    assert_eq!(
        (
            answered.code,
            per_source["source"].as_str(),
            per_source["attempts"].as_u64(),
            per_source["items_committed"].as_u64(),
            per_source["not_planned"].as_u64()
        ),
        (EXIT_OK, Some(SOURCE), Some(0), Some(0), Some(1))
    );
    Ok(())
}

/// P10 (section 3.9), edgar through the generic path: the daily index payload, which no item row
/// names, is indexed and linked, and every blob file has a `blobs` row. The edgar index answers with
/// no allowlisted header in `Payloads`, so each link stores `[]`.
#[test]
fn every_body_an_edgar_run_stores_is_indexed_and_linked_to_its_attempt() -> TestResult {
    let root = TempDir::new()?;
    let ask = ask(root.path(), A_LISTED_SESSION);
    let provider = cmd_collect::open_provider(&ask)?;
    cmd_collect::run(&ask, &provider, &Payloads::a_good_day()?, &Fixed)?;
    let db = Connection::open_with_flags(
        root.path().join(COMMIT_LOG),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let indexed: std::collections::BTreeSet<String> = db
        .prepare("SELECT digest FROM blobs")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let mut on_disk = std::collections::BTreeSet::new();
    for first in std::fs::read_dir(root.path().join("blobs/sha256"))? {
        for second in std::fs::read_dir(first?.path())? {
            for file in std::fs::read_dir(second?.path())? {
                on_disk.insert(file?.file_name().to_string_lossy().into_owned());
            }
        }
    }
    let linked: Vec<(String, bool, bool, String)> = db
        .prepare(
            "SELECT p.phase, p.listed_date IS NOT NULL, p.truncated, p.headers
               FROM attempts a JOIN payloads p ON p.attempt_id = a.attempt_id
              ORDER BY p.phase DESC, a.attempt_id",
        )?
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<Result<_, _>>()?;
    let document = ("document".to_owned(), true, false, "[]".to_owned());
    assert_eq!(
        (on_disk, linked),
        (
            indexed,
            vec![
                ("index".to_owned(), false, false, "[]".to_owned()),
                document.clone(),
                document
            ]
        )
    );
    Ok(())
}

#[test]
fn two_identical_collects_leave_the_right_events_one_blob_set_and_items_new_one_then_zero(
) -> TestResult {
    let root = TempDir::new()?;
    let mut counted = Vec::new();
    for run in 0..2 {
        let mut ask = ask(root.path(), A_LISTED_SESSION);
        ask.schedule = Some(format!("{SCHEDULE}-{run}"));
        let provider = cmd_collect::open_provider(&ask)?;
        let answer = cmd_collect::run(&ask, &provider, &Payloads::a_good_day()?, &Fixed)?;
        counted.push((
            answer.envelope["attempts"].as_array().map(Vec::len),
            answer.envelope["items_committed"].as_u64(),
            answer.envelope["blobs_indexed"].as_u64(),
        ));
        assert!(answer.envelope["extract_version"].is_null());
    }
    assert_eq!(
        counted,
        vec![
            (
                Some(ATTEMPTS_PER_RUN),
                Some(u64::from(IN_UNIVERSE_FILINGS)),
                Some(BLOBS_INDEXED_PER_GOOD_DAY)
            ),
            (
                Some(ATTEMPTS_PER_RUN),
                Some(u64::from(IN_UNIVERSE_FILINGS)),
                Some(0)
            ),
        ]
    );
    // `items_new` is the store's own count of the rows that became the kept sighting for their
    // story, so the second run's two rows add nothing: two over six attempts, never four.
    let db = Connection::open_with_flags(
        root.path().join(COMMIT_LOG),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let items_new: i64 = db.query_row(
        "SELECT COALESCE(SUM(items_new), 0) FROM attempts",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(items_new, i64::from(IN_UNIVERSE_FILINGS));
    // `extract_version` NULL on every edgar-press attempt (section 3.2). Mutation 2026-11-16, both
    // edgar `Request` literals given a version: red.
    let versioned: i64 = db.query_row(
        "SELECT COUNT(*) FROM attempts WHERE extract_version IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(versioned, 0);
    Ok(())
}

/// Accepted attack A3.7: the address SEC requires in the User-Agent lands in an archive the write
/// gate makes undeletable. The credential travels by NAME and is substituted inside `Wire::send`,
/// so this asserts over what was WRITTEN rather than over the code: the run envelope the caller
/// gets, the one on disk, and every text column of the two ledger tables. Mutation 2026-11-13:
/// one line adding `"contact": ask.contact` to `cmd_collect::envelope_of` turns it red.
#[test]
fn a_literal_contact_shaped_header_value_turns_a_named_test_red() -> TestResult {
    let root = TempDir::new()?;
    let answer = collected(root.path(), A_LISTED_SESSION, &Payloads::a_good_day()?)?;
    let mut written: Vec<(String, String)> = vec![(
        "the envelope on stdout".to_owned(),
        answer.envelope.to_string(),
    )];
    for (path, bytes) in tree(root.path()) {
        if path.ends_with(".json") {
            written.push((path, String::from_utf8_lossy(&bytes).into_owned()));
        }
    }
    let db = Connection::open_with_flags(
        root.path().join(COMMIT_LOG),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    for table in ["attempts", "items"] {
        let mut columns = db.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?;
        let names: Vec<String> = columns
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<String>, _>>()?;
        assert!(!names.is_empty(), "{table} has no columns");
        for column in names {
            let mut values = db.prepare(&format!(
                "SELECT CAST({column} AS TEXT) FROM {table} WHERE {column} IS NOT NULL"
            ))?;
            let held: Vec<String> = values
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<String>, _>>()?;
            written.push((format!("{table}.{column}"), held.join("\n")));
        }
    }
    for (where_it_is, text) in written {
        assert!(
            !text.contains(CONTACT_ADDRESS),
            "{where_it_is} carries the contact address itself"
        );
        // The shape and not only this fixture's value: an address is the one thing in a request
        // that is a person, and no column of either table has another use for `@`.
        assert!(
            !text.contains('@'),
            "{where_it_is} carries an address-shaped value"
        );
    }
    Ok(())
}

/// Section 11.1, `attempts.story_id`: the driver names a Document attempt's story from
/// `Provider::story_of` and an index attempt's not at all. Mutation 2026-11-15, the driver
/// passing None: red.
#[test]
fn a_document_attempt_opens_under_the_story_its_provider_names() -> TestResult {
    let root = TempDir::new()?;
    collected(root.path(), A_LISTED_SESSION, &Payloads::a_good_day()?)?;
    let db = Connection::open_with_flags(
        root.path().join(COMMIT_LOG),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut statement = db.prepare("SELECT story_id FROM attempts ORDER BY story_id")?;
    let stories = statement
        .query_map([], |row| row.get::<_, Option<String>>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut expected = vec![None];
    for url in [APPLE_URL, MICROSOFT_URL] {
        let accession = url.rsplit('/').next().unwrap_or_default();
        let story = canon::digest::story_id(SOURCE, accession.trim_end_matches(".txt"))
            .map_err(|failure| failure.to_string())?;
        expected.push(Some(story.hex().to_owned()));
    }
    expected.sort();
    assert_eq!(stories, expected);
    Ok(())
}

#[test]
fn the_index_attempt_counts_the_vendors_rows_and_our_own_filtering_separately() -> TestResult {
    let root = TempDir::new()?;
    let answer = collected(root.path(), A_LISTED_SESSION, &Payloads::a_good_day()?)?;
    let index = &answer.envelope["attempts"][0];
    assert_eq!(
        (
            index["phase"].as_str(),
            index["vendor_rows"].as_u64(),
            index["filtered_out"].as_u64()
        ),
        (
            Some("index"),
            Some(u64::from(VENDOR_ROWS_IN_THE_INDEX)),
            Some(u64::from(VENDOR_ROWS_IN_THE_INDEX - IN_UNIVERSE_FILINGS))
        )
    );
    Ok(())
}

#[test]
fn an_attempt_a_dead_run_left_open_is_closed_interrupted_by_the_next_run() -> TestResult {
    let root = TempDir::new()?;
    let dead = RunId::new("the-run-that-died");
    let mut store = Committer::open(root.path(), &dead)?;
    let ask = ask(root.path(), A_LISTED_SESSION);
    let provider = cmd_collect::open_provider(&ask)?;
    let abandoned = Request {
        source: SourceId::new(SOURCE),
        target_window: TargetWindow::new(A_LISTED_SESSION),
        phase: RequestPhase::Index,
        provenance: Provenance::LiveIngest,
        url: INDEX_URL.to_owned(),
        headers: vec![(
            news_provider::HeaderName::new("Accept"),
            HeaderValue::Literal("text/plain".to_owned()),
        )],
        params: Vec::new(),
        universe_version: provider.provider.universe_version(),
        listed_date: None,
        hosts_version: None,
        extract_version: None,
    };
    let orphan = AttemptSlot::open(
        &mut store,
        &abandoned,
        None,
        &dead,
        0,
        ObservationTime::new(at()),
    )?;
    let orphaned = orphan.attempt_id().hex().to_owned();
    drop(orphan);
    drop(store);
    let answer = collected(root.path(), A_LISTED_SESSION, &Payloads::a_good_day()?)?;
    let swept: Vec<&str> = answer.envelope["swept"]
        .as_array()
        .map(|rows| rows.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert_eq!(swept, vec![orphaned.as_str()]);
    assert_eq!(answer.exit, Exit::PartialCollect);
    Ok(())
}

#[test]
fn items_without_at_is_a_usage_error() -> TestResult {
    let root = TempDir::new()?;
    let answered = binary(&["items", "--store", &path(root.path())])?;
    assert_eq!((answered.code, answered.stdout.len()), (EXIT_USAGE, 0));
    Ok(())
}

#[test]
fn items_reports_rows_visible_beside_stories_visible() -> TestResult {
    let root = a_collected_store(2)?;
    let report = cmd_items::read(root.path(), &items_ask())?;
    assert_eq!((report.rows_visible, report.stories_visible), (2, 1));
    Ok(())
}

#[test]
fn per_story_latest_returns_the_highest_seq_among_the_visible_siblings() -> TestResult {
    let root = a_collected_store(2)?;
    let all = cmd_items::read(root.path(), &items_ask())?;
    let latest = cmd_items::read(
        root.path(),
        &cmd_items::Ask {
            per_story: PerStory::Latest,
            ..items_ask()
        },
    )?;
    assert_eq!((all.returned, latest.returned), (2, 1));
    Ok(())
}

/// The fatal fix reaching the view. The Microsoft fixture carries no acceptance instant, so its
/// publication value is the index's own DAY in New York; a day bounded by UTC midnight is 19:00
/// there and would admit the whole afternoon.
#[test]
fn a_day_dated_item_is_invisible_until_its_own_calendar_day_has_closed() -> TestResult {
    let root = a_collected_store(1)?;
    let during = cmd_items::read(root.path(), &items_ask())?;
    let after = cmd_items::read(
        root.path(),
        &cmd_items::Ask {
            at: AFTER_THE_NEW_YORK_DAY_CLOSED.parse()?,
            ..items_ask()
        },
    )?;
    let precision = |report: &cmd_items::Report| -> Vec<String> {
        report
            .items
            .iter()
            .filter_map(|item| item["valid_precision"].as_str().map(str::to_owned))
            .collect()
    };
    assert_eq!(precision(&during), vec!["instant".to_owned()]);
    assert_eq!(
        precision(&after),
        vec!["instant".to_owned(), "day".to_owned()]
    );
    Ok(())
}

fn items_ask() -> cmd_items::Ask {
    cmd_items::Ask {
        at: at(),
        from: None,
        to: None,
        source: None,
        limit: None,
        fields: None,
        per_story: PerStory::All,
    }
}

/// The production policy file, not a copy of it: one fact, one home, the same rule `rules()`
/// follows. Its numbers are a measurement and this file asserts the plumbing, never the value.
fn lag_policy() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../news-provider/policy/observation-lag.json")
}

/// A window earlier than the run's own date with the policy behind it, which is the arm decision
/// D5 refuses without one.
fn an_archive_ask(store: &Path) -> Ask {
    Ask {
        at: AFTER_THE_NEW_YORK_DAY_CLOSED
            .parse()
            .unwrap_or(DateTime::UNIX_EPOCH),
        lag_policy: Some(lag_policy()),
        ..ask(store, A_LISTED_SESSION)
    }
}

#[test]
fn a_vendor_archive_window_collects_once_the_measured_lag_is_handed_to_the_provider() -> TestResult
{
    let root = TempDir::new()?;
    let without = Ask {
        lag_policy: None,
        ..an_archive_ask(root.path())
    };
    let opened = cmd_collect::open_provider(&without)?;
    let refused = cmd_collect::run(&without, &opened, &Payloads::a_good_day()?, &Fixed);
    assert!(refused.is_err(), "the window was not refused without a lag");
    let with = an_archive_ask(root.path());
    let opened = cmd_collect::open_provider(&with)?;
    let answer = cmd_collect::run(&with, &opened, &Payloads::a_good_day()?, &Fixed)?;
    assert_eq!(answer.exit, Exit::Ok, "{:#}", answer.envelope);
    assert_eq!(
        answer.envelope["attempts"][0]["provenance"].as_str(),
        Some("vendor_archive"),
        "{:#}",
        answer.envelope
    );
    Ok(())
}

#[test]
fn the_run_envelope_names_the_policy_and_the_probe_that_dated_its_archive_windows() -> TestResult {
    let root = TempDir::new()?;
    let with = an_archive_ask(root.path());
    let opened = cmd_collect::open_provider(&with)?;
    let answer = cmd_collect::run(&with, &opened, &Payloads::a_good_day()?, &Fixed)?;
    let stated = news_provider::LagPolicy::read(&std::fs::read(lag_policy())?)?;
    let Some(lag) = stated.lag(&SourceId::new(SOURCE), Provenance::VendorArchive) else {
        return Err("the production policy states no edgar_press vendor_archive lag".into());
    };
    assert_eq!(
        (
            answer.envelope["observation_lag_policy_version"].as_str(),
            answer.envelope["observation_lag_probe_id"].as_str(),
            answer.envelope["observation_lag_seconds"].as_i64(),
        ),
        (
            Some(stated.version().hex()),
            Some(lag.probe_id.as_str()),
            Some(lag.seconds)
        ),
        "{:#}",
        answer.envelope
    );
    // A live-ingest run states no lag at all rather than a zero, because none dated it.
    let live = ask(root.path(), A_LISTED_SESSION);
    let opened = cmd_collect::open_provider(&live)?;
    let plain = cmd_collect::run(&live, &opened, &Payloads::a_good_day()?, &Fixed)?;
    assert!(
        plain.envelope["observation_lag_probe_id"].is_null(),
        "{:#}",
        plain.envelope
    );
    Ok(())
}

#[test]
fn the_policy_that_dated_a_run_is_in_the_vault_under_its_own_digest() -> TestResult {
    let root = TempDir::new()?;
    let with = an_archive_ask(root.path());
    let opened = cmd_collect::open_provider(&with)?;
    let answer = cmd_collect::run(&with, &opened, &Payloads::a_good_day()?, &Fixed)?;
    let bytes = std::fs::read(lag_policy())?;
    let digest = canon::FileDigest::of(&bytes);
    let stored = root
        .path()
        .join("policy")
        .join(format!("{}.json", digest.hex()));
    assert_eq!(
        std::fs::read(&stored).ok().as_ref(),
        Some(&bytes),
        "{stored:?}"
    );
    assert_eq!(
        answer.envelope["observation_lag_policy_version"].as_str(),
        Some(digest.hex())
    );
    // Second use of the same bytes is the same path and no staging file is left behind.
    let again = Ask {
        schedule: Some("second".to_owned()),
        ..an_archive_ask(root.path())
    };
    let opened = cmd_collect::open_provider(&again)?;
    cmd_collect::run(&again, &opened, &Payloads::a_good_day()?, &Fixed)?;
    let kept = tree(&root.path().join("policy"));
    assert_eq!(kept.len(), 1, "{:?}", kept.keys().collect::<Vec<_>>());
    Ok(())
}

#[test]
fn a_dry_run_dates_nothing_and_copies_no_policy_into_the_vault() -> TestResult {
    let root = TempDir::new()?;
    let dry = Ask {
        dry_run: true,
        ..an_archive_ask(root.path())
    };
    let opened = cmd_collect::open_provider(&dry)?;
    let exchange = Payloads::a_good_day()?;
    let answer = cmd_collect::run(&dry, &opened, &exchange, &Fixed)?;
    assert_eq!((exchange.sends(), answer.exit), (0, Exit::Ok));
    assert!(!root.path().join("policy").exists());
    Ok(())
}

#[test]
fn an_unreadable_lag_policy_is_named_and_is_not_a_window_with_no_lag() -> TestResult {
    let root = TempDir::new()?;
    let broken = root.path().join("broken.json");
    std::fs::write(&broken, br#"{"schema": "calendar/1", "lags": []}"#)?;
    let ask = Ask {
        lag_policy: Some(broken),
        ..an_archive_ask(root.path())
    };
    let Err(why) = cmd_collect::open_provider(&ask) else {
        return Err("an unreadable policy opened a provider".into());
    };
    let said = why.to_string();
    assert!(said.contains("observation-lag policy"), "{said}");
    assert!(!said.contains("no probed observation lag"), "{said}");
    Ok(())
}

#[test]
fn a_policy_stating_only_the_other_provenance_leaves_the_archive_window_refused() -> TestResult {
    let root = TempDir::new()?;
    let live_only = root.path().join("live_only.json");
    std::fs::write(
        &live_only,
        std::fs::read_to_string(lag_policy())?.replace("vendor_archive", "live_ingest"),
    )?;
    let ask = Ask {
        lag_policy: Some(live_only),
        ..an_archive_ask(root.path())
    };
    let opened = cmd_collect::open_provider(&ask)?;
    let Err(why) = cmd_collect::run(&ask, &opened, &Payloads::a_good_day()?, &Fixed) else {
        return Err("a lag measured for the live arm dated the archive arm".into());
    };
    assert!(
        why.to_string().contains("no probed observation lag"),
        "{why}"
    );
    Ok(())
}

#[test]
fn a_staged_policy_copy_a_crash_left_behind_is_refused_rather_than_overwritten() -> TestResult {
    let root = TempDir::new()?;
    let with = an_archive_ask(root.path());
    let bytes = std::fs::read(lag_policy())?;
    let digest = canon::FileDigest::of(&bytes);
    let dir = root.path().join("policy");
    std::fs::create_dir_all(&dir)?;
    let staging = dir.join(format!(".tmp-{}.json", digest.hex()));
    std::fs::write(&staging, b"bytes a dead run staged")?;
    let opened = cmd_collect::open_provider(&with)?;
    let Err(why) = cmd_collect::run(&with, &opened, &Payloads::a_good_day()?, &Fixed) else {
        return Err("the copy reopened a staged file instead of refusing".into());
    };
    assert!(why.to_string().contains("exists"), "{why}");
    assert_eq!(std::fs::read(&staging)?, b"bytes a dead run staged");
    Ok(())
}

#[test]
fn every_option_the_collect_parser_takes_is_in_the_machine_readable_schema() -> TestResult {
    // The manifest is hand-written and the parser is derived, so the two drift silently: measured
    // 2026-11-13, `--lag-policy` worked on the command line and the schema did not name it.
    let help = String::from_utf8_lossy(&binary(&["collect", "--help"])?.stdout).to_string();
    let schema: Value = serde_json::from_slice(&binary(&["schema", "--format", "json"])?.stdout)?;
    let named: Vec<String> = schema["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|command| command["path"][0].as_str() == Some("collect"))
        .and_then(|command| command["options"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|option| option["name"].as_str().map(|name| name.replace('_', "-")))
        .collect();
    let mut missing = Vec::new();
    for word in help.split_whitespace() {
        let flag = word.trim_end_matches(|c: char| !c.is_alphanumeric());
        if let Some(long) = flag.strip_prefix("--") {
            if !long.is_empty() && long != "help" && !named.contains(&long.to_owned()) {
                missing.push(long.to_owned());
            }
        }
    }
    assert!(missing.is_empty(), "the schema names none of {missing:?}");
    Ok(())
}

/// Run 1 of a `vendor_archive` day where Microsoft fails transiently, so only Apple is covered.
fn partial_archive_day() -> Result<Payloads, Box<dyn Error>> {
    Payloads::of(&[
        (INDEX_URL, OK, "form.20261110.idx"),
        (APPLE_URL, OK, "filing_8k_apple.txt"),
        (MICROSOFT_URL, FORBIDDEN, "error_403_traffic.html"),
    ])
}

fn ledger_ask() -> Result<cmd_ledger::Ask, Box<dyn Error>> {
    Ok(cmd_ledger::Ask {
        at: AFTER_THE_NEW_YORK_DAY_CLOSED.parse()?,
        from: A_LISTED_SESSION.parse::<NaiveDate>()?,
        to: A_LISTED_SESSION.parse::<NaiveDate>()?,
        source: None,
        expected_runs: None,
        limit: None,
        check: true,
    })
}

fn asked(exchange: &Payloads) -> Vec<String> {
    exchange.asked.borrow().clone()
}

/// `attempts.held` of every index row (no story, and edgar fetches no robots), oldest first.
fn index_held(root: &Path) -> Result<Vec<Option<i64>>, Box<dyn Error>> {
    let db = Connection::open_with_flags(root.join(COMMIT_LOG), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let held = db
        .prepare("SELECT held FROM attempts WHERE story_id IS NULL ORDER BY opened_at, attempt_id")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<Vec<Option<i64>>, _>>()?;
    Ok(held)
}

/// §8 row R20: run 2 re-fetches only Microsoft, which run 1 left uncovered; `held` stays NULL
/// (§3.2). Mutations: 2026-11-13, `EdgarPress::follow_ups_to_issue` unfiltered: red (Apple asked
/// again); 2026-11-15, the driver writing `held` on the archive arm too: red.
#[test]
fn a_vendor_archive_rerun_fetches_only_the_uncovered_accession() -> TestResult {
    let root = TempDir::new()?;
    let first = an_archive_ask(root.path());
    let opened = cmd_collect::open_provider(&first)?;
    let answer = cmd_collect::run(&first, &opened, &partial_archive_day()?, &Fixed)?;
    assert_eq!(answer.exit, Exit::PartialCollect, "{:#}", answer.envelope);
    let second = Ask {
        schedule: Some("second".to_owned()),
        ..an_archive_ask(root.path())
    };
    let opened = cmd_collect::open_provider(&second)?;
    let good = Payloads::a_good_day()?;
    let answer = cmd_collect::run(&second, &opened, &good, &Fixed)?;
    assert_eq!(answer.exit, Exit::Ok, "{:#}", answer.envelope);
    assert_eq!(
        (asked(&good), index_held(root.path())?),
        (
            vec![INDEX_URL.to_owned(), MICROSOFT_URL.to_owned()],
            vec![None, None]
        )
    );
    Ok(())
}

/// §8 row R16: a live re-run fetches BOTH documents run 1 committed; each index row holds 0.
/// Mutation 2026-11-13, the `provenance != VendorArchive` guard in `follow_ups_to_issue` dropped:
/// red (run 2 asks for the index alone).
#[test]
fn a_live_rerun_still_polls_every_matched_row_even_when_the_story_is_covered() -> TestResult {
    let root = TempDir::new()?;
    let mut first = ask(root.path(), A_LISTED_SESSION);
    first.schedule = Some("first".to_owned());
    let opened = cmd_collect::open_provider(&first)?;
    cmd_collect::run(&first, &opened, &Payloads::a_good_day()?, &Fixed)?;
    let mut second = ask(root.path(), A_LISTED_SESSION);
    second.schedule = Some("second".to_owned());
    let opened = cmd_collect::open_provider(&second)?;
    let good = Payloads::a_good_day()?;
    cmd_collect::run(&second, &opened, &good, &Fixed)?;
    assert_eq!(
        (asked(&good), index_held(root.path())?),
        (
            vec![
                INDEX_URL.to_owned(),
                APPLE_URL.to_owned(),
                MICROSOFT_URL.to_owned()
            ],
            vec![Some(0), Some(0)]
        )
    );
    Ok(())
}

/// §8 row R20's reader half. After a backfill leaves one accession uncovered the ledger reports
/// the gap per (source, target window) and `--check` raises `coverage_incomplete`; once a re-run
/// closes the gap the finding is gone. Mutations 2026-11-13: dropping the `coverage_incomplete`
/// push in `cmd_ledger::read` turns the finding assertions red; summing `matched` instead of
/// taking its MAX in `coverage_up_to` turns the `(2, 2)` assertion red, because the two runs'
/// index attempts would add to a planned of 4; making `record_planned_coverage` a no-op turns the
/// `find` red, because no coverage row exists at all.
#[test]
fn a_vendor_archive_day_with_an_uncovered_accession_reports_coverage_incomplete() -> TestResult {
    let root = TempDir::new()?;
    let first = an_archive_ask(root.path());
    let opened = cmd_collect::open_provider(&first)?;
    cmd_collect::run(&first, &opened, &partial_archive_day()?, &Fixed)?;
    let report = cmd_ledger::read(root.path(), &ledger_ask()?)?;
    let gap = report
        .coverage
        .iter()
        .find(|row| row.source == SOURCE && row.target_window == A_LISTED_SESSION)
        .ok_or("no coverage row for the window")?;
    assert_eq!((gap.planned, gap.covered), (2, 1));
    assert!(
        report
            .check
            .findings
            .iter()
            .any(|finding| finding.code == "coverage_incomplete"),
        "{:?}",
        report.check.findings
    );
    let second = Ask {
        schedule: Some("second".to_owned()),
        ..an_archive_ask(root.path())
    };
    let opened = cmd_collect::open_provider(&second)?;
    cmd_collect::run(&second, &opened, &Payloads::a_good_day()?, &Fixed)?;
    let report = cmd_ledger::read(root.path(), &ledger_ask()?)?;
    let closed = report
        .coverage
        .iter()
        .find(|row| row.target_window == A_LISTED_SESSION)
        .ok_or("no coverage row for the window")?;
    assert_eq!((closed.planned, closed.covered), (2, 2));
    assert!(
        !report
            .check
            .findings
            .iter()
            .any(|finding| finding.code == "coverage_incomplete"),
        "{:?}",
        report.check.findings
    );
    Ok(())
}

/// §8 row R20's scope: coverage is the vendor_archive arm's alone. A live_ingest window with an
/// uncovered follow-up records no `matched` and raises no `coverage_incomplete`, because a live
/// re-run re-polls every row (R16) and a live shortfall is "not yet", not an unconverged backfill.
/// Mutation 2026-11-13: recording `matched` for a live index attempt (dropping the `provenance ==
/// VendorArchive` guard in the driver) turns this red — the live window then reports a gap.
#[test]
fn a_live_window_with_an_uncovered_follow_up_reports_no_coverage_gap() -> TestResult {
    let root = TempDir::new()?;
    let mut live = ask(root.path(), A_LISTED_SESSION);
    live.schedule = Some("first".to_owned());
    let opened = cmd_collect::open_provider(&live)?;
    cmd_collect::run(&live, &opened, &partial_archive_day()?, &Fixed)?;
    let report = cmd_ledger::read(root.path(), &ledger_ask()?)?;
    assert!(
        !report
            .coverage
            .iter()
            .any(|row| row.target_window == A_LISTED_SESSION),
        "a live window must record no coverage row: {:?}",
        report.coverage
    );
    assert!(
        !report
            .check
            .findings
            .iter()
            .any(|finding| finding.code == "coverage_incomplete"),
        "{:?}",
        report.check.findings
    );
    Ok(())
}

/// The outcomes of every attempt row the run made against one url, in attempt order. A retry shows
/// as a second entry, so a transient-then-success document reads `["failed", "ok"]`.
fn outcomes_for(answer: &cmd_collect::Answer, url: &str) -> Vec<String> {
    answer.envelope["attempts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|attempt| attempt["url"].as_str() == Some(url))
        .filter_map(|attempt| attempt["outcome"].as_str().map(str::to_owned))
        .collect()
}

/// §8 row R21. A document whose fetch fails transiently (an HTTP 503) is re-queued and re-attempted
/// within the SAME run, so the item it names is committed and the day is complete rather than left
/// to the next run. Microsoft answers 503 once and then its real filing; the run ends with one
/// failed attempt row and one ok attempt row for that url, both filings committed, and exit 0.
/// Mutation 2026-11-13: setting `MAX_TRANSIENT_RETRIES` to 0, or dropping the re-queue in
/// `collect`'s queue loop, turns this red — Microsoft is asked once, stays failed, one filing
/// commits and the run exits `partial_collect`.
#[test]
fn a_transient_fetch_failure_is_retried_and_commits_the_item_within_one_run() -> TestResult {
    let root = TempDir::new()?;
    let flaky = Flaky::of(&[
        (INDEX_URL, &[(OK, "form.20261110.idx")]),
        (APPLE_URL, &[(OK, "filing_8k_apple.txt")]),
        (
            MICROSOFT_URL,
            &[
                (SERVICE_UNAVAILABLE, "error_403_traffic.html"),
                (OK, "filing_8k_microsoft_no_acceptance.txt"),
            ],
        ),
    ])?;
    let answer = collected(root.path(), A_LISTED_SESSION, &flaky)?;
    assert_eq!(
        (
            answer.exit,
            answer.envelope["items_committed"].as_u64(),
            flaky
                .asked
                .borrow()
                .iter()
                .filter(|url| *url == MICROSOFT_URL)
                .count(),
        ),
        (Exit::Ok, Some(u64::from(IN_UNIVERSE_FILINGS)), 2),
        "{:#}",
        answer.envelope
    );
    assert_eq!(
        outcomes_for(&answer, MICROSOFT_URL),
        vec!["failed".to_owned(), "ok".to_owned()],
        "{:#}",
        answer.envelope
    );
    // per_source.failed counts the failed attempt ROW even though the retry succeeded and the run
    // exits Ok: per_source summarises attempts, the exit is final disposition, and the two diverge
    // once a transient is retried (§8 row R21). This pins that divergence.
    assert_eq!(
        (
            answer.exit,
            answer.envelope["per_source"][0]["failed"].as_u64()
        ),
        (Exit::Ok, Some(1)),
        "{:#}",
        answer.envelope
    );
    Ok(())
}

/// §8 row R21. A PERMANENT failure (a 4xx carrying the generic access-denied body) is attempted
/// exactly once and never re-queued: retrying a permanent refusal wastes calls and reads as abuse.
/// Mutation 2026-11-13: making the retry test in `collect` fire for a permanent failure (re-queuing
/// whenever the outcome is `failed`, dropping the `!permanent` guard) turns this red — Microsoft is
/// then asked `MAX_TRANSIENT_RETRIES + 1` times.
#[test]
fn a_permanent_fetch_failure_is_attempted_once_and_never_retried() -> TestResult {
    let root = TempDir::new()?;
    let refused = Payloads::of(&[
        (INDEX_URL, OK, "form.20261110.idx"),
        (APPLE_URL, OK, "filing_8k_apple.txt"),
        (MICROSOFT_URL, FORBIDDEN, "error_403_access_denied.xml"),
    ])?;
    let answer = collected(root.path(), A_LISTED_SESSION, &refused)?;
    assert_eq!(
        (
            answer.exit,
            answer.envelope["items_committed"].as_u64(),
            asked(&refused)
                .iter()
                .filter(|url| *url == MICROSOFT_URL)
                .count(),
        ),
        (Exit::PartialCollect, Some(1), 1),
        "{:#}",
        answer.envelope
    );
    assert_eq!(
        outcomes_for(&answer, MICROSOFT_URL),
        vec!["failed".to_owned()]
    );
    Ok(())
}

/// §8 row R21. The retry is BOUNDED: a request that fails transiently on every attempt produces
/// exactly `MAX_TRANSIENT_RETRIES + 1` attempt rows and the run terminates, rather than looping
/// forever. Microsoft answers 503 on every call; it is asked three times (one initial attempt plus
/// two re-queues) and no more, Apple still commits, and the residual gap leaves the run
/// `partial_collect`. Mutation 2026-11-13: widening the bound from `retries < MAX_TRANSIENT_RETRIES`
/// to `retries <= MAX_TRANSIENT_RETRIES` turns this red (four asks, not three); dropping the bound
/// entirely makes `collect` non-terminating against this fixture, which the test surfaces as a hang.
#[test]
fn a_request_that_fails_transiently_every_time_is_bounded_to_n_plus_one_attempts() -> TestResult {
    let root = TempDir::new()?;
    let always = Payloads::of(&[
        (INDEX_URL, OK, "form.20261110.idx"),
        (APPLE_URL, OK, "filing_8k_apple.txt"),
        (MICROSOFT_URL, SERVICE_UNAVAILABLE, "error_403_traffic.html"),
    ])?;
    let answer = collected(root.path(), A_LISTED_SESSION, &always)?;
    let microsoft_attempts = asked(&always)
        .iter()
        .filter(|url| *url == MICROSOFT_URL)
        .count();
    assert_eq!(
        (answer.exit, answer.envelope["items_committed"].as_u64()),
        (Exit::PartialCollect, Some(1)),
        "{:#}",
        answer.envelope
    );
    assert_eq!(
        microsoft_attempts,
        usize::try_from(news_provider::MAX_TRANSIENT_RETRIES + 1).unwrap_or(usize::MAX),
    );
    assert_eq!(
        outcomes_for(&answer, MICROSOFT_URL),
        vec!["failed".to_owned(); microsoft_attempts]
    );
    Ok(())
}

const OPEN_WEB: &str = "open_web_wires";
/// Inside Saturday 2026-11-14, the day the GNW probes ran (architecture.md section 11.6).
const SATURDAY_AT: &str = "2026-11-14T19:30:00Z";
const SATURDAY: &str = "2026-11-14";
/// Thirty minutes before UTC midnight: already 2026-11-15 in any zone east of UTC+00:30.
const LATE_SATURDAY_AT: &str = "2026-11-14T23:30:00Z";
/// UTC+14, the zone furthest ahead of UTC, so its local date is the next day for most of UTC's.
const A_ZONE_AHEAD_OF_UTC: &str = "Pacific/Kiritimati";
const GNW_INDEX: &str = "https://rss.globenewswire.com/rssfeed/";
/// `synthetic_index_one_release.xml`'s one `www.` link, rewritten to `rss.` as the rules say.
const GNW_RELEASE: &str =
    "https://rss.globenewswire.com/news-release/2026/11/14/9999999/0/en/synthetic-release.html";
/// The committed hosts file admits GNW and PR Newswire, so a plan holds two index requests.
const ADMITTED_HOSTS: u64 = 2;
/// `open_web_hosts()` admits GNW alone.
const GNW_ONLY_ADMITTED_HOSTS: u64 = 1;
/// Before `SATURDAY_AT`: when the seeded robots fetches were ledgered.
const ROBOTS_FETCHED_AT: &str = "2026-11-14T18:00:00Z";

/// §8 row R4(b): the committed GNW row's robots fetch, ledgered so an open-web collect may run.
fn seed_robots(store: &Path) -> TestResult {
    seed::refresh(
        store,
        &open_web_hosts(),
        ROBOTS_FETCHED_AT,
        (OK, &seed::gnw_robots()?),
    )?;
    Ok(())
}

fn open_web_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../providers/open-web-feed")
}

/// The GNW row alone: a byte copy of the committed hosts file before PRN was admitted.
fn open_web_hosts() -> PathBuf {
    open_web_dir().join("tests/fixtures/hosts/gnw_admitted.json")
}

/// The production rules file, as `rules()` is edgar's.
fn open_web_rules() -> PathBuf {
    open_web_dir().join("rules/open-web-feed.rules.json")
}

fn gnw(name: &str) -> PathBuf {
    open_web_dir().join("tests/fixtures/gnw").join(name)
}

fn instant(text: &str) -> DateTime<Utc> {
    text.parse().unwrap_or(DateTime::UNIX_EPOCH)
}

fn open_web_ask(store: &Path, at: &str) -> Ask {
    Ask {
        sources: vec![OPEN_WEB.to_owned()],
        window: cmd_collect::TODAY.to_owned(),
        schedule: Some(SCHEDULE.to_owned()),
        expected_runs: EXPECTED_RUNS,
        store: store.to_path_buf(),
        calendar: None,
        listing: None,
        roster: None,
        hosts: Some(open_web_hosts()),
        rules: open_web_rules(),
        lag_policy: None,
        contact: CONTACT.to_owned(),
        at: instant(at),
        dry_run: false,
    }
}

/// A clock standing at one named instant.
struct At(DateTime<Utc>);

impl Clock for At {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

fn gnw_payloads(answers: &[(&str, u16, &str)]) -> Result<Payloads, Box<dyn Error>> {
    let mut held = BTreeMap::new();
    for (url, status, name) in answers {
        held.insert((*url).to_owned(), (*status, std::fs::read(gnw(name))?));
    }
    Ok(Payloads {
        answers: held,
        asked: RefCell::new(Vec::new()),
    })
}

fn open_web_args(store: &Path, extra: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = [
        "collect",
        "--source",
        OPEN_WEB,
        "--window",
        cmd_collect::TODAY,
        "--store",
        &path(store),
        "--hosts",
        &path(&open_web_hosts()),
        "--rules",
        &path(&open_web_rules()),
        "--at",
        SATURDAY_AT,
        "--format",
        "json",
        "--dry-run",
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect();
    args.extend(extra.iter().map(|arg| (*arg).to_owned()));
    args
}

fn refused_flag(refused: Result<cmd_collect::Opened, cmd_collect::CollectRunError>) -> String {
    match refused {
        Err(cmd_collect::CollectRunError::FlagRefused { flag, .. }) => format!("refused {flag}"),
        Err(cmd_collect::CollectRunError::FlagRequired { flag, .. }) => format!("required {flag}"),
        Err(other) => format!("another refusal: {other}"),
        Ok(opened) => format!("opened {opened:?}"),
    }
}

/// Section 11.2: the open-web run takes neither a calendar nor a listing, and no file of edgar's;
/// a flag it does not read is refused, never ignored. Mutation 2026-11-15: dropping the cadence
/// check in `open_provider` turns this red.
#[test]
fn an_open_web_run_refuses_a_calendar_or_listing_flag() -> TestResult {
    let root = TempDir::new()?;
    let given = open_web_ask(root.path(), SATURDAY_AT);
    let mut said = Vec::new();
    for flag in ["calendar", "listing", "roster", "lag-policy"] {
        let mut ask = given.clone();
        match flag {
            "calendar" => ask.calendar = Some(fixture("calendar.json")),
            "listing" => ask.listing = Some(fixture("quarter_index.json")),
            "roster" => ask.roster = Some(fixture("roster.json")),
            _ => ask.lag_policy = Some(lag_policy()),
        }
        said.push(refused_flag(cmd_collect::open_provider(&ask)));
    }
    assert_eq!(
        said,
        vec![
            "refused calendar",
            "refused listing",
            "refused roster",
            "refused lag-policy"
        ]
    );
    let calendar = path(&fixture("calendar.json"));
    let args = open_web_args(root.path(), &["--calendar", &calendar]);
    let answered = binary(&args.iter().map(String::as_str).collect::<Vec<&str>>())?;
    assert_eq!((answered.code, answered.stdout.len()), (EXIT_USAGE, 0));
    assert!(tree(root.path()).is_empty());
    Ok(())
}

/// The other side of the table: edgar needs its calendar, and takes no hosts file. Refused before
/// the store is touched. Mutation 2026-11-15: dropping the cadence check in `open_provider`
/// turns this red (the binary writes a run envelope, then `collect` refuses).
#[test]
fn an_edgar_run_without_a_calendar_is_a_usage_error() -> TestResult {
    let root = TempDir::new()?;
    let without = Ask {
        calendar: None,
        ..ask(root.path(), A_LISTED_SESSION)
    };
    let with_hosts = Ask {
        hosts: Some(open_web_hosts()),
        ..ask(root.path(), A_LISTED_SESSION)
    };
    assert_eq!(
        (
            refused_flag(cmd_collect::open_provider(&without)),
            refused_flag(cmd_collect::open_provider(&with_hosts)),
            refused_flag(cmd_collect::open_provider(&Ask {
                hosts: None,
                ..open_web_ask(root.path(), SATURDAY_AT)
            })),
        ),
        (
            "required calendar".to_owned(),
            "refused hosts".to_owned(),
            "required hosts".to_owned()
        )
    );
    let args: Vec<String> = collect_args(root.path(), A_LISTED_SESSION, &[])
        .into_iter()
        .collect();
    let at = args
        .iter()
        .position(|arg| arg == "--calendar")
        .ok_or("the edgar arguments name a calendar")?;
    let without: Vec<&str> = args
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != at && *index != at + 1)
        .map(|(_, arg)| arg.as_str())
        .collect();
    let answered = binary(&without)?;
    assert_eq!((answered.code, answered.stdout.len()), (EXIT_USAGE, 0));
    assert!(
        tree(root.path()).is_empty(),
        "{:?}",
        tree(root.path()).keys()
    );
    Ok(())
}

/// `--window today` is the UTC date of `--at`, never the machine's local date: run under a zone
/// where 23:30Z is already the next day. Mutation 2026-11-15: resolving from `chrono::Local`
/// turns this red (the plan is refused as not the run's day).
#[test]
fn window_today_resolves_to_the_utc_date_of_at() -> TestResult {
    let root = TempDir::new()?;
    seed_robots(root.path())?;
    let mut args = open_web_args(root.path(), &[]);
    let at = args
        .iter()
        .position(|arg| arg == SATURDAY_AT)
        .ok_or("the arguments name --at")?;
    LATE_SATURDAY_AT.clone_into(&mut args[at]);
    let answered = binary_env(
        &args.iter().map(String::as_str).collect::<Vec<&str>>(),
        &[
            ("BELLWETHER_FEED_CONTACT", CONTACT),
            ("TZ", A_ZONE_AHEAD_OF_UTC),
        ],
    )?;
    assert_eq!(answered.code, EXIT_OK);
    let envelope: Value = serde_json::from_slice(&answered.stdout)?;
    assert_eq!(
        (
            envelope["window"].as_str(),
            envelope["plan"]["requests"].as_u64()
        ),
        (Some(SATURDAY), Some(GNW_ONLY_ADMITTED_HOSTS))
    );
    Ok(())
}

/// Section 11.1: the hosts file's digest is in the run envelope and in every attempt row; edgar's
/// envelope names none and a calendar. Mutation 2026-11-15: `"hosts_version": null` in
/// `envelope_of` turns this red.
#[test]
fn the_run_envelope_names_the_hosts_file_it_was_allowed_to_use() -> TestResult {
    let root = TempDir::new()?;
    seed_robots(root.path())?;
    let ask = open_web_ask(root.path(), SATURDAY_AT);
    let opened = cmd_collect::open_provider(&ask)?;
    let exchange = gnw_payloads(&[
        (GNW_INDEX, OK, "synthetic_index_one_release.xml"),
        (GNW_RELEASE, OK, "synthetic_canonical_differs.html"),
    ])?;
    let answer = cmd_collect::run(&ask, &opened, &exchange, &At(instant(SATURDAY_AT)))?;
    let digest = canon::FileDigest::of(&std::fs::read(open_web_hosts())?);
    assert_eq!(
        (
            answer.exit,
            answer.envelope["hosts_version"].as_str(),
            answer.envelope["calendar_version"].is_null(),
            answer.envelope["source"].as_str(),
            answer.envelope["items_committed"].as_u64(),
        ),
        (Exit::Ok, Some(digest.hex()), true, Some(OPEN_WEB), Some(1)),
        "{:#}",
        answer.envelope
    );
    let db = Connection::open_with_flags(
        root.path().join(COMMIT_LOG),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut rows = db.prepare("SELECT hosts_version FROM attempts WHERE source = ?1")?;
    let stored: Vec<Option<String>> = rows
        .query_map([OPEN_WEB], |row| row.get(0))?
        .collect::<Result<Vec<Option<String>>, _>>()?;
    assert_eq!(stored, vec![Some(digest.hex().to_owned()); 2]);
    let edgar = collected(
        &root.path().join("edgar"),
        A_LISTED_SESSION,
        &Payloads::a_good_day()?,
    )?;
    assert_eq!(
        (
            edgar.envelope["hosts_version"].is_null(),
            edgar.envelope["calendar_version"].is_string()
        ),
        (true, true)
    );
    Ok(())
}

/// Section 11.2: a second SourceId in one invocation needs a run envelope listing one
/// `hosts_version` per source, which is not built. Mutation 2026-11-15: `chosen` returning the
/// first source instead of refusing turns this red.
#[test]
fn two_sources_in_one_invocation_are_refused() -> TestResult {
    let both = [SOURCE.to_owned(), OPEN_WEB.to_owned()];
    let twice = [SOURCE.to_owned(), SOURCE.to_owned()];
    assert!(matches!(
        cmd_collect::Registered::chosen(&both),
        Err(cmd_collect::CollectRunError::MoreThanOneSource(_))
    ));
    assert!(matches!(
        cmd_collect::Registered::chosen(&twice),
        Ok(cmd_collect::Registered::EdgarPress)
    ));
    let root = TempDir::new()?;
    let answered = run_collect(root.path(), A_LISTED_SESSION, &["--source", OPEN_WEB])?;
    assert_eq!((answered.code, answered.stdout.len()), (EXIT_USAGE, 0));
    assert!(tree(root.path()).is_empty());
    Ok(())
}

/// The committed hosts file, both robots rows seeded. Mutation 2026-11-16, its PRN row set to
/// `candidate`: red (one request).
#[test]
fn an_open_web_dry_run_plans_one_index_request_per_admitted_host_and_writes_nothing() -> TestResult
{
    let root = TempDir::new()?;
    seed::refresh_committed(root.path(), ROBOTS_FETCHED_AT)?;
    let before = tree_digests(root.path());
    let dry = Ask {
        dry_run: true,
        hosts: Some(seed::committed_hosts()),
        ..open_web_ask(root.path(), SATURDAY_AT)
    };
    let opened = cmd_collect::open_provider(&dry)?;
    let exchange = gnw_payloads(&[])?;
    let answer = cmd_collect::run(&dry, &opened, &exchange, &At(instant(SATURDAY_AT)))?;
    assert_eq!(
        (
            exchange.sends(),
            answer.exit,
            answer.envelope["plan"]["requests"].as_u64(),
            answer.envelope["changed"].as_bool(),
        ),
        (0, Exit::Ok, Some(ADMITTED_HOSTS), Some(false)),
        "{:#}",
        answer.envelope
    );
    assert_eq!(tree_digests(root.path()), before);
    Ok(())
}

/// An every-day provider collects the clock's own UTC day; an `--at` on another day would label
/// today's fetch with that day. Mutation 2026-11-15: removing the check in `cmd_collect::run`
/// turns this red (the run writes its envelope and asks the index).
#[test]
fn an_open_web_run_whose_at_is_not_the_clocks_day_is_refused_before_anything_is_written(
) -> TestResult {
    let root = TempDir::new()?;
    let ask = open_web_ask(root.path(), SATURDAY_AT);
    let opened = cmd_collect::open_provider(&ask)?;
    let exchange = gnw_payloads(&[(GNW_INDEX, OK, "synthetic_index_one_release.xml")])?;
    let refused = cmd_collect::run(&ask, &opened, &exchange, &Fixed);
    assert!(
        matches!(
            refused,
            Err(cmd_collect::CollectRunError::AtIsNotTheClockDay { .. })
        ),
        "{refused:?}"
    );
    assert_eq!(exchange.sends(), 0);
    assert!(tree(root.path()).is_empty());
    Ok(())
}

/// Section 5.1: an absent hosts file, one admitting no host and an absent rules file are
/// `input_absent`, exit 4, as an absent or empty roster is. Mutations 2026-11-15: removing the
/// `OpenWebRefused` arm in `exit_code` turns this red (all exit 2); removing only its
/// `RulesAbsent` arm was green before the rules case was added.
#[test]
fn an_open_web_run_with_an_absent_or_empty_hosts_file_or_an_absent_rules_file_exits_four(
) -> TestResult {
    let root = TempDir::new()?;
    let absent = path(&root.path().join("no-such-hosts.json"));
    let none_admitted = path(&open_web_dir().join("tests/fixtures/hosts/none_admitted.json"));
    let no_rules = path(&root.path().join("no-such-rules.json"));
    let mut exits = Vec::new();
    for (flag, given) in [
        ("--hosts", absent),
        ("--hosts", none_admitted),
        ("--rules", no_rules),
    ] {
        let mut args = open_web_args(root.path(), &[]);
        let at = args
            .iter()
            .position(|arg| arg == flag)
            .ok_or("the arguments name the flag")?;
        args[at + 1] = given;
        let answered = binary(&args.iter().map(String::as_str).collect::<Vec<&str>>())?;
        exits.push((answered.code, answered.stdout.len()));
    }
    assert_eq!(exits, vec![(EXIT_NOT_FOUND, 0); 3]);
    Ok(())
}

/// A store at a version above this build's is refused before the run writes its envelope or
/// anything else into it. Review 2026-11-15 measured a `runs/` file left behind by the refusal.
#[test]
fn a_run_against_a_store_at_a_newer_user_version_leaves_the_store_unchanged() -> TestResult {
    let root = TempDir::new()?;
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    db.pragma_update(None, "user_version", A_NEWER_USER_VERSION)?;
    drop(db);
    let before = tree_digests(root.path());
    let refused = collected(root.path(), A_LISTED_SESSION, &Payloads::a_good_day()?);
    let said = refused
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    assert!(
        said.contains(&format!("user_version {A_NEWER_USER_VERSION}")),
        "{said}"
    );
    assert_eq!(tree_digests(root.path()), before);
    Ok(())
}

/// The v0 sibling of the test above: a v0 `attempts` that `SCHEMA_V0` does not match. Review
/// 2026-11-15 measured `LOCK` and a `runs/` file left behind by `Committer::open`'s refusal.
#[test]
fn a_run_against_a_v0_store_of_another_shape_leaves_the_store_unchanged() -> TestResult {
    let root = TempDir::new()?;
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    db.execute_batch("CREATE TABLE attempts (attempt_id TEXT PRIMARY KEY, run_id TEXT NOT NULL);")?;
    drop(db);
    let before = tree_digests(root.path());
    let refused = collected(root.path(), A_LISTED_SESSION, &Payloads::a_good_day()?);
    let said = refused
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    assert!(said.contains("differs from SCHEMA_V0"), "{said}");
    assert_eq!(tree_digests(root.path()), before);
    Ok(())
}

fn tree_digests(root: &Path) -> Vec<(String, String)> {
    tree(root)
        .into_iter()
        .map(|(name, bytes)| (name, canon::FileDigest::of(&bytes).hex().to_owned()))
        .collect()
}

/// A usage error is reported before any input file is read, so an absent file beside it cannot
/// turn exit 2 into exit 4. Mutation 2026-11-15: the calendar and listing checks moved back after
/// the provider's `open` turns this red (both report the absent file).
#[test]
fn a_flag_error_beside_an_absent_input_file_is_still_a_usage_error() -> TestResult {
    let root = TempDir::new()?;
    let absent = root.path().join("absent.json");
    let edgar = Ask {
        calendar: None,
        roster: Some(absent.clone()),
        ..ask(root.path(), A_LISTED_SESSION)
    };
    let open_web = Ask {
        calendar: Some(fixture("calendar.json")),
        hosts: Some(absent.clone()),
        ..open_web_ask(root.path(), SATURDAY_AT)
    };
    assert_eq!(
        [edgar, open_web].map(|ask| refused_flag(cmd_collect::open_provider(&ask))),
        ["required calendar", "refused calendar"]
    );
    let calendar = path(&fixture("calendar.json"));
    let mut args = open_web_args(root.path(), &["--calendar", &calendar]);
    let at = args
        .iter()
        .position(|arg| arg == "--hosts")
        .ok_or("the arguments name --hosts")?;
    args[at + 1] = path(&absent);
    let answered = binary(&args.iter().map(String::as_str).collect::<Vec<&str>>())?;
    assert_eq!((answered.code, answered.stdout.len()), (EXIT_USAGE, 0));
    Ok(())
}

/// `today` is a UTC date and edgar plans New York session days: before this refusal a run at
/// 2026-11-11T02:00Z planned 2026-11-11 (review 2026-11-15).
#[test]
fn an_edgar_run_refuses_window_today() -> TestResult {
    let root = TempDir::new()?;
    let today = Ask {
        dry_run: true,
        ..ask(root.path(), cmd_collect::TODAY)
    };
    let refused = cmd_collect::open_provider(&today).err();
    assert!(
        matches!(
            refused,
            Some(cmd_collect::CollectRunError::TodayRefused { source_id: SOURCE })
        ),
        "{refused:?}"
    );
    Ok(())
}

/// Section 5.1's `invalid_argument` row: every `collect` usage check writes the prefix
/// `usage_exit` writes, one row per check.
#[test]
fn every_collect_usage_error_is_written_as_a_usage_error() -> TestResult {
    let root = TempDir::new()?;
    let without_flag = |args: Vec<String>, flag: &str| -> Vec<String> {
        let at = args
            .iter()
            .position(|arg| arg == flag)
            .unwrap_or(args.len());
        args.into_iter()
            .enumerate()
            .filter(|(index, _)| *index != at && *index != at + 1)
            .map(|(_, arg)| arg)
            .collect()
    };
    let with_source = |args: Vec<String>, source: &str| -> Vec<String> {
        let mut args = args;
        args.extend(["--source".to_owned(), source.to_owned()]);
        args
    };
    let mut unknown = open_web_args(root.path(), &[]);
    unknown[2] = "no_such_source".to_owned();
    let off_the_clock_day: Vec<String> = open_web_args(root.path(), &[])
        .into_iter()
        .filter(|arg| arg != "--dry-run")
        .collect();
    let with_window = |window: &str| -> Vec<String> {
        let mut args = open_web_args(root.path(), &[]);
        args[4] = window.to_owned();
        args
    };
    let calendar = path(&fixture("calendar.json"));
    let rows = [
        ("window unreadable", with_window("garbage")),
        ("window not the run day", with_window("2026-11-13")),
        (
            "flag required",
            without_flag(
                collect_args(root.path(), A_LISTED_SESSION, &[]),
                "--calendar",
            ),
        ),
        (
            "flag refused",
            open_web_args(root.path(), &["--calendar", &calendar]),
        ),
        (
            "second source",
            with_source(open_web_args(root.path(), &[]), SOURCE),
        ),
        ("unknown source", unknown),
        ("at off the clock day", off_the_clock_day),
        (
            "today for edgar",
            collect_args(root.path(), cmd_collect::TODAY, &["--dry-run"]),
        ),
    ];
    for (row, args) in rows {
        let answered = binary(&args.iter().map(String::as_str).collect::<Vec<&str>>())?;
        let said = String::from_utf8_lossy(&answered.stderr).into_owned();
        assert_eq!(
            (answered.code, answered.stdout.len()),
            (EXIT_USAGE, 0),
            "{row}: {said}"
        );
        assert!(said.starts_with("usage error: "), "{row}: {said}");
    }
    Ok(())
}

/// `Registered::cadence` decides the calendar and listing flags before the provider is opened, so
/// it must be what each provider declares once open.
#[test]
fn every_registered_cadence_is_the_one_its_provider_declares() -> TestResult {
    let root = TempDir::new()?;
    let mut declared = Vec::new();
    for ask in [
        ask(root.path(), A_LISTED_SESSION),
        open_web_ask(root.path(), SATURDAY_AT),
    ] {
        let opened = cmd_collect::open_provider(&ask)?;
        declared.push((opened.provider.cadence(), opened.registered.cadence()));
    }
    assert_eq!(declared.len(), cmd_collect::Registered::ALL.len());
    for (opened, registered) in declared {
        assert_eq!(opened, registered);
    }
    Ok(())
}

/// The table's credential name is the one the provider's own User-Agent asks `Wire` for; a
/// mismatch stores the contact where `Wire::send` never reads it. Mutation 2026-11-15: the
/// open-web entry naming edgar's credential was green before this.
#[test]
fn every_registered_credential_is_the_one_its_providers_user_agent_names() -> TestResult {
    let root = TempDir::new()?;
    let mut named = Vec::new();
    for ask in [
        ask(root.path(), A_LISTED_SESSION),
        open_web_ask(root.path(), SATURDAY_AT),
    ] {
        let opened = cmd_collect::open_provider(&ask)?;
        named.push((
            opened.provider.transport().user_agent,
            HeaderValue::FromCredential(CredentialRef::new(opened.registered.credential())),
        ));
    }
    assert_eq!(named.len(), cmd_collect::Registered::ALL.len());
    for (asked_for, held_under) in named {
        assert_eq!(asked_for, held_under);
    }
    Ok(())
}

/// Each provider's contact comes from its own variable: SEC's address set alone leaves the
/// open-web User-Agent with none, refused at startup. Mutation 2026-11-15: `contact_variable`
/// naming `BELLWETHER_SEC_CONTACT` for both turns this red (exit 0).
#[test]
fn an_open_web_run_reads_its_contact_from_its_own_variable() -> TestResult {
    let root = TempDir::new()?;
    seed_robots(root.path())?;
    let args = open_web_args(root.path(), &[]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let sec_only = binary_env(&args, &[("BELLWETHER_SEC_CONTACT", CONTACT)])?;
    let own = binary_env(&args, &[("BELLWETHER_FEED_CONTACT", CONTACT)])?;
    assert_eq!(
        (sec_only.code, sec_only.stdout.len(), own.code),
        (EXIT_USAGE, 0, EXIT_OK)
    );
    Ok(())
}
