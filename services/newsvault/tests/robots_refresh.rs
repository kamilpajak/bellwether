//! `newsvault robots refresh` over a stub `Exchange` and a fixed clock, never the network
//! (architecture.md section 11.2), and what the ledger reads of the runs it leaves (section 10).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use canon::FileDigest;
use chrono::{DateTime, Utc};
use news_provider::{
    Clock, Exchange, Provider, RawResponse, Request, SendFailure, SendPermit, TransportError,
};
use newsvault::cmd_collect::Registered;
use newsvault::cmd_ledger;
use newsvault::cmd_robots::{self, Ask, Emitted, RobotsError, RobotsExit, ROBOTS_SOURCE};
use newsvault::store::{StoreError, COMMIT_LOG, RUNS_DIR};
use pit::ObservationTime;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tempfile::TempDir;

mod loopback;

type TestResult = Result<(), Box<dyn Error>>;

const SOURCE: &str = "open_web_wires";
const CONTACT: &str = "bellwether collector contact@example.test";
const CONTACT_ADDRESS: &str = "contact@example.test";
const NOW: &str = "2026-11-15T10:00:00Z";
const RUN_DAY: &str = "2026-11-15";
const RUN_ID: &str = "robots-20261115T100000Z";
const GNW_HOST: &str = "rss.globenewswire.com";
const GNW_ROBOTS: &str = "https://rss.globenewswire.com/robots.txt";
const PRN_ROBOTS: &str = "https://www.prnewswire.com/robots.txt";
const ROBOTS_PATH: &str = "/robots.txt";
/// The GNW row's pin, in the committed hosts file and `gnw_admitted.json`:
/// `robots_rss_globenewswire_com.txt`'s sha256.
const GNW_PIN: &str = "eb8bb698171dd56c139c224c9217070adf9947f6ed6cd02336eba55aa4155bfe";
/// The committed hosts file's PRN pin: `robots_www_prnewswire_com.txt`'s sha256.
const PRN_PIN: &str = "906828bd8cf3ae9fd8bfd1475ae1abe7db4cfda54a2ddbf88dcb80cd55f522b2";
const PRN_HOST: &str = "www.prnewswire.com";
const CNW_ROBOTS: &str = "https://www.newswire.ca/robots.txt";
/// The committed hosts file's CNW pin: `robots_www_newswire_ca.txt`'s sha256.
const CNW_PIN: &str = "ab9873af0f8c94c48918cd75345a8fc609665602dfcd96aa1e574881a34fb2a0";
const CNW_HOST: &str = "www.newswire.ca";
const FED_ROBOTS: &str = "https://www.federalreserve.gov/robots.txt";
const FED_HOST: &str = "www.federalreserve.gov";
const YAHOO_ROBOTS: &str = "https://finance.yahoo.com/robots.txt";
/// The committed hosts file's Yahoo pin: `robots_finance_yahoo_com.txt`'s sha256.
const YAHOO_PIN: &str = "a01715ceceb6faead3058d083abc97bdec68c7e4c6f522f3211dd0819ce38eb2";
const YAHOO_HOST: &str = "finance.yahoo.com";
const PRN_BODY: &[u8] = b"User-agent: *\nDisallow: /search/\n";
const CHANGED_BODY: &[u8] = b"User-agent: *\nDisallow: /\n";
const OK: u16 = 200;
const MOVED_PERMANENTLY: u16 = 301;
const NOT_FOUND: u16 = 404;
const FORBIDDEN: u16 = 403;
const UNAVAILABLE: u16 = 503;
const TOO_MANY_REQUESTS: u16 = 429;
const REQUEST_TIMEOUT: u16 = 408;
const EXIT_OK: i32 = 0;
const EXIT_USAGE: i32 = 2;
const EXIT_NOT_FOUND: i32 = 4;
const EXIT_PARTIAL: u8 = 8;
const RUNS_BELOW_EXPECTED: &str = "runs_below_expected";
const HOLDS_A_FAILURE: &str = "holds_a_failure";

fn provider_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../providers/open-web-feed")
}

/// The GNW row alone: a byte copy of the committed hosts file before PRN was admitted.
fn gnw_hosts() -> PathBuf {
    provider_dir().join("tests/fixtures/hosts/gnw_admitted.json")
}

/// The committed GNW row admitted, and a PRN candidate with no pin.
fn two_row_hosts() -> PathBuf {
    provider_dir().join("tests/fixtures/hosts/one_admitted_one_candidate.json")
}

fn committed_hosts() -> PathBuf {
    provider_dir().join("hosts/open_web_wires.hosts.json")
}

fn gnw_body() -> Result<Vec<u8>, std::io::Error> {
    std::fs::read(provider_dir().join("tests/fixtures/gnw/robots_rss_globenewswire_com.txt"))
}

fn prn_body() -> Result<Vec<u8>, std::io::Error> {
    std::fs::read(provider_dir().join("tests/fixtures/prn/robots_www_prnewswire_com.txt"))
}

fn cnw_body() -> Result<Vec<u8>, std::io::Error> {
    std::fs::read(provider_dir().join("tests/fixtures/cnw/robots_www_newswire_ca.txt"))
}

fn yahoo_body() -> Result<Vec<u8>, std::io::Error> {
    std::fs::read(provider_dir().join("tests/fixtures/yahoo/robots_finance_yahoo_com.txt"))
}

/// The Fed's 404 page at its robots path, trimmed; never parsed.
fn fed_404_body() -> Result<Vec<u8>, std::io::Error> {
    std::fs::read(provider_dir().join("tests/fixtures/fed/robots_404_www_federalreserve_gov.html"))
}

fn ask(store: &Path, hosts: PathBuf) -> Ask {
    Ask {
        source: SOURCE.to_owned(),
        hosts,
        rules: provider_dir().join("rules/open-web-feed.rules.json"),
        store: store.to_path_buf(),
        contact: CONTACT.to_owned(),
        dry_run: false,
    }
}

struct Fixed;

impl Clock for Fixed {
    fn now(&self) -> DateTime<Utc> {
        NOW.parse().unwrap_or(DateTime::UNIX_EPOCH)
    }
}

/// Each url answered with a status and a body; a url it does not hold fails in transport.
struct Robots {
    answers: BTreeMap<String, (u16, Vec<u8>)>,
    asked: RefCell<Vec<String>>,
}

impl Robots {
    fn of(answers: &[(&str, u16, &[u8])]) -> Self {
        Self {
            answers: answers
                .iter()
                .map(|(url, status, body)| ((*url).to_owned(), (*status, body.to_vec())))
                .collect(),
            asked: RefCell::new(Vec::new()),
        }
    }
}

impl Exchange for Robots {
    fn send(
        &self,
        permit: SendPermit,
        request: &Request,
        clock: &dyn Clock,
    ) -> Result<RawResponse, SendFailure> {
        self.asked.borrow_mut().push(request.url.clone());
        let attempt = permit.attempt_id().clone();
        let at = ObservationTime::new(clock.now());
        match self.answers.get(&request.url) {
            Some((status, body)) => Ok(RawResponse::new(attempt, *status, body.clone(), at, at)),
            None => Err(SendFailure::Transport(TransportError::new(
                attempt,
                at,
                at,
                format!("no stub answers {}", request.url),
            ))),
        }
    }
}

fn refreshed(
    store: &Path,
    hosts: PathBuf,
    exchange: &Robots,
) -> Result<cmd_robots::Answer, Box<dyn Error>> {
    let ask = ask(store, hosts);
    let feed = cmd_robots::open(&ask)?;
    Ok(cmd_robots::run(&ask, &feed, exchange, &Fixed)?)
}

fn read_only(store: &Path) -> Result<Connection, rusqlite::Error> {
    Connection::open_with_flags(store.join(COMMIT_LOG), OpenFlags::SQLITE_OPEN_READ_ONLY)
}

/// `(host, answer, blob)` of one robots row.
type RobotsRow = (String, String, Option<String>);
/// `(source, target_window, outcome)` of one attempt.
type AttemptRow = (String, String, String);

fn robots_rows(store: &Path) -> Result<Vec<RobotsRow>, Box<dyn Error>> {
    let db = read_only(store)?;
    // From user_version 6 a robots body is named by its `payloads` row and `robots.blob` is NULL;
    // a 404's body is linked too, and pins nothing.
    let mut statement = db.prepare(
        "SELECT r.host, r.answer,
                CASE r.answer WHEN 'present' THEN COALESCE(r.blob, p.raw_blob) END
           FROM robots r LEFT JOIN payloads p ON p.attempt_id = r.attempt_id ORDER BY r.host",
    )?;
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `(url, raw_blob, truncated, headers)` of every `payloads` row, by url.
type PayloadRow = (String, String, bool, String);

fn payload_rows(store: &Path) -> Result<Vec<PayloadRow>, Box<dyn Error>> {
    let db = read_only(store)?;
    let mut statement =
        db.prepare("SELECT url, raw_blob, truncated, headers FROM payloads ORDER BY url")?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn attempts(store: &Path) -> Result<Vec<AttemptRow>, Box<dyn Error>> {
    let db = read_only(store)?;
    let mut statement =
        db.prepare("SELECT source, target_window, outcome FROM attempts ORDER BY target_window")?;
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn first_row(answer: &cmd_robots::Answer) -> Value {
    answer.report["rows"][0].clone()
}

fn ledger(store: &Path, expected_runs: Option<u32>) -> Result<cmd_ledger::Report, Box<dyn Error>> {
    Ok(cmd_ledger::read(
        store,
        &cmd_ledger::Ask {
            at: "2026-11-15T23:00:00Z".parse()?,
            from: RUN_DAY.parse()?,
            to: RUN_DAY.parse()?,
            source: None,
            expected_runs,
            limit: None,
            check: true,
        },
    )?)
}

fn finding_codes(report: &cmd_ledger::Report) -> Vec<&'static str> {
    report
        .check
        .findings
        .iter()
        .map(|found| found.code)
        .collect()
}

#[test]
fn a_robots_refresh_writes_one_attempt_per_hosts_row_under_source_robots() -> TestResult {
    let root = TempDir::new()?;
    let body = gnw_body()?;
    let exchange = Robots::of(&[(GNW_ROBOTS, OK, &body), (PRN_ROBOTS, OK, PRN_BODY)]);
    let answer = refreshed(root.path(), two_row_hosts(), &exchange)?;
    let db = read_only(root.path())?;
    // `extract_version` NULL (section 3.2). Mutation 2026-11-16, `request_for` given a version: red.
    let versions: Vec<(String, Option<String>, Option<String>)> = db
        .prepare("SELECT run_id, hosts_version, extract_version FROM attempts")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let hosts_version = FileDigest::of(&std::fs::read(two_row_hosts())?);
    assert_eq!(
        (
            answer.exit,
            exchange.asked.borrow().clone(),
            attempts(root.path())?,
            versions,
        ),
        (
            RobotsExit::Ok,
            vec![GNW_ROBOTS.to_owned(), PRN_ROBOTS.to_owned()],
            vec![
                (
                    ROBOTS_SOURCE.to_owned(),
                    GNW_HOST.to_owned(),
                    "ok".to_owned()
                ),
                (
                    ROBOTS_SOURCE.to_owned(),
                    "www.prnewswire.com".to_owned(),
                    "ok".to_owned()
                ),
            ],
            vec![
                (
                    RUN_ID.to_owned(),
                    Some(hosts_version.hex().to_owned()),
                    None
                );
                2
            ],
        )
    );
    assert_eq!(
        robots_rows(root.path())?,
        vec![
            (
                GNW_HOST.to_owned(),
                "present".to_owned(),
                Some(GNW_PIN.to_owned())
            ),
            (
                "www.prnewswire.com".to_owned(),
                "present".to_owned(),
                Some(FileDigest::of(PRN_BODY).hex().to_owned())
            ),
        ]
    );
    let envelope: Value = serde_json::from_slice(&std::fs::read(
        root.path().join(RUNS_DIR).join(format!("{RUN_ID}.json")),
    )?)?;
    assert_eq!(
        (envelope["kind"].as_str(), envelope["finished_at"].is_null()),
        (Some("robots_refresh"), false)
    );
    // The candidate pins nothing, so it has no answer to the pin question.
    assert_eq!(
        answer.report["rows"][1]["matches_pin"],
        Value::Null,
        "{:#}",
        answer.report
    );
    Ok(())
}

/// Section 11.1: a 404 is "no robots", recorded and not digest-pinned. Mutation 2026-11-15, 404
/// mapped to a failure: red.
#[test]
fn a_robots_404_is_recorded_as_absent_and_pins_no_digest() -> TestResult {
    let root = TempDir::new()?;
    let exchange = Robots::of(&[(GNW_ROBOTS, NOT_FOUND, b"not here")]);
    let answer = refreshed(root.path(), gnw_hosts(), &exchange)?;
    let row = first_row(&answer);
    assert_eq!(
        (
            robots_rows(root.path())?,
            payload_rows(root.path())?,
            attempts(root.path())?,
            row["answer"].as_str(),
            row["fetched_sha256"].clone(),
        ),
        (
            vec![(GNW_HOST.to_owned(), "absent".to_owned(), None)],
            vec![(
                GNW_ROBOTS.to_owned(),
                FileDigest::of(b"not here").hex().to_owned(),
                false,
                "[]".to_owned()
            )],
            vec![(
                ROBOTS_SOURCE.to_owned(),
                GNW_HOST.to_owned(),
                "empty".to_owned()
            )],
            Some("absent"),
            Value::Null,
        )
    );
    Ok(())
}

/// Section 11.1: bot protection is permanent and a 4xx is not "allow all"; a rate limit, a 5xx
/// and a lost connection are transient. None files a robots row. Mutations 2026-11-15, 403 mapped
/// to absent / 429 mapped to a permanent failure: each red.
#[test]
fn a_robots_403_5xx_or_transport_error_is_a_failed_attempt_with_no_robots_row() -> TestResult {
    let mut seen = Vec::new();
    for (answers, exit) in [
        (vec![(GNW_ROBOTS, FORBIDDEN, &b"Access Denied"[..])], 8),
        (vec![(GNW_ROBOTS, UNAVAILABLE, &b""[..])], 7),
        (vec![(GNW_ROBOTS, TOO_MANY_REQUESTS, &b""[..])], 7),
        (Vec::new(), 7),
    ] {
        let root = TempDir::new()?;
        let answer = refreshed(root.path(), gnw_hosts(), &Robots::of(&answers))?;
        seen.push((
            robots_rows(root.path())?.len(),
            payload_rows(root.path())?.len(),
            attempts(root.path())?,
            first_row(&answer)["answer"].as_str().map(str::to_owned),
            answer.exit.code(),
            answer.exit.carries_the_result_on_stdout(),
        ));
        assert_eq!(answer.exit.code(), exit);
    }
    let failed = (
        0,
        vec![(
            ROBOTS_SOURCE.to_owned(),
            GNW_HOST.to_owned(),
            "failed".to_owned(),
        )],
        Some("failed".to_owned()),
    );
    assert_eq!(
        seen,
        vec![
            (failed.0, 1, failed.1.clone(), failed.2.clone(), 8, true),
            (failed.0, 1, failed.1.clone(), failed.2.clone(), 7, false),
            (failed.0, 1, failed.1.clone(), failed.2.clone(), 7, false),
            (failed.0, 0, failed.1, failed.2, 7, false),
        ]
    );
    Ok(())
}

/// Section 10, robots 408: the owner's 408 rule names the providers only, so robots keeps it
/// permanent. Mutation 2026-11-15, 408 mapped to a transient failure in `verdict_of`: red.
#[test]
fn a_robots_408_stays_a_permanent_failure_with_no_robots_row() -> TestResult {
    let root = TempDir::new()?;
    let exchange = Robots::of(&[(GNW_ROBOTS, REQUEST_TIMEOUT, &b""[..])]);
    let answer = refreshed(root.path(), gnw_hosts(), &exchange)?;
    let verdicts: Vec<Option<String>> = read_only(root.path())?
        .prepare("SELECT verdict FROM attempts")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    assert_eq!(
        (
            robots_rows(root.path())?.len(),
            verdicts,
            answer.exit.code()
        ),
        (0, vec![Some("permanent_failure".to_owned())], EXIT_PARTIAL)
    );
    Ok(())
}

/// What one refresh left when GNW's robots url went through the real open-web `Wire` to a loopback
/// server answering `301 Location: /elsewhere`, where `/elsewhere` serves the pinned body: robots
/// rows, (outcome, status, verdict) per attempt, the reported row's answer and status, the exit,
/// the urls sent, the paths the server saw, and the payload rows as (truncated, bytes, headers).
type Refreshed = (
    usize,
    Vec<(String, Option<i64>, Option<String>)>,
    (Option<String>, Option<i64>),
    u8,
    Vec<String>,
    Vec<String>,
    Vec<(bool, i64, String)>,
);

fn refreshed_through_a_redirect(body_over_the_limit: bool) -> Result<Refreshed, Box<dyn Error>> {
    let root = TempDir::new()?;
    let ask = ask(root.path(), gnw_hosts());
    let feed = cmd_robots::open(&ask)?;
    let server = if body_over_the_limit {
        let over = usize::try_from(feed.transport().body_limit)? + 1;
        loopback::Loopback::redirecting_with(ROBOTS_PATH, vec![b'x'; over], gnw_body()?)?
    } else {
        loopback::Loopback::redirecting(ROBOTS_PATH, gnw_body()?)?
    };
    let unanswered = Robots::of(&[]);
    let exchange = loopback::Relay {
        stub: &unanswered,
        wire: cmd_robots::wire_for(&ask, &feed),
        relayed: BTreeMap::from([(GNW_ROBOTS.to_owned(), server.url(ROBOTS_PATH))]),
        asked: RefCell::default(),
    };
    let answer = cmd_robots::run(&ask, &feed, &exchange, &Fixed)?;
    let (sent, hops) = (exchange.sent(), server.seen()?);
    server.finish()?;
    let recorded: Vec<(String, Option<i64>, Option<String>)> = read_only(root.path())?
        .prepare("SELECT outcome, transport_status, verdict FROM attempts")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let linked: Vec<(bool, i64, String)> = read_only(root.path())?
        .prepare(
            "SELECT p.truncated, b.bytes, p.headers FROM payloads p
               JOIN blobs b ON b.digest = p.raw_blob",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let row = first_row(&answer);
    Ok((
        robots_rows(root.path())?.len(),
        recorded,
        (
            row["answer"].as_str().map(str::to_owned),
            row["transport_status"].as_i64(),
        ),
        answer.exit.code(),
        sent,
        hops,
        linked,
    ))
}

/// Both refusals below leave this, beside the one payload row each links: the loopback 301's
/// `Content-Type` is kept, whole body or cut, and its `Location` is not (section 11.2).
fn refused_robots_redirect(truncated: bool, bytes: i64) -> Refreshed {
    (
        0,
        vec![(
            "failed".to_owned(),
            Some(i64::from(MOVED_PERMANENTLY)),
            Some("permanent_failure".to_owned()),
        )],
        (
            Some("failed".to_owned()),
            Some(i64::from(MOVED_PERMANENTLY)),
        ),
        EXIT_PARTIAL,
        vec![GNW_ROBOTS.to_owned()],
        vec![ROBOTS_PATH.to_owned()],
        vec![(
            truncated,
            bytes,
            format!(
                r#"[["location","{}"],["content-type","{}"]]"#,
                loopback::REDIRECT_TARGET,
                loopback::MOVED_CONTENT_TYPE
            ),
        )],
    )
}

/// The loopback 301's body, `b"moved"` in `tests/loopback/mod.rs`.
const MOVED_BODY_BYTES: i64 = 5;
/// open-web's `BODY_LIMIT` (2 MiB, `providers/open-web-feed/src/lib.rs`), private to that crate.
const OPEN_WEB_BODY_LIMIT: i64 = 2 * 1_024 * 1_024;

/// Section 11.2, §11.5 step 5: the refresh's open-web `Wire` refuses a robots.txt 3xx, so the
/// target, which serves the pinned body, is never fetched. Mutations 2026-11-15, open-web following
/// one redirect / a 3xx mapped to `Empty` or `Usable` in `verdict_of`: each red.
#[test]
fn a_robots_redirect_is_a_permanent_failure_with_no_robots_row_and_its_target_never_fetched(
) -> TestResult {
    assert_eq!(
        refreshed_through_a_redirect(false)?,
        refused_robots_redirect(false, MOVED_BODY_BYTES)
    );
    Ok(())
}

/// A robots 301 whose body is over the profile's limit fails as `BodyOverLimit` and keeps the 301
/// on its attempt row and in the report; its body is stored cut at the limit (P10, section 3.9).
#[test]
fn a_robots_redirect_whose_body_is_over_the_limit_keeps_its_status() -> TestResult {
    assert_eq!(
        refreshed_through_a_redirect(true)?,
        refused_robots_redirect(true, OPEN_WEB_BODY_LIMIT)
    );
    Ok(())
}

/// The second refresh's clock: an hour after `NOW`, so its `Wire` owes no wait on `NOW`'s grant.
const AN_HOUR_LATER: &str = "2026-11-15T11:00:00Z";
const LOOPBACK_HOST_PACE_FILE: &str = "127.0.0.1.per-host";
const PACE_DIR: &str = "pace";

struct At(&'static str);

impl Clock for At {
    fn now(&self) -> DateTime<Utc> {
        self.0.parse().unwrap_or(DateTime::UNIX_EPOCH)
    }
}

/// A pace file's name and the instant it holds.
type Grant = (String, DateTime<Utc>);

/// Every `<store>/pace/` file with the instant it holds, sorted by name.
fn pace_grants(store: &Path) -> Result<Vec<Grant>, Box<dyn Error>> {
    let mut grants = Vec::new();
    for entry in std::fs::read_dir(store.join(PACE_DIR))? {
        let path = entry?.path();
        let held = std::fs::read_to_string(&path)?;
        let name = path.file_name().ok_or("no file name")?;
        grants.push((
            name.to_string_lossy().into_owned(),
            DateTime::parse_from_rfc3339(held.trim())?.with_timezone(&Utc),
        ));
    }
    grants.sort();
    Ok(grants)
}

/// Section 8 row R19 holes 11 and 12: a refresh and a collect on one store reserve the same
/// `pace/<host>.per-host` file, and open-web writes no per-IP file (owner decision 1, 2026-11-17).
#[test]
fn a_robots_refresh_and_a_collect_on_one_store_reserve_one_pace_file_per_host() -> TestResult {
    let root = TempDir::new()?;
    let ask = ask(root.path(), gnw_hosts());
    let feed = cmd_robots::open(&ask)?;
    let server = loopback::Loopback::redirecting(REDIRECT_NOWHERE, gnw_body()?)?;
    let relayed = BTreeMap::from([(GNW_ROBOTS.to_owned(), server.url(ROBOTS_PATH))]);
    let unanswered = Robots::of(&[]);
    let by_robots = loopback::Relay {
        stub: &unanswered,
        wire: cmd_robots::wire_for(&ask, &feed),
        relayed: relayed.clone(),
        asked: RefCell::default(),
    };
    cmd_robots::run(&ask, &feed, &by_robots, &Fixed)?;
    let after_refresh = pace_grants(root.path())?;
    let collect_ask = newsvault::cmd_collect::Ask {
        sources: vec![SOURCE.to_owned()],
        window: newsvault::cmd_collect::TODAY.to_owned(),
        schedule: None,
        expected_runs: 1,
        store: root.path().to_path_buf(),
        calendar: None,
        listing: None,
        roster: None,
        hosts: Some(gnw_hosts()),
        rules: ask.rules.clone(),
        lag_policy: None,
        contact: CONTACT.to_owned(),
        at: At(AN_HOUR_LATER).now(),
        deadline: None,
        dry_run: false,
    };
    let opened = newsvault::cmd_collect::open_provider(&collect_ask)?;
    let by_collect = loopback::Relay {
        stub: &unanswered,
        wire: newsvault::cmd_collect::wire_for(&collect_ask, &opened),
        relayed,
        asked: RefCell::default(),
    };
    cmd_robots::run(&ask, &feed, &by_collect, &At(AN_HOUR_LATER))?;
    let after_collect_wire = pace_grants(root.path())?;
    server.finish()?;
    let file = LOOPBACK_HOST_PACE_FILE.to_owned();
    assert_eq!(
        (after_refresh, after_collect_wire),
        (
            vec![(file.clone(), Fixed.now())],
            vec![(file, At(AN_HOUR_LATER).now())]
        )
    );
    Ok(())
}

/// A path the server answers 301 and nothing here asks for, so every other path is a 200.
const REDIRECT_NOWHERE: &str = "/never-asked";

/// Decision D1: a changed robots.txt is "the answer is no", reported with both digests, and the
/// hosts file is left as it was. Mutation 2026-11-15, `matches_pin` always true: red.
#[test]
fn a_fetched_robots_digest_other_than_the_pin_exits_one_and_names_both() -> TestResult {
    let root = TempDir::new()?;
    let before = std::fs::read(gnw_hosts())?;
    let exchange = Robots::of(&[(GNW_ROBOTS, OK, CHANGED_BODY)]);
    let answer = refreshed(root.path(), gnw_hosts(), &exchange)?;
    let row = first_row(&answer);
    let changed = FileDigest::of(CHANGED_BODY);
    assert_eq!(
        (
            answer.exit,
            answer.exit.carries_the_result_on_stdout(),
            row["fetched_sha256"].as_str(),
            row["pinned"].as_str(),
            row["matches_pin"].as_bool(),
            std::fs::read(gnw_hosts())? == before,
        ),
        (
            RobotsExit::PinDiffers,
            true,
            Some(changed.hex()),
            Some(GNW_PIN),
            Some(false),
            true,
        )
    );
    Ok(())
}

/// The committed file's five rows, candidate CNW asked too, each answered with its committed body
/// (the Fed's 404). Mutations 2026-11-16 and 2026-11-17, one character of the PRN, CNW or Yahoo
/// pin changed, the Fed pin a digest: each red (CNW's by `matches_pin`, the others exit 1).
#[test]
fn the_committed_hosts_file_refreshed_with_its_committed_bodies_matches_every_pin() -> TestResult {
    let root = TempDir::new()?;
    let (gnw, prn, cnw, fed) = (gnw_body()?, prn_body()?, cnw_body()?, fed_404_body()?);
    let yahoo = yahoo_body()?;
    let exchange = Robots::of(&[
        (GNW_ROBOTS, OK, &gnw),
        (PRN_ROBOTS, OK, &prn),
        (CNW_ROBOTS, OK, &cnw),
        (FED_ROBOTS, NOT_FOUND, &fed),
        (YAHOO_ROBOTS, OK, &yahoo),
    ]);
    let answer = refreshed(root.path(), committed_hosts(), &exchange)?;
    let matched: Vec<(Option<&str>, Option<bool>, Option<&str>)> = (0..5)
        .map(|nth| {
            let row = &answer.report["rows"][nth];
            (
                row["status"].as_str(),
                row["matches_pin"].as_bool(),
                row["parse_verdict"].as_str(),
            )
        })
        .collect();
    assert_eq!(
        (answer.exit, matched, robots_rows(root.path())?),
        (
            RobotsExit::Ok,
            vec![
                (Some("admitted"), Some(true), Some("rules")),
                (Some("admitted"), Some(true), Some("rules")),
                (Some("candidate"), Some(true), Some("rules")),
                (Some("admitted"), Some(true), Some("no_rules_absent")),
                (Some("admitted"), Some(true), Some("rules")),
            ],
            // `robots_rows` orders by host.
            vec![
                (
                    YAHOO_HOST.to_owned(),
                    "present".to_owned(),
                    Some(YAHOO_PIN.to_owned())
                ),
                (
                    GNW_HOST.to_owned(),
                    "present".to_owned(),
                    Some(GNW_PIN.to_owned())
                ),
                (FED_HOST.to_owned(), "absent".to_owned(), None),
                (
                    CNW_HOST.to_owned(),
                    "present".to_owned(),
                    Some(CNW_PIN.to_owned())
                ),
                (
                    PRN_HOST.to_owned(),
                    "present".to_owned(),
                    Some(PRN_PIN.to_owned())
                ),
            ]
        ),
        "{:#}",
        answer.report
    );
    Ok(())
}

/// Exit 1 is for an admitted row: a candidate is never collected, so its changed robots.txt is
/// reported and answers nothing. Mutation 2026-11-15, `row.admitted` dropped from `exit_of`: red.
#[test]
fn a_candidate_whose_fetch_differs_from_its_pin_exits_zero() -> TestResult {
    let root = TempDir::new()?;
    let hosts = root.path().join("hosts.json");
    let file = serde_json::json!({
        "schema": "open-web-feed.hosts/2",
        "hosts": [
            {"host": GNW_HOST, "status": "admitted", "index_url": "https://rss.globenewswire.com/rssfeed/",
             "rule": "gnw", "robots": GNW_PIN},
            {"host": "www.prnewswire.com", "status": "candidate",
             "index_url": "https://www.prnewswire.com/rss/news-releases-list.rss", "rule": "gnw",
             "robots": FileDigest::of(CHANGED_BODY).hex()},
        ],
    });
    std::fs::write(&hosts, serde_json::to_vec(&file)?)?;
    let body = gnw_body()?;
    let exchange = Robots::of(&[(GNW_ROBOTS, OK, &body), (PRN_ROBOTS, OK, PRN_BODY)]);
    let answer = refreshed(&root.path().join("vault"), hosts, &exchange)?;
    assert_eq!(
        (
            answer.exit,
            answer.report["rows"][1]["matches_pin"].as_bool()
        ),
        (RobotsExit::Ok, Some(false))
    );
    Ok(())
}

/// Section 5.1's iff rule for a refresh: the report on stdout for exits 0, 1 and 8, an error object
/// on stderr for 7. Mutation 2026-11-15, `Answer::emitted` always choosing stdout: red.
#[test]
fn a_refresh_writes_its_report_on_stdout_for_exits_zero_one_and_eight_only() -> TestResult {
    let body = gnw_body()?;
    let mut seen = Vec::new();
    for (status, answered) in [
        (OK, body.as_slice()),
        (OK, CHANGED_BODY),
        (FORBIDDEN, &b"Access Denied"[..]),
        (UNAVAILABLE, &b""[..]),
    ] {
        let root = TempDir::new()?;
        let exchange = Robots::of(&[(GNW_ROBOTS, status, answered)]);
        let answer = refreshed(root.path(), gnw_hosts(), &exchange)?;
        seen.push(match answer.emitted(vec![RUN_ID.to_owned()]) {
            Emitted::Stdout(report) => (answer.exit.code(), "stdout", report["schema"].clone()),
            Emitted::Stderr(error) => (answer.exit.code(), "stderr", error["code"].clone()),
        });
    }
    let report = Value::from(cmd_robots::SCHEMA);
    assert_eq!(
        seen,
        vec![
            (0, "stdout", report.clone()),
            (1, "stdout", report.clone()),
            (8, "stdout", report),
            (7, "stderr", Value::from("vendor_unreachable")),
        ]
    );
    Ok(())
}

/// Major 3 since the owner decision of 2026-11-17 removed `refused_not_utf8`, a value major 2
/// released (architecture.md section 10, `parse_verdict` row).
#[test]
fn a_refresh_report_is_robots_schema_major_three() -> TestResult {
    let root = TempDir::new()?;
    let body = gnw_body()?;
    let exchange = Robots::of(&[(GNW_ROBOTS, OK, body.as_slice())]);
    let answer = refreshed(root.path(), gnw_hosts(), &exchange)?;
    assert_eq!(answer.report["schema"], "newsvault.robots/3");
    Ok(())
}

/// The binary's failures before any fetch: a source with no hosts file is a `usage error:` exiting
/// 2, an absent hosts file exits 4, stdout empty in both, and each exit is one the schema lists.
/// Mutations 2026-11-15, `NoHostsFile` not a usage error / `robots refresh`'s absent-input arm in
/// `exit_code` mapped to 2 / exit 4 dropped from the schema: each red.
#[test]
fn a_refresh_refused_before_any_fetch_exits_two_or_four_with_an_empty_stdout() -> TestResult {
    let root = TempDir::new()?;
    let rules = provider_dir().join("rules/open-web-feed.rules.json");
    let binary = |source: &str, hosts: &Path| {
        Command::new(env!("CARGO_BIN_EXE_newsvault"))
            .args(["robots", "refresh", "--source", source, "--dry-run"])
            .arg("--hosts")
            .arg(hosts)
            .arg("--rules")
            .arg(&rules)
            .arg("--store")
            .arg(root.path())
            .env("BELLWETHER_FEED_CONTACT", CONTACT)
            .stdin(Stdio::null())
            .output()
    };
    let schema: Value = serde_json::from_slice(
        &Command::new(env!("CARGO_BIN_EXE_newsvault"))
            .args(["schema", "--format", "json"])
            .output()?
            .stdout,
    )?;
    let exits = schema["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|command| command["path"] == serde_json::json!(["robots", "refresh"]))
        .map(|command| command["exits"].clone())
        .unwrap_or_default();
    let mut seen = Vec::new();
    for output in [
        binary("edgar_press", &gnw_hosts())?,
        binary(SOURCE, &root.path().join("absent.json"))?,
    ] {
        let code = output.status.code();
        let stderr = String::from_utf8_lossy(&output.stderr);
        seen.push((
            code,
            output.stdout.len(),
            stderr.split(':').next().map(str::to_owned),
            code.is_some_and(|code| exits.get(code.to_string()).is_some()),
        ));
    }
    assert_eq!(
        seen,
        vec![
            (Some(EXIT_USAGE), 0, Some("usage error".to_owned()), true),
            (Some(EXIT_NOT_FOUND), 0, Some("error".to_owned()), true),
        ]
    );
    Ok(())
}

/// The library dry run and the binary's: one url per row, nothing written, nothing sent, no
/// `parse_verdict`. Mutation 2026-11-16, the dry run's verdict set to `rules`: red.
#[test]
fn a_robots_refresh_dry_run_plans_one_url_per_row_and_writes_nothing() -> TestResult {
    let root = TempDir::new()?;
    let store = root.path().join("vault");
    let exchange = Robots::of(&[]);
    let ask = Ask {
        dry_run: true,
        ..ask(&store, two_row_hosts())
    };
    let feed = cmd_robots::open(&ask)?;
    let answer = cmd_robots::run(&ask, &feed, &exchange, &Fixed)?;
    let urls = |report: &Value| -> Vec<String> {
        report["rows"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| row["url"].as_str().map(str::to_owned))
            .collect()
    };
    let output = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .args(["robots", "refresh", "--source", SOURCE, "--dry-run"])
        .args(["--format", "json", "--non-interactive"])
        .arg("--hosts")
        .arg(two_row_hosts())
        .arg("--rules")
        .arg(&ask.rules)
        .arg("--store")
        .arg(&store)
        .env("BELLWETHER_FEED_CONTACT", CONTACT)
        .stdin(Stdio::null())
        .output()?;
    let printed: Value = serde_json::from_slice(&output.stdout)?;
    let planned = vec![GNW_ROBOTS.to_owned(), PRN_ROBOTS.to_owned()];
    let verdicts = |report: &Value| -> Vec<Value> {
        report["rows"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|row| row["parse_verdict"].clone())
            .collect()
    };
    assert_eq!(
        (verdicts(&answer.report), verdicts(&printed)),
        (vec![Value::Null; 2], vec![Value::Null; 2])
    );
    assert_eq!(
        (
            answer.exit,
            urls(&answer.report),
            exchange.asked.borrow().len(),
            output.status.code(),
            urls(&printed),
            store.exists(),
        ),
        (
            RobotsExit::Ok,
            planned.clone(),
            0,
            Some(EXIT_OK),
            planned,
            false
        )
    );
    Ok(())
}

/// The refresh unit passes the contact only as a file: a dry run with only the `_FILE` twin set
/// reads it and prints the address on neither stream.
#[test]
fn a_robots_refresh_reads_its_contact_from_the_file_variable() -> TestResult {
    let root = TempDir::new()?;
    let file = root.path().join("feed-contact");
    std::fs::write(&file, format!("{CONTACT}\n"))?;
    let output = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .args(["robots", "refresh", "--source", SOURCE, "--dry-run"])
        .args(["--format", "json", "--non-interactive"])
        .arg("--hosts")
        .arg(two_row_hosts())
        .arg("--rules")
        .arg(provider_dir().join("rules/open-web-feed.rules.json"))
        .arg("--store")
        .arg(root.path().join("vault"))
        .env_remove("BELLWETHER_FEED_CONTACT")
        .env("BELLWETHER_FEED_CONTACT_FILE", &file)
        .stdin(Stdio::null())
        .output()?;
    let printed = [&output.stdout, &output.stderr]
        .map(|stream| String::from_utf8_lossy(stream).contains(CONTACT_ADDRESS));
    assert_eq!(
        (output.status.code(), printed),
        (Some(EXIT_OK), [false, false])
    );
    Ok(())
}

/// Every path under `root`, relative, so a refusal can be shown to have written nothing.
fn paths(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            found.extend(path.strip_prefix(root).ok().map(Path::to_path_buf));
        }
    }
    found.sort();
    found
}

/// §3.1: a store of another shape is refused before the envelope, the lease and any send, and a
/// dry run says so too. Mutations 2026-11-15, the `refuse_an_unreadable_version` call removed /
/// placed after the dry-run return again: each red.
#[test]
fn a_robots_refresh_refuses_a_store_of_another_shape_and_leaves_it_as_found() -> TestResult {
    let mut seen = Vec::new();
    for dry_run in [false, true] {
        let root = TempDir::new()?;
        let db = Connection::open(root.path().join(COMMIT_LOG))?;
        db.execute_batch("CREATE TABLE attempts (attempt_id TEXT PRIMARY KEY)")?;
        drop(db);
        let before = paths(root.path());
        let exchange = Robots::of(&[]);
        let ask = Ask {
            dry_run,
            ..ask(root.path(), gnw_hosts())
        };
        let feed = cmd_robots::open(&ask)?;
        let refused = cmd_robots::run(&ask, &feed, &exchange, &Fixed);
        seen.push((
            matches!(
                refused,
                Err(RobotsError::Store(StoreError::ShapeDiffers {
                    found: 0,
                    ..
                }))
            ),
            paths(root.path()) == before,
            exchange.asked.borrow().len(),
        ));
    }
    assert_eq!(seen, vec![(true, true, 0); 2]);
    Ok(())
}

/// Section 10: a robots run is not a collect run, so it fills no expected run. Mutation
/// 2026-11-15, `day_row` counting every envelope: red.
#[test]
fn a_robots_refresh_run_is_not_counted_against_expected_runs() -> TestResult {
    let root = TempDir::new()?;
    let body = gnw_body()?;
    refreshed(
        root.path(),
        gnw_hosts(),
        &Robots::of(&[(GNW_ROBOTS, OK, &body)]),
    )?;
    let report = ledger(root.path(), Some(1))?;
    let day = report.days.first().ok_or("one day")?;
    assert_eq!(
        (day.runs_observed, finding_codes(&report)),
        (0, vec![RUNS_BELOW_EXPECTED])
    );
    Ok(())
}

/// Every kind of envelope names a known run, so a robots attempt is no orphan and the report is
/// complete. Mutation 2026-11-15, `known_runs` built from `collect` envelopes only: red.
#[test]
fn a_robots_refresh_runs_attempts_are_not_orphans() -> TestResult {
    let root = TempDir::new()?;
    let body = gnw_body()?;
    refreshed(
        root.path(),
        gnw_hosts(),
        &Robots::of(&[(GNW_ROBOTS, OK, &body)]),
    )?;
    let report = ledger(root.path(), None)?;
    assert_eq!(
        (
            report.attempts_without_a_run_envelope.len(),
            report.is_complete()
        ),
        (0, true)
    );
    Ok(())
}

/// A robots fetch asks a host, not a source: it moves no asking clock and a failed one is no
/// `holds_a_failure`. Its signal is the refresh's own exit. Mutation 2026-11-15, the asking
/// clock's `source <> 'robots'` removed: red.
#[test]
fn a_robots_fetch_moves_neither_the_asking_clock_nor_a_ledger_check_finding() -> TestResult {
    let root = TempDir::new()?;
    let exchange = Robots::of(&[(GNW_ROBOTS, FORBIDDEN, b"Access Denied")]);
    refreshed(root.path(), gnw_hosts(), &exchange)?;
    let report = ledger(root.path(), None)?;
    assert_eq!(
        (
            report.freshness.asking_clock.newest.clone(),
            finding_codes(&report).contains(&HOLDS_A_FAILURE),
            report
                .days
                .iter()
                .map(|day| day.per_source.len())
                .sum::<usize>(),
        ),
        (None, false, 0)
    );
    Ok(())
}

/// `ROBOTS_SOURCE` is kept out of the ledger by name, so a source of that name would vanish.
#[test]
fn no_registered_source_is_named_robots() {
    assert!(Registered::ALL
        .iter()
        .all(|entry| entry.source() != ROBOTS_SOURCE));
}

/// The hand-written manifest against the parser it describes, as the collect test does.
#[test]
fn every_option_the_robots_refresh_parser_takes_is_in_the_machine_readable_schema() -> TestResult {
    let binary = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_newsvault"))
            .args(args)
            .output()
    };
    let help =
        String::from_utf8_lossy(&binary(&["robots", "refresh", "--help"])?.stdout).to_string();
    let schema: Value = serde_json::from_slice(&binary(&["schema", "--format", "json"])?.stdout)?;
    let named: Vec<String> = schema["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|command| command["path"] == serde_json::json!(["robots", "refresh"]))
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
    assert!(
        !named.is_empty() && missing.is_empty(),
        "{named:?} {missing:?}"
    );
    Ok(())
}

fn robots_fixture(name: &str) -> Result<Vec<u8>, std::io::Error> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/robots")
            .join(name),
    )
}

/// Owner decision 3d (2026-11-16): each answered row carries the verdict `collect` would reach
/// over the same body, in the JSON row and on the human line, and the schema lists exactly these
/// values; a failed fetch carries none. Mutations 2026-11-16, the Crawl-delay check dropped from
/// `parse_verdict_of`: red; `parse_verdict` dropped from `render_human`: red; the check written
/// as `>=` in `parse_verdict_of`: red (the 7 s delay equal to the 7 s interval, 2026-11-19).
#[test]
fn a_refresh_reports_the_parse_verdict_of_each_answered_row() -> TestResult {
    let gnw = gnw_body()?;
    let answers: Vec<(u16, Vec<u8>)> = vec![
        (OK, PRN_BODY.to_vec()),
        (OK, b"# nothing\n\n".to_vec()),
        (
            OK,
            b"Crawl-delay: 30\nUser-agent: *\nDisallow: /x\n".to_vec(),
        ),
        (
            OK,
            b"Disallow: /o\nUser-agent: *\nCrawl-delay: 30\n".to_vec(),
        ),
        (OK, robots_fixture("synthetic_html_page.txt")?),
        (OK, robots_fixture("www.nasdaq.com.txt")?),
        (OK, b"User-agent: *\nCrawl-delay: 7\n".to_vec()),
        (OK, b"User-agent: *\nCrawl-delay: soon\n".to_vec()),
        (OK, b"Sitemap: https://www.prnewswire.com/s.xml\n".to_vec()),
        (OK, b"User-agent: *\nDisallow: /\xe4\n".to_vec()),
        (NOT_FOUND, b"not here".to_vec()),
        (UNAVAILABLE, Vec::new()),
    ];
    let mut verdicts = Vec::new();
    let mut shown = Vec::new();
    for (status, body) in &answers {
        let root = TempDir::new()?;
        let exchange = Robots::of(&[(GNW_ROBOTS, OK, &gnw), (PRN_ROBOTS, *status, body)]);
        let answer = refreshed(root.path(), two_row_hosts(), &exchange)?;
        let rows = &answer.report["rows"];
        verdicts.push((
            rows[0]["parse_verdict"].clone(),
            rows[1]["parse_verdict"].clone(),
        ));
        let mut human = Vec::new();
        cmd_robots::render_human(&answer.report, &mut human)?;
        let line = String::from_utf8(human)?
            .lines()
            .find(|line| line.trim_start().starts_with(PRN_HOST))
            .and_then(|line| line.split_whitespace().last())
            .map(str::to_owned);
        shown.push(line.unwrap_or_default());
    }
    let expected = [
        "rules",
        "no_rules_empty",
        "orphan_rules_ignored",
        "crawl_delay_over_interval",
        "refused_html",
        "crawl_delay_over_interval",
        "rules",
        "refused_crawl_delay",
        "no_rules_no_group",
        "rules",
        "no_rules_absent",
    ];
    let mut wanted: Vec<(Value, Value)> = expected
        .iter()
        .map(|verdict| (Value::from("rules"), Value::from(*verdict)))
        .collect();
    wanted.push((Value::from("rules"), Value::Null));
    assert_eq!(verdicts, wanted);
    assert_eq!(
        shown,
        expected.iter().copied().chain(["none"]).collect::<Vec<_>>()
    );
    let schema: Value = serde_json::from_slice(
        &Command::new(env!("CARGO_BIN_EXE_newsvault"))
            .args(["schema", "--format", "json"])
            .output()?
            .stdout,
    )?;
    let listed: BTreeSet<String> = schema["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|command| command["path"] == serde_json::json!(["robots", "refresh"]))
        .and_then(|command| command["output_fields"][0]["values"].as_array().cloned())
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        listed,
        expected
            .iter()
            .map(|verdict| (*verdict).to_owned())
            .collect()
    );
    Ok(())
}
