//! `newsvault roster fetch` and `roster show` over a stub `Exchange` and a set clock, never the
//! network (architecture.md section 10, "The roster"), and what the ledger reads of their runs.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;
use std::process::Command;

use canon::FileDigest;
use chrono::{DateTime, Utc};
use chrono::{SecondsFormat, TimeDelta};
use news_provider::{
    BudgetError, Clock, Exchange, Exit, Headers, PaceRoot, RawResponse, Request, RunId,
    SendFailure, SendPermit, TransportError, TransportKind,
};
use newsvault::cmd_collect::Registered;
use newsvault::cmd_ledger;
use newsvault::cmd_robots::{RobotsError, ROBOTS_SOURCE};
use newsvault::cmd_roster::{self, Ask, RosterError, FILES, ROSTER_SOURCE};
use newsvault::store::{Committer, COMMIT_LOG, RUNS_DIR};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use tempfile::TempDir;
use vault_core::blobs::{BlobStore, WritePolicy};

mod loopback;

type TestResult = Result<(), Box<dyn Error>>;

const CONTACT: &str = "bellwether collector contact@example.test";
const ROBOTS_URL: &str = "https://www.sec.gov/robots.txt";
const FILE_URL: &str = "https://www.sec.gov/files/company_tickers_exchange.json";
const FILE_KEY: &str = "sec_company_tickers_exchange";
const SEC_HOST: &str = "www.sec.gov";
const SEC_INTERVAL: std::time::Duration = std::time::Duration::from_millis(110);
const ROSTER_BODY_LIMIT: u64 = 8 * 1_024 * 1_024;
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
const MOVED: u16 = 301;
const TOO_MANY: u16 = 429;
/// Each `Tick` read moves the clock this far, so an attempt's open, answer and close differ.
const TICK_S: i64 = 10;

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

/// A clock that moves on by [`TICK_S`] each time it is read.
struct Tick(Cell<DateTime<Utc>>);

impl Tick {
    fn at(text: &str) -> Result<Self, Box<dyn Error>> {
        Ok(Self(Cell::new(text.parse()?)))
    }
}

impl Clock for Tick {
    fn now(&self) -> DateTime<Utc> {
        let now = self.0.get();
        self.0.set(now + TimeDelta::seconds(TICK_S));
        now
    }
}

/// A send that returns no status: a body cut at the limit, or a pace that refused the call.
#[derive(Clone, Copy)]
enum Failing {
    OverLimit,
    Budget,
}

/// One answer per url: a status, a body and the headers a server sent.
type Answer = (u16, Vec<u8>, Vec<(String, String)>);

/// Each url answered as set; a url it does not hold fails in transport.
#[derive(Default)]
struct Stub {
    answers: BTreeMap<String, Answer>,
    failing: BTreeMap<String, Failing>,
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

    fn failing(mut self, url: &str, failing: Failing) -> Self {
        self.failing.insert(url.to_owned(), failing);
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
        let failed = || TransportError::new(attempt.clone(), at, at, "cut".to_owned());
        match self.failing.get(&request.url) {
            Some(Failing::OverLimit) => {
                return Err(SendFailure::Transport(
                    failed().of_kind(TransportKind::BodyOverLimit),
                ))
            }
            Some(Failing::Budget) => return Err(SendFailure::Budget(BudgetError::NoPace)),
            None => {}
        }
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
    let blobs = BlobStore::new(root.path().join("blobs"), WritePolicy::RawUntilDictionary);
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
/// 110 ms pace or a body that is no robots.txt asks nothing and opens no roster attempt, and exits
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

/// A 200 that is not the file's shape (a challenge page, other or reordered field names) is a
/// failed fetch kept as received, an empty roster is no roster, 403, 429 and 5xx are transient as
/// `sec_edgar::body_of_200` reads SEC, and any other status is permanent; none is worked around. Mutations
/// 2026-11-23, each red: the shape check dropped (any 200 usable); the field-name check dropped; an
/// empty `data` usable; `Empty` counted as work done; 5xx permanent; 408 and 429 permanent; any
/// other status transient.
#[test]
fn a_challenge_page_an_empty_roster_and_a_403_are_failures_kept_as_received() -> TestResult {
    let mut seen = Vec::new();
    for (status, body) in [
        (OK, CHALLENGE),
        (OK, EMPTY_ROSTER),
        (FORBIDDEN, b"Access Denied".as_slice()),
        (
            OK,
            br#"{"fields":["cik","name","ticker"],"data":[[1,"A","A"]]}"#.as_slice(),
        ),
        (
            OK,
            br#"{"fields":["cik","ticker","name","exchange"],"data":[[1,"A","A","N"]]}"#.as_slice(),
        ),
        (TOO_MANY, b"slow down".as_slice()),
        (UNAVAILABLE, b"down".as_slice()),
        (NOT_FOUND, b"gone".as_slice()),
        (MOVED, b"moved".as_slice()),
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
            (
                Exit::VendorRefusedAll,
                json!("permanent_failure"),
                kept(br#"{"fields":["cik","name","ticker"],"data":[[1,"A","A"]]}"#)
            ),
            (
                Exit::VendorRefusedAll,
                json!("permanent_failure"),
                kept(br#"{"fields":["cik","ticker","name","exchange"],"data":[[1,"A","A","N"]]}"#)
            ),
            (
                Exit::VendorUnreachable,
                json!("transient_failure"),
                kept(b"slow down")
            ),
            (
                Exit::VendorUnreachable,
                json!("transient_failure"),
                kept(b"down")
            ),
            (
                Exit::VendorRefusedAll,
                json!("permanent_failure"),
                kept(b"gone")
            ),
            (
                Exit::VendorRefusedAll,
                json!("permanent_failure"),
                kept(b"moved")
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
/// `ROSTER_SOURCE`: red. The files are under the pace root named and none is in the store
/// (section 12.2).
#[test]
fn a_roster_fetch_through_the_wire_pays_into_edgar_press_s_per_ip_pace() -> TestResult {
    let (root, shared) = (TempDir::new()?, TempDir::new()?);
    let server = loopback::Loopback::redirecting("/never", ROSTER_BODY.to_vec())?;
    let stub = Stub::sec(ROBOTS_BODY, b"");
    let ask = ask(root.path());
    let pace_root = PaceRoot::resolve(Some(shared.path()), root.path())?;
    let exchange = loopback::Relay {
        stub: &stub,
        wire: cmd_roster::wire_for(&ask, &pace_root),
        relayed: BTreeMap::from([(FILE_URL.to_owned(), server.url("/files/roster.json"))]),
        asked: RefCell::new(Vec::new()),
    };
    let answer = cmd_roster::run(&ask, &exchange, &Set::at(FIRST_RUN)?)?;
    let (sent, seen) = (exchange.sent(), server.seen()?);
    server.finish()?;
    let pace = shared.path().join("pace");
    assert_eq!(
        (
            answer.exit,
            sent,
            seen,
            pace.join(format!("{}.per-ip", edgar_press::SOURCE))
                .exists(),
            pace.join(format!("{ROSTER_SOURCE}.per-ip")).exists(),
            root.path().join("pace").exists(),
        ),
        (
            Exit::Ok,
            vec![ROBOTS_URL.to_owned(), FILE_URL.to_owned()],
            vec!["/files/roster.json".to_owned()],
            true,
            false,
            false
        )
    );
    Ok(())
}

/// One metadata for the code (section 5.1): the entry `newsvault schema` lists is the code and
/// the `retryable` of the object a refused run writes and the exit it takes, and both commands'
/// exit tables name it at 7.
#[test]
fn the_schema_lists_gate_refused_with_the_exit_and_retryable_a_refused_run_reports() -> TestResult {
    let printed = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .args(["schema", "--format", "json"])
        .output()?;
    let schema: Value = serde_json::from_slice(&printed.stdout)?;
    let refusal = RosterError::Robots(RobotsError::GateRefused {
        attempt_id: "an attempt".to_owned(),
        why: BudgetError::NeverCleared {
            tries: 8,
            owed: SEC_INTERVAL,
        },
    });
    let (object, exit) = refusal.ended_at_the_gate(Vec::new()).unwrap_or_default();
    let listed = json!({
        "code": object["code"],
        "exit": exit,
        "retryable": object["retryable"],
    });
    let named_at_seven = |command: &[&str]| {
        schema["commands"]
            .as_array()
            .and_then(|all| all.iter().find(|entry| entry["path"] == json!(command)))
            .and_then(|entry| entry["exits"]["7"].as_str())
            .is_some_and(|text| text.contains("gate_refused"))
    };
    assert_eq!(
        (
            schema["error_codes"]
                .as_array()
                .is_some_and(|codes| codes.contains(&listed)),
            listed,
            named_at_seven(&["roster", "fetch"]),
            named_at_seven(&["robots", "refresh"]),
        ),
        (
            true,
            json!({"code": "gate_refused", "exit": 7, "retryable": true}),
            true,
            true
        )
    );
    Ok(())
}

/// `(source, outcome, requested_at, slots, closed)` of one stored attempt.
type ClosedAttempt = (String, String, Option<String>, Option<i64>, bool);

/// SEC's per-IP gate file kept locked by a second open file, as another sending process keeps
/// it: the file's send waits its 32 intervals (3.52 s) and is refused. The run ends there with
/// `gate_refused`, exit 7, retryable; the attempt is closed `not_sent` with nothing requested,
/// no payload and no slot spent, none is left open for a sweep, and the server saw no request.
#[test]
fn a_roster_fetch_whose_send_the_gate_refuses_ends_retryable_with_its_attempt_closed_not_sent(
) -> TestResult {
    let (root, shared) = (TempDir::new()?, TempDir::new()?);
    let server = loopback::Loopback::redirecting("/never", ROSTER_BODY.to_vec())?;
    let stub = Stub::sec(ROBOTS_BODY, b"");
    let ask = ask(root.path());
    let pace_root = PaceRoot::resolve(Some(shared.path()), root.path())?;
    let pace = shared.path().join("pace");
    std::fs::create_dir_all(&pace)?;
    let gate = pace.join(format!("{}.per-ip.sent", edgar_press::SOURCE));
    let held = std::fs::File::create(&gate)?;
    held.lock()?;
    let exchange = loopback::Relay {
        stub: &stub,
        wire: cmd_roster::wire_for(&ask, &pace_root),
        relayed: BTreeMap::from([(FILE_URL.to_owned(), server.url("/files/roster.json"))]),
        asked: RefCell::new(Vec::new()),
    };
    let refused = cmd_roster::run(&ask, &exchange, &Set::at(FIRST_RUN)?).err();
    drop(held);
    let seen = server.seen()?;
    server.finish()?;
    let (object, exit) = refused
        .as_ref()
        .and_then(|refusal| refusal.ended_at_the_gate(vec!["again".to_owned()]))
        .unwrap_or_default();
    let human = newsvault::cmd_robots::human_error(&object);
    let db = read_only(root.path())?;
    let attempts: Vec<ClosedAttempt> = db
        .prepare(
            "SELECT source, outcome, requested_at, slots, closed_at IS NOT NULL
               FROM attempts ORDER BY rowid",
        )?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    let payloads: i64 = db.query_row(
        "SELECT COUNT(*) FROM payloads p JOIN attempts a ON a.attempt_id = p.attempt_id
          WHERE a.source = ?1",
        [ROSTER_SOURCE],
        |row| row.get(0),
    )?;
    let run_file: Value = serde_json::from_slice(&std::fs::read(
        root.path()
            .join(RUNS_DIR)
            .join("roster-20261123T012000Z.json"),
    )?)?;
    assert_eq!(
        (
            exit,
            human.starts_with("error: gate_refused (retryable true): the send gate refused")
                && human.contains(&gate.display().to_string()),
            (
                object["code"].clone(),
                object["retryable"].clone(),
                object["suggestions"].clone(),
                object["details"]["refusal"]
                    .as_str()
                    .is_some_and(|text| text.contains(&gate.display().to_string())),
            ),
            seen,
            attempts.len(),
            attempts.last().cloned(),
            payloads,
            run_file["finished_at"].is_string(),
            run_file["refused"].clone(),
        ),
        (
            7,
            true,
            (json!("gate_refused"), json!(true), json!([["again"]]), true),
            Vec::new(),
            2,
            Some((
                ROSTER_SOURCE.to_owned(),
                "not_sent".to_owned(),
                None,
                Some(0),
                true
            )),
            0,
            true,
            json!("gate_refused"),
        )
    );
    Ok(())
}

/// The roster's row of SEC's interval table and the roster's own body limit (section 8 row R19).
/// Before this test the row at 200 ms was 0 red of 1,307 (2026-11-25).
#[test]
fn the_roster_asks_sec_at_most_ten_times_a_second_and_keeps_at_most_eight_mebibytes() {
    let profile = cmd_roster::transport();
    assert_eq!(
        (
            profile.budget.min_interval,
            profile.budget.per_host,
            profile.body_limit
        ),
        (Some(SEC_INTERVAL), Some(SEC_INTERVAL), ROSTER_BODY_LIMIT)
    );
}

fn shown(store: &Path, at: &str) -> Result<cmd_roster::Shown, Box<dyn Error>> {
    Ok(cmd_roster::show(store, at.parse()?)?)
}

fn current(shown: &cmd_roster::Shown) -> Value {
    shown.report["files"][0]["current"].clone()
}

fn last(shown: &cmd_roster::Shown) -> Value {
    shown.report["files"][0]["last_attempt"].clone()
}

/// The roster attempt's `opened_at`, `received_at` and `closed_at` as stored.
fn roster_times(store: &Path) -> Result<(String, String, String), Box<dyn Error>> {
    Ok(read_only(store)?.query_row(
        "SELECT opened_at, received_at, closed_at FROM attempts WHERE source = ?1",
        [ROSTER_SOURCE],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?)
}

fn micros(text: &str) -> Result<Value, Box<dyn Error>> {
    Ok(json!(text
        .parse::<DateTime<Utc>>()?
        .to_rfc3339_opts(SecondsFormat::Micros, true)))
}

/// `show` names the newest copy both of whose times are at or before `--at`; a failed or empty later
/// attempt is the last attempt, never the copy, and an attempt opened after `--at` is not counted.
/// Mutations 2026-11-23, each red: the observation a day early; the oldest visible copy kept; a
/// failed attempt's payload taken as a copy; an `empty` copy taken; the `opened_at` filter dropped.
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
    clock.to("2026-11-26T01:20:00Z")?;
    cmd_roster::run(
        &ask(root.path()),
        &Stub::sec(ROBOTS_BODY, EMPTY_ROSTER),
        &clock,
    )?;
    let before = shown(root.path(), "2026-11-23T01:19:59Z")?;
    let between = shown(root.path(), "2026-11-24T00:00:00Z")?;
    let after = shown(root.path(), "2026-11-27T00:00:00Z")?;
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
            last(&after)["verdict"].clone(),
            after.report["files"][0]["attempts"].clone(),
            between.report["files"][0]["attempts"].clone(),
            last(&between)["opened_at"].clone(),
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
            json!("empty"),
            json!(4),
            json!(1),
            json!("2026-11-23T01:20:00.000000Z"),
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

/// On a clock that moves between an attempt's open, answer and close: at `opened_at` there is no
/// copy and no verdict, at `received_at` the copy is current and the attempt still in flight, and
/// only at `closed_at` is its verdict reported. Mutations 2026-11-23, each red: the observation
/// taken at `opened_at`; the verdict reported before the close.
#[test]
fn roster_show_reports_a_copy_once_received_and_a_verdict_once_closed() -> TestResult {
    let root = TempDir::new()?;
    cmd_roster::run(
        &ask(root.path()),
        &Stub::sec(ROBOTS_BODY, ROSTER_BODY),
        &Tick::at(FIRST_RUN)?,
    )?;
    let (opened, received, closed) = roster_times(root.path())?;
    let at_open = shown(root.path(), &opened)?;
    let at_received = shown(root.path(), &received)?;
    let at_closed = shown(root.path(), &closed)?;
    assert_eq!(
        (
            opened < received && received < closed,
            current(&at_open),
            last(&at_open)["in_flight"].clone(),
            last(&at_open)["verdict"].clone(),
            current(&at_received)["observed_at"].clone(),
            last(&at_received)["in_flight"].clone(),
            last(&at_received)["outcome"].clone(),
            last(&at_closed)["in_flight"].clone(),
            last(&at_closed)["verdict"].clone(),
        ),
        (
            true,
            Value::Null,
            json!(true),
            Value::Null,
            micros(&received)?,
            json!(true),
            Value::Null,
            json!(false),
            json!("usable"),
        )
    );
    Ok(())
}

/// A copy whose server clock is ahead of ours is listed as refused only once we hold it: a read
/// between the attempt's open and its answer lists nothing. Mutation 2026-11-23, the `received`
/// guard dropped: red.
#[test]
fn a_skewed_copy_is_refused_only_from_its_received_at() -> TestResult {
    let root = TempDir::new()?;
    let stub = Stub::sec(ROBOTS_BODY, b"").answering(
        FILE_URL,
        OK,
        ROSTER_BODY,
        &[("date", "Mon, 23 Nov 2026 03:00:00 GMT")],
    );
    cmd_roster::run(&ask(root.path()), &stub, &Tick::at(FIRST_RUN)?)?;
    let (opened, received, _) = roster_times(root.path())?;
    let refused = |at: &str| -> Result<Value, Box<dyn Error>> {
        Ok(shown(root.path(), at)?.report["files"][0]["refused"].clone())
    };
    assert_eq!(
        (
            refused(&opened)?.as_array().map(Vec::len),
            refused(&received)?.as_array().map(Vec::len)
        ),
        (Some(0), Some(1))
    );
    Ok(())
}

/// `(source, outcome)` of an attempt.
type SourceOutcome = (String, Option<String>);

/// `(source, outcome)` of every attempt left with no `closed_at`.
fn open_attempts(store: &Path) -> Result<Vec<SourceOutcome>, Box<dyn Error>> {
    let db = read_only(store)?;
    let mut statement =
        db.prepare("SELECT source, outcome FROM attempts WHERE closed_at IS NULL")?;
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// An error between an attempt's open and its close (here a pace that refuses the send) closes
/// that attempt as `interrupted` before the run fails, for the file and for its robots.txt; left
/// open, the next collect's startup sweep would close it and exit 8 for it. Mutation 2026-11-23,
/// either `or_abandon` returning the error alone: red.
#[test]
fn an_error_between_open_and_close_leaves_no_attempt_open() -> TestResult {
    let mut seen = Vec::new();
    for url in [FILE_URL, ROBOTS_URL] {
        let root = TempDir::new()?;
        let stub = Stub::sec(ROBOTS_BODY, ROSTER_BODY).failing(url, Failing::Budget);
        let refused = cmd_roster::run(&ask(root.path()), &stub, &Set::at(FIRST_RUN)?);
        let interrupted: i64 = read_only(root.path())?.query_row(
            "SELECT COUNT(*) FROM attempts WHERE outcome = 'interrupted'",
            [],
            |row| row.get(0),
        )?;
        seen.push((
            refused.as_ref().err().map(RosterError::exit),
            open_attempts(root.path())?,
            interrupted,
        ));
    }
    assert_eq!(
        seen,
        vec![(Some(2), Vec::new(), 1), (Some(2), Vec::new(), 1)]
    );
    Ok(())
}

/// A file with no answer at all is a closed `transient_failure` attempt with no payload, exit 7;
/// a body over the limit is `permanent_failure`, exit 8. Mutations 2026-11-23, each red: a
/// transport error returned before the close; a transport error permanent; an over-limit body
/// transient.
#[test]
fn a_file_with_no_answer_or_a_body_over_the_limit_is_closed_and_counted() -> TestResult {
    let mut seen = Vec::new();
    for stub in [
        Stub::default().answering(ROBOTS_URL, OK, ROBOTS_BODY, &[]),
        Stub::sec(ROBOTS_BODY, ROSTER_BODY).failing(FILE_URL, Failing::OverLimit),
    ] {
        let root = TempDir::new()?;
        let answer = cmd_roster::run(&ask(root.path()), &stub, &Set::at(FIRST_RUN)?)?;
        let roster_payloads: i64 = read_only(root.path())?.query_row(
            "SELECT COUNT(*) FROM payloads p JOIN attempts a ON a.attempt_id = p.attempt_id
              WHERE a.source = ?1",
            [ROSTER_SOURCE],
            |row| row.get(0),
        )?;
        seen.push((
            answer.exit,
            file_row(&answer)["verdict"].clone(),
            open_attempts(root.path())?.len(),
            roster_payloads,
        ));
    }
    assert_eq!(
        seen[0],
        (Exit::VendorUnreachable, json!("transient_failure"), 0, 0)
    );
    assert_eq!(
        (seen[1].0, seen[1].1.clone(), seen[1].2),
        (Exit::VendorRefusedAll, json!("permanent_failure"), 0)
    );
    Ok(())
}

/// A usable copy whose stored headers do not parse, or carry neither `Last-Modified` nor `Date`,
/// fails the read by name (exit 2), never a hidden copy. Mutations 2026-11-23, each red: either
/// case read as no copy.
#[test]
fn a_usable_copy_with_no_readable_valid_time_fails_the_read_by_name() -> TestResult {
    let undated = TempDir::new()?;
    let stub = Stub::sec(ROBOTS_BODY, b"").answering(FILE_URL, OK, ROSTER_BODY, &[]);
    cmd_roster::run(&ask(undated.path()), &stub, &Set::at(FIRST_RUN)?)?;
    let corrupt = TempDir::new()?;
    cmd_roster::run(
        &ask(corrupt.path()),
        &Stub::sec(ROBOTS_BODY, ROSTER_BODY),
        &Set::at(FIRST_RUN)?,
    )?;
    Connection::open(corrupt.path().join(COMMIT_LOG))?.execute(
        "UPDATE payloads SET headers = '{' WHERE url = ?1",
        [FILE_URL],
    )?;
    let at: DateTime<Utc> = "2026-11-24T00:00:00Z".parse()?;
    let read = |root: &Path| {
        cmd_roster::show(root, at)
            .map(|shown| shown.every_file_visible)
            .map_err(|why| (matches!(why, RosterError::StoredTime { .. }), why.exit()))
    };
    assert_eq!(
        (read(undated.path()), read(corrupt.path())),
        (Err((true, 2)), Err((true, 2)))
    );
    Ok(())
}

/// A run that finds the store lease held exits 7 and leaves its envelope finished and marked
/// `lease_held`, as a refused collect's is. Mutations 2026-11-23, each red: `lease_held` mapped to
/// 2; the envelope left unfinished.
#[test]
fn a_held_lease_exits_7_and_marks_the_envelope() -> TestResult {
    let root = TempDir::new()?;
    let holder = Committer::open(root.path(), &RunId::new("holder".to_owned()))?;
    let stub = Stub::sec(ROBOTS_BODY, ROSTER_BODY);
    let refused = cmd_roster::run(&ask(root.path()), &stub, &Set::at(FIRST_RUN)?);
    drop(holder);
    let envelope: Value = serde_json::from_slice(&std::fs::read(
        root.path()
            .join(RUNS_DIR)
            .join("roster-20261123T012000Z.json"),
    )?)?;
    assert_eq!(
        (
            refused.as_ref().err().map(RosterError::exit),
            stub.asked.borrow().len(),
            envelope["refused"].clone(),
            envelope["finished_at"].is_string(),
        ),
        (Some(7), 0, json!("lease_held"), true)
    );
    Ok(())
}

/// Exit 7 carries the report in the error object (stderr); 0 and 8 carry it on stdout. The
/// binary's choice of stream reads this; no test drives the binary to exit 7 (it sends to SEC).
#[test]
fn only_an_unreachable_run_rides_the_error_object() {
    let object = |exit: Exit| {
        cmd_roster::Answer {
            report: json!({}),
            exit,
        }
        .error_object(Vec::new())
        .map(|object| object["code"].clone())
    };
    assert_eq!(
        (
            object(Exit::VendorUnreachable),
            object(Exit::PartialCollect),
            object(Exit::VendorRefusedAll),
            object(Exit::Ok),
        ),
        (Some(json!("vendor_unreachable")), None, None, None)
    );
}

/// The four contact variables, cleared so the developer's own shell cannot answer a test.
const CONTACT_VARIABLES: [&str; 4] = [
    "BELLWETHER_FEED_CONTACT",
    "BELLWETHER_FEED_CONTACT_FILE",
    "BELLWETHER_SEC_CONTACT",
    "BELLWETHER_SEC_CONTACT_FILE",
];

/// `roster fetch` reads the feed's contact file, the one the VPS unit mounts, and not SEC's: a
/// dry run with only the feed file set plans, and with only SEC's set is refused (exit 2, stdout
/// empty). Mutation 2026-11-23, `Registered::EdgarPress` for the contact: red.
#[test]
fn roster_fetch_reads_the_feed_contact_file_the_unit_mounts() -> TestResult {
    let root = TempDir::new()?;
    let file = root.path().join("contact");
    std::fs::write(&file, CONTACT)?;
    let dry_run = |variable: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_newsvault"));
        for name in CONTACT_VARIABLES {
            command.env_remove(name);
        }
        command
            .args([
                "roster",
                "fetch",
                "--dry-run",
                "--format",
                "json",
                "--store",
            ])
            .arg(root.path().join("vault"))
            .env(variable, &file)
            .output()
    };
    let feed = dry_run("BELLWETHER_FEED_CONTACT_FILE")?;
    let sec = dry_run("BELLWETHER_SEC_CONTACT_FILE")?;
    assert_eq!(
        (feed.status.code(), sec.status.code(), sec.stdout.is_empty()),
        (Some(0), Some(2), true)
    );
    Ok(())
}

/// `roster show` exits 1 when a file has no copy at `--at` ("the answer is no") and 0 when every
/// file has one, the report on stdout both times. Mutation 2026-11-23, exit 1 never returned: red.
#[test]
fn roster_show_exits_1_before_the_first_copy_and_0_after() -> TestResult {
    let root = TempDir::new()?;
    cmd_roster::run(
        &ask(root.path()),
        &Stub::sec(ROBOTS_BODY, ROSTER_BODY),
        &Set::at(FIRST_RUN)?,
    )?;
    let show = |at: &str| -> Result<(Option<i32>, Value), Box<dyn Error>> {
        let output = Command::new(env!("CARGO_BIN_EXE_newsvault"))
            .args(["roster", "show", "--at", at, "--format", "json", "--store"])
            .arg(root.path())
            .output()?;
        let report: Value = serde_json::from_slice(&output.stdout)?;
        Ok((output.status.code(), report["every_file_visible"].clone()))
    };
    assert_eq!(
        (show("2026-11-23T01:19:59Z")?, show("2026-11-23T02:00:00Z")?),
        ((Some(1), json!(false)), (Some(0), json!(true)))
    );
    Ok(())
}
