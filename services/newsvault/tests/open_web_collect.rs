//! `newsvault collect --source open_web_wires` from plan to committed items, over the GNW fixtures
//! in `providers/open-web-feed/tests/fixtures/gnw` (architecture.md section 11.5 step 2).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use news_provider::{
    Clock, Exchange, Exit, PlanRefusal, RawResponse, Request, SendFailure, SendPermit,
    TransportError,
};
use newsvault::cmd_collect::{self, Ask, CollectRunError};
use newsvault::cmd_items::{self, PerStory};
use newsvault::store::COMMIT_LOG;
use open_web_feed::OpenWebFeed;
use pit::ObservationTime;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tempfile::TempDir;

mod seed;

type TestResult = Result<(), Box<dyn Error>>;

const SOURCE: &str = "open_web_wires";
const CONTACT: &str = "bellwether collector contact@example.test";
/// Saturday 2026-11-14, thirty seconds into the newest fixture pubDate's minute (19:00 GMT): late
/// enough that no row is stored Undated, early enough that `items` can see that minute still open.
const RUN_AT: &str = "2026-11-14T19:00:30Z";
const YESTERDAY: &str = "2026-11-13";
/// Inside the 19:00 minute and past the run's observation, then the instant that minute closes.
const BEFORE_THE_MINUTE_CLOSED: &str = "2026-11-14T19:00:45Z";
const AFTER_THE_MINUTE_CLOSED: &str = "2026-11-14T19:01:00Z";
const INDEX: &str = "https://rss.globenewswire.com/rssfeed/";
const OK: u16 = 200;
const SENT_BEFORE_THE_CLOCK: i64 = 2;
const RECEIVED_BEFORE_THE_CLOCK: i64 = 1;
/// The stub's `received_at` for every page: `RUN_AT` less `RECEIVED_BEFORE_THE_CLOCK`.
const RECEIVED_AT: &str = "2026-11-14T19:00:29.000000Z";
const EXIT_USAGE: i32 = 2;
const EXIT_NOT_FOUND: i32 = 4;
const NOT_FOUND: u16 = 404;
const FORBIDDEN: u16 = 403;
const UNAVAILABLE: u16 = 503;
/// Two robots fetches before `RUN_AT`, the second the newer.
const ROBOTS_EARLIER: &str = "2026-11-14T17:00:00Z";
const ROBOTS_LATER: &str = "2026-11-14T18:00:00Z";
/// The committed hosts file's GNW pin, `robots_rss_globenewswire_com.txt`'s sha256.
const GNW_PIN: &str = "eb8bb698171dd56c139c224c9217070adf9947f6ed6cd02336eba55aa4155bfe";
const CHANGED_ROBOTS: &[u8] = b"User-agent: *\nDisallow: /\n";

/// The three `rssfeed_3_items.xml` links after the www -> rss rewrite, the page each is answered
/// with, its pubDate text, and the canonical link that page names (`None`: not committed).
/// Only 3403075 was captured; the other two are answered with the synthetic pages.
const RELEASES: [(&str, &str, &str, Option<&str>); 3] = [
    (
        "rss.globenewswire.com/news-release/2026/11/14/3403075/0/en/usta-florida-releases-2026-state-of-florida-tennis-report.html",
        "article_3403075.html",
        "Sat, 14 Nov 2026 19:00 GMT",
        Some("https://rss.globenewswire.com/news-release/2026/11/14/3403075/0/en/usta-florida-releases-2026-state-of-florida-tennis-report.html"),
    ),
    (
        "rss.globenewswire.com/news-release/2026/11/14/3403074/673/en/frvo-investor-news-if-you-have-suffered-losses-in-fervo-energy-company-you-are-encouraged-to-contact-the-rosen-law-firm-about-your-rights.html",
        "synthetic_canonical_differs.html",
        "Sat, 14 Nov 2026 18:45 GMT",
        Some("https://www.globenewswire.com/en/news-release/2026/11/14/9999999/0/en/a-renamed-slug.html"),
    ),
    (
        "rss.globenewswire.com/news-release/2026/11/14/3403073/0/en/hitn-media-honors-puerto-rican-trailblazers-at-centro.html",
        "synthetic_no_article_container.html",
        "Sat, 14 Nov 2026 18:38 GMT",
        None,
    ),
];

fn provider_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../providers/open-web-feed")
}

fn hosts() -> PathBuf {
    provider_dir().join("hosts/open_web_wires.hosts.json")
}

fn hosts_fixture(name: &str) -> PathBuf {
    provider_dir().join("tests/fixtures/hosts").join(name)
}

/// The committed GNW row's robots fetch at `at`, answered with the committed body.
fn seed_gnw(store: &Path, hosts: &Path, at: &str) -> TestResult {
    seed::refresh(store, hosts, at, (OK, &seed::gnw_robots()?))?;
    Ok(())
}

fn instant(text: &str) -> DateTime<Utc> {
    text.parse().unwrap_or(DateTime::UNIX_EPOCH)
}

fn ask(store: &Path, window: &str) -> Ask {
    Ask {
        sources: vec![SOURCE.to_owned()],
        window: window.to_owned(),
        schedule: None,
        expected_runs: 1,
        store: store.to_path_buf(),
        calendar: None,
        listing: None,
        roster: None,
        hosts: Some(hosts()),
        rules: provider_dir().join("rules/open-web-feed.rules.json"),
        lag_policy: None,
        contact: CONTACT.to_owned(),
        at: instant(RUN_AT),
        dry_run: false,
    }
}

struct Fixed;

impl Clock for Fixed {
    fn now(&self) -> DateTime<Utc> {
        instant(RUN_AT)
    }
}

/// Answers each url out of a committed fixture, sent two seconds and received one second before
/// the clock's instant, so the three instants differ and the stored observation names one of them.
struct Gnw {
    pages: BTreeMap<String, Vec<u8>>,
    asked: RefCell<Vec<String>>,
}

impl Gnw {
    fn day() -> Result<Self, Box<dyn Error>> {
        let fixture =
            |name: &str| std::fs::read(provider_dir().join("tests/fixtures/gnw").join(name));
        let mut pages = BTreeMap::from([(INDEX.to_owned(), fixture("rssfeed_3_items.xml")?)]);
        for (key, page, _, _) in RELEASES {
            pages.insert(format!("https://{key}"), fixture(page)?);
        }
        Ok(Self {
            pages,
            asked: RefCell::new(Vec::new()),
        })
    }
}

impl Exchange for Gnw {
    fn send(
        &self,
        permit: SendPermit,
        request: &Request,
        clock: &dyn Clock,
    ) -> Result<RawResponse, SendFailure> {
        self.asked.borrow_mut().push(request.url.clone());
        let attempt = permit.attempt_id().clone();
        let at = ObservationTime::new(clock.now());
        let sent = ObservationTime::new(clock.now() - TimeDelta::seconds(SENT_BEFORE_THE_CLOCK));
        let received =
            ObservationTime::new(clock.now() - TimeDelta::seconds(RECEIVED_BEFORE_THE_CLOCK));
        match self.pages.get(&request.url) {
            Some(body) => Ok(RawResponse::new(attempt, OK, body.clone(), sent, received)),
            None => Err(SendFailure::Transport(TransportError::new(
                attempt,
                at,
                at,
                format!("no fixture answers {}", request.url),
            ))),
        }
    }
}

fn digest(text: Result<canon::Digest, canon::CanonError>) -> Option<String> {
    text.ok().map(|found| found.hex().to_owned())
}

/// Every column the design names for a committed open-web row, keyed by story.
type Row = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
);

fn stored_items(db: &Connection) -> Result<BTreeMap<String, Row>, Box<dyn Error>> {
    let mut query = db.prepare(
        "SELECT story_id, doc_id, valid_minute, valid_at, valid_raw, valid_field, valid_source,
                observed_at FROM items",
    )?;
    let rows = query
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ),
            ))
        })?
        .collect::<Result<BTreeMap<String, Row>, _>>()?;
    Ok(rows)
}

fn visible(store: &Path, at: &str) -> Result<usize, Box<dyn Error>> {
    let report = cmd_items::read(
        store,
        &cmd_items::Ask {
            at: instant(at),
            from: None,
            to: None,
            source: None,
            limit: None,
            fields: None,
            per_story: PerStory::All,
        },
    )?;
    Ok(report.returned)
}

/// Mutations 2026-11-15: `rows_of` writing NULL for `valid_raw`, and `document_of` labelling the
/// minute `Published::At` (an instant), each turn this red.
#[test]
fn an_open_web_run_collects_a_globenewswire_day_end_to_end() -> TestResult {
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
    let ask = ask(root.path(), cmd_collect::TODAY);
    let opened = cmd_collect::open_provider(&ask)?;
    let exchange = Gnw::day()?;
    let answer = cmd_collect::run(&ask, &opened, &exchange, &Fixed)?;
    let envelope = &answer.envelope;
    let attempts = envelope["attempts"].as_array().ok_or("attempts")?;
    let total = |field: &str| -> u64 { attempts.iter().filter_map(|a| a[field].as_u64()).sum() };
    let hosts_version = canon::FileDigest::of(&std::fs::read(hosts())?);
    assert_eq!(
        (
            answer.exit,
            envelope["window"].as_str(),
            envelope["calendar_version"].is_null(),
            envelope["hosts_version"].as_str(),
            attempts.len(),
            envelope["provenance_counts"]["live_ingest"].as_u64(),
        ),
        (
            Exit::Ok,
            Some("2026-11-14"),
            true,
            Some(hosts_version.hex()),
            1 + RELEASES.len(),
            Some(1 + RELEASES.len() as u64),
        ),
        "{envelope:#}"
    );
    // The index's 3 vendor rows are 3 follow-ups and 0 filtered; each page is one vendor row.
    assert_eq!(
        (
            attempts[0]["vendor_rows"].as_u64(),
            attempts[0]["filtered_out"].as_u64(),
            total("vendor_rows"),
            total("unusable_rows") + total("items_committed"),
            total("unusable_rows"),
        ),
        (Some(3), Some(0), 6, 3, 1)
    );
    let db = Connection::open_with_flags(
        root.path().join(COMMIT_LOG),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut versions = db.prepare("SELECT hosts_version FROM attempts WHERE source = ?1")?;
    let versions = versions
        .query_map([SOURCE], |row| row.get::<_, Option<String>>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(versions, vec![Some(hosts_version.hex().to_owned()); 4]);
    let expected: BTreeMap<String, Row> = RELEASES
        .iter()
        .filter_map(|(key, _, raw, canonical)| {
            let minute = format!(
                "{}:00.000000Z",
                chrono::NaiveDateTime::parse_from_str(raw, "%a, %d %b %Y %H:%M GMT")
                    .ok()?
                    .format("%Y-%m-%dT%H:%M")
            );
            Some((
                digest(canon::digest::story_id(SOURCE, key))?,
                (
                    digest(canon::digest::doc_id((*canonical)?)),
                    Some(minute),
                    None,
                    Some((*raw).to_owned()),
                    Some("pubDate".to_owned()),
                    Some("vendor_published".to_owned()),
                    RECEIVED_AT.to_owned(),
                ),
            ))
        })
        .collect();
    assert_eq!(stored_items(&db)?, expected);
    // Mutation 2026-11-15: `collect` observing at `requested_at` instead of `received_at` was
    // green before the stub told the two apart.
    let mut clocks = db.prepare(
        "SELECT i.observed_at, a.requested_at, a.received_at
           FROM items i JOIN attempts a ON a.attempt_id = i.attempt_id",
    )?;
    let clocks = clocks
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)? == row.get::<_, String>(2)?,
                row.get::<_, String>(0)? == row.get::<_, String>(1)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(clocks, vec![(true, false); expected.len()]);
    // The 19:00 minute is still open at 19:00:45: only the 18:45 release is visible.
    assert_eq!(
        (
            visible(root.path(), BEFORE_THE_MINUTE_CLOSED)?,
            visible(root.path(), AFTER_THE_MINUTE_CLOSED)?
        ),
        (1, 2)
    );
    Ok(())
}

/// Section 5.1 `invalid_argument`: the driven path refuses the window before `commit.sqlite3`, the
/// lock or a run envelope exist. Mutation 2026-11-15, `refuse_the_window` removed: red.
#[test]
fn a_driven_collect_refuses_an_unreadable_or_off_day_window_before_writing_anything() -> TestResult
{
    for window in ["garbage", YESTERDAY] {
        let root = TempDir::new()?;
        let ask = ask(root.path(), window);
        let opened = cmd_collect::open_provider(&ask)?;
        let exchange = Gnw::day()?;
        let refused = cmd_collect::run(&ask, &opened, &exchange, &Fixed);
        assert!(
            matches!(refused, Err(CollectRunError::WindowRefused(_))),
            "{window}: {refused:?}"
        );
        let left = [COMMIT_LOG, "LOCK", "runs"].map(|name| root.path().join(name).exists());
        assert_eq!(left, [false; 3], "{window}");
    }
    Ok(())
}

/// Section 8 row R10, the driver half: a window that is not the run's UTC day sends nothing, and
/// the binary exits 2. Mutation 2026-11-15: removing the run-day check in `plan` turns this red.
#[test]
fn an_open_web_run_for_yesterday_sends_nothing() -> TestResult {
    let root = TempDir::new()?;
    let ask = ask(root.path(), YESTERDAY);
    let opened = cmd_collect::open_provider(&ask)?;
    let exchange = Gnw::day()?;
    let refused = cmd_collect::run(&ask, &opened, &exchange, &Fixed);
    assert!(
        matches!(
            refused,
            Err(CollectRunError::WindowRefused(
                PlanRefusal::NotTheRunDay { .. }
            ))
        ),
        "{refused:?}"
    );
    assert_eq!(exchange.asked.borrow().len(), 0);
    // A dry run: the binary owns a real network client, and must not reach it if this goes red.
    let args = [
        "collect",
        "--source",
        SOURCE,
        "--window",
        YESTERDAY,
        "--at",
        RUN_AT,
        "--format",
        "json",
        "--dry-run",
    ];
    let output = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .args(args)
        .arg("--store")
        .arg(root.path().join("dry"))
        .arg("--hosts")
        .arg(hosts())
        .arg("--rules")
        .arg(&ask.rules)
        .env("BELLWETHER_FEED_CONTACT", CONTACT)
        .stdin(Stdio::null())
        .output()?;
    assert_eq!(
        (output.status.code(), output.stdout.len()),
        (Some(EXIT_USAGE), 0)
    );
    Ok(())
}

/// Every file under `root` with its bytes, so a refusal can be shown to have written nothing.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.insert(path.clone(), std::fs::read(&path).unwrap_or_default());
            }
        }
    }
    files
}

/// `collect` over `hosts` into `store`, as a dry run when `dry_run`: the robots check runs in both.
/// An error is the test's own setup failing; the string is `verdict` of what `collect` answered.
fn collected(store: &Path, hosts: PathBuf, dry_run: bool) -> Result<String, Box<dyn Error>> {
    let ask = Ask {
        hosts: Some(hosts),
        dry_run,
        ..ask(store, cmd_collect::TODAY)
    };
    let opened = cmd_collect::open_provider(&ask)?;
    Ok(verdict(&cmd_collect::run(
        &ask,
        &opened,
        &Gnw::day()?,
        &Fixed,
    )))
}

/// What the robots check said: `admitted`, or the refusal's variant with its fetched digest.
fn verdict(answered: &Result<cmd_collect::Answer, CollectRunError>) -> String {
    match answered {
        Ok(_) => "admitted".to_owned(),
        Err(CollectRunError::RobotsUnverified { fetched, .. }) => {
            format!("unverified {}", fetched.as_deref().unwrap_or("none"))
        }
        Err(CollectRunError::RobotsRefused { fetched, .. }) => format!("refused {fetched}"),
        Err(CollectRunError::RobotsUnreadable { why, .. }) => format!("unreadable: {why}"),
        Err(CollectRunError::RobotsDisallowsPath { path, .. }) => format!("disallows {path}"),
        Err(CollectRunError::PaceBelowFloor {
            per_host, floor, ..
        }) => format!("paced {per_host:?} below {floor:?}"),
        Err(other) => format!("another refusal: {other}"),
    }
}

/// §8 row R4(b), section 11.4: no answered robots fetch of an admitted host, in a store with no
/// commit log or one holding only a failed fetch, refuses before the envelope, the lock or a send.
/// Mutation 2026-11-15, the lookup in `refuse_an_unverified_robots` removed: red.
#[test]
fn an_admitted_row_without_a_ledgered_robots_fetch_is_refused() -> TestResult {
    let empty = TempDir::new()?;
    let exchange = Gnw::day()?;
    let ask = ask(empty.path(), cmd_collect::TODAY);
    let opened = cmd_collect::open_provider(&ask)?;
    let refused = cmd_collect::run(&ask, &opened, &exchange, &Fixed);
    assert_eq!(verdict(&refused), "unverified none");
    assert_eq!(exchange.asked.borrow().len(), 0);
    assert!(snapshot(empty.path()).is_empty());
    let failed_only = TempDir::new()?;
    seed::refresh(
        failed_only.path(),
        &hosts(),
        ROBOTS_EARLIER,
        (FORBIDDEN, b"Access Denied"),
    )?;
    let before = snapshot(failed_only.path());
    assert_eq!(
        collected(failed_only.path(), hosts(), false)?,
        "unverified none"
    );
    assert_eq!(snapshot(failed_only.path()), before);
    Ok(())
}

/// Only the NEWEST answered fetch is compared, both ways round. Mutation 2026-11-15, the reader
/// ordered oldest first: red.
#[test]
fn an_admitted_row_whose_newest_robots_fetch_differs_from_its_pin_is_refused() -> TestResult {
    let changed = canon::FileDigest::of(CHANGED_ROBOTS);
    let changed_last = TempDir::new()?;
    seed_gnw(changed_last.path(), &hosts(), ROBOTS_EARLIER)?;
    seed::refresh(
        changed_last.path(),
        &hosts(),
        ROBOTS_LATER,
        (OK, CHANGED_ROBOTS),
    )?;
    let pinned_last = TempDir::new()?;
    seed::refresh(
        pinned_last.path(),
        &hosts(),
        ROBOTS_EARLIER,
        (OK, CHANGED_ROBOTS),
    )?;
    seed_gnw(pinned_last.path(), &hosts(), ROBOTS_LATER)?;
    assert_eq!(
        (
            collected(changed_last.path(), hosts(), true)?,
            collected(pinned_last.path(), hosts(), true)?,
        ),
        (format!("refused {}", changed.hex()), "admitted".to_owned())
    );
    Ok(())
}

/// A pin of `absent` is a 404 the store ledgers, never a host nobody asked. Mutation 2026-11-15,
/// an absent pin accepted without the lookup: red.
#[test]
fn an_admitted_row_pinned_absent_is_admitted_only_by_a_ledgered_404() -> TestResult {
    let pinned_absent = hosts_fixture("robots_pin_absent.json");
    let mut said = Vec::new();
    for answer in [
        None,
        Some((OK, seed::gnw_robots()?)),
        Some((NOT_FOUND, b"not here".to_vec())),
    ] {
        let root = TempDir::new()?;
        if let Some((status, body)) = &answer {
            seed::refresh(root.path(), &pinned_absent, ROBOTS_EARLIER, (*status, body))?;
        }
        said.push(collected(root.path(), pinned_absent.clone(), true)?);
    }
    assert_eq!(
        said,
        vec![
            "unverified none".to_owned(),
            format!("refused {GNW_PIN}"),
            "admitted".to_owned()
        ]
    );
    Ok(())
}

/// A failed fetch files no robots row, so it neither admits nor un-admits a host. Mutation
/// 2026-11-15, the reader left-joining `attempts` to `robots`: red.
#[test]
fn a_failed_robots_fetch_after_a_matching_one_leaves_the_host_admitted() -> TestResult {
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
    seed::refresh(root.path(), &hosts(), ROBOTS_LATER, (UNAVAILABLE, b""))?;
    assert_eq!(collected(root.path(), hosts(), true)?, "admitted");
    Ok(())
}

/// A candidate is never planned, so it needs no fetch: here its own fetch failed in transport.
#[test]
fn a_candidate_row_needs_no_robots_fetch() -> TestResult {
    let root = TempDir::new()?;
    let two_rows = hosts_fixture("one_admitted_one_candidate.json");
    seed_gnw(root.path(), &two_rows, ROBOTS_EARLIER)?;
    assert_eq!(collected(root.path(), two_rows, true)?, "admitted");
    Ok(())
}

/// The pin names bytes the store must still hold, not only a row that once named them. Mutation
/// 2026-11-15, `BlobAbsent` skipped: red.
#[test]
fn a_pinned_robots_blob_missing_on_disk_is_unverified() -> TestResult {
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
    let blob = root
        .path()
        .join("blobs/sha256")
        .join(&GNW_PIN[..2])
        .join(&GNW_PIN[2..4])
        .join(GNW_PIN);
    std::fs::remove_file(blob)?;
    assert_eq!(
        collected(root.path(), hosts(), false)?,
        format!("unverified {GNW_PIN}")
    );
    Ok(())
}

/// A dry run validates everything, robots included, and still writes nothing. Mutation
/// 2026-11-15, the call in `planned` removed: red.
#[test]
fn a_dry_run_refuses_an_unverified_host_too() -> TestResult {
    let root = TempDir::new()?;
    assert_eq!(collected(root.path(), hosts(), true)?, "unverified none");
    assert!(snapshot(root.path()).is_empty());
    Ok(())
}

/// Section 5.1: `robots_unverified` exits 4 and `robots_refused` 2, each a `newsvault.error/1`
/// object on stderr whose suggestion is the refresh as an argv array; stdout stays empty.
/// Mutation 2026-11-15, the refusal written as plain `error:` text: red.
#[test]
fn a_robots_refusal_names_the_refresh_command_as_an_argv_suggestion() -> TestResult {
    let unverified = TempDir::new()?;
    let refused = TempDir::new()?;
    seed::refresh(
        refused.path(),
        &hosts(),
        ROBOTS_EARLIER,
        (OK, CHANGED_ROBOTS),
    )?;
    let rules = provider_dir().join("rules/open-web-feed.rules.json");
    let mut seen = Vec::new();
    for store in [unverified.path(), refused.path()] {
        let output = Command::new(env!("CARGO_BIN_EXE_newsvault"))
            .args([
                "collect",
                "--source",
                SOURCE,
                "--window",
                cmd_collect::TODAY,
            ])
            .args(["--at", RUN_AT, "--format", "json", "--dry-run"])
            .arg("--store")
            .arg(store)
            .arg("--hosts")
            .arg(hosts())
            .arg("--rules")
            .arg(&rules)
            .env("BELLWETHER_FEED_CONTACT", CONTACT)
            .stdin(Stdio::null())
            .output()?;
        let error: Value = serde_json::from_slice(&output.stderr)?;
        let shown = |path: &Path| path.display().to_string();
        let refresh = [
            "newsvault",
            "robots",
            "refresh",
            "--source",
            SOURCE,
            "--hosts",
            &shown(&hosts()),
            "--rules",
            &shown(&rules),
            "--store",
            &shown(store),
        ]
        .map(Value::from);
        assert_eq!(
            error["suggestions"],
            Value::from(vec![Value::from(refresh.to_vec())])
        );
        seen.push((
            output.status.code(),
            output.stdout.len(),
            error["schema"].as_str().map(str::to_owned),
            error["code"].as_str().map(str::to_owned),
            error["details"]["host"].as_str().map(str::to_owned),
        ));
    }
    let gnw = Some("rss.globenewswire.com".to_owned());
    let schema = Some("newsvault.error/1".to_owned());
    assert_eq!(
        seen,
        vec![
            (
                Some(EXIT_NOT_FOUND),
                0,
                schema.clone(),
                Some("robots_unverified".to_owned()),
                gnw.clone()
            ),
            (
                Some(EXIT_USAGE),
                0,
                schema,
                Some("robots_refused".to_owned()),
                gnw
            ),
        ]
    );
    Ok(())
}

/// A hosts file in `dir` whose one GNW row pins `body`, and a store in `dir` whose newest robots
/// fetch of GNW answered `body`: what `collect` then says.
fn collected_under_robots(
    dir: &Path,
    body: &[u8],
    dry_run: bool,
) -> Result<String, Box<dyn Error>> {
    let (store, hosts) = pinned_to(dir, body)?;
    collected(&store, hosts, dry_run)
}

/// The store and the hosts file of `collected_under_robots`, before any collect.
fn pinned_to(dir: &Path, body: &[u8]) -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
    let hosts = dir.join("hosts.json");
    let store = dir.join("store");
    let pin = canon::FileDigest::of(body);
    let file = serde_json::json!({
        "schema": "open-web-feed.hosts/2",
        "hosts": [{"host": "rss.globenewswire.com", "status": "admitted", "index_url": INDEX,
                   "rule": "gnw", "robots": pin.hex()}],
    });
    std::fs::write(&hosts, serde_json::to_vec(&file)?)?;
    seed::refresh(&store, &hosts, ROBOTS_EARLIER, (OK, body))?;
    Ok((store, hosts))
}

fn robots_fixture(name: &str) -> Result<Vec<u8>, std::io::Error> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/robots")
            .join(name),
    )
}

/// Section 11.2: the index path with its query and the rule's `path_prefix` must both be allowed.
/// A narrower Disallow under the prefix passes: documents are not checked per request. Mutation
/// 2026-11-15, the path check removed: red.
#[test]
fn an_admitted_row_whose_index_or_path_prefix_robots_disallows_is_refused() -> TestResult {
    let mut said = Vec::new();
    for body in [
        "User-agent: *\nDisallow: /rssfeed/\n",
        "User-agent: *\nDisallow: /news-release/\n",
        "User-agent: *\nDisallow: /news-release/2026/\n",
    ] {
        let dir = TempDir::new()?;
        said.push(collected_under_robots(dir.path(), body.as_bytes(), true)?);
    }
    assert_eq!(
        said,
        [
            "disallows /rssfeed/",
            "disallows /news-release/",
            "admitted"
        ]
    );
    Ok(())
}

/// A pinned body this reader refuses is `robots_refused`, never a robots.txt with no rule.
/// Mutation 2026-11-15, `Unreadable` read as no robots: red.
#[test]
fn a_pinned_robots_body_that_does_not_parse_is_refused() -> TestResult {
    let dir = TempDir::new()?;
    let said = collected_under_robots(
        dir.path(),
        &robots_fixture("synthetic_html_page.txt")?,
        true,
    )?;
    assert_eq!(said, "unreadable: the body names no user-agent");
    Ok(())
}

/// `open_provider` reads the pinned body before the ledger check; a refresh that writes it in
/// between is unverified, not read as no robots. Mutation 2026-11-15, `BlobMissing` read as no
/// robots: red.
#[test]
fn a_robots_blob_written_after_the_provider_opened_is_unverified() -> TestResult {
    let root = TempDir::new()?;
    let ask = Ask {
        dry_run: true,
        ..ask(root.path(), cmd_collect::TODAY)
    };
    let opened = cmd_collect::open_provider(&ask)?;
    seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
    let answered = cmd_collect::run(&ask, &opened, &Gnw::day()?, &Fixed);
    assert_eq!(verdict(&answered), format!("unverified {GNW_PIN}"));
    Ok(())
}

/// §8 row R19(b): nasdaq's 30 s is admitted once handed to the provider, refused without it.
/// Mutations 2026-11-15, the floor check removed / `max(cd, 10 s)` as `cd` / `>=` as `>` / the
/// hand-off removed: each red.
#[test]
fn a_hosts_row_faster_than_its_robots_crawl_delay_is_refused() -> TestResult {
    let secs = |n: u64| Some(Duration::from_secs(n));
    let dir = TempDir::new()?;
    let (store, hosts) = pinned_to(dir.path(), &robots_fixture("www.nasdaq.com.txt")?)?;
    let ask = Ask {
        hosts: Some(hosts.clone()),
        dry_run: true,
        ..ask(&store, cmd_collect::TODAY)
    };
    let mut opened = cmd_collect::open_provider(&ask)?;
    let slowed = opened.provider.transport().budget.per_host;
    let admitted = verdict(&cmd_collect::run(&ask, &opened, &Gnw::day()?, &Fixed));
    opened.provider = Box::new(OpenWebFeed::open(&hosts, &ask.rules, CONTACT)?);
    let unslowed = verdict(&cmd_collect::run(&ask, &opened, &Gnw::day()?, &Fixed));
    assert_eq!(
        (slowed, admitted.as_str(), unslowed.as_str()),
        (secs(30), "admitted", "paced Some(10s) below 30s")
    );
    // (per_host, Crawl-delay, admitted): the floor is 10 s with no delay or a smaller one.
    let cases = [
        (secs(10), secs(30), false),
        (secs(30), secs(30), true),
        (secs(10), secs(5), true),
        (secs(10), None, true),
        (secs(5), secs(5), false),
        (secs(5), None, false),
        (None, None, false),
    ];
    for (per_host, crawl_delay, ok) in cases {
        let checked = cmd_collect::refuse_a_pace_below_the_floor(INDEX, per_host, crawl_delay);
        assert_eq!(checked.is_ok(), ok, "{per_host:?} {crawl_delay:?}");
    }
    Ok(())
}

/// Section 5.1: a disallowed path and an unreadable pinned body are `robots_refused`, exit 2, with
/// the path or the reason in `details` and no suggestion, because a refresh cannot change a pinned
/// body. Mutation 2026-11-15, the path arm dropped from `robots_refusal`: red.
#[test]
fn a_path_or_unreadable_robots_refusal_names_its_fact_and_suggests_no_refresh() -> TestResult {
    let mut seen = Vec::new();
    for body in [
        b"User-agent: *\nDisallow: /rssfeed/\n".to_vec(),
        robots_fixture("synthetic_html_page.txt")?,
    ] {
        let dir = TempDir::new()?;
        let (store, hosts) = pinned_to(dir.path(), &body)?;
        let output = Command::new(env!("CARGO_BIN_EXE_newsvault"))
            .args([
                "collect",
                "--source",
                SOURCE,
                "--window",
                cmd_collect::TODAY,
            ])
            .args(["--at", RUN_AT, "--format", "json", "--dry-run"])
            .arg("--store")
            .arg(&store)
            .arg("--hosts")
            .arg(&hosts)
            .arg("--rules")
            .arg(provider_dir().join("rules/open-web-feed.rules.json"))
            .env("BELLWETHER_FEED_CONTACT", CONTACT)
            .stdin(Stdio::null())
            .output()?;
        let error: Value = serde_json::from_slice(&output.stderr)?;
        seen.push((
            output.status.code(),
            output.stdout.len(),
            error["code"].clone(),
            error["details"]["path"].clone(),
            error["details"]["why"].clone(),
            error["suggestions"].clone(),
        ));
    }
    let refused = Value::from("robots_refused");
    assert_eq!(
        seen,
        vec![
            (
                Some(EXIT_USAGE),
                0,
                refused.clone(),
                Value::from("/rssfeed/"),
                Value::Null,
                Value::Array(Vec::new())
            ),
            (
                Some(EXIT_USAGE),
                0,
                refused,
                Value::Null,
                Value::from("the body names no user-agent"),
                Value::Array(Vec::new())
            ),
        ]
    );
    Ok(())
}
