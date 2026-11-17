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
    Archive, ArchiveError, AttemptEvent, AttemptReceipt, Clock, CollectedItem, Committed, Exchange,
    Exit, Headers, LedgerError, PlanRefusal, RawResponse, Request, RunId, SendFailure, SendPermit,
    Sink, SourceId, TargetWindow, TransportError,
};
use newsvault::cmd_collect::{self, Ask, CollectRunError};
use newsvault::cmd_items::{self, PerStory};
use newsvault::store::{Committer, StoreError, COMMIT_LOG, SCHEMA_VERSION};
use open_web_feed::OpenWebFeed;
use pit::ObservationTime;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use tempfile::TempDir;

mod loopback;
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
const REQUEST_TIMEOUT: u16 = 408;
const MOVED_PERMANENTLY: u16 = 301;
/// The one header `Gnw` answers a page with, as P9 keeps it (section 3.9).
const CONTENT_TYPE: &str = "content-type";
const PAGE_TYPE: &str = "text/html; charset=utf-8";
/// The loopback 301's body, `MOVED_BODY` in `tests/loopback/mod.rs`, and its length.
const MOVED_BODY: &[u8] = b"moved";
const MOVED_BODY_BYTES: i64 = 5;
/// open-web's `BODY_LIMIT` (2 MiB, `providers/open-web-feed/src/lib.rs`), private to that crate.
const OPEN_WEB_BODY_LIMIT: i64 = 2 * 1_024 * 1_024;
/// Two robots fetches before `RUN_AT`, the second the newer.
const ROBOTS_EARLIER: &str = "2026-11-14T17:00:00Z";
const ROBOTS_LATER: &str = "2026-11-14T18:00:00Z";
/// The GNW row's pin, in the committed hosts file and `gnw_admitted.json`:
/// `robots_rss_globenewswire_com.txt`'s sha256.
const GNW_PIN: &str = "eb8bb698171dd56c139c224c9217070adf9947f6ed6cd02336eba55aa4155bfe";
const CHANGED_ROBOTS: &[u8] = b"User-agent: *\nDisallow: /\n";
/// One above `store::SCHEMA_VERSION`, which this build must refuse.
const NEWER_STORE: i32 = SCHEMA_VERSION + 1;

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

const PRN_INDEX: &str = "https://www.prnewswire.com/rss/news-releases-list.rss";
/// `prn/rss_3_items.xml`'s three links with the page each is answered with. 302913655 is a
/// correction notice listed under the release's link (§8 row R3; its page was never fetched), and
/// is answered with the synthetic page that has no `section.release-body`.
const PRN_RELEASES: [(&str, &str); 3] = [
    (
        "www.prnewswire.com/news-releases/north-carolinas-electric-cooperatives-launches-evconnectnc-powered-by-weavegrid-to-maximize-charging-during-lower-demand-and-power-cost-periods-302913655.html",
        "synthetic_no_release_body.html",
    ),
    (
        "www.prnewswire.com/news-releases/novos-wegovy-was-associated-with-a-reduction-in-food-noise-according-to-real-world-studies-at-obesityweek-2026-302939553.html",
        "article_302939553.html",
    ),
    (
        "www.prnewswire.com/news-releases/lilly-to-present-new-data-across-zepbound-foundayo-eloratzp-and-retatrutide-at-obesityweek-2026-moving-obesity-care-beyond-the-scale-302938569.html",
        "article_302938569.html",
    ),
];

const CNW_INDEX: &str = "https://www.newswire.ca/sitemap-news.xml?page=1";
/// `cnw/sitemap_news_3_urls.xml`'s three links with the page each is answered with. HKEX's page
/// is not kept, so 807505308 is answered with the synthetic page that has no `section.release-body`.
const CNW_RELEASES: [(&str, &str); 3] = [
    (
        "www.newswire.ca/news-releases/global-voltage-travel-ready-protection-elegrp-powers-dyson-supersonic-travel-hair-dryer-805423040.html",
        "article_805423040.html",
    ),
    (
        "www.newswire.ca/news-releases/segway-powersports-secures-class-2-one-two-finish-on-score-baja-1000-debut-823884942.html",
        "article_823884942.html",
    ),
    (
        "www.newswire.ca/news-releases/hkex-launches-offshore-rmb-bond-index-807505308.html",
        "synthetic_no_release_body.html",
    ),
];
/// Monday 2026-11-16, after the CNW fixtures' publication minutes (07:22Z newest), so no CNW item
/// is observed before it was published; one second before it, the stub's `received_at`.
const MONDAY_AT: &str = "2026-11-16T08:05:30Z";
const MONDAY_RECEIVED_AT: &str = "2026-11-16T08:05:29.000000Z";

fn provider_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../providers/open-web-feed")
}

/// The GNW row alone: a byte copy of the committed hosts file before PRN was admitted.
fn hosts() -> PathBuf {
    hosts_fixture("gnw_admitted.json")
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

struct Monday;

impl Clock for Monday {
    fn now(&self) -> DateTime<Utc> {
        instant(MONDAY_AT)
    }
}

/// Answers each url out of a committed fixture, sent two seconds and received one second before
/// the clock's instant, so the three instants differ and the stored observation names one of them.
struct Gnw {
    pages: BTreeMap<String, Vec<u8>>,
    asked: RefCell<Vec<String>>,
    /// Statuses a url answers with, front first and with an empty body, before its page.
    scripted: RefCell<BTreeMap<String, Vec<u16>>>,
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
            scripted: RefCell::new(BTreeMap::new()),
        })
    }

    /// `day`, and PR Newswire's index and `PRN_RELEASES` answered from `fixtures/prn`.
    fn two_hosts() -> Result<Self, Box<dyn Error>> {
        let mut both = Self::day()?;
        let fixture =
            |name: &str| std::fs::read(provider_dir().join("tests/fixtures/prn").join(name));
        both.pages
            .insert(PRN_INDEX.to_owned(), fixture("rss_3_items.xml")?);
        for (key, page) in PRN_RELEASES {
            both.pages.insert(format!("https://{key}"), fixture(page)?);
        }
        Ok(both)
    }

    /// `two_hosts`, and Canada Newswire's sitemap and `CNW_RELEASES` answered from `fixtures/cnw`.
    fn three_hosts() -> Result<Self, Box<dyn Error>> {
        let mut all = Self::two_hosts()?;
        let fixture =
            |name: &str| std::fs::read(provider_dir().join("tests/fixtures/cnw").join(name));
        all.pages
            .insert(CNW_INDEX.to_owned(), fixture("sitemap_news_3_urls.xml")?);
        for (key, page) in CNW_RELEASES {
            all.pages.insert(format!("https://{key}"), fixture(page)?);
        }
        Ok(all)
    }

    fn answering(self, url: &str, statuses: &[u16]) -> Self {
        self.scripted
            .borrow_mut()
            .insert(url.to_owned(), statuses.to_vec());
        self
    }

    /// The urls sent since the last call, and the count of them that were not the index.
    fn sent(&self) -> (Vec<String>, usize) {
        let sent = self.asked.take();
        let documents = sent.iter().filter(|url| url.as_str() != INDEX).count();
        (sent, documents)
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
        if let Some(statuses) = self.scripted.borrow_mut().get_mut(&request.url) {
            if !statuses.is_empty() {
                let status = statuses.remove(0);
                return Ok(RawResponse::new(
                    attempt,
                    status,
                    Vec::new(),
                    sent,
                    received,
                ));
            }
        }
        match self.pages.get(&request.url) {
            Some(body) => Ok(RawResponse::new(attempt, OK, body.clone(), sent, received)
                .with_headers(Headers::new(vec![(
                    CONTENT_TYPE.to_owned(),
                    PAGE_TYPE.to_owned(),
                )]))),
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

/// The rows `Gnw::day` commits for `RELEASES`, keyed by story.
fn gnw_items() -> BTreeMap<String, Row> {
    gnw_items_received(RECEIVED_AT)
}

/// `gnw_items`, each page received at `received`.
fn gnw_items_received(received: &str) -> BTreeMap<String, Row> {
    RELEASES
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
                    received.to_owned(),
                ),
            ))
        })
        .collect()
}

/// The committed row of the release keyed `key`, whose canonical is its own link, read from the
/// index field `field` and received at `received`.
fn own_link_item(
    key: &str,
    field: &str,
    (minute, raw): (&str, &str),
    received: &str,
) -> Result<(String, Row), Box<dyn Error>> {
    let story = digest(canon::digest::story_id(SOURCE, key)).ok_or("story id")?;
    Ok((
        story,
        (
            digest(canon::digest::doc_id(&format!("https://{key}"))),
            Some(minute.to_owned()),
            None,
            Some(raw.to_owned()),
            Some(field.to_owned()),
            Some("vendor_published".to_owned()),
            received.to_owned(),
        ),
    ))
}

/// The committed row of `PRN_RELEASES[release]`.
fn prn_item(
    release: usize,
    dated: (&str, &str),
    received: &str,
) -> Result<(String, Row), Box<dyn Error>> {
    let (key, _) = PRN_RELEASES.get(release).ok_or("no such release")?;
    own_link_item(key, "pubDate", dated, received)
}

/// The committed row of `CNW_RELEASES[release]`.
fn cnw_item(
    release: usize,
    dated: (&str, &str),
    received: &str,
) -> Result<(String, Row), Box<dyn Error>> {
    let (key, _) = CNW_RELEASES.get(release).ok_or("no such release")?;
    own_link_item(key, "news:publication_date", dated, received)
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
    let expected = gnw_items();
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

/// Every blob file under `<store>/blobs/sha256`, by the hex it is named by.
fn blob_files(store: &Path) -> Result<std::collections::BTreeSet<String>, Box<dyn Error>> {
    let mut names = std::collections::BTreeSet::new();
    for first in std::fs::read_dir(store.join("blobs/sha256"))? {
        for second in std::fs::read_dir(first?.path())? {
            for file in std::fs::read_dir(second?.path())? {
                names.insert(file?.file_name().to_string_lossy().into_owned());
            }
        }
    }
    Ok(names)
}

/// `(url, phase, listed_date is set, truncated, headers)` of each collect attempt's link row,
/// `None` where the attempt has none.
type Linked = (String, Option<(String, bool, bool, String)>);

/// P10 (section 3.9): every body a run and its robots seed stored is indexed by a `blobs` row, and
/// each answered attempt names its body: the index, the unusable page and a 403 page, which no
/// item row names. Mutations 2026-11-16, `link` a no-op, and `blobs_of` dropping the payload: red.
#[test]
fn every_body_an_open_web_run_stores_is_indexed_and_linked_to_its_attempt() -> TestResult {
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
    let ask = ask(root.path(), cmd_collect::TODAY);
    let opened = cmd_collect::open_provider(&ask)?;
    let forbidden = release_url(1)?;
    let exchange = Gnw::day()?.answering(&forbidden, &[FORBIDDEN]);
    cmd_collect::run(&ask, &opened, &exchange, &Fixed)?;
    let db = Connection::open_with_flags(
        root.path().join(COMMIT_LOG),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let indexed: std::collections::BTreeSet<String> = db
        .prepare("SELECT digest FROM blobs")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let linked: Vec<Linked> = db
        .prepare(
            "SELECT p.url, p.phase, p.listed_date IS NOT NULL, p.truncated, p.headers
               FROM attempts a LEFT JOIN payloads p ON p.attempt_id = a.attempt_id
              WHERE a.source = ?1 ORDER BY a.opened_at, a.attempt_id",
        )?
        .query_map([SOURCE], |row| {
            let url: Option<String> = row.get(0)?;
            Ok((
                url.clone().unwrap_or_default(),
                match url {
                    Some(_) => Some((row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                    None => None,
                },
            ))
        })?
        .collect::<Result<_, _>>()?;
    let page = format!("[[\"{CONTENT_TYPE}\",\"{PAGE_TYPE}\"]]");
    let document = |url: String, headers: &str| -> Linked {
        (
            url,
            Some(("document".to_owned(), true, false, headers.to_owned())),
        )
    };
    let mut expected = vec![(
        INDEX.to_owned(),
        Some(("index".to_owned(), false, false, page.clone())),
    )];
    for release in 0..RELEASES.len() {
        let url = release_url(release)?;
        let headers = if url == forbidden {
            "[]"
        } else {
            page.as_str()
        };
        expected.push(document(url, headers));
    }
    let mut linked_sorted = linked.clone();
    linked_sorted.sort();
    expected.sort();
    assert_eq!(
        (blob_files(root.path())?, linked_sorted),
        (indexed, expected),
        "{linked:?}"
    );
    Ok(())
}

/// Section 11.2, the committed hosts file: the GNW, PRN and CNW indexes, then each host's three
/// pages, on Monday 2026-11-16. The PRN and CNW mutations that turn it red, re-run 2026-11-17, are
/// §11.5 step 6's tables.
#[test]
fn an_open_web_run_collects_a_gnw_a_prn_and_a_cnw_day_end_to_end() -> TestResult {
    const ATTEMPTS: usize = 12;
    let root = TempDir::new()?;
    seed::refresh_committed(root.path(), ROBOTS_EARLIER)?;
    let ask = Ask {
        hosts: Some(seed::committed_hosts()),
        at: instant(MONDAY_AT),
        ..ask(root.path(), cmd_collect::TODAY)
    };
    let opened = cmd_collect::open_provider(&ask)?;
    let answer = cmd_collect::run(&ask, &opened, &Gnw::three_hosts()?, &Monday)?;
    let envelope = &answer.envelope;
    let attempts = envelope["attempts"].as_array().ok_or("attempts")?;
    let hosts_version = canon::FileDigest::of(&std::fs::read(seed::committed_hosts())?);
    // Every index is asked before any page.
    let indexes: Vec<Option<&str>> = [0, 1, 2].map(|nth| attempts[nth]["url"].as_str()).into();
    assert_eq!(
        (
            answer.exit,
            envelope["window"].as_str(),
            envelope["hosts_version"].as_str(),
            attempts.len(),
            indexes,
            envelope["items_committed"].as_u64(),
        ),
        (
            Exit::Ok,
            Some("2026-11-16"),
            Some(hosts_version.hex()),
            ATTEMPTS,
            vec![Some(INDEX), Some(PRN_INDEX), Some(CNW_INDEX)],
            Some(6)
        ),
        "{envelope:#}"
    );
    // Per url: (vendor_rows, unusable_rows, items_committed).
    type Counts = (Option<u64>, Option<u64>, Option<u64>);
    let counted: BTreeMap<String, Counts> = attempts
        .iter()
        .filter_map(|row| {
            Some((
                row["url"].as_str()?.to_owned(),
                (
                    row["vendor_rows"].as_u64(),
                    row["unusable_rows"].as_u64(),
                    row["items_committed"].as_u64(),
                ),
            ))
        })
        .collect();
    let mut expected_counts = BTreeMap::new();
    for index in [INDEX, PRN_INDEX, CNW_INDEX] {
        expected_counts.insert(index.to_owned(), (Some(3), Some(0), Some(0)));
    }
    let gnw_pages = RELEASES
        .iter()
        .map(|(key, _, _, canonical)| (*key, canonical.is_some()));
    let prn_pages = PRN_RELEASES
        .iter()
        .enumerate()
        .map(|(nth, (key, _))| (*key, nth != 0));
    let cnw_pages = CNW_RELEASES
        .iter()
        .enumerate()
        .map(|(nth, (key, _))| (*key, nth != 2));
    for (key, committed) in gnw_pages.chain(prn_pages).chain(cnw_pages) {
        let (unusable, items) = if committed { (0, 1) } else { (1, 0) };
        expected_counts.insert(
            format!("https://{key}"),
            (Some(1), Some(unusable), Some(items)),
        );
    }
    assert_eq!(counted, expected_counts);
    let db = Connection::open_with_flags(
        root.path().join(COMMIT_LOG),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut versions = db.prepare("SELECT hosts_version FROM attempts WHERE source = ?1")?;
    let versions = versions
        .query_map([SOURCE], |row| row.get::<_, Option<String>>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        versions,
        vec![Some(hosts_version.hex().to_owned()); ATTEMPTS]
    );
    // Section 2's `extract_version`, rules plus extraction code: on the envelope and on every
    // attempt row. Mutation 2026-11-16, `commit_opened` writing NULL for it: red.
    let extraction = OpenWebFeed::open(&seed::committed_hosts(), &ask.rules, CONTACT)?
        .extract_version()
        .hex()
        .to_owned();
    let mut extracted = db.prepare("SELECT extract_version FROM attempts WHERE source = ?1")?;
    let extracted = extracted
        .query_map([SOURCE], |row| row.get::<_, Option<String>>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        (envelope["extract_version"].as_str(), extracted),
        (
            Some(extraction.as_str()),
            vec![Some(extraction.clone()); ATTEMPTS]
        )
    );
    let mut expected = gnw_items_received(MONDAY_RECEIVED_AT);
    expected.extend([
        prn_item(
            1,
            (
                "2026-11-14T17:15:00.000000Z",
                "Sat, 14 Nov 2026 17:15:00 +0000",
            ),
            MONDAY_RECEIVED_AT,
        )?,
        prn_item(
            2,
            (
                "2026-11-14T17:01:00.000000Z",
                "Sat, 14 Nov 2026 17:01:00 +0000",
            ),
            MONDAY_RECEIVED_AT,
        )?,
        // The sitemap's -05:00 local minute, stored as its UTC minute beside the raw text.
        cnw_item(
            0,
            ("2026-11-16T07:22:00.000000Z", "2026-11-16T02:22:00-05:00"),
            MONDAY_RECEIVED_AT,
        )?,
        cnw_item(
            1,
            ("2026-11-16T06:47:00.000000Z", "2026-11-16T01:47:00-05:00"),
            MONDAY_RECEIVED_AT,
        )?,
    ]);
    assert_eq!(stored_items(&db)?, expected);
    // Nothing is visible before the run observed it, and all six after.
    assert_eq!(
        (
            visible(root.path(), "2026-11-16T08:05:00Z")?,
            visible(root.path(), "2026-11-16T08:06:00Z")?
        ),
        (0, 6)
    );
    Ok(())
}

/// The committed three-host file on Monday, on a fresh store: each attempt's url and id, in order.
fn three_host_attempts(exchange: &Gnw) -> Result<Vec<(String, String)>, Box<dyn Error>> {
    let root = TempDir::new()?;
    seed::refresh_committed(root.path(), ROBOTS_EARLIER)?;
    let ask = Ask {
        hosts: Some(seed::committed_hosts()),
        at: instant(MONDAY_AT),
        ..ask(root.path(), cmd_collect::TODAY)
    };
    let opened = cmd_collect::open_provider(&ask)?;
    let answer = cmd_collect::run(&ask, &opened, exchange, &Monday)?;
    let attempts = answer.envelope["attempts"].as_array().ok_or("attempts")?;
    attempts
        .iter()
        .map(
            |row| match (row["url"].as_str(), row["attempt_id"].as_str()) {
                (Some(url), Some(id)) => Ok((url.to_owned(), id.to_owned())),
                _ => Err(format!("no url or attempt_id in {row}").into()),
            },
        )
        .collect()
}

/// The three indexes, then one release of GNW, PRN and CNW per turn; and GNW's first release.
fn round_robin_of_three_hosts() -> (Vec<String>, String) {
    let [(gnw, gnw_pages), (prn, prn_pages), (cnw, cnw_pages)] = three_host_urls();
    let mut order = vec![gnw, prn, cnw];
    for nth in 0..RELEASES.len() {
        order.extend([
            gnw_pages[nth].clone(),
            prn_pages[nth].clone(),
            cnw_pages[nth].clone(),
        ]);
    }
    (order, gnw_pages[0].clone())
}

/// `(host's index, host's releases)` as urls, GNW, PRN, CNW.
fn three_host_urls() -> [(String, Vec<String>); 3] {
    let pages = |keys: Vec<&str>| keys.iter().map(|key| format!("https://{key}")).collect();
    [
        (
            INDEX.to_owned(),
            pages(RELEASES.iter().map(|(key, ..)| *key).collect()),
        ),
        (
            PRN_INDEX.to_owned(),
            pages(PRN_RELEASES.iter().map(|(key, _)| *key).collect()),
        ),
        (
            CNW_INDEX.to_owned(),
            pages(CNW_RELEASES.iter().map(|(key, _)| *key).collect()),
        ),
    ]
}

/// Owner decision 2026-11-17 (§10): `collect` serves its hosts round-robin, in first-appearance
/// order, so the three indexes, then one release of each host per turn.
#[test]
fn a_three_host_run_asks_its_hosts_round_robin() -> TestResult {
    let (expected, _) = round_robin_of_three_hosts();
    let attempts = three_host_attempts(&Gnw::three_hosts()?)?;
    let urls: Vec<String> = attempts.into_iter().map(|(url, _)| url).collect();
    assert_eq!(urls, expected);
    Ok(())
}

/// §8 R21 under round-robin: GNW's first release fails every time; each retry goes to the back of
/// GNW's lane, after every request already queued, and stops at `MAX_TRANSIENT_RETRIES`.
#[test]
fn a_retried_release_waits_behind_the_other_hosts_and_stops_at_the_cap() -> TestResult {
    let (mut expected, failing) = round_robin_of_three_hosts();
    expected.extend([failing.clone(), failing.clone()]);
    let exchange = Gnw::three_hosts()?.answering(&failing, &[UNAVAILABLE; 3]);
    let attempts = three_host_attempts(&exchange)?;
    let urls: Vec<String> = attempts.into_iter().map(|(url, _)| url).collect();
    assert_eq!(urls, expected);
    Ok(())
}

/// An attempt id hashes (run, source, window, ordinal), and the ordinal is the position in the
/// round-robin order: the same plan and answers on two fresh stores give the same ids, url by url.
#[test]
fn a_replayed_three_host_run_gives_every_attempt_the_same_id_and_url() -> TestResult {
    let first = three_host_attempts(&Gnw::three_hosts()?)?;
    let second = three_host_attempts(&Gnw::three_hosts()?)?;
    let distinct: std::collections::BTreeSet<&String> = first.iter().map(|(_, id)| id).collect();
    assert_eq!((first.len(), distinct.len(), &first), (12, 12, &second));
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
        Err(CollectRunError::RobotsBlobAltered { found, .. }) => format!("altered {found}"),
        Err(CollectRunError::RobotsDisallowsPath { path, .. }) => format!("disallows {path}"),
        Err(CollectRunError::PaceBelowFloor {
            per_host, floor, ..
        }) => format!("paced {per_host:?} below {floor:?}"),
        Err(CollectRunError::CrawlDelayOverInterval {
            crawl_delay,
            interval,
            ..
        }) => format!("crawl-delay {crawl_delay:?} over {interval:?}"),
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
    for failed in [
        (FORBIDDEN, b"Access Denied".as_slice()),
        (MOVED_PERMANENTLY, b"moved".as_slice()),
    ] {
        let failed_only = TempDir::new()?;
        seed::refresh(failed_only.path(), &hosts(), ROBOTS_EARLIER, failed)?;
        let before = snapshot(failed_only.path());
        assert_eq!(
            collected(failed_only.path(), hosts(), false)?,
            "unverified none",
            "{}",
            failed.0
        );
        assert_eq!(snapshot(failed_only.path()), before);
    }
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

/// A failed fetch files no robots row, so it neither admits nor un-admits a host; a 3xx is one
/// (section 11.2). Mutation 2026-11-15, the reader left-joining `attempts` to `robots`: red.
#[test]
fn a_failed_robots_fetch_after_a_matching_one_leaves_the_host_admitted() -> TestResult {
    for failed in [
        (UNAVAILABLE, b"".as_slice()),
        (MOVED_PERMANENTLY, b"moved".as_slice()),
    ] {
        let root = TempDir::new()?;
        seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
        seed::refresh(root.path(), &hosts(), ROBOTS_LATER, failed)?;
        assert_eq!(
            collected(root.path(), hosts(), true)?,
            "admitted",
            "{}",
            failed.0
        );
    }
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
    std::fs::remove_file(gnw_blob(root.path()))?;
    assert_eq!(
        collected(root.path(), hosts(), false)?,
        format!("unverified {GNW_PIN}")
    );
    Ok(())
}

fn gnw_blob(store: &Path) -> PathBuf {
    store
        .join("blobs/sha256")
        .join(&GNW_PIN[..2])
        .join(&GNW_PIN[2..4])
        .join(GNW_PIN)
}

/// The pinned blob is re-hashed when read: bytes changed on disk are not the pin, and a later
/// refresh keeps the changed file, so it repairs nothing and no refresh is suggested. Mutation
/// 2026-11-15, the hash comparison in `BlobStore::read_verified` removed: red.
#[test]
fn a_pinned_robots_blob_changed_on_disk_is_refused_and_suggests_no_refresh() -> TestResult {
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
    std::fs::write(gnw_blob(root.path()), CHANGED_ROBOTS)?;
    seed_gnw(root.path(), &hosts(), ROBOTS_LATER)?;
    let found = canon::FileDigest::of(CHANGED_ROBOTS).hex().to_owned();
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
        .arg(root.path())
        .arg("--hosts")
        .arg(hosts())
        .arg("--rules")
        .arg(provider_dir().join("rules/open-web-feed.rules.json"))
        .env("BELLWETHER_FEED_CONTACT", CONTACT)
        .stdin(Stdio::null())
        .output()?;
    let error: Value = serde_json::from_slice(&output.stderr)?;
    assert_eq!(
        (
            collected(root.path(), hosts(), true)?,
            output.status.code(),
            error["code"].clone(),
            error["details"]["found"].clone(),
            error["suggestions"].clone(),
        ),
        (
            format!("altered {found}"),
            Some(EXIT_USAGE),
            Value::from("robots_refused"),
            Value::from(found.as_str()),
            Value::Array(Vec::new()),
        )
    );
    Ok(())
}

/// The robots check reads the store before the version check, and a store above this build's
/// user_version is a version refusal there, not `robots_unverified`. Mutation 2026-11-15, the
/// version check in `RobotsLedger::open` removed: red.
#[test]
fn an_open_web_collect_against_a_newer_store_is_a_version_refusal() -> TestResult {
    let root = TempDir::new()?;
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    db.pragma_update(None, "user_version", NEWER_STORE)?;
    drop(db);
    let ask = ask(root.path(), cmd_collect::TODAY);
    let opened = cmd_collect::open_provider(&ask)?;
    let refused = cmd_collect::run(&ask, &opened, &Gnw::day()?, &Fixed);
    assert!(
        matches!(
            refused,
            Err(CollectRunError::Store(StoreError::SchemaVersion {
                found: NEWER_STORE,
                ..
            }))
        ),
        "{}",
        verdict(&refused)
    );
    Ok(())
}

/// A hosts file in `dir` admitting GNW and PRN, both pinned to the committed GNW body, and a store
/// whose one refresh answered each url in `answered` with that body: what a dry-run collect says.
fn collected_over_two_admitted_hosts(
    dir: &Path,
    answered: &[&str],
) -> Result<String, Box<dyn Error>> {
    let hosts = dir.join("hosts.json");
    let store = dir.join("store");
    let row = |host: &str, index: &str| {
        serde_json::json!({"host": host, "status": "admitted", "index_url": index,
                           "rule": "gnw", "robots": GNW_PIN})
    };
    let file = serde_json::json!({
        "schema": "open-web-feed.hosts/2",
        "hosts": [row("rss.globenewswire.com", INDEX),
                  row("www.prnewswire.com", "https://www.prnewswire.com/rss/news-releases-list.rss")],
    });
    std::fs::write(&hosts, serde_json::to_vec(&file)?)?;
    let body = seed::gnw_robots()?;
    let answers: Vec<(&str, u16, &[u8])> = answered
        .iter()
        .map(|url| (*url, OK, body.as_slice()))
        .collect();
    seed::refresh_answering(&store, &hosts, ROBOTS_EARLIER, &answers)?;
    collected(&store, hosts, true)
}

/// §8 row R4: every admitted row is checked, not the first. Mutation 2026-11-15, the loop in
/// `refuse_an_unverified_robots` cut to its first row: red.
#[test]
fn a_second_admitted_host_without_a_ledgered_fetch_is_refused() -> TestResult {
    let dir = TempDir::new()?;
    let said = collected_over_two_admitted_hosts(dir.path(), &[seed::GNW_ROBOTS])?;
    assert_eq!(said, "unverified none");
    Ok(())
}

/// §8 row R4: the lookup is keyed by host, so another host's fetch of the same bytes admits
/// nothing. Mutation 2026-11-15, `newest_robots`' host condition replaced by `?2 IS NOT NULL`: red.
#[test]
fn another_hosts_fetch_matching_the_pin_does_not_admit_a_host() -> TestResult {
    let dir = TempDir::new()?;
    let said = collected_over_two_admitted_hosts(dir.path(), &[seed::PRN_ROBOTS])?;
    assert_eq!(said, "unverified none");
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
/// object on stderr; stdout stays empty. The unverified one suggests the refresh as an argv array;
/// a changed pin suggests nothing, because a refresh records the same change again. Mutations
/// 2026-11-15, the refusal written as plain `error:` text / the changed pin suggesting the
/// refresh: each red.
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
        let suggested = if store == unverified.path() {
            vec![Value::from(refresh.to_vec())]
        } else {
            Vec::new()
        };
        assert_eq!(error["suggestions"], Value::from(suggested));
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

/// A pinned body this reader refuses is `robots_refused`, never a robots.txt with no rule; an
/// empty or comment-only body (owner decision 3a) and text with no group (recommendation #2) are
/// no rule and admit the host. Mutation 2026-11-15, `Unreadable` read as no robots: red.
#[test]
fn a_pinned_robots_body_that_does_not_parse_is_refused() -> TestResult {
    let mut said = Vec::new();
    for body in [
        robots_fixture("synthetic_html_page.txt")?,
        b"Service unavailable\n".to_vec(),
        b"# nothing to say\n".to_vec(),
    ] {
        let dir = TempDir::new()?;
        said.push(collected_under_robots(dir.path(), &body, true)?);
    }
    assert_eq!(
        said,
        [
            "unreadable: the body is an HTML page: its first non-whitespace character is <",
            "admitted",
            "admitted",
        ]
    );
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

/// §8 row R19(b), owner decision 1 (2026-11-16): a pinned Crawl-delay above the one shared 10 s
/// interval refuses the host, nasdaq's 30 s and the B2 body's 30 s alike; the provider is never
/// slowed. Mutations 2026-11-16, the Crawl-delay arm removed: red; `>` as `>=`: red (10 s case);
/// the 10 s floor check removed: red.
#[test]
fn a_hosts_row_whose_robots_crawl_delay_is_above_the_shared_interval_is_refused() -> TestResult {
    let secs = |n: u64| Some(Duration::from_secs(n));
    let mut seen = Vec::new();
    for body in [
        robots_fixture("www.nasdaq.com.txt")?,
        b"User-agent: *\nCrawl-delay: 30\nUser-agent: bingbot\nDisallow: /prywatne/\n".to_vec(),
        b"User-agent: *\nCrawl-delay: 10\n".to_vec(),
    ] {
        let dir = TempDir::new()?;
        let (store, hosts) = pinned_to(dir.path(), &body)?;
        let ask = Ask {
            hosts: Some(hosts),
            dry_run: true,
            ..ask(&store, cmd_collect::TODAY)
        };
        let opened = cmd_collect::open_provider(&ask)?;
        seen.push((
            opened.provider.transport().budget.per_host,
            verdict(&cmd_collect::run(&ask, &opened, &Gnw::day()?, &Fixed)),
        ));
    }
    let over = "crawl-delay 30s over 10s".to_owned();
    assert_eq!(
        seen,
        [
            (secs(10), over.clone()),
            (secs(10), over),
            (secs(10), "admitted".to_owned())
        ]
    );
    // (per_host, Crawl-delay, admitted): 10 s at least, and the delay at most per_host.
    let cases = [
        (secs(10), secs(30), false),
        (secs(30), secs(30), true),
        (secs(10), secs(10), true),
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

/// Section 5.1: a disallowed path, an unreadable pinned body and a Crawl-delay above the shared
/// interval are `robots_refused`, exit 2, with their fact in `details` and no suggestion, because
/// a refresh cannot change a pinned body. Mutations: 2026-11-15, the path arm dropped from
/// `robots_refusal`: red; 2026-11-16, the Crawl-delay arm dropped: red.
#[test]
fn a_path_or_unreadable_robots_refusal_names_its_fact_and_suggests_no_refresh() -> TestResult {
    let mut seen = Vec::new();
    for body in [
        b"User-agent: *\nDisallow: /rssfeed/\n".to_vec(),
        robots_fixture("synthetic_html_page.txt")?,
        robots_fixture("www.nasdaq.com.txt")?,
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
        let details = &error["details"];
        seen.push((
            output.status.code(),
            output.stdout.len(),
            error["code"].clone(),
            ["path", "why", "crawl_delay_seconds", "interval_seconds"]
                .map(|field| details[field].clone()),
            error["suggestions"].clone(),
        ));
    }
    let refused = |facts: [Value; 4]| {
        (
            Some(EXIT_USAGE),
            0,
            Value::from("robots_refused"),
            facts,
            Value::Array(Vec::new()),
        )
    };
    assert_eq!(
        seen,
        vec![
            refused([
                Value::from("/rssfeed/"),
                Value::Null,
                Value::Null,
                Value::Null
            ]),
            refused([
                Value::Null,
                Value::from("the body is an HTML page: its first non-whitespace character is <"),
                Value::Null,
                Value::Null
            ]),
            refused([
                Value::Null,
                Value::Null,
                Value::from(30.0),
                Value::from(10.0)
            ]),
        ]
    );
    Ok(())
}

// ---- section 11.1: the held filter ----

/// A second poll of the same day, and the first poll of the next day.
const LATER_THE_SAME_DAY: &str = "2026-11-14T19:05:00Z";
const THE_NEXT_DAY: &str = "2026-11-15T09:00:00Z";
const LATER_THE_NEXT_DAY: &str = "2026-11-15T15:00:00Z";
/// `RELEASES` index of the page answered with `synthetic_no_article_container.html`.
const NO_ARTICLE: usize = 2;

struct At(DateTime<Utc>);

impl Clock for At {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

/// One live collect of the clock's own day at `at`, keyed by `schedule` so two polls of one store
/// never share a run id.
fn poll(
    store: &Path,
    at: &str,
    schedule: &str,
    exchange: &dyn Exchange,
) -> Result<Exit, Box<dyn Error>> {
    Ok(polled(store, at, schedule, exchange)?.exit)
}

fn polled(
    store: &Path,
    at: &str,
    schedule: &str,
    exchange: &dyn Exchange,
) -> Result<cmd_collect::Answer, Box<dyn Error>> {
    let ask = Ask {
        at: instant(at),
        schedule: Some(schedule.to_owned()),
        ..ask(store, cmd_collect::TODAY)
    };
    let opened = cmd_collect::open_provider(&ask)?;
    Ok(cmd_collect::run(&ask, &opened, exchange, &At(instant(at)))?)
}

fn release_url(index: usize) -> Result<String, Box<dyn Error>> {
    let (key, _, _, _) = RELEASES.get(index).ok_or("no such release")?;
    Ok(format!("https://{key}"))
}

/// Section 11.4 (a), §8 R16: every story the first poll answered is held, so the second sends the
/// index alone. An all-held run commits nothing and exits ok. Mutation 2026-11-15, collect.rs
/// passing an empty set for the live arm again: red.
#[test]
fn a_second_live_poll_of_one_day_fetches_no_held_document() -> TestResult {
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
    let exchange = Gnw::day()?;
    poll(root.path(), RUN_AT, "first", &exchange)?;
    let (_, first) = exchange.sent();
    let exit = poll(root.path(), LATER_THE_SAME_DAY, "second", &exchange)?;
    let (second, documents) = exchange.sent();
    assert_eq!(
        (first, second, documents, exit),
        (RELEASES.len(), vec![INDEX.to_owned()], 0, Exit::Ok)
    );
    Ok(())
}

/// Section 11.4 (b), §8 R20: held is keyed on (source, story) over every window. Mutation
/// 2026-11-15, `AND a.target_window = ?2` added to both arms of the held read: red.
#[test]
fn a_story_held_under_yesterdays_window_is_not_fetched_today() -> TestResult {
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
    let exchange = Gnw::day()?;
    poll(root.path(), RUN_AT, "first", &exchange)?;
    let (_, first) = exchange.sent();
    let exit = poll(root.path(), THE_NEXT_DAY, "second", &exchange)?;
    let (second, documents) = exchange.sent();
    assert_eq!(
        (first, second, documents, exit),
        (RELEASES.len(), vec![INDEX.to_owned()], 0, Exit::Ok)
    );
    Ok(())
}

/// Section 11.4, §8 R16: a 404 is a permanent verdict, which holds the story with no item.
/// Mutation 2026-11-15, the held read keeping its items arm only: red.
#[test]
fn a_permanently_failed_document_is_not_fetched_again() -> TestResult {
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
    let failed = release_url(0)?;
    let exchange = Gnw::day()?.answering(&failed, &[NOT_FOUND]);
    poll(root.path(), RUN_AT, "first", &exchange)?;
    let (first, _) = exchange.sent();
    poll(root.path(), LATER_THE_SAME_DAY, "second", &exchange)?;
    let (second, _) = exchange.sent();
    assert_eq!(
        (first.contains(&failed), second.contains(&failed)),
        (true, false)
    );
    Ok(())
}

/// A release's first attempt row: its `transport_status` and `verdict`.
fn first_attempt_of(root: &Path, release: usize) -> Result<(Option<i64>, String), Box<dyn Error>> {
    let db = Connection::open_with_flags(root.join(COMMIT_LOG), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let (key, _, _, _) = RELEASES.get(release).ok_or("no such release")?;
    let story = digest(canon::digest::story_id(SOURCE, key)).ok_or("story id")?;
    Ok(db.query_row(
        "SELECT transport_status, verdict FROM attempts WHERE story_id = ?1
         ORDER BY opened_at, attempt_id LIMIT 1",
        [&story],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

/// What two polls of one day left when release 0 went through the real open-web `Wire` to a
/// loopback server answering its path `301 Location: /elsewhere`: how often the first poll sent
/// release 0, the urls the second sent, the paths the server saw in each, release 0's first
/// attempt row, the status the first envelope reported for it, and both exits.
struct TwoPolls {
    first_times: usize,
    second: Vec<String>,
    hops: (Vec<String>, Vec<String>),
    first_attempt: (Option<i64>, String),
    reported: Vec<Option<i64>>,
    exits: (Exit, Exit),
    /// Release 0's first link row: truncated, the blob's stored bytes, its digest, and headers.
    first_payload: Option<LinkedBlob>,
    /// The first envelope's `blobs_written` and `bytes_written`.
    first_written: (u64, u64),
}

/// The 301's body: the loopback server's own short one, or one byte over the profile's limit.
#[derive(Clone, Copy)]
enum MovedBody {
    Short,
    OverTheLimit,
}

fn polled_twice_through_a_redirect(moved_body: MovedBody) -> Result<TwoPolls, Box<dyn Error>> {
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
    let redirected = release_url(0)?;
    let gnw = Gnw::day()?;
    let moved = release_0_path()?;
    let arrived = gnw.pages.get(&redirected).ok_or("no page")?.clone();
    let ask = ask(root.path(), cmd_collect::TODAY);
    let opened = cmd_collect::open_provider(&ask)?;
    let server = match moved_body {
        MovedBody::Short => loopback::Loopback::redirecting(&moved, arrived)?,
        MovedBody::OverTheLimit => {
            let over = usize::try_from(opened.provider.transport().body_limit)? + 1;
            loopback::Loopback::redirecting_with(&moved, vec![b'x'; over], arrived)?
        }
    };
    let exchange = loopback::Relay {
        stub: &gnw,
        wire: cmd_collect::wire_for(&ask, &opened),
        relayed: BTreeMap::from([(redirected.clone(), server.url(&moved))]),
        asked: RefCell::default(),
    };
    let first_answer = polled(root.path(), RUN_AT, "first", &exchange)?;
    let (first, first_hops) = (exchange.sent(), server.seen()?);
    let second_exit = poll(root.path(), LATER_THE_SAME_DAY, "second", &exchange)?;
    let (second, second_hops) = (exchange.sent(), server.seen()?);
    server.finish()?;
    let reported = first_answer.envelope["attempts"]
        .as_array()
        .ok_or("no attempts in the envelope")?
        .iter()
        .filter(|row| row["url"].as_str() == Some(redirected.as_str()))
        .map(|row| row["transport_status"].as_i64())
        .collect();
    Ok(TwoPolls {
        first_times: first.iter().filter(|url| **url == redirected).count(),
        second,
        hops: (first_hops, second_hops),
        first_attempt: first_attempt_of(root.path(), 0)?,
        reported,
        exits: (first_answer.exit, second_exit),
        first_payload: first_payload_of(root.path(), 0)?,
        first_written: (
            first_answer.envelope["blobs_written"]
                .as_u64()
                .ok_or("no blobs_written")?,
            first_answer.envelope["bytes_written"]
                .as_u64()
                .ok_or("no bytes_written")?,
        ),
    })
}

/// A link row: truncated, the stored blob's bytes, its digest, and its headers.
type LinkedBlob = (bool, i64, String, String);

/// The loopback 301's link row headers: its `Content-Type`, never its `Location` (section 11.2).
fn moved_headers() -> String {
    format!(r#"[["content-type","{}"]]"#, loopback::MOVED_CONTENT_TYPE)
}

/// A release's first link row.
fn first_payload_of(root: &Path, release: usize) -> Result<Option<LinkedBlob>, Box<dyn Error>> {
    let db = Connection::open_with_flags(root.join(COMMIT_LOG), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let (key, _, _, _) = RELEASES.get(release).ok_or("no such release")?;
    let story = digest(canon::digest::story_id(SOURCE, key)).ok_or("story id")?;
    Ok(db
        .query_row(
            "SELECT p.truncated, b.bytes, p.raw_blob, p.headers FROM attempts a
               JOIN payloads p ON p.attempt_id = a.attempt_id
               JOIN blobs b ON b.digest = p.raw_blob
              WHERE a.story_id = ?1 ORDER BY a.opened_at, a.attempt_id LIMIT 1",
            [&story],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?)
}

/// Release 0's path as the loopback server sees it.
fn release_0_path() -> Result<String, Box<dyn Error>> {
    let (_, path) = RELEASES[0]
        .0
        .split_once('/')
        .ok_or("release 0 has no path")?;
    Ok(format!("/{path}"))
}

/// Section 11.1, owner decision of 2026-11-15 (§10), §11.5 step 5: the 3xx is the answer, a
/// permanent verdict that holds nothing, so the next poll asks again and the hop is never sent.
/// Mutations 2026-11-15: open-web following one redirect, or the held read's 3xx exclusion
/// dropped: red.
#[test]
fn a_document_answered_with_a_redirect_is_fetched_again_by_the_next_poll() -> TestResult {
    let polled = polled_twice_through_a_redirect(MovedBody::Short)?;
    let moved = release_0_path()?;
    assert_eq!(
        polled.first_payload,
        Some((
            false,
            MOVED_BODY_BYTES,
            canon::FileDigest::of(MOVED_BODY).hex().to_owned(),
            moved_headers()
        ))
    );
    assert_eq!(
        (
            polled.first_times,
            polled.second,
            polled.hops,
            polled.first_attempt,
            polled.reported,
            polled.exits,
        ),
        (
            1,
            vec![INDEX.to_owned(), release_url(0)?],
            (vec![moved.clone()], vec![moved]),
            (
                Some(i64::from(MOVED_PERMANENTLY)),
                "permanent_failure".to_owned()
            ),
            vec![Some(i64::from(MOVED_PERMANENTLY))],
            (Exit::PartialCollect, Exit::VendorRefusedAll),
        )
    );
    Ok(())
}

/// A 301 whose body is over open-web's `BODY_LIMIT` fails as `BodyOverLimit`, and the failure keeps
/// the 301, so it is not held either (section 11.1). Before 2026-11-15 its status was NULL and the
/// story was held for good. Its body is stored cut at the limit and linked `truncated` (P10,
/// section 3.9), and the verdict stays `permanent_failure`.
#[test]
fn a_redirect_whose_body_is_over_the_limit_is_fetched_again_by_the_next_poll() -> TestResult {
    let polled = polled_twice_through_a_redirect(MovedBody::OverTheLimit)?;
    let moved = release_0_path()?;
    let limit = usize::try_from(OPEN_WEB_BODY_LIMIT)?;
    assert_eq!(
        polled.first_payload,
        Some((
            true,
            OPEN_WEB_BODY_LIMIT,
            canon::FileDigest::of(&vec![b'x'; limit]).hex().to_owned(),
            moved_headers()
        ))
    );
    assert_eq!(
        (
            polled.first_times,
            polled.second,
            polled.hops,
            polled.first_attempt,
            polled.reported,
            polled.exits,
        ),
        (
            1,
            vec![INDEX.to_owned(), release_url(0)?],
            (vec![moved.clone()], vec![moved]),
            (
                Some(i64::from(MOVED_PERMANENTLY)),
                "permanent_failure".to_owned()
            ),
            vec![Some(i64::from(MOVED_PERMANENTLY))],
            (Exit::PartialCollect, Exit::VendorRefusedAll),
        )
    );
    Ok(())
}

/// Section 3.9: the run envelope's `blobs_written` and `bytes_written` count a body cut at the
/// limit, so the two runs differ only by the 301 body's length.
#[test]
fn a_body_cut_at_the_limit_counts_in_the_envelope_s_blobs_and_bytes_written() -> TestResult {
    let short = polled_twice_through_a_redirect(MovedBody::Short)?.first_written;
    let over = polled_twice_through_a_redirect(MovedBody::OverTheLimit)?.first_written;
    let grown = u64::try_from(OPEN_WEB_BODY_LIMIT - MOVED_BODY_BYTES)?;
    assert_eq!(over, (short.0, short.1 + grown));
    Ok(())
}

/// Section 11.4, §8 R21: a transient failure holds nothing, so after the run's own retries run out
/// the next poll asks again. Mutation 2026-11-15, `transient_failure` added to the held verdicts:
/// red.
#[test]
fn a_transiently_failed_document_is_fetched_again_by_the_next_poll() -> TestResult {
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
    let failing = release_url(0)?;
    let tries = 1 + news_provider::MAX_TRANSIENT_RETRIES as usize;
    let exchange = Gnw::day()?.answering(&failing, &vec![UNAVAILABLE; tries]);
    poll(root.path(), RUN_AT, "first", &exchange)?;
    let (first, _) = exchange.sent();
    let exit = poll(root.path(), LATER_THE_SAME_DAY, "second", &exchange)?;
    let (second, _) = exchange.sent();
    let times = |sent: &[String]| sent.iter().filter(|url| **url == failing).count();
    assert_eq!(
        (times(&first), second, exit),
        (tries, vec![INDEX.to_owned(), failing.clone()], Exit::Ok)
    );
    Ok(())
}

/// Owner decision of 2026-11-15 (§10): a 408 is retried in-run and never held, so the next poll asks
/// again.
#[test]
fn a_document_answered_408_is_fetched_again_by_the_next_poll() -> TestResult {
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
    let timing_out = release_url(0)?;
    let tries = 1 + news_provider::MAX_TRANSIENT_RETRIES as usize;
    let exchange = Gnw::day()?.answering(&timing_out, &vec![REQUEST_TIMEOUT; tries]);
    poll(root.path(), RUN_AT, "first", &exchange)?;
    let (first, _) = exchange.sent();
    let exit = poll(root.path(), LATER_THE_SAME_DAY, "second", &exchange)?;
    let (second, _) = exchange.sent();
    let times = |sent: &[String]| sent.iter().filter(|url| **url == timing_out).count();
    assert_eq!(
        (times(&first), second, exit),
        (tries, vec![INDEX.to_owned(), timing_out.clone()], Exit::Ok)
    );
    Ok(())
}

/// Section 11.1: a page that yielded only unusable rows the PAGE decided is a usable verdict with
/// no item, and is held. Mutation 2026-11-15, the held read keeping its items arm only: red.
#[test]
fn a_document_that_yielded_only_unusable_rows_is_not_fetched_again() -> TestResult {
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
    let no_article = release_url(NO_ARTICLE)?;
    let exchange = Gnw::day()?;
    poll(root.path(), RUN_AT, "first", &exchange)?;
    let (first, _) = exchange.sent();
    poll(root.path(), LATER_THE_SAME_DAY, "second", &exchange)?;
    let (second, _) = exchange.sent();
    let db = Connection::open_with_flags(
        root.path().join(COMMIT_LOG),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let (key, _, _, _) = RELEASES[NO_ARTICLE];
    let story = digest(canon::digest::story_id(SOURCE, key)).ok_or("story id")?;
    let items: i64 = db.query_row(
        "SELECT COUNT(*) FROM items WHERE story_id = ?1",
        [&story],
        |row| row.get(0),
    )?;
    assert_eq!(
        (
            first.contains(&no_article),
            items,
            second.contains(&no_article)
        ),
        (true, 0, false)
    );
    Ok(())
}

/// A third poll of the same day, after `LATER_THE_SAME_DAY`.
const LATEST_THE_SAME_DAY: &str = "2026-11-14T19:10:00Z";
/// Release 0's pubDate, and the same instant with seconds, which §11.1 reads as no minute.
const LISTED: &str = "<pubDate>Sat, 14 Nov 2026 19:00 GMT</pubDate>";
const LISTED_WITH_SECONDS: &str = "<pubDate>Sat, 14 Nov 2026 19:00:00 GMT</pubDate>";

/// `Gnw::day`'s index with release 0's pubDate given seconds: NoParseableValidTime, decided by the
/// index row.
fn an_index_whose_first_listing_does_not_parse(gnw: &Gnw) -> Result<Vec<u8>, Box<dyn Error>> {
    let index = String::from_utf8(gnw.pages.get(INDEX).ok_or("no index")?.clone())?;
    if index.matches(LISTED).count() != 1 {
        return Err("release 0's pubDate is not in the index once".into());
    }
    Ok(index.replacen(LISTED, LISTED_WITH_SECONDS, 1).into_bytes())
}

/// The `valid_minute` of every item committed under `RELEASES[release]`'s story.
fn minutes_of(root: &Path, release: usize) -> Result<Vec<Option<String>>, Box<dyn Error>> {
    let db = Connection::open_with_flags(root.join(COMMIT_LOG), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let (key, _, _, _) = RELEASES.get(release).ok_or("no such release")?;
    let story = digest(canon::digest::story_id(SOURCE, key)).ok_or("story id")?;
    let mut statement = db.prepare("SELECT valid_minute FROM items WHERE story_id = ?1")?;
    let minutes = statement
        .query_map([&story], |row| row.get(0))?
        .collect::<Result<Vec<Option<String>>, _>>()?;
    Ok(minutes)
}

/// Section 11.1, owner decision of 2026-11-15 (§10): an unusable row the index row decided holds
/// nothing, so once the listing parses the next poll commits the story. Mutations in §8 row R16.
#[test]
fn a_document_whose_listed_time_did_not_parse_is_fetched_again_and_committed_once_the_listing_parses(
) -> TestResult {
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
    let release = release_url(0)?;
    let mut exchange = Gnw::day()?;
    let listed = exchange.pages.get(INDEX).ok_or("no index")?.clone();
    let unparsed = an_index_whose_first_listing_does_not_parse(&exchange)?;
    exchange.pages.insert(INDEX.to_owned(), unparsed);
    poll(root.path(), RUN_AT, "first", &exchange)?;
    let (first, _) = exchange.sent();
    let after_first = minutes_of(root.path(), 0)?;
    exchange.pages.insert(INDEX.to_owned(), listed);
    poll(root.path(), LATER_THE_SAME_DAY, "second", &exchange)?;
    let (second, _) = exchange.sent();
    assert_eq!(
        (
            first.contains(&release),
            after_first,
            second,
            minutes_of(root.path(), 0)?
        ),
        (
            true,
            Vec::new(),
            vec![INDEX.to_owned(), release.clone()],
            vec![Some("2026-11-14T19:00:00.000000Z".to_owned())]
        )
    );
    Ok(())
}

/// The cost of the rule above (§11.1): a listing that never parses is fetched by every poll while
/// the index names it. Mutations in §8 row R16.
#[test]
fn a_story_whose_listing_stays_unparseable_is_fetched_by_every_poll() -> TestResult {
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
    let release = release_url(0)?;
    let mut exchange = Gnw::day()?;
    let unparsed = an_index_whose_first_listing_does_not_parse(&exchange)?;
    exchange.pages.insert(INDEX.to_owned(), unparsed);
    let mut fetched = Vec::new();
    for (at, schedule) in [
        (RUN_AT, "first"),
        (LATER_THE_SAME_DAY, "second"),
        (LATEST_THE_SAME_DAY, "third"),
    ] {
        poll(root.path(), at, schedule, &exchange)?;
        let (sent, _) = exchange.sent();
        fetched.push(sent.iter().filter(|url| **url == release).count());
    }
    assert_eq!(
        (fetched, minutes_of(root.path(), 0)?),
        (vec![1, 1, 1], Vec::new())
    );
    Ok(())
}

/// A real store whose first `commit` of a run refuses: the index attempt, since a live poll
/// opens no other attempt before it.
struct RefusingTheIndex {
    store: Committer,
    commits: RefCell<usize>,
}

impl Sink for RefusingTheIndex {
    fn write(&mut self, event: &AttemptEvent) -> Result<(), LedgerError> {
        self.store.write(event)
    }
}

impl Archive for RefusingTheIndex {
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
        if self.commits.replace_with(|nth| *nth + 1) == 0 {
            return Err(ArchiveError {
                act: "commit",
                detail: "refused the index".to_owned(),
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
    fn record_relisted(
        &mut self,
        attempt_id: &str,
        relisted: &[news_provider::Relisted],
    ) -> Result<(), ArchiveError> {
        Archive::record_relisted(&mut self.store, attempt_id, relisted)
    }
}

/// Section 3.2 `held`: an index whose commit the store refused stays open with no `vendor_rows`,
/// so it records no `held` either. Mutation 2026-11-15, collect.rs recording `held` whatever the
/// commit answered: red.
#[test]
fn a_live_index_whose_commit_was_refused_records_no_held() -> TestResult {
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
    let exchange = Gnw::day()?;
    poll(root.path(), RUN_AT, "first", &exchange)?;
    let exit = polled_through_a_refused_index(root.path(), &exchange)?;
    let db = Connection::open_with_flags(
        root.path().join(COMMIT_LOG),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let refused: (Option<i64>, Option<i64>) = db.query_row(
        "SELECT vendor_rows, held FROM attempts WHERE story_id IS NULL
         ORDER BY opened_at DESC, attempt_id DESC LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!((exit, refused), (Exit::CommitFailed, (None, None)));
    Ok(())
}

/// Section 8 row R15, decision 10: an index whose commit was refused re-lists committed release 0
/// under another minute and records no `relisted` row and no `relisted_compared` on its open
/// attempt. Mutation 2026-11-16, a refused index recording an empty comparison: red.
#[test]
fn a_live_index_whose_commit_was_refused_records_no_relisted() -> TestResult {
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
    let mut exchange = Gnw::day()?;
    poll(root.path(), RUN_AT, "first", &exchange)?;
    let relisting = an_index_relisting_release_0(&exchange)?;
    exchange.pages.insert(INDEX.to_owned(), relisting);
    let exit = polled_through_a_refused_index(root.path(), &exchange)?;
    let db = Connection::open_with_flags(
        root.path().join(COMMIT_LOG),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let relisted: i64 = db.query_row("SELECT COUNT(*) FROM relisted", [], |row| row.get(0))?;
    let compared: Option<i64> = db.query_row(
        "SELECT relisted_compared FROM attempts WHERE story_id IS NULL
         ORDER BY opened_at DESC, attempt_id DESC LIMIT 1",
        [],
        |row| row.get(0),
    )?;
    assert_eq!((exit, relisted, compared), (Exit::CommitFailed, 0, None));
    Ok(())
}

/// A second poll, later the same day, through a store that refuses its index commit.
fn polled_through_a_refused_index(root: &Path, exchange: &Gnw) -> Result<Exit, Box<dyn Error>> {
    let ask = Ask {
        at: instant(LATER_THE_SAME_DAY),
        schedule: Some("refused".to_owned()),
        ..ask(root, cmd_collect::TODAY)
    };
    let opened = cmd_collect::open_provider(&ask)?;
    let run = RunId::new("refused");
    let mut archive = RefusingTheIndex {
        store: Committer::open(root, &run)?,
        commits: RefCell::new(0),
    };
    let driven = cmd_collect::drive(
        &ask,
        opened.provider.as_ref(),
        &mut archive,
        exchange,
        &At(instant(LATER_THE_SAME_DAY)),
        &run,
    )?;
    Ok(driven.collected.exit())
}

const EXIT_OK: i32 = 0;
/// Past both polls, so the ledger reads every row they wrote.
const LEDGER_AT: &str = "2026-11-16T00:00:00Z";
/// Prefix-compared against stored stamps, which carry microseconds the clock constants lack.
const NEXT_DAY_DATE: &str = "2026-11-15";
/// After both robots seeds and at or before the first poll's index row.
const FIRST_POLL_HOUR: &str = "2026-11-14T19";

/// Run 1 commits the day's three stories under 2026-11-14; run 2, on 2026-11-15, lists the same
/// three and finds every one held.
fn an_all_held_next_day(store: &Path) -> TestResult {
    seed_gnw(store, &hosts(), ROBOTS_EARLIER)?;
    let exchange = Gnw::day()?;
    poll(store, RUN_AT, "first", &exchange)?;
    poll(store, THE_NEXT_DAY, "second", &exchange)?;
    Ok(())
}

/// `(vendor_rows, held, rows_committed)` per source of 2026-11-15, the check's finding codes from
/// the library read, and the binary's `--check` exit.
type NextDayLedger = (Vec<(i64, i64, i64)>, Vec<&'static str>, Option<i32>);

fn ledger_of_the_next_day(store: &Path) -> Result<NextDayLedger, Box<dyn Error>> {
    let day: chrono::NaiveDate = NEXT_DAY_DATE.parse()?;
    let report = newsvault::cmd_ledger::read(
        store,
        &newsvault::cmd_ledger::Ask {
            at: instant(LEDGER_AT),
            from: day,
            to: day,
            source: None,
            expected_runs: None,
            limit: None,
            check: true,
        },
    )?;
    let counts = report
        .days
        .iter()
        .flat_map(|one| &one.per_source)
        .map(|row| (row.vendor_rows, row.held, row.rows_committed))
        .collect();
    let codes = report
        .check
        .findings
        .iter()
        .map(|finding| finding.code)
        .collect();
    let exit = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("ledger")
        .args(["--store", &store.to_string_lossy()])
        .args([
            "--at",
            LEDGER_AT,
            "--from",
            NEXT_DAY_DATE,
            "--to",
            NEXT_DAY_DATE,
        ])
        .args(["--check", "--format", "json"])
        .output()?
        .status
        .code();
    Ok((counts, codes, exit))
}

/// Section 11.4, §8 R4: an all-held day commits nothing against three vendor rows and is not a
/// finding, in the library read and in the binary's exit. Mutations 2026-11-15, the `- row.held`
/// subtraction removed and `record_held` made a no-op: red.
#[test]
fn a_quiet_day_whose_every_listed_story_is_held_passes_ledger_check() -> TestResult {
    let root = TempDir::new()?;
    an_all_held_next_day(root.path())?;
    assert_eq!(
        ledger_of_the_next_day(root.path())?,
        (vec![(3, 3, 0)], Vec::new(), Some(EXIT_OK))
    );
    Ok(())
}

/// §8 R4: `held` adds up over the day's runs as `vendor_rows` does. Mutation 2026-11-15, cmd_ledger
/// `row.held += entry.held` made `=`: red.
#[test]
fn two_all_held_polls_of_one_day_hold_every_vendor_row_of_both() -> TestResult {
    let root = TempDir::new()?;
    an_all_held_next_day(root.path())?;
    poll(root.path(), LATER_THE_NEXT_DAY, "third", &Gnw::day()?)?;
    assert_eq!(
        ledger_of_the_next_day(root.path())?,
        (vec![(6, 6, 0)], Vec::new(), Some(EXIT_OK))
    );
    Ok(())
}

/// §8 R4: each poll of 2026-11-15 refetches release 0, whose listing never parses, and the ledger
/// counts both fetches. The refetch is a vendor row nothing held or committed, so `--check` says
/// no. Mutations in §8 row R4.
#[test]
fn the_ledger_counts_every_fetch_of_a_story_whose_listing_stayed_unparseable() -> TestResult {
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
    let mut exchange = Gnw::day()?;
    let unparsed = an_index_whose_first_listing_does_not_parse(&exchange)?;
    exchange.pages.insert(INDEX.to_owned(), unparsed);
    poll(root.path(), RUN_AT, "first", &exchange)?;
    poll(root.path(), THE_NEXT_DAY, "second", &exchange)?;
    poll(root.path(), LATER_THE_NEXT_DAY, "third", &exchange)?;
    let day: chrono::NaiveDate = NEXT_DAY_DATE.parse()?;
    let report = newsvault::cmd_ledger::read(
        root.path(),
        &newsvault::cmd_ledger::Ask {
            at: instant(LEDGER_AT),
            from: day,
            to: day,
            source: None,
            expected_runs: None,
            limit: None,
            check: true,
        },
    )?;
    let counts: Vec<(i64, i64, i64, i64)> = report
        .days
        .iter()
        .flat_map(|one| &one.per_source)
        .map(|row| {
            (
                row.vendor_rows,
                row.held,
                row.listing_unusable,
                row.rows_committed,
            )
        })
        .collect();
    let findings: Vec<(&str, &str)> = report
        .check
        .findings
        .iter()
        .map(|finding| (finding.code, finding.detail.as_str()))
        .collect();
    assert_eq!(
        (counts, findings),
        (
            vec![(8, 4, 2, 0)],
            vec![(
                "rows_committed_zero_against_vendor_rows",
                "8 vendor rows, 4 held, and no item row committed"
            )]
        )
    );
    Ok(())
}

/// Section 11.1, `attempts.held`: `matched - issued` on a live index row, 0 included, and NULL on
/// a Document row. Mutation 2026-11-15, `record_held` made a no-op: red.
#[test]
fn an_all_held_index_attempt_records_its_held_count() -> TestResult {
    let root = TempDir::new()?;
    an_all_held_next_day(root.path())?;
    let db = Connection::open_with_flags(
        root.path().join(COMMIT_LOG),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let held_of = |filter: &str| -> Result<Vec<Option<i64>>, rusqlite::Error> {
        db.prepare(&format!(
            "SELECT held FROM attempts WHERE {filter} ORDER BY opened_at, attempt_id"
        ))?
        .query_map([NEXT_DAY_DATE, FIRST_POLL_HOUR], |row| row.get(0))?
        .collect()
    };
    let next_day_rows = held_of("opened_at > ?1 AND ?2 IS NOT NULL")?;
    let first_index = held_of("story_id IS NULL AND opened_at >= ?2 AND opened_at < ?1")?;
    let documents = held_of("story_id IS NOT NULL AND ?1 IS NOT NULL AND ?2 IS NOT NULL")?;
    assert_eq!(
        (next_day_rows, first_index, documents),
        (vec![Some(3)], vec![Some(0)], vec![None; RELEASES.len()])
    );
    Ok(())
}

/// Release 0's pubDate moved five minutes later, as a re-listing under the same link states it.
const RELISTED: &str = "<pubDate>Sat, 14 Nov 2026 19:05 GMT</pubDate>";

/// The day's index with release 0 listed under `RELISTED` instead of its own pubDate.
fn an_index_relisting_release_0(gnw: &Gnw) -> Result<Vec<u8>, Box<dyn Error>> {
    let index = String::from_utf8(gnw.pages.get(INDEX).ok_or("no index")?.clone())?;
    if index.matches(LISTED).count() != 1 {
        return Err("release 0's pubDate is not in the index once".into());
    }
    Ok(index.replacen(LISTED, RELISTED, 1).into_bytes())
}

/// Decision x: of the second poll's 3 held releases, 0 (differing) and 1 (equal) are compared;
/// release 2's page has no article container, so no item holds a minute to compare with.
const COMPARED_BY_THE_SECOND_POLL: i64 = 2;

/// Section 8 row R15, owner decision 10 (2026-11-16): a second poll lists committed release 0
/// under another minute, as PRN's correction 302913655 was. No fetch, the `stored_items` columns
/// unchanged, one `relisted` row on the second index attempt, and the ledger counts it and the
/// comparisons made (decision x; mutation 2026-11-16, `compared` counting differences only: red).
#[test]
fn a_held_story_listed_under_another_minute_is_recorded_without_a_fetch_and_counted() -> TestResult
{
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts(), ROBOTS_EARLIER)?;
    let mut exchange = Gnw::day()?;
    poll(root.path(), RUN_AT, "first", &exchange)?;
    exchange.sent();
    let db = Connection::open_with_flags(
        root.path().join(COMMIT_LOG),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let before = stored_items(&db)?;
    let relisting = an_index_relisting_release_0(&exchange)?;
    exchange.pages.insert(INDEX.to_owned(), relisting);
    poll(root.path(), LATER_THE_SAME_DAY, "second", &exchange)?;
    let (second, _) = exchange.sent();
    let recorded: Vec<(String, String, String, String, String, String)> = db
        .prepare(
            "SELECT p.url, a.run_id, r.story_id, r.listed_raw, r.listed_minute, r.stored_minute
               FROM relisted r JOIN attempts a ON a.attempt_id = r.attempt_id
               JOIN payloads p ON p.attempt_id = r.attempt_id",
        )?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    let second_run: String = db.query_row(
        "SELECT run_id FROM attempts ORDER BY opened_at DESC, attempt_id LIMIT 1",
        [],
        |row| row.get(0),
    )?;
    let (key, _, _, _) = RELEASES[0];
    let story = digest(canon::digest::story_id(SOURCE, key)).ok_or("story id")?;
    let day: chrono::NaiveDate = "2026-11-14".parse()?;
    let report = newsvault::cmd_ledger::read(
        root.path(),
        &newsvault::cmd_ledger::Ask {
            at: instant(LEDGER_AT),
            from: day,
            to: day,
            source: None,
            expected_runs: None,
            limit: None,
            check: false,
        },
    )?;
    let counted: Vec<(i64, i64)> = report
        .days
        .iter()
        .flat_map(|one| &one.per_source)
        .map(|row| (row.relisted_disputed, row.relisted_compared))
        .collect();
    assert_eq!(
        (second, stored_items(&db)?, recorded, counted),
        (
            vec![INDEX.to_owned()],
            before,
            vec![(
                INDEX.to_owned(),
                second_run,
                story,
                "Sat, 14 Nov 2026 19:05 GMT".to_owned(),
                "2026-11-14T19:05:00.000000Z".to_owned(),
                "2026-11-14T19:00:00.000000Z".to_owned()
            )],
            vec![(1, COMPARED_BY_THE_SECOND_POLL)]
        )
    );
    Ok(())
}
