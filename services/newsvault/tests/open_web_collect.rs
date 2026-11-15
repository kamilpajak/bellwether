//! `newsvault collect --source open_web_wires` from plan to committed items, over the GNW fixtures
//! in `providers/open-web-feed/tests/fixtures/gnw` (architecture.md section 11.5 step 2).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{DateTime, TimeDelta, Utc};
use news_provider::{
    Clock, Exchange, Exit, PlanRefusal, RawResponse, Request, SendFailure, SendPermit,
    TransportError,
};
use newsvault::cmd_collect::{self, Ask, CollectRunError};
use newsvault::cmd_items::{self, PerStory};
use newsvault::store::COMMIT_LOG;
use pit::ObservationTime;
use rusqlite::{Connection, OpenFlags};
use tempfile::TempDir;

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
    let mut versions = db.prepare("SELECT hosts_version FROM attempts")?;
    let versions = versions
        .query_map([], |row| row.get::<_, Option<String>>(0))?
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
