//! `newsvault roster fetch` and `roster show` over a stub `Exchange` and a set clock, never the
//! network (architecture.md section 10, "The roster"), and what the ledger reads of their runs.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;
use std::process::Command;

use canon::FileDigest;
use chrono::{DateTime, Utc};
use news_provider::{
    Clock, Exchange, Exit, Headers, RawResponse, Request, SendFailure, SendPermit, TransportError,
};
use newsvault::blobs::BlobStore;
use newsvault::cmd_collect::Registered;
use newsvault::cmd_ledger;
use newsvault::cmd_robots::ROBOTS_SOURCE;
use newsvault::cmd_roster::{self, Ask, RosterError, FILES, ROSTER_SOURCE};
use newsvault::store::{COMMIT_LOG, RUNS_DIR};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use tempfile::TempDir;

mod loopback;

type TestResult = Result<(), Box<dyn Error>>;

const CONTACT: &str = "bellwether collector contact@example.test";
const ROBOTS_URL: &str = "https://www.sec.gov/robots.txt";
const FILE_URL: &str = "https://www.sec.gov/files/company_tickers_exchange.json";
const FILE_KEY: &str = "sec_company_tickers_exchange";
const SEC_HOST: &str = "www.sec.gov";
/// SEC's robots.txt shape on 2026-11-23: `*` disallows paths, none under `/files/`.
const ROBOTS_BODY: &[u8] = b"User-agent: *\nDisallow: /cgi-bin\nDisallow: /Archives/bin\n";
/// Three rows of `company_tickers_exchange.json`'s shape, written for this test.
const ROSTER_BODY: &[u8] = br#"{"fields":["cik","name","ticker","exchange"],"data":[[1,"Alpha Corp","ALP","Nasdaq"],[2,"Beta Inc.","BET","NYSE"],[3,"Gamma Co","GAM",null]]}"#;
const EMPTY_ROSTER: &[u8] = br#"{"fields":["cik","name","ticker","exchange"],"data":[]}"#;
const CHALLENGE: &[u8] = b"<html><body>Checking your browser</body></html>";
const LAST_MODIFIED: &str = "Fri, 20 Nov 2026 21:53:43 GMT";
const DATE: &str = "Mon, 23 Nov 2026 01:20:01 GMT";
const FIRST_RUN: &str = "2026-11-23T01:20:00Z";
const SECOND_RUN: &str = "2026-11-24T01:20:00Z";
const OK: u16 = 200;
const NOT_FOUND: u16 = 404;
const FOUND: u16 = 302;
const FORBIDDEN: u16 = 403;
const UNAVAILABLE: u16 = 503;

/// A clock a test moves between runs.
struct Set(Cell<DateTime<Utc>>);

impl Set {
    fn at(text: &str) -> Result<Self, Box<dyn Error>> {
        Ok(Self(Cell::new(text.parse()?)))
    }

    fn to(&self, text: &str) -> TestResult {
        self.0.set(text.parse()?);
        Ok(())
    }
}

impl Clock for Set {
    fn now(&self) -> DateTime<Utc> {
        self.0.get()
    }
}

/// One answer per url: a status, a body and the headers a server sent.
type Answer = (u16, Vec<u8>, Vec<(String, String)>);

/// Each url answered as set; a url it does not hold fails in transport.
#[derive(Default)]
struct Stub {
    answers: BTreeMap<String, Answer>,
    asked: RefCell<Vec<String>>,
}

impl Stub {
    fn answering(mut self, url: &str, status: u16, body: &[u8], headers: &[(&str, &str)]) -> Self {
        let kept = headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        self.answers
            .insert(url.to_owned(), (status, body.to_vec(), kept));
        self
    }

    /// SEC as measured: robots.txt allows `/files/` and the file carries its `Last-Modified`.
    fn sec(robots: &[u8], file: &[u8]) -> Self {
        Self::default()
            .answering(ROBOTS_URL, OK, robots, &[])
            .answering(
                FILE_URL,
                OK,
                file,
                &[
                    ("content-type", "application/json"),
                    ("last-modified", LAST_MODIFIED),
                    ("date", DATE),
                ],
            )
    }
}

impl Exchange for Stub {
    fn send(
        &self,
        permit: SendPermit,
        request: &Request,
        clock: &dyn Clock,
    ) -> Result<RawResponse, SendFailure> {
        self.asked.borrow_mut().push(request.url.clone());
        let attempt = permit.attempt_id().clone();
        let at = pit::ObservationTime::new(clock.now());
        match self.answers.get(&request.url) {
            Some((status, body, headers)) => {
                Ok(RawResponse::new(attempt, *status, body.clone(), at, at)
                    .with_headers(Headers::new(headers.clone())))
            }
            None => Err(SendFailure::Transport(TransportError::new(
                attempt,
                at,
                at,
                format!("no stub answers {}", request.url),
            ))),
        }
    }
}

fn ask(store: &Path) -> Ask {
    Ask {
        store: store.to_path_buf(),
        contact: CONTACT.to_owned(),
        dry_run: false,
    }
}

fn read_only(store: &Path) -> Result<Connection, rusqlite::Error> {
    Connection::open_with_flags(store.join(COMMIT_LOG), OpenFlags::SQLITE_OPEN_READ_ONLY)
}

/// `(source, target_window, outcome, verdict, vendor_rows)` of every attempt, in opening order.
type AttemptRow = (String, String, String, String, Option<i64>);

fn attempts(store: &Path) -> Result<Vec<AttemptRow>, Box<dyn Error>> {
    let db = read_only(store)?;
    let mut statement = db.prepare(
        "SELECT source, target_window, outcome, verdict, vendor_rows FROM attempts
          ORDER BY opened_at, attempt_id",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn count(store: &Path, table: &str) -> Result<i64, Box<dyn Error>> {
    Ok(
        read_only(store)?.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })?,
    )
}

fn file_row(answer: &cmd_roster::Answer) -> Value {
    answer.report["files"][0].clone()
}

/// The roster file's bytes are kept exactly, under its own attempt row, with the headers P9 keeps,
/// after one robots attempt for its host; no item row. Mutations 2026-11-23, each red: the file's
/// `record` given no payload; the run envelope written as `collect`.
#[test]
fn a_roster_fetch_keeps_the_file_as_received_under_one_attempt_after_its_robots() -> TestResult {
    let root = TempDir::new()?;
    let stub = Stub::sec(ROBOTS_BODY, ROSTER_BODY);
    let answer = cmd_roster::run(&ask(root.path()), &stub, &Set::at(FIRST_RUN)?)?;
    let db = read_only(root.path())?;
    let (url, raw_blob, headers): (String, String, String) = db.query_row(
        "SELECT p.url, p.raw_blob, p.headers FROM payloads p
           JOIN attempts a ON a.attempt_id = p.attempt_id WHERE a.source = ?1",
        [ROSTER_SOURCE],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let digest = FileDigest::of(ROSTER_BODY);
    let blobs = BlobStore::new(root.path().join("blobs"));
    let kept = blobs
        .named(&raw_blob)
        .map(|named| blobs.read_verified(&named));
    let envelope: Value = serde_json::from_slice(&std::fs::read(
        root.path()
            .join(RUNS_DIR)
            .join("roster-20261123T012000Z.json"),
    )?)?;
    assert_eq!(
        (
            answer.exit,
            stub.asked.borrow().clone(),
            attempts(root.path())?,
            count(root.path(), "items")?,
        ),
        (
            Exit::Ok,
            vec![ROBOTS_URL.to_owned(), FILE_URL.to_owned()],
            vec![
                (
                    ROBOTS_SOURCE.to_owned(),
                    SEC_HOST.to_owned(),
                    "ok".to_owned(),
                    "usable".to_owned(),
                    Some(0)
                ),
                (
                    ROSTER_SOURCE.to_owned(),
                    FILE_KEY.to_owned(),
                    "ok".to_owned(),
                    "usable".to_owned(),
                    Some(3)
                ),
            ],
            0,
        )
    );
    assert_eq!(
        (
            url,
            raw_blob.clone(),
            kept.and_then(Result::ok),
            Headers::from_json(&headers)?
                .first("last-modified")
                .map(str::to_owned),
            envelope["kind"].as_str(),
            file_row(&answer)["raw_blob"].as_str(),
            file_row(&answer)["rows"].as_u64(),
        ),
        (
            FILE_URL.to_owned(),
            digest.hex().to_owned(),
            Some(ROSTER_BODY.to_vec()),
            Some(LAST_MODIFIED.to_owned()),
            Some("roster_fetch"),
            Some(digest.hex()),
            Some(3),
        )
    );
    Ok(())
}

/// robots.txt is honoured before the file: a rule over its path, a Crawl-delay slower than SEC's
/// 100 ms pace or a body that is no robots.txt asks nothing and opens no roster attempt, and exits
/// 8. Mutations 2026-11-23, each red: `allows` not consulted; the Crawl-delay check dropped; a
/// body `Robots::parse` refuses read as no rules.
#[test]
fn a_robots_txt_that_keeps_us_from_the_file_asks_nothing_and_exits_8() -> TestResult {
    let mut seen = Vec::new();
    for body in [
        b"User-agent: *\nDisallow: /files/\n".as_slice(),
        b"User-agent: *\nCrawl-delay: 1\nDisallow: /cgi-bin\n".as_slice(),
        b"<!DOCTYPE html><html><title>Page Not Available</title></html>".as_slice(),
    ] {
        let root = TempDir::new()?;
        let stub = Stub::sec(body, ROSTER_BODY);
        let answer = cmd_roster::run(&ask(root.path()), &stub, &Set::at(FIRST_RUN)?)?;
        let roster_attempts = attempts(root.path())?
            .iter()
            .filter(|row| row.0 == ROSTER_SOURCE)
            .count();
        seen.push((
            answer.exit,
            stub.asked.borrow().clone(),
            roster_attempts,
            file_row(&answer)["not_asked"].clone(),
        ));
    }
    let asked_robots = vec![ROBOTS_URL.to_owned()];
    assert_eq!(
        seen,
        vec![
            (
                Exit::PartialCollect,
                asked_robots.clone(),
                0,
                json!("robots_disallowed")
            ),
            (
                Exit::PartialCollect,
                asked_robots.clone(),
                0,
                json!("robots_refused")
            ),
            (
                Exit::PartialCollect,
                asked_robots,
                0,
                json!("robots_refused")
            ),
        ]
    );
    Ok(())
}

/// A robots.txt with no answer is never guessed at: a 503 asks no file and exits 7, a 302 (what
/// Nasdaq Trader's robots path answers, measured 2026-11-23) asks none and exits 8; a 404 is no
/// rules, so the file is asked. Mutation 2026-11-23, a failed robots fetch read as no rules: red.
#[test]
fn a_robots_txt_with_no_answer_asks_no_file_and_a_404_asks_it() -> TestResult {
    let mut seen = Vec::new();
    for status in [UNAVAILABLE, FOUND, NOT_FOUND] {
        let root = TempDir::new()?;
        let stub = Stub::sec(b"", ROSTER_BODY).answering(ROBOTS_URL, status, b"", &[]);
        let answer = cmd_roster::run(&ask(root.path()), &stub, &Set::at(FIRST_RUN)?)?;
        seen.push((
            status,
            answer.exit,
            stub.asked.borrow().len(),
            file_row(&answer)["not_asked"].clone(),
        ));
    }
    assert_eq!(
        seen,
        vec![
            (
                UNAVAILABLE,
                Exit::VendorUnreachable,
                1,
                json!("robots_unverified")
            ),
            (FOUND, Exit::VendorRefusedAll, 1, json!("robots_unverified")),
            (NOT_FOUND, Exit::Ok, 2, Value::Null),
        ]
    );
    Ok(())
}

/// A 200 that is not the file's shape (a challenge page) is a failed fetch kept as received, an
/// empty roster is no roster, and a 403 is transient as edgar-press reads SEC; none is worked
/// around. Mutations 2026-11-23, each red: the shape check dropped (any 200 usable); an empty
/// `data` usable; `Empty` counted as work done in the exit.
#[test]
fn a_challenge_page_an_empty_roster_and_a_403_are_failures_kept_as_received() -> TestResult {
    let mut seen = Vec::new();
    for (status, body) in [
        (OK, CHALLENGE),
        (OK, EMPTY_ROSTER),
        (FORBIDDEN, b"Access Denied".as_slice()),
    ] {
        let root = TempDir::new()?;
        let stub = Stub::sec(ROBOTS_BODY, b"").answering(FILE_URL, status, body, &[]);
        let answer = cmd_roster::run(&ask(root.path()), &stub, &Set::at(FIRST_RUN)?)?;
        seen.push((
            answer.exit,
            file_row(&answer)["verdict"].clone(),
            file_row(&answer)["raw_blob"].clone(),
        ));
    }
    let kept = |body: &[u8]| json!(FileDigest::of(body).hex());
    assert_eq!(
        seen,
        vec![
            (
                Exit::VendorRefusedAll,
                json!("permanent_failure"),
                kept(CHALLENGE)
            ),
            (Exit::VendorRefusedAll, json!("empty"), kept(EMPTY_ROSTER)),
            (
                Exit::VendorUnreachable,
                json!("transient_failure"),
                kept(b"Access Denied")
            ),
        ]
    );
    Ok(())
}

/// SEC refuses a User-Agent with no contact, so an empty one stops the run before the store is
/// touched or anything is asked; a dry run asks nothing and creates nothing. Mutation 2026-11-23,
/// the contact check dropped: red (two requests sent).
#[test]
fn an_empty_contact_and_a_dry_run_ask_nothing_and_write_nothing() -> TestResult {
    let root = TempDir::new()?;
    let store = root.path().join("vault");
    let stub = Stub::sec(ROBOTS_BODY, ROSTER_BODY);
    let refused = cmd_roster::run(
        &Ask {
            contact: " ".to_owned(),
            ..ask(&store)
        },
        &stub,
        &Set::at(FIRST_RUN)?,
    );
    let planned = cmd_roster::run(
        &Ask {
            dry_run: true,
            ..ask(&store)
        },
        &stub,
        &Set::at(FIRST_RUN)?,
    )?;
    assert_eq!(
        (
            matches!(refused, Err(RosterError::ContactMissing)),
            refused.as_ref().err().map(RosterError::exit),
            stub.asked.borrow().len(),
            store.exists(),
            planned.exit,
            file_row(&planned)["url"].clone(),
        ),
        (true, Some(2), 0, false, Exit::Ok, json!(FILE_URL))
    );
    Ok(())
}

/// Through the real `Wire`: SEC's transport, and SEC's per-IP pace file shared with edgar-press,
/// never one of the roster's own (section 8 row R19). Mutation 2026-11-23, `wire_for` built with
/// `ROSTER_SOURCE`: red.
#[test]
fn a_roster_fetch_through_the_wire_pays_into_edgar_press_s_per_ip_pace() -> TestResult {
    let root = TempDir::new()?;
    let server = loopback::Loopback::redirecting("/never", ROSTER_BODY.to_vec())?;
    let stub = Stub::sec(ROBOTS_BODY, b"");
    let ask = ask(root.path());
    let exchange = loopback::Relay {
        stub: &stub,
        wire: cmd_roster::wire_for(&ask),
        relayed: BTreeMap::from([(FILE_URL.to_owned(), server.url("/files/roster.json"))]),
        asked: RefCell::new(Vec::new()),
    };
    let answer = cmd_roster::run(&ask, &exchange, &Set::at(FIRST_RUN)?)?;
    let (sent, seen) = (exchange.sent(), server.seen()?);
    server.finish()?;
    let pace = root.path().join("pace");
    assert_eq!(
        (
            answer.exit,
            sent,
            seen,
            pace.join(format!("{}.per-ip", edgar_press::SOURCE))
                .exists(),
            pace.join(format!("{ROSTER_SOURCE}.per-ip")).exists(),
        ),
        (
            Exit::Ok,
            vec![ROBOTS_URL.to_owned(), FILE_URL.to_owned()],
            vec!["/files/roster.json".to_owned()],
            true,
            false
        )
    );
    Ok(())
}

fn shown(store: &Path, at: &str) -> Result<cmd_roster::Shown, Box<dyn Error>> {
    Ok(cmd_roster::show(store, at.parse()?)?)
}

fn current(shown: &cmd_roster::Shown) -> Value {
    shown.report["files"][0]["current"].clone()
}

/// `show` names the newest copy both of whose times are at or before `--at`; a failed later attempt
/// is the last attempt, never the copy. Mutations 2026-11-23, each red: the observation a day early;
/// the oldest visible copy kept; a failed attempt's payload taken as a copy.
#[test]
fn roster_show_names_the_copy_visible_at_an_instant_by_both_times() -> TestResult {
    let root = TempDir::new()?;
    let clock = Set::at(FIRST_RUN)?;
    let second_body = br#"{"fields":["cik","name","ticker","exchange"],"data":[[1,"Alpha Corp","ALP","Nasdaq"]]}"#;
    cmd_roster::run(
        &ask(root.path()),
        &Stub::sec(ROBOTS_BODY, ROSTER_BODY),
        &clock,
    )?;
    clock.to(SECOND_RUN)?;
    cmd_roster::run(
        &ask(root.path()),
        &Stub::sec(ROBOTS_BODY, second_body),
        &clock,
    )?;
    clock.to("2026-11-25T01:20:00Z")?;
    cmd_roster::run(
        &ask(root.path()),
        &Stub::sec(ROBOTS_BODY, b"").answering(FILE_URL, OK, CHALLENGE, &[("date", DATE)]),
        &clock,
    )?;
    let before = shown(root.path(), "2026-11-23T01:19:59Z")?;
    let between = shown(root.path(), "2026-11-24T00:00:00Z")?;
    let after = shown(root.path(), "2026-11-26T00:00:00Z")?;
    assert_eq!(
        (
            before.every_file_visible,
            current(&before),
            between.every_file_visible,
            current(&between)["universe_version"].clone(),
            current(&between)["valid_at"].clone(),
            current(&between)["valid_from"].clone(),
            current(&between)["observed_at"].clone(),
            current(&after)["universe_version"].clone(),
            after.report["files"][0]["last_attempt"]["verdict"].clone(),
            after.report["files"][0]["attempts"].clone(),
        ),
        (
            false,
            Value::Null,
            true,
            json!(FileDigest::of(ROSTER_BODY).hex()),
            json!("2026-11-20T21:53:43.000000Z"),
            json!("last-modified"),
            json!("2026-11-23T01:20:00.000000Z"),
            json!(FileDigest::of(second_body).hex()),
            json!("permanent_failure"),
            json!(3),
        )
    );
    Ok(())
}

/// With no `Last-Modified` the server's `Date` is the copy's valid time; a copy received before
/// its own valid time (a server clock ahead of ours) is listed as refused and never current, and
/// the read goes on. Mutations 2026-11-23, each red: the `Date` fallback dropped; a refused copy
/// failing the whole read.
#[test]
fn a_copy_takes_its_date_with_no_last_modified_and_one_received_before_it_is_refused() -> TestResult
{
    let dated = TempDir::new()?;
    let stub = Stub::sec(ROBOTS_BODY, b"").answering(
        FILE_URL,
        OK,
        ROSTER_BODY,
        &[("date", "Mon, 23 Nov 2026 01:19:59 GMT")],
    );
    cmd_roster::run(&ask(dated.path()), &stub, &Set::at(FIRST_RUN)?)?;
    let from_date = shown(dated.path(), "2026-11-23T02:00:00Z")?;
    let early = TempDir::new()?;
    let stub = Stub::sec(ROBOTS_BODY, b"").answering(FILE_URL, OK, ROSTER_BODY, &[("date", DATE)]);
    cmd_roster::run(&ask(early.path()), &stub, &Set::at(FIRST_RUN)?)?;
    let skewed = shown(early.path(), "2026-11-25T00:00:00Z")?;
    assert_eq!(
        (
            current(&from_date)["valid_from"].clone(),
            current(&from_date)["valid_at"].clone(),
            skewed.every_file_visible,
            current(&skewed),
            skewed.report["files"][0]["refused"]
                .as_array()
                .map(Vec::len),
        ),
        (
            json!("date"),
            json!("2026-11-23T01:19:59.000000Z"),
            false,
            Value::Null,
            Some(1)
        )
    );
    Ok(())
}

/// A roster run is a known run whose attempts are no orphans, it is no collect day row, and it
/// moves no asking clock: a daily SEC fetch must not hide a dead news collector from
/// `NewsvaultCollectStale`. Mutations 2026-11-23, each red: `ROSTER_SOURCE` dropped from the
/// asking clock's exclusions; the envelope written with kind `collect`.
#[test]
fn a_roster_run_moves_no_asking_clock_and_is_no_collect_run_or_orphan() -> TestResult {
    let root = TempDir::new()?;
    cmd_roster::run(
        &ask(root.path()),
        &Stub::sec(ROBOTS_BODY, ROSTER_BODY),
        &Set::at(FIRST_RUN)?,
    )?;
    let report = cmd_ledger::read(
        root.path(),
        &cmd_ledger::Ask {
            at: "2026-11-23T23:00:00Z".parse()?,
            from: "2026-11-23".parse()?,
            to: "2026-11-23".parse()?,
            source: None,
            expected_runs: None,
            limit: None,
            check: true,
        },
    )?;
    assert_eq!(
        (
            report.freshness.asking_clock.newest.clone(),
            report.is_complete(),
            report
                .days
                .iter()
                .map(|day| (day.runs_observed, day.per_source.len()))
                .collect::<Vec<(u32, usize)>>(),
        ),
        (None, true, vec![(0, 0)])
    );
    Ok(())
}

/// `ROSTER_SOURCE` is kept out of the asking clock by name, so a source of that name would vanish.
#[test]
fn no_registered_source_is_named_roster() {
    assert!(Registered::ALL
        .iter()
        .all(|entry| entry.source() != ROSTER_SOURCE));
}

/// One file today, SEC's; Nasdaq Trader's wait for the owner's reading of its terms (section 10).
#[test]
fn the_roster_asks_exactly_sec_s_exchange_file() {
    assert_eq!(
        FILES
            .iter()
            .map(|file| (file.key, file.url()))
            .collect::<Vec<_>>(),
        vec![(FILE_KEY, FILE_URL.to_owned())]
    );
}

/// The hand-written manifest against the parser it describes, for both subcommands.
#[test]
fn every_option_the_roster_parsers_take_is_in_the_machine_readable_schema() -> TestResult {
    let binary = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_newsvault"))
            .args(args)
            .output()
    };
    let schema: Value = serde_json::from_slice(&binary(&["schema", "--format", "json"])?.stdout)?;
    let mut missing = Vec::new();
    for action in ["fetch", "show"] {
        let help =
            String::from_utf8_lossy(&binary(&["roster", action, "--help"])?.stdout).to_string();
        let named: Vec<String> = schema["commands"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|command| command["path"] == json!(["roster", action]))
            .and_then(|command| command["options"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|option| option["name"].as_str().map(|name| name.replace('_', "-")))
            .collect();
        if named.is_empty() {
            missing.push(format!("{action}: no schema entry"));
        }
        for word in help.split_whitespace() {
            let flag = word.trim_end_matches(|c: char| !c.is_alphanumeric());
            if let Some(long) = flag.strip_prefix("--") {
                if !long.is_empty() && long != "help" && !named.contains(&long.to_owned()) {
                    missing.push(format!("{action}: {long}"));
                }
            }
        }
    }
    assert_eq!(missing, Vec::<String>::new());
    Ok(())
}

/// `show` over a store with no commit log is `input_absent`, exit 4, never an empty answer.
#[test]
fn roster_show_over_no_store_exits_4() -> TestResult {
    let root = TempDir::new()?;
    let output = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .args(["roster", "show", "--at", FIRST_RUN, "--format", "json"])
        .arg("--store")
        .arg(root.path())
        .output()?;
    assert_eq!(
        (output.status.code(), output.stdout.is_empty()),
        (Some(4), true)
    );
    Ok(())
}
