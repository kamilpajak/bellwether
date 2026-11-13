//! `newsvault collect` and `newsvault items`, driven over the committed EDGAR fixtures and the
//! real provider. No test here opens a socket: `Exchange` is the seam and this file's
//! implementation of it answers out of `providers/edgar-press/tests/fixtures/`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{DateTime, Utc};
use news_provider::provider::Provider;
use news_provider::{
    Archive, ArchiveError, AttemptEvent, AttemptReceipt, AttemptSlot, Clock, CollectedItem,
    Committed, Exchange, Exit, HeaderValue, LedgerError, Provenance, RawResponse, Request,
    RequestPhase, RunId, SendFailure, SendPermit, Sink, SourceId, TargetWindow,
};
use newsvault::cmd_collect::{self, Ask};
use newsvault::cmd_items::{self, PerStory};
use newsvault::store::{Committer, COMMIT_LOG};
use pit::ObservationTime;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tempfile::TempDir;

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

const EXIT_OK: i32 = 0;
const EXIT_USAGE: i32 = 2;
const EXIT_LEASE_HELD: i32 = 7;
const EXIT_INCOMPLETE: i32 = 8;

/// The index names four filings, two of them in universe under an in-list form type.
const VENDOR_ROWS_IN_THE_INDEX: u32 = 4;
const IN_UNIVERSE_FILINGS: u32 = 2;
/// One index attempt plus one attempt per follow-up.
const ATTEMPTS_PER_RUN: usize = 3;
/// The two filings' raw payloads and the two bodies extracted out of them. The index payload is
/// stored and NOT indexed: no item row references it.
const BLOBS_INDEXED_PER_GOOD_DAY: u64 = 4;

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
        calendar: fixture("calendar.json"),
        listing: fixture("quarter_index.json"),
        roster: fixture("roster.json"),
        rules: rules(),
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
        raw_blob: Option<&str>,
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
        Archive::commit(&mut self.store, closed, items, raw_blob, observed)
    }

    fn ok_vendor_rows(&self, source: &SourceId) -> Result<Vec<u32>, ArchiveError> {
        Archive::ok_vendor_rows(&self.store, source)
    }

    fn attempts_in_flight(&self) -> Result<Vec<String>, ArchiveError> {
        Archive::attempts_in_flight(&self.store)
    }
}

/// One run through the library, with the exchange the caller chose.
fn collected(
    store: &Path,
    window: &str,
    exchange: &Payloads,
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
}

/// Runs the binary with stdin closed, which is the shape a scheduler gives it.
fn binary(args: &[&str]) -> Result<Answered, Box<dyn Error>> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .args(args)
        .env("BELLWETHER_SEC_CONTACT", CONTACT)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    drop(child.stdin.take());
    let output = child.wait_with_output()?;
    Ok(Answered {
        code: output.status.code().unwrap_or(-1),
        stdout: output.stdout,
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
        &provider,
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
        universe_version: provider.universe_version(),
    };
    let orphan = AttemptSlot::open(&mut store, &abandoned, &dead, 0, ObservationTime::new(at()))?;
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
