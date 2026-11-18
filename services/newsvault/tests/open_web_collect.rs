//! `newsvault collect --source open_web_wires` from plan to committed items, over the GNW fixtures
//! in `providers/open-web-feed/tests/fixtures/gnw` (architecture.md section 11.5 step 2).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use news_provider::provider::RawBlob;
use news_provider::{
    Archive, ArchiveError, AttemptEvent, AttemptReceipt, Clock, CollectedItem, Committed, Exchange,
    Exit, Headers, LedgerError, PlanRefusal, RawResponse, Request, RunId, SendFailure, SendPermit,
    Sink, SourceId, TargetWindow, TransportError,
};
use newsvault::cmd_collect::{self, Ask, CollectRunError};
use newsvault::cmd_items::{self, PerStory};
use newsvault::store::{Committer, StoreError, COMMIT_LOG, RUNS_DIR, SCHEMA_VERSION};
use open_web_feed::{OpenWebFeed, GENERATION_HEADER};
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

/// PR Newswire's paged sitemap (section 11.2). `prn/sitemap_news_4_urls.xml` lists release
/// 302939742 and then `PRN_RELEASES`, each under its sitemap time.
const PRN_SITEMAP: &str = "https://www.prnewswire.com/sitemap-news.xml?page=1";
const PRN_SITEMAP_PAGE_2: &str = "https://www.prnewswire.com/sitemap-news.xml?page=2";
const PRN_SITEMAP_PAGE_3: &str = "https://www.prnewswire.com/sitemap-news.xml?page=3";
const PRN_RELEASE_PREFIX: &str = "https://www.prnewswire.com/news-releases/";
const PRN_SITEMAP_ONLY: &str = "www.prnewswire.com/news-releases/stark-future-closes-financing-round-in-excess-of-150-million-in-total-transactional-value-as-its-electric-motorcycles-overtake-combustion-on-the-world-stage-302939742.html";
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
const FED_INDEX: &str = "https://www.federalreserve.gov/feeds/press_all.xml";
/// `fed/press_all_3_items.xml`'s three links with the page each is answered with. The FOMC
/// schedule's page is not kept, so monetary20261112a is answered with the synthetic page that
/// has no `div#article`.
const FED_RELEASES: [(&str, &str); 3] = [
    (
        "www.federalreserve.gov/newsevents/pressreleases/bcreg20261113a.htm",
        "article_bcreg20261113a.html",
    ),
    (
        "www.federalreserve.gov/newsevents/pressreleases/monetary20261112a.htm",
        "synthetic_no_article_block.html",
    ),
    (
        "www.federalreserve.gov/newsevents/pressreleases/orders20261109a.htm",
        "article_orders20261109a.html",
    ),
];
const YAHOO_INDEX: &str = "https://finance.yahoo.com/news-sitemap.xml";
/// `yahoo/news_sitemap_8_urls.xml`'s two followed links, `business-wire.com` then
/// `accesswire.ca`, with the page each is answered with. No `accesswire.ca` page was fetched, so
/// that row is answered with the synthetic page that has no article body.
const YAHOO_RELEASES: [(&str, &str); 2] = [
    (
        "finance.yahoo.com/healthcare/articles/laverock-therapeutics-appoints-parker-moss-091700844.html",
        "article_laverock_091700844.html",
    ),
    (
        "finance.yahoo.com/technology/ai/articles/reo-dev-launches-100-ai-084000068.html",
        "synthetic_no_article_body.html",
    ),
];
/// Monday 2026-11-16, after every fixture's publication minute (Yahoo's 09:17Z newest) and at the
/// Yahoo fixture's own poll; one second before it, the stub's `received_at`.
const MONDAY_AT: &str = "2026-11-16T09:30:30Z";
const MONDAY_RECEIVED_AT: &str = "2026-11-16T09:30:29.000000Z";

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
    /// `(url prefix, page)`: what a url no page answers is answered with when it starts so.
    fallback: Option<(String, Vec<u8>)>,
    /// The `x-sitemap-generated-at` a url's page is answered with, when it has one.
    stamps: BTreeMap<String, String>,
    /// The `location` a url's scripted 3xx carries, with `MOVED_BODY` as its body.
    locations: BTreeMap<String, String>,
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
            fallback: None,
            stamps: BTreeMap::new(),
            locations: BTreeMap::new(),
        })
    }

    /// GNW's index answered with `fixtures/gnw/<rss>`, and every release url, listed or walked,
    /// with probe10's walked page of 3404438 (12:00:00Z).
    fn walked(rss: &str) -> Result<Self, Box<dyn Error>> {
        let fixture =
            |name: &str| std::fs::read(provider_dir().join("tests/fixtures/gnw").join(name));
        Ok(Self {
            pages: BTreeMap::from([(INDEX.to_owned(), fixture(rss)?)]),
            asked: RefCell::new(Vec::new()),
            scripted: RefCell::new(BTreeMap::new()),
            fallback: Some((RELEASE_PREFIX.to_owned(), fixture(WALKED_PAGE)?)),
            stamps: BTreeMap::new(),
            locations: BTreeMap::new(),
        })
    }

    /// `url` answered once with a 301 to `location`.
    fn moved(mut self, url: &str, location: &str) -> Self {
        self.locations.insert(url.to_owned(), location.to_owned());
        self.answering(url, &[MOVED_PERMANENTLY])
    }

    /// `five_hosts`, PRN's sitemap pages answered from `fixtures/prn` (`(url, file)`), and every
    /// other PRN release with the synthetic page that has no `section.release-body`.
    fn paged(sitemap: &[(&str, &str)]) -> Result<Self, Box<dyn Error>> {
        let prn = |name: &str| std::fs::read(provider_dir().join("tests/fixtures/prn").join(name));
        let mut all = Self::five_hosts()?;
        for (url, file) in sitemap {
            all.pages.insert((*url).to_owned(), prn(file)?);
        }
        all.fallback = Some((
            PRN_RELEASE_PREFIX.to_owned(),
            prn("synthetic_no_release_body.html")?,
        ));
        Ok(all)
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

    /// `three_hosts`, and the Fed's feed and `FED_RELEASES` answered from `fixtures/fed`.
    fn four_hosts() -> Result<Self, Box<dyn Error>> {
        let mut all = Self::three_hosts()?;
        let fixture =
            |name: &str| std::fs::read(provider_dir().join("tests/fixtures/fed").join(name));
        all.pages
            .insert(FED_INDEX.to_owned(), fixture("press_all_3_items.xml")?);
        for (key, page) in FED_RELEASES {
            all.pages.insert(format!("https://{key}"), fixture(page)?);
        }
        Ok(all)
    }

    /// `four_hosts`, Yahoo's sitemap and `YAHOO_RELEASES` answered from `fixtures/yahoo`, and PR
    /// Newswire's sitemap page 1 and its one sitemap-only release from `fixtures/prn`.
    fn five_hosts() -> Result<Self, Box<dyn Error>> {
        let mut all = Self::four_hosts()?;
        let fixture =
            |name: &str| std::fs::read(provider_dir().join("tests/fixtures/yahoo").join(name));
        all.pages
            .insert(YAHOO_INDEX.to_owned(), fixture("news_sitemap_8_urls.xml")?);
        for (key, page) in YAHOO_RELEASES {
            all.pages.insert(format!("https://{key}"), fixture(page)?);
        }
        let prn = |name: &str| std::fs::read(provider_dir().join("tests/fixtures/prn").join(name));
        all.pages
            .insert(PRN_SITEMAP.to_owned(), prn("sitemap_news_4_urls.xml")?);
        all.pages.insert(
            format!("https://{PRN_SITEMAP_ONLY}"),
            prn("article_302939742.html")?,
        );
        Ok(all)
    }

    /// Each `(url, stamp)`'s page answered with `stamp` as its `x-sitemap-generated-at`.
    fn stamped(mut self, stamps: &[(&str, &str)]) -> Self {
        for (url, stamp) in stamps {
            self.stamps.insert((*url).to_owned(), (*stamp).to_owned());
        }
        self
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
                return Ok(match self.locations.get(&request.url) {
                    Some(location) => {
                        RawResponse::new(attempt, status, MOVED_BODY.to_vec(), sent, received)
                            .with_headers(Headers::new(vec![(
                                LOCATION.to_owned(),
                                location.clone(),
                            )]))
                    }
                    None => RawResponse::new(attempt, status, Vec::new(), sent, received),
                });
            }
        }
        let fallback = self
            .fallback
            .as_ref()
            .filter(|(prefix, _)| request.url.starts_with(prefix.as_str()))
            .map(|(_, page)| page);
        let stamp = self
            .stamps
            .get(&request.url)
            .map(|stamp| (GENERATION_HEADER.to_owned(), stamp.clone()));
        let headers = [(CONTENT_TYPE.to_owned(), PAGE_TYPE.to_owned())]
            .into_iter()
            .chain(stamp)
            .collect();
        match self.pages.get(&request.url).or(fallback) {
            Some(body) => Ok(RawResponse::new(attempt, OK, body.clone(), sent, received)
                .with_headers(Headers::new(headers))),
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

/// GNW's host keys a story by `(release id, language)` since the id walk (section 11.2): its
/// `<host>/news-release/<y>/<m>/<d>/<id>/<n>/<language>/<slug>` key becomes
/// `<host>/news-release/<id>/<language>`; any other key is its host and path.
fn keyed(key: &str) -> String {
    let parts: Vec<&str> = key.split('/').collect();
    match parts.as_slice() {
        ["rss.globenewswire.com", "news-release", _, _, _, id, _, language, _] => {
            format!("rss.globenewswire.com/news-release/{id}/{language}")
        }
        _ => key.to_owned(),
    }
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

/// `(item_id, raw_blob, body_blob)` of a committed row: `item_id` hashes the title and body.
type Content = (String, String, Option<String>);

fn stored_content(db: &Connection) -> Result<BTreeMap<String, Content>, Box<dyn Error>> {
    let mut query = db.prepare("SELECT story_id, item_id, raw_blob, body_blob FROM items")?;
    let rows = query
        .query_map([], |row| {
            Ok((row.get(0)?, (row.get(1)?, row.get(2)?, row.get(3)?)))
        })?
        .collect::<Result<BTreeMap<String, Content>, _>>()?;
    Ok(rows)
}

/// The content the release keyed `key` must be stored with when answered by `fixtures/<dir>/<page>`:
/// that page's bytes as `raw_blob`, its `golden_bodies.json` body as `body_blob`, and an `item_id`
/// over `title` and that body. The minute is the stored one, which `stored_items` checks.
fn committed_content(
    (dir, key, page, title): (&str, &str, &str, &str),
    golden: &Value,
    stored: &BTreeMap<String, Row>,
) -> Result<(String, Content), Box<dyn Error>> {
    let story = digest(canon::digest::story_id(SOURCE, &keyed(key))).ok_or("story id")?;
    let raw = std::fs::read(provider_dir().join("tests/fixtures").join(dir).join(page))?;
    let body = golden["bodies"][format!("{dir}/{page}")]
        .as_str()
        .ok_or("no golden body")?;
    let minute: DateTime<Utc> = stored
        .get(&story)
        .and_then(|row| row.1.as_deref())
        .ok_or("no stored minute")?
        .parse()?;
    let item = digest(canon::digest::item_id(&canon::digest::ItemFields {
        source: SOURCE,
        source_item_id: &keyed(key),
        url: &format!("https://{key}"),
        valid: canon::Stamp::Minute(minute),
        title,
        body_ref: Some(body),
    }))
    .ok_or("item id")?;
    Ok((
        story,
        (
            item,
            canon::FileDigest::of(&raw).hex().to_owned(),
            Some(body.to_owned()),
        ),
    ))
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
                digest(canon::digest::story_id(SOURCE, &keyed(key)))?,
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
    let story = digest(canon::digest::story_id(SOURCE, &keyed(key))).ok_or("story id")?;
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

/// The committed row of `FED_RELEASES[release]`: no Fed page names a canonical link, so its
/// `doc_id` is the requested url.
fn fed_item(
    release: usize,
    dated: (&str, &str),
    received: &str,
) -> Result<(String, Row), Box<dyn Error>> {
    let (key, _) = FED_RELEASES.get(release).ok_or("no such release")?;
    own_link_item(key, "pubDate", dated, received)
}

/// The committed row of `YAHOO_RELEASES[release]`, whose canonical is its own link.
fn yahoo_item(
    release: usize,
    dated: (&str, &str),
    received: &str,
) -> Result<(String, Row), Box<dyn Error>> {
    let (key, _) = YAHOO_RELEASES.get(release).ok_or("no such release")?;
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

/// Section 11.2, the committed hosts file on Monday 2026-11-16: five indexes, PRN's sitemap page 1
/// (the RSS releases and 302939742), then each host's pages, each release fetched once. The
/// mutations that turn it red are §11.5 step 6's tables.
#[test]
fn an_open_web_run_collects_a_gnw_a_prn_a_cnw_a_fed_and_a_yahoo_day_end_to_end() -> TestResult {
    const ATTEMPTS: usize = 21;
    const ITEMS: usize = 10;
    let root = TempDir::new()?;
    seed::refresh_committed(root.path(), ROBOTS_EARLIER)?;
    let ask = Ask {
        hosts: Some(seed::committed_hosts()),
        at: instant(MONDAY_AT),
        ..ask(root.path(), cmd_collect::TODAY)
    };
    let opened = cmd_collect::open_provider(&ask)?;
    let exchange = Gnw::five_hosts()?;
    let answer = cmd_collect::run(&ask, &opened, &exchange, &Monday)?;
    let envelope = &answer.envelope;
    let attempts = envelope["attempts"].as_array().ok_or("attempts")?;
    let hosts_version = canon::FileDigest::of(&std::fs::read(seed::committed_hosts())?);
    // Every index is asked before any page.
    let indexes: Vec<Option<&str>> = [0, 1, 2, 3, 4]
        .map(|nth| attempts[nth]["url"].as_str())
        .into();
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
            vec![
                Some(INDEX),
                Some(PRN_INDEX),
                Some(CNW_INDEX),
                Some(FED_INDEX),
                Some(YAHOO_INDEX)
            ],
            Some(ITEMS as u64)
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
    for index in [INDEX, PRN_INDEX, CNW_INDEX, FED_INDEX] {
        expected_counts.insert(index.to_owned(), (Some(3), Some(0), Some(0)));
    }
    expected_counts.insert(YAHOO_INDEX.to_owned(), (Some(8), Some(0), Some(0)));
    expected_counts.insert(PRN_SITEMAP.to_owned(), (Some(4), Some(0), Some(0)));
    expected_counts.insert(
        format!("https://{PRN_SITEMAP_ONLY}"),
        (Some(1), Some(0), Some(1)),
    );
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
    let fed_pages = FED_RELEASES
        .iter()
        .enumerate()
        .map(|(nth, (key, _))| (*key, nth != 1));
    let yahoo_pages = YAHOO_RELEASES
        .iter()
        .enumerate()
        .map(|(nth, (key, _))| (*key, nth != 1));
    for (key, committed) in gnw_pages
        .chain(prn_pages)
        .chain(cnw_pages)
        .chain(fed_pages)
        .chain(yahoo_pages)
    {
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
        // A day before the 10th is printed unpadded, and read as that day.
        fed_item(
            0,
            (
                "2026-11-13T21:15:00.000000Z",
                "Fri, 13 Nov 2026 21:15:00 GMT",
            ),
            MONDAY_RECEIVED_AT,
        )?,
        fed_item(
            2,
            (
                "2026-11-09T21:00:00.000000Z",
                "Mon, 9 Nov 2026 21:00:00 GMT",
            ),
            MONDAY_RECEIVED_AT,
        )?,
        yahoo_item(
            0,
            ("2026-11-16T09:17:00.000000Z", "2026-11-16T09:17:00Z"),
            MONDAY_RECEIVED_AT,
        )?,
        // Listed by the sitemap alone, so dated by its `news:publication_date`.
        own_link_item(
            PRN_SITEMAP_ONLY,
            "news:publication_date",
            ("2026-11-16T08:00:00.000000Z", "2026-11-16T03:00:00-05:00"),
            MONDAY_RECEIVED_AT,
        )?,
    ]);
    let stored = stored_items(&db)?;
    assert_eq!(stored, expected);
    // Each story holds the page its url was answered with. Mutations 2026-11-17, the stub's pages
    // swapped for Fed 0/2 (green before this assertion), PRN 1/2 and CNW 0/1: each red.
    let golden: Value = serde_json::from_slice(&std::fs::read(
        provider_dir().join("tests/fixtures/golden_bodies.json"),
    )?)?;
    // The page and title are written out, not read from the stub's tables they check.
    let committed_pages = [
        (
            "gnw",
            RELEASES[0].0,
            "article_3403075.html",
            "USTA FLORIDA RELEASES 2026 STATE OF FLORIDA TENNIS REPORT",
        ),
        (
            "gnw",
            RELEASES[1].0,
            "synthetic_canonical_differs.html",
            "Synthetic Release",
        ),
        (
            "prn",
            PRN_RELEASES[1].0,
            "article_302939553.html",
            "Novo's Wegovy® was associated with a reduction in food noise according to real-world studies at ObesityWeek® 2026",
        ),
        (
            "prn",
            PRN_RELEASES[2].0,
            "article_302938569.html",
            "Lilly to present new data across Zepbound, Foundayo, EloraTZP and retatrutide at ObesityWeek 2026, moving obesity care beyond the scale",
        ),
        (
            "cnw",
            CNW_RELEASES[0].0,
            "article_805423040.html",
            "Global Voltage, Travel-Ready Protection: ELEGRP Powers Dyson Supersonic Travel hair dryer",
        ),
        (
            "cnw",
            CNW_RELEASES[1].0,
            "article_823884942.html",
            "Segway Powersports Secures Class 2 One-Two Finish on SCORE Baja 1000 Debut",
        ),
        (
            "fed",
            FED_RELEASES[0].0,
            "article_bcreg20261113a.html",
            "Agencies extend comment period for proposed third-party risk management guidance",
        ),
        (
            "fed",
            FED_RELEASES[2].0,
            "article_orders20261109a.html",
            "Federal Reserve Board announces approval of two applications by Mutual Federal Bancorp, MHC, and MFB Bancorp, Inc.",
        ),
        (
            "yahoo",
            YAHOO_RELEASES[0].0,
            "article_laverock_091700844.html",
            "Laverock Therapeutics Appoints Parker Moss as Chief Executive Officer and Paul J. Sekhri as Executive Chairman",
        ),
        (
            "prn",
            PRN_SITEMAP_ONLY,
            "article_302939742.html",
            "Stark Future Closes Financing Round in Excess of €150 Million in Total Transactional Value as Its Electric Motorcycles Overtake Combustion on the World Stage",
        ),
    ];
    let expected_content = committed_pages
        .into_iter()
        .map(|page| committed_content(page, &golden, &stored))
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    assert_eq!(stored_content(&db)?, expected_content);
    // Nothing is visible before the run observed it, and all after.
    assert_eq!(
        (
            visible(root.path(), "2026-11-16T09:30:00Z")?,
            visible(root.path(), "2026-11-16T09:31:00Z")?
        ),
        (0, ITEMS)
    );
    // A fresh store's sitemap poll is `first_poll` at page 1, and holds the three releases the RSS
    // listed first. Mutation 2026-11-17, the run's issued set left out of `collect`'s held
    // stories: red (each shared release asked twice).
    let sitemap_attempt = attempts
        .iter()
        .find(|row| row["url"] == PRN_SITEMAP)
        .and_then(|row| row["attempt_id"].as_str())
        .ok_or("no sitemap attempt")?;
    let (continuity, held): (Option<String>, Option<i64>) = db.query_row(
        "SELECT continuity, held FROM attempts WHERE attempt_id = ?1",
        [sitemap_attempt],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (sent, _) = exchange.sent();
    let asked: Vec<usize> = PRN_RELEASES
        .iter()
        .map(|(key, _)| key)
        .chain([&PRN_SITEMAP_ONLY])
        .map(|key| {
            sent.iter()
                .filter(|url| **url == format!("https://{key}"))
                .count()
        })
        .collect();
    assert_eq!(
        (&envelope["continuity"], continuity.as_deref(), held, asked),
        (
            &serde_json::json!([{
                "index_url": PRN_SITEMAP,
                "attempt_id": sitemap_attempt,
                "pages_read": [1],
                "basis_keys": null,
                "continuity": "first_poll",
                "boundaries": {
                    "same_generation": 0,
                    "older_generation": 0,
                    "newer_generation": 0,
                    "generation_unknown": 0,
                },
                "error": null,
            }]),
            Some("first_poll"),
            Some(3),
            vec![1, 1, 1, 1],
        )
    );
    // Option C on a fresh store: the first direct answer is this run's, after every keyed row's
    // minute, so the three cross-check rows are counted and none is compared.
    let checked = &envelope["cross_check"];
    assert_eq!(
        (
            checked["index_url"].as_str(),
            checked["watched_from"].as_str(),
            checked["direct_payloads_read"].as_u64(),
            checked["direct_payloads_parsed"].as_u64(),
            &checked["result"]["rows"],
            checked["result"]["not_compared"].as_u64(),
            checked["result"]["unlisted_count"].as_u64(),
            checked["error"].is_null(),
        ),
        (
            Some(YAHOO_INDEX),
            Some(MONDAY_RECEIVED_AT),
            Some(4),
            Some(4),
            &serde_json::json!({"followed": 2, "cross_checked": 3, "other_key": 2, "no_key": 1}),
            Some(3),
            Some(0),
            true,
        ),
        "{checked:#}"
    );
    // The human rendering carries the same counts; a dry run looks at nothing, so it has none.
    // Review 2026-11-17: the human line dropped, and a dry run's `{}`, each green before this.
    let mut human = Vec::new();
    cmd_collect::render_human(envelope, &mut human)?;
    let dry = Ask {
        dry_run: true,
        ..ask.clone()
    };
    let planned = cmd_collect::run(&dry, &opened, &Gnw::five_hosts()?, &Monday)?;
    let human = String::from_utf8(human)?;
    let line = |start: &str| {
        human
            .lines()
            .find(|line| line.starts_with(start))
            .map(str::to_owned)
    };
    assert_eq!(
        (
            line("cross-check"),
            line("continuity"),
            planned.envelope["cross_check"].is_null(),
            planned.envelope["continuity"].is_null(),
        ),
        (
            Some(
                "cross-check      0 unlisted directly, 0 listed, 3 not compared, 0 not looked"
                    .to_owned()
            ),
            Some(format!(
                "continuity       {PRN_SITEMAP} pages 1 first_poll same_generation 0 \
                 older_generation 0 newer_generation 0 generation_unknown 0"
            )),
            true,
            true,
        )
    );
    Ok(())
}

struct FixedAt(DateTime<Utc>);

impl Clock for FixedAt {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

/// A store over the committed five hosts with every robots fetch ledgered.
fn five_host_store() -> Result<(TempDir, PathBuf), Box<dyn Error>> {
    let root = TempDir::new()?;
    seed::refresh_committed(root.path(), ROBOTS_EARLIER)?;
    Ok((root, seed::committed_hosts()))
}

/// One collect at `at` over `hosts`: its exit and its envelope's `cross_check`.
fn cross_checked_at(
    root: &Path,
    hosts: &Path,
    at: &str,
    exchange: &Gnw,
) -> Result<(Exit, Value), Box<dyn Error>> {
    let ask = Ask {
        hosts: Some(hosts.to_path_buf()),
        at: instant(at),
        ..ask(root, cmd_collect::TODAY)
    };
    let opened = cmd_collect::open_provider(&ask)?;
    let answer = cmd_collect::run(&ask, &opened, exchange, &FixedAt(instant(at)))?;
    Ok((answer.exit, answer.envelope["cross_check"].clone()))
}

/// Two runs over the five hosts, Yahoo 503 in the first and retried in the second: two wire-key
/// rows no direct index listed are named and one is counted. The first run's four direct answers
/// are older than `read_from` and are not read. Mutations 2026-11-17: the `since` bound removed,
/// `watched_from` the newest direct answer, the first keyed attempt read: each red.
#[test]
fn a_yahoo_row_under_a_wire_key_no_direct_index_listed_is_named_in_the_run_envelope() -> TestResult
{
    const EARLY_AT: &str = "2026-11-16T05:00:30Z";
    let (root, hosts) = five_host_store()?;
    let run_at = |at: &str, exchange: &Gnw| {
        cross_checked_at(root.path(), &hosts, at, exchange).map(|(_, checked)| checked)
    };
    let early = run_at(
        EARLY_AT,
        &Gnw::five_hosts()?.answering(YAHOO_INDEX, &[UNAVAILABLE; 3]),
    )?;
    let checked = run_at(
        MONDAY_AT,
        &Gnw::five_hosts()?.answering(YAHOO_INDEX, &[UNAVAILABLE]),
    )?;
    let unlisted: Vec<(Option<&str>, Option<&str>)> = checked["result"]["unlisted"]
        .as_array()
        .ok_or("unlisted")?
        .iter()
        .map(|row| (row["key"].as_str(), row["url"].as_str()))
        .collect();
    assert_eq!(
        (
            early["result"].is_null(),
            checked["watched_from"].as_str(),
            checked["direct_payloads_read"].as_u64(),
            checked["result"]["not_compared"].as_u64(),
            checked["result"]["listed_directly"].as_u64(),
            unlisted,
        ),
        (
            true,
            Some("2026-11-16T05:00:29.000000Z"),
            Some(4),
            Some(0),
            Some(1),
            vec![
                (
                    Some("globenewswire.com"),
                    Some("https://finance.yahoo.com/energy/articles/geely-holding-announces-evsa-global-090500744.html")
                ),
                (
                    Some("prnewswire.com"),
                    Some("https://finance.yahoo.com/economy/articles/kyiv-based-horizon-capital-reaches-090000908.html")
                ),
            ],
        ),
        "{checked:#}"
    );
    Ok(())
}

/// A row's own index that did not answer after its minute has not looked for it: PRN 503 in the
/// second run leaves both PRN rows `not_looked`, the GNW row is still compared and named, and a
/// 503 body is read but never parsed. Review 2026-11-17: a CNW 503 named three rows unlisted.
#[test]
fn a_cross_check_row_whose_own_index_did_not_answer_after_is_not_looked() -> TestResult {
    let (root, hosts) = five_host_store()?;
    cross_checked_at(
        root.path(),
        &hosts,
        "2026-11-16T00:00:30Z",
        &Gnw::five_hosts()?.answering(YAHOO_INDEX, &[UNAVAILABLE; 3]),
    )?;
    let (_, checked) = cross_checked_at(
        root.path(),
        &hosts,
        MONDAY_AT,
        &Gnw::five_hosts()?.answering(PRN_INDEX, &[UNAVAILABLE; 3]),
    )?;
    let unlisted: Vec<Option<&str>> = checked["result"]["unlisted"]
        .as_array()
        .ok_or("unlisted")?
        .iter()
        .map(|row| row["key"].as_str())
        .collect();
    assert_eq!(
        (
            checked["direct_payloads_read"].as_u64(),
            checked["direct_payloads_parsed"].as_u64(),
            checked["result"]["not_looked"].as_u64(),
            checked["result"]["listed_directly"].as_u64(),
            unlisted,
        ),
        (
            Some(6),
            Some(3),
            Some(2),
            Some(0),
            vec![Some("globenewswire.com")]
        ),
        "{checked:#}"
    );
    Ok(())
}

/// The lead through collect: a direct answer received exactly `DIRECT_LEAD` (90 min) before the
/// earliest `cross_check` minute (07:22Z, so 05:52:00Z) is read, one a second earlier is not.
/// Mutations 2026-11-17, the lead 89 or 91 min and `>=` as `>` in `read_answers`: each red.
#[test]
fn a_direct_answer_received_exactly_one_lead_before_the_earliest_row_is_read() -> TestResult {
    let mut read = Vec::new();
    for early in ["2026-11-16T05:52:01Z", "2026-11-16T05:52:00Z"] {
        let (root, hosts) = five_host_store()?;
        cross_checked_at(
            root.path(),
            &hosts,
            early,
            &Gnw::five_hosts()?.answering(YAHOO_INDEX, &[UNAVAILABLE; 3]),
        )?;
        let (_, checked) = cross_checked_at(root.path(), &hosts, MONDAY_AT, &Gnw::five_hosts()?)?;
        read.push(checked["direct_payloads_read"].as_u64());
    }
    assert_eq!(read, [Some(8), Some(4)]);
    Ok(())
}

type Damage = fn(&Path, &[u8]) -> std::io::Result<()>;
/// How many blobs were damaged, the run's exit, its cross-check, and (envelopes, any unfinished).
type DamagedRun = (usize, Exit, Value, (usize, bool));

/// A second run after the first run's CNW sitemap blob was `damage`d on disk.
fn run_over_a_damaged_sitemap(damage: Damage) -> Result<DamagedRun, Box<dyn Error>> {
    let (root, hosts) = five_host_store()?;
    cross_checked_at(
        root.path(),
        &hosts,
        "2026-11-16T07:20:30Z",
        &Gnw::five_hosts()?.answering(YAHOO_INDEX, &[UNAVAILABLE; 3]),
    )?;
    let mut damaged = 0;
    for file in blob_paths(&root.path().join("blobs"))? {
        let bytes = std::fs::read(&file)?;
        let text = String::from_utf8_lossy(&bytes);
        if text.contains("<urlset") && text.contains("newswire.ca") {
            damage(&file, &bytes)?;
            damaged += 1;
        }
    }
    // CNW 503 now, so this run stores no fresh copy of the same sitemap over the damaged one.
    let (exit, checked) = cross_checked_at(
        root.path(),
        &hosts,
        MONDAY_AT,
        &Gnw::five_hosts()?
            .answering(YAHOO_INDEX, &[UNAVAILABLE])
            .answering(CNW_INDEX, &[UNAVAILABLE; 3]),
    )?;
    let unfinished = std::fs::read_dir(root.path().join(RUNS_DIR))?
        .map(|entry| -> Result<bool, Box<dyn Error>> {
            let envelope: Value = serde_json::from_slice(&std::fs::read(entry?.path())?)?;
            Ok(envelope["finished_at"].is_null())
        })
        .collect::<Result<Vec<bool>, _>>()?;
    Ok((
        damaged,
        exit,
        checked,
        (unfinished.len(), unfinished.contains(&true)),
    ))
}

/// The cross-check reads after the commit: a direct blob altered or missing on disk is its
/// `error`, and the run still finishes. Review 2026-11-17: an altered blob failed the run with
/// `BlobAltered` and left `finished_at` null; a missing one silently added no titles.
#[test]
fn a_damaged_direct_blob_is_a_cross_check_error_and_the_run_finishes() -> TestResult {
    let altered: Damage = |file, bytes| std::fs::write(file, [bytes, b" "].concat());
    let removed: Damage = |file, _| std::fs::remove_file(file);
    let mut seen = Vec::new();
    for damage in [altered, removed] {
        let (damaged, exit, checked, envelopes) = run_over_a_damaged_sitemap(damage)?;
        seen.push((
            damaged,
            exit,
            checked["error"]["code"].as_str().map(str::to_owned),
            checked["result"].is_null(),
            envelopes,
        ));
    }
    assert_eq!(
        seen,
        [
            (
                1,
                Exit::PartialCollect,
                Some("blob_altered".to_owned()),
                true,
                (3, false)
            ),
            (
                1,
                Exit::PartialCollect,
                Some("blob_absent".to_owned()),
                true,
                (3, false)
            ),
        ]
    );
    Ok(())
}

fn blob_paths(dir: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                found.push(path);
            }
        }
    }
    Ok(found)
}

/// The committed five-host file on Monday, on a fresh store: each attempt's url and id, in order.
fn five_host_attempts(exchange: &Gnw) -> Result<Vec<(String, String)>, Box<dyn Error>> {
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

/// The five indexes, then one request of each host's lane per turn, a lane that ran out skipped.
fn round_robin(hosts: &[(String, Vec<String>)]) -> Vec<String> {
    let mut order: Vec<String> = hosts.iter().map(|(index, _)| index.clone()).collect();
    let longest = hosts.iter().map(|(_, lane)| lane.len()).max().unwrap_or(0);
    for nth in 0..longest {
        order.extend(hosts.iter().filter_map(|(_, lane)| lane.get(nth).cloned()));
    }
    order
}

/// `round_robin` of the committed five hosts, and GNW's first release.
fn round_robin_of_five_hosts() -> (Vec<String>, String) {
    let hosts = five_host_urls();
    let first = hosts[0].1.first().cloned().unwrap_or_default();
    (round_robin(&hosts), first)
}

/// `(host's index, the rest of the host's lane)` as urls, GNW, PRN, CNW, Fed, Yahoo. PRN's lane
/// holds its sitemap page 1, the three RSS releases, then the sitemap-only release.
fn five_host_urls() -> [(String, Vec<String>); 5] {
    let pages = |keys: Vec<&str>| keys.iter().map(|key| format!("https://{key}")).collect();
    let prn_lane: Vec<String> = std::iter::once(PRN_SITEMAP.to_owned())
        .chain(pages(PRN_RELEASES.iter().map(|(key, _)| *key).collect()))
        .chain([format!("https://{PRN_SITEMAP_ONLY}")])
        .collect();
    [
        (
            INDEX.to_owned(),
            pages(RELEASES.iter().map(|(key, ..)| *key).collect()),
        ),
        (PRN_INDEX.to_owned(), prn_lane),
        (
            CNW_INDEX.to_owned(),
            pages(CNW_RELEASES.iter().map(|(key, _)| *key).collect()),
        ),
        (
            FED_INDEX.to_owned(),
            pages(FED_RELEASES.iter().map(|(key, _)| *key).collect()),
        ),
        (
            YAHOO_INDEX.to_owned(),
            pages(YAHOO_RELEASES.iter().map(|(key, _)| *key).collect()),
        ),
    ]
}

/// Owner decision 2026-11-17 (§10): `collect` serves its hosts round-robin, in first-appearance
/// order, so the five indexes, then one release of each host per turn; Yahoo's two run out first.
#[test]
fn a_five_host_run_asks_its_hosts_round_robin() -> TestResult {
    let (expected, _) = round_robin_of_five_hosts();
    let attempts = five_host_attempts(&Gnw::five_hosts()?)?;
    let urls: Vec<String> = attempts.into_iter().map(|(url, _)| url).collect();
    assert_eq!(urls, expected);
    Ok(())
}

/// §8 R21 under round-robin: GNW's first release fails every time; each retry goes to the back of
/// GNW's lane, after every request already queued, and stops at `MAX_TRANSIENT_RETRIES`.
#[test]
fn a_retried_release_waits_behind_the_other_hosts_and_stops_at_the_cap() -> TestResult {
    let (_, failing) = round_robin_of_five_hosts();
    let mut hosts = five_host_urls();
    hosts[0].1.extend([failing.clone(), failing.clone()]);
    let expected = round_robin(&hosts);
    let exchange = Gnw::five_hosts()?.answering(&failing, &[UNAVAILABLE; 3]);
    let attempts = five_host_attempts(&exchange)?;
    let urls: Vec<String> = attempts.into_iter().map(|(url, _)| url).collect();
    assert_eq!(urls, expected);
    Ok(())
}

/// §8 R21 with lanes of unequal length: PRN's RSS fails every time, so each retry is asked on a
/// later PRN turn and never waits for the other hosts' releases to drain. The sitemap, asked
/// between, lists every PRN release, so all four are fetched from it.
#[test]
fn a_retried_index_keeps_its_host_turn_while_the_other_hosts_releases_are_served() -> TestResult {
    let mut hosts = five_host_urls();
    let prn_releases: Vec<String> = PRN_RELEASES
        .iter()
        .map(|(key, _)| format!("https://{key}"))
        .collect();
    hosts[1].1 = [PRN_SITEMAP.to_owned(), PRN_INDEX.to_owned()]
        .into_iter()
        .chain([format!("https://{PRN_SITEMAP_ONLY}")])
        .chain(prn_releases)
        .chain([PRN_INDEX.to_owned()])
        .collect();
    let expected = round_robin(&hosts);
    let exchange = Gnw::five_hosts()?.answering(PRN_INDEX, &[UNAVAILABLE; 3]);
    let attempts = five_host_attempts(&exchange)?;
    let urls: Vec<String> = attempts.into_iter().map(|(url, _)| url).collect();
    assert_eq!(urls, expected);
    Ok(())
}

/// An attempt id hashes (run, source, window, ordinal), and the ordinal is the position in the
/// round-robin order: the same plan and answers on two fresh stores give the same ids, url by url.
#[test]
fn a_replayed_five_host_run_gives_every_attempt_the_same_id_and_url() -> TestResult {
    let first = five_host_attempts(&Gnw::five_hosts()?)?;
    let second = five_host_attempts(&Gnw::five_hosts()?)?;
    let distinct: std::collections::BTreeSet<&String> = first.iter().map(|(_, id)| id).collect();
    assert_eq!((first.len(), distinct.len(), &first), (21, 21, &second));
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
    pinned_to_index(dir, body, INDEX)
}

/// `pinned_to`, the GNW row reading its index from `index`.
fn pinned_to_index(
    dir: &Path,
    body: &[u8],
    index: &str,
) -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
    let hosts = dir.join("hosts.json");
    let store = dir.join("store");
    let pin = canon::FileDigest::of(body);
    let file = serde_json::json!({
        "schema": "open-web-feed.hosts/2",
        "hosts": [{"host": "rss.globenewswire.com", "status": "admitted", "index_url": index,
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
/// A narrower Disallow under the prefix passes; its documents are refused one by one
/// (`a_gnw_release_under_a_narrower_disallow_is_not_fetched_and_counted`). Mutation 2026-11-15,
/// the path check removed: red.
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

/// The index url is checked with its query, before every run, as robots.cc reads the url.
#[test]
fn an_index_whose_query_the_robots_disallows_is_refused_and_a_sibling_query_admitted() -> TestResult
{
    let mut said = Vec::new();
    for body in [
        "User-agent: *\nDisallow: /rssfeed/?page=\n",
        "User-agent: *\nDisallow: /rssfeed/?page=2\n",
    ] {
        let dir = TempDir::new()?;
        let (store, hosts) = pinned_to_index(
            dir.path(),
            body.as_bytes(),
            "https://rss.globenewswire.com/rssfeed/?page=1",
        )?;
        said.push(collected(&store, hosts, true)?);
    }
    assert_eq!(said, ["disallows /rssfeed/?page=1", "admitted"]);
    Ok(())
}

/// Owner decision 2026-11-17 (section 11.5 step 7): one release under a Disallow narrower than
/// the path prefix is not asked and is counted on its index attempt, in the envelope, the store
/// and the ledger; its two siblings are fetched.
#[test]
fn a_gnw_release_under_a_narrower_disallow_is_not_fetched_and_counted() -> TestResult {
    let dir = TempDir::new()?;
    let body = "User-agent: *\nDisallow: /news-release/2026/11/14/3403074/\n";
    let (store, hosts) = pinned_to(dir.path(), body.as_bytes())?;
    let ask = Ask {
        hosts: Some(hosts),
        ..ask(&store, cmd_collect::TODAY)
    };
    let exchange = Gnw::day()?;
    let opened = cmd_collect::open_provider(&ask)?;
    let answer = cmd_collect::run(&ask, &opened, &exchange, &Fixed)?;
    let (sent, _) = exchange.sent();
    let index = &answer.envelope["attempts"][0];
    let mut human = Vec::new();
    cmd_collect::render_human(&answer.envelope, &mut human)?;
    let human = String::from_utf8(human)?;
    assert_eq!(
        (
            sent,
            index["vendor_rows"].as_u64(),
            index["filtered_out"].as_u64(),
            index["robots_disallowed"].as_u64(),
            answer.envelope["per_source"][0]["robots_disallowed"].as_u64(),
            human.lines().nth(3).map(str::to_owned),
            stored_robots_disallowed(&store)?,
            ledgered_robots_disallowed(&store, "2026-11-14")?,
        ),
        (
            vec![
                INDEX.to_owned(),
                format!("https://{}", RELEASES[0].0),
                format!("https://{}", RELEASES[2].0),
            ],
            Some(3),
            Some(0),
            Some(1),
            Some(1),
            Some("  index     ok       rows 3 filtered 0 robots 1 items 0".to_owned()),
            BTreeMap::from([(INDEX.to_owned(), Some(1))]),
            vec![1],
        ),
        "{human}"
    );
    Ok(())
}

/// A disallowed row is never held, so every poll counts it again, and the ledger sums the polls
/// of one day. Mutation 2026-11-17, the ledger's per-source `+=` made `=`: green before this test.
#[test]
fn a_disallowed_row_is_counted_by_every_poll_and_the_ledger_sums_the_polls() -> TestResult {
    let dir = TempDir::new()?;
    let body = "User-agent: *\nDisallow: /news-release/2026/11/14/3403074/\n";
    let (store, hosts) = pinned_to(dir.path(), body.as_bytes())?;
    let exchange = Gnw::day()?;
    for (at, schedule) in [(RUN_AT, "first"), (LATER_THE_SAME_DAY, "second")] {
        let ask = Ask {
            hosts: Some(hosts.clone()),
            at: instant(at),
            schedule: Some(schedule.to_owned()),
            ..ask(&store, cmd_collect::TODAY)
        };
        let opened = cmd_collect::open_provider(&ask)?;
        cmd_collect::run(&ask, &opened, &exchange, &At(instant(at)))?;
    }
    let (sent, _) = exchange.sent();
    assert_eq!(
        (
            sent.iter().filter(|url| url.contains("/3403074/")).count(),
            ledgered_robots_disallowed(&store, "2026-11-14")?,
        ),
        (0, vec![2])
    );
    Ok(())
}

/// Each collect index attempt's url and `attempts.robots_disallowed`; robots fetches left out.
fn stored_robots_disallowed(store: &Path) -> Result<BTreeMap<String, Option<i64>>, Box<dyn Error>> {
    let db = Connection::open_with_flags(store.join(COMMIT_LOG), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut rows = db.prepare(
        "SELECT p.url, a.robots_disallowed FROM attempts a
           JOIN payloads p ON p.attempt_id = a.attempt_id
          WHERE p.phase = 'index' AND p.url NOT LIKE '%/robots.txt'",
    )?;
    let rows = rows
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

/// The ledger's `robots_disallowed` for each source row of `day`, read after the day ended.
fn ledgered_robots_disallowed(store: &Path, day: &str) -> Result<Vec<i64>, Box<dyn Error>> {
    let day: chrono::NaiveDate = day.parse()?;
    let after = day
        .succ_opt()
        .ok_or("no next day")?
        .and_time(chrono::NaiveTime::MIN);
    let report = newsvault::cmd_ledger::read(
        store,
        &newsvault::cmd_ledger::Ask {
            at: after.and_utc(),
            from: day,
            to: day,
            source: None,
            expected_runs: None,
            limit: None,
            check: false,
        },
    )?;
    Ok(report
        .days
        .iter()
        .flat_map(|one| &one.per_source)
        .map(|row| row.robots_disallowed)
        .collect())
}

/// Decision 4 through the real hand-off: robots is asked about the fetched url, whose path keeps
/// its percent escapes as written and carries no query. Verdicts from robots.cc 50a0dac compiled
/// with clang++, re-run 2026-11-17 on these release paths: `%C3%A9` disallowed and `%c3%a9`
/// allowed by `Disallow: /…/café`; GNW's `*/news-release/*.html?pdf=1` allows `/…/x.html`.
/// UPPER and LOWER are one release, so the second is followed only when robots refuse the first.
#[test]
fn a_document_path_is_checked_as_googles_parser_reads_the_fetched_url() -> TestResult {
    const UPPER: &str =
        "https://rss.globenewswire.com/news-release/2026/11/14/1/0/en/caf%C3%A9.html";
    const LOWER: &str =
        "https://rss.globenewswire.com/news-release/2026/11/14/1/0/en/caf%c3%a9.html";
    const PDF: &str = "https://rss.globenewswire.com/news-release/2026/11/14/2/0/en/x.html";
    let item = |link: &str| {
        format!("<item><link>{link}</link><pubDate>Sat, 14 Nov 2026 19:00 GMT</pubDate></item>")
    };
    let index = format!(
        "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel>{}{}{}</channel></rss>",
        item(UPPER),
        item(LOWER),
        item(&format!("{PDF}?pdf=1")),
    );
    let mut followed = Vec::new();
    for body in [
        "User-agent: *\nDisallow: /news-release/2026/11/14/1/0/en/café\n"
            .as_bytes()
            .to_vec(),
        seed::gnw_robots()?,
    ] {
        let dir = TempDir::new()?;
        let (store, hosts) = pinned_to(dir.path(), &body)?;
        let ask = Ask {
            hosts: Some(hosts),
            ..ask(&store, cmd_collect::TODAY)
        };
        let opened = cmd_collect::open_provider(&ask)?;
        let provider = opened.provider.as_ref();
        let plan = provider.plan(
            &TargetWindow::new("2026-11-14"),
            None,
            None,
            ObservationTime::new(instant(RUN_AT)),
        )?;
        let request = plan.requests.first().ok_or("no index planned")?;
        let extracted = provider.extract(
            request,
            &RawBlob {
                digest: canon::FileDigest::of(index.as_bytes()).hex().to_owned(),
                bytes: index.as_bytes().to_vec(),
            },
        );
        let urls: Vec<String> = extracted
            .follow_ups
            .iter()
            .map(|follow_up| follow_up.url.clone())
            .collect();
        followed.push((urls, extracted.robots_disallowed));
    }
    let owned = |urls: &[&str]| urls.iter().map(|url| (*url).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        followed,
        [(owned(&[LOWER, PDF]), 1), (owned(&[UPPER, PDF]), 0)]
    );
    Ok(())
}

/// Owner decision 2026-11-17 on the five-host stub: Yahoo's Laverock row moved under its real
/// `Disallow: /m/` is not asked and is counted on Yahoo's index only; its sibling is asked, and
/// the Fed, pinned `absent` (a 404), has every release asked.
#[test]
fn a_yahoo_row_under_a_robots_disallow_is_not_fetched_and_its_siblings_are() -> TestResult {
    const MOVED: &str =
        "https://finance.yahoo.com/m/laverock-therapeutics-appoints-parker-moss-091700844.html";
    let root = TempDir::new()?;
    seed::refresh_committed(root.path(), ROBOTS_EARLIER)?;
    let ask = Ask {
        hosts: Some(seed::committed_hosts()),
        at: instant(MONDAY_AT),
        ..ask(root.path(), cmd_collect::TODAY)
    };
    let mut exchange = Gnw::five_hosts()?;
    let sitemap = String::from_utf8(exchange.pages.get(YAHOO_INDEX).cloned().unwrap_or_default())?
        .replace(
            "finance.yahoo.com/healthcare/articles/laverock",
            "finance.yahoo.com/m/laverock",
        );
    exchange
        .pages
        .insert(YAHOO_INDEX.to_owned(), sitemap.into_bytes());
    let opened = cmd_collect::open_provider(&ask)?;
    let answer = cmd_collect::run(&ask, &opened, &exchange, &Monday)?;
    let (sent, _) = exchange.sent();
    let fed_asked = FED_RELEASES
        .iter()
        .filter(|(key, _)| sent.contains(&format!("https://{key}")))
        .count();
    let indexes: Vec<(Option<&str>, Option<u64>, Option<u64>)> = answer.envelope["attempts"]
        .as_array()
        .ok_or("attempts")?
        .iter()
        .filter(|row| row["phase"] == "index")
        .map(|row| {
            (
                row["url"].as_str(),
                row["filtered_out"].as_u64(),
                row["robots_disallowed"].as_u64(),
            )
        })
        .collect();
    assert_eq!(
        (
            sent.iter().any(|url| url == MOVED),
            sent.contains(&format!("https://{}", YAHOO_RELEASES[1].0)),
            fed_asked,
            indexes,
            stored_robots_disallowed(root.path())?
                .get(YAHOO_INDEX)
                .copied(),
            stored_robots_disallowed(root.path())?
                .values()
                .flatten()
                .sum::<i64>(),
            ledgered_robots_disallowed(root.path(), "2026-11-16")?,
        ),
        (
            false,
            true,
            FED_RELEASES.len(),
            vec![
                (Some(INDEX), Some(0), Some(0)),
                (Some(PRN_INDEX), Some(0), Some(0)),
                (Some(CNW_INDEX), Some(0), Some(0)),
                (Some(FED_INDEX), Some(0), Some(0)),
                (Some(YAHOO_INDEX), Some(6), Some(1)),
                (Some(PRN_SITEMAP), Some(0), Some(0)),
            ],
            Some(Some(1)),
            1,
            vec![1],
        )
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
    let story = digest(canon::digest::story_id(SOURCE, &keyed(key))).ok_or("story id")?;
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

/// The loopback 301's link row headers: its `Location`, kept since the id walk (PG-D4, section
/// 3.9's P9), and its `Content-Type`.
fn moved_headers() -> String {
    format!(
        r#"[["location","{}"],["content-type","{}"]]"#,
        loopback::REDIRECT_TARGET,
        loopback::MOVED_CONTENT_TYPE
    )
}

/// A release's first link row.
fn first_payload_of(root: &Path, release: usize) -> Result<Option<LinkedBlob>, Box<dyn Error>> {
    let db = Connection::open_with_flags(root.join(COMMIT_LOG), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let (key, _, _, _) = RELEASES.get(release).ok_or("no such release")?;
    let story = digest(canon::digest::story_id(SOURCE, &keyed(key))).ok_or("story id")?;
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
    let story = digest(canon::digest::story_id(SOURCE, &keyed(key))).ok_or("story id")?;
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
    let story = digest(canon::digest::story_id(SOURCE, &keyed(key))).ok_or("story id")?;
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

    fn record_deferred(&mut self, deferred: &news_provider::Deferred) -> Result<(), ArchiveError> {
        Archive::record_deferred(&mut self.store, deferred)
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
    let story = digest(canon::digest::story_id(SOURCE, &keyed(key))).ok_or("story id")?;
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

// ---- PRN's paged sitemap across polls (section 11.2), probe10's pages on 2026-11-17 ----

/// Probe10's polls, 30 s into each cycle's minute (§11.6).
const PROBE10_1200: &str = "2026-11-17T12:00:30Z";
const PROBE10_1205: &str = "2026-11-17T12:05:30Z";
const PROBE10_1405: &str = "2026-11-17T14:05:30Z";
const PROBE10_1410: &str = "2026-11-17T14:10:30Z";
const PROBE10_1415: &str = "2026-11-17T14:15:30Z";
const PROBE10_1420: &str = "2026-11-17T14:20:30Z";

/// One collect over the committed hosts at `at`, PRN's sitemap pages answered as `sitemap` names
/// them: the answer, and every url sent.
fn paged_poll(
    store: &Path,
    at: &str,
    sitemap: &[(&str, &str)],
) -> Result<(cmd_collect::Answer, Vec<String>), Box<dyn Error>> {
    paged_poll_over(store, at, Gnw::paged(sitemap)?)
}

/// `paged_poll` over `exchange`.
fn paged_poll_over(
    store: &Path,
    at: &str,
    exchange: Gnw,
) -> Result<(cmd_collect::Answer, Vec<String>), Box<dyn Error>> {
    let ask = Ask {
        hosts: Some(seed::committed_hosts()),
        at: instant(at),
        schedule: Some("probe10".to_owned()),
        ..ask(store, cmd_collect::TODAY)
    };
    let opened = cmd_collect::open_provider(&ask)?;
    let answer = cmd_collect::run(&ask, &opened, &exchange, &At(instant(at)))?;
    let (sent, _) = exchange.sent();
    Ok((answer, sent))
}

/// The poll's pages read and its continuity, from the envelope.
fn poll_continuity(answer: &cmd_collect::Answer) -> (Vec<u64>, Option<String>) {
    let poll = &answer.envelope["continuity"][0];
    (
        poll["pages_read"]
            .as_array()
            .map(|pages| pages.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default(),
        poll["continuity"].as_str().map(str::to_owned),
    )
}

/// The ledger's report for 2026-11-17.
fn ledgered(store: &Path) -> Result<newsvault::cmd_ledger::Report, Box<dyn Error>> {
    let day: chrono::NaiveDate = "2026-11-17".parse()?;
    Ok(newsvault::cmd_ledger::read(
        store,
        &newsvault::cmd_ledger::Ask {
            at: instant("2026-11-18T00:00:00Z"),
            from: day,
            to: day,
            source: None,
            expected_runs: None,
            limit: None,
            check: false,
        },
    )?)
}

/// The ledger's boundary counts for 2026-11-17, `[same, older, newer, unknown]`.
fn ledgered_boundaries(store: &Path) -> Result<Vec<[i64; 4]>, Box<dyn Error>> {
    Ok(ledgered(store)?
        .days
        .iter()
        .flat_map(|one| &one.per_source)
        .map(|row| {
            let counts = row.boundaries;
            [
                counts.same_generation,
                counts.older_generation,
                counts.newer_generation,
                counts.generation_unknown,
            ]
        })
        .collect())
}

/// The ledger's continuity counts for 2026-11-17, `[complete, boundary, no previous, first,
/// next page unread]`.
fn ledgered_continuity(store: &Path) -> Result<Vec<[i64; 5]>, Box<dyn Error>> {
    Ok(ledgered(store)?
        .days
        .iter()
        .flat_map(|one| &one.per_source)
        .map(|row| {
            let counts = row.continuity;
            [
                counts.complete,
                counts.boundary_no_shared_key,
                counts.no_previous_key,
                counts.first_poll,
                counts.next_page_unread,
            ]
        })
        .collect())
}

fn sends_of(sent: &[String], url: &str) -> usize {
    sent.iter().filter(|asked| *asked == url).count()
}

/// Probe10 12:00Z then 12:05Z: `first_poll`, then `complete`, page 1 each; the shared release is
/// fetched once and RSS-only releases are kept under `pubDate`. Mutation 2026-11-17, `basis_of`'s
/// map replaced by an empty one: red.
#[test]
fn a_poll_whose_page_one_overlaps_the_first_poll_is_complete_and_reads_one_page() -> TestResult {
    let root = TempDir::new()?;
    seed::refresh_committed(root.path(), ROBOTS_EARLIER)?;
    let (first, first_sent) = paged_poll(
        root.path(),
        PROBE10_1200,
        &[(PRN_SITEMAP, "sitemap_p1_1200.xml")],
    )?;
    let (second, second_sent) = paged_poll(
        root.path(),
        PROBE10_1205,
        &[(PRN_SITEMAP, "sitemap_p1_1205.xml")],
    )?;
    let shared = "https://www.prnewswire.com/news-releases/designer-brands-inc-announces-third-quarter-2026-earnings-release-date-302941008.html";
    let db = Connection::open_with_flags(
        root.path().join(COMMIT_LOG),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let rss_fields: Vec<String> = PRN_RELEASES
        .iter()
        .skip(1)
        .map(|(key, _)| {
            let story = digest(canon::digest::story_id(SOURCE, &keyed(key))).unwrap_or_default();
            db.query_row(
                "SELECT valid_field FROM items WHERE story_id = ?1",
                [story],
                |row| row.get(0),
            )
            .unwrap_or_default()
        })
        .collect();
    assert_eq!(
        (
            poll_continuity(&first),
            poll_continuity(&second),
            sends_of(&first_sent, PRN_SITEMAP_PAGE_2) + sends_of(&second_sent, PRN_SITEMAP_PAGE_2),
            sends_of(&first_sent, shared) + sends_of(&second_sent, shared),
            rss_fields,
            ledgered_continuity(root.path())?,
        ),
        (
            (vec![1], Some("first_poll".to_owned())),
            (vec![1], Some("complete".to_owned())),
            0,
            1,
            vec!["pubDate".to_owned(), "pubDate".to_owned()],
            vec![[1, 0, 0, 1, 0]],
        )
    );
    Ok(())
}

/// Probe10 14:10Z (§11.6): two pages, `boundary_no_shared_key`, and a basis (option B), so 14:15Z
/// with the same pages is `complete` at page 1. Mutations: §11.5 step 6's PRN completeness table.
#[test]
fn the_1410_poll_reads_two_pages_records_its_gap_and_is_the_basis_of_the_next() -> TestResult {
    let root = TempDir::new()?;
    seed::refresh_committed(root.path(), ROBOTS_EARLIER)?;
    let pages_of_1410 = [
        (PRN_SITEMAP, "sitemap_p1_1410.xml"),
        (PRN_SITEMAP_PAGE_2, "sitemap_p2_1410.xml"),
    ];
    let (before, _) = paged_poll(
        root.path(),
        PROBE10_1405,
        &[(PRN_SITEMAP, "sitemap_p1_1405.xml")],
    )?;
    let (gap, gap_sent) = paged_poll(root.path(), PROBE10_1410, &pages_of_1410)?;
    let (after, after_sent) = paged_poll(root.path(), PROBE10_1415, &pages_of_1410)?;
    let db = Connection::open_with_flags(
        root.path().join(COMMIT_LOG),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let gap_attempt = gap.envelope["continuity"][0]["attempt_id"]
        .as_str()
        .ok_or("no page-1 attempt")?;
    let page_2_listed: Option<String> = db.query_row(
        "SELECT listed_date FROM payloads WHERE url = ?1 ORDER BY attempt_id LIMIT 1",
        [PRN_SITEMAP_PAGE_2],
        |row| row.get(0),
    )?;
    let stored: Option<String> = db.query_row(
        "SELECT continuity FROM attempts WHERE attempt_id = ?1",
        [gap_attempt],
        |row| row.get(0),
    )?;
    assert_eq!(
        (
            poll_continuity(&before),
            poll_continuity(&gap),
            stored,
            sends_of(&gap_sent, PRN_SITEMAP_PAGE_2),
            page_2_listed,
            poll_continuity(&after),
            sends_of(&after_sent, PRN_SITEMAP_PAGE_2),
            gap.exit,
        ),
        (
            (vec![1], Some("first_poll".to_owned())),
            (vec![1, 2], Some("boundary_no_shared_key".to_owned())),
            Some("boundary_no_shared_key".to_owned()),
            1,
            Some("2026-11-17T09:00:00-05:00".to_owned()),
            (vec![1], Some("complete".to_owned())),
            0,
            Exit::Ok,
        )
    );
    Ok(())
}

/// The review's case (PR #47 F1): 14:10Z and 14:15Z each cross a broken boundary. 14:10Z is a
/// basis, so 14:15Z's page 2, holding a key of 14:10Z's page 2 and none of 14:05Z, ends the walk.
/// Mutations: §11.5 step 6's PRN completeness table.
#[test]
fn a_poll_after_a_broken_boundary_reads_back_only_to_that_poll() -> TestResult {
    let root = TempDir::new()?;
    seed::refresh_committed(root.path(), ROBOTS_EARLIER)?;
    let (first, _) = paged_poll(
        root.path(),
        PROBE10_1405,
        &[(PRN_SITEMAP, "sitemap_p1_1405.xml")],
    )?;
    let (second, _) = paged_poll(
        root.path(),
        PROBE10_1410,
        &[
            (PRN_SITEMAP, "sitemap_p1_1410.xml"),
            (PRN_SITEMAP_PAGE_2, "sitemap_p2_1410.xml"),
        ],
    )?;
    let (third, third_sent) = paged_poll(
        root.path(),
        PROBE10_1415,
        &[
            (PRN_SITEMAP, "sitemap_tie_0900.xml"),
            (PRN_SITEMAP_PAGE_2, "sitemap_p2_after_1410.xml"),
        ],
    )?;
    assert_eq!(
        (
            poll_continuity(&first),
            poll_continuity(&second),
            poll_continuity(&third),
            third.envelope["continuity"][0]["basis_keys"].as_u64(),
            sends_of(&third_sent, PRN_SITEMAP_PAGE_3),
            ledgered_continuity(root.path())?,
        ),
        (
            (vec![1], Some("first_poll".to_owned())),
            (vec![1, 2], Some("boundary_no_shared_key".to_owned())),
            (vec![1, 2], Some("boundary_no_shared_key".to_owned())),
            Some(6),
            0,
            vec![[0, 2, 0, 1, 0]],
        )
    );
    Ok(())
}

/// A poll whose boundary broke and whose planned page 3 failed read less than it planned, so it
/// is `next_page_unread`, not `boundary_no_shared_key`, and no basis: 14:15Z anchors to 12:05Z.
/// Mutations: §11.5 step 6's PRN completeness table.
#[test]
fn a_broken_poll_whose_next_page_failed_is_next_page_unread_and_no_basis() -> TestResult {
    let root = TempDir::new()?;
    seed::refresh_committed(root.path(), ROBOTS_EARLIER)?;
    paged_poll(
        root.path(),
        PROBE10_1205,
        &[(PRN_SITEMAP, "sitemap_p1_1205.xml")],
    )?;
    let failing = Gnw::paged(&[
        (PRN_SITEMAP, "sitemap_tie_0900.xml"),
        (PRN_SITEMAP_PAGE_2, "sitemap_p2_1410.xml"),
    ])?
    .answering(PRN_SITEMAP_PAGE_3, &[UNAVAILABLE; 3]);
    let (unread, unread_sent) = paged_poll_over(root.path(), PROBE10_1410, failing)?;
    let (next, next_sent) = paged_poll(
        root.path(),
        PROBE10_1415,
        &[(PRN_SITEMAP, "sitemap_p1_1205.xml")],
    )?;
    assert_eq!(
        (
            poll_continuity(&unread),
            sends_of(&unread_sent, PRN_SITEMAP_PAGE_3),
            unread.envelope["continuity"][0]["boundaries"]["generation_unknown"].as_u64(),
            poll_continuity(&next),
            next.envelope["continuity"][0]["basis_keys"].as_u64(),
            sends_of(&next_sent, PRN_SITEMAP_PAGE_2),
        ),
        (
            (vec![1, 2], Some("next_page_unread".to_owned())),
            3,
            Some(1),
            (vec![1], Some("complete".to_owned())),
            Some(3),
            0,
        )
    );
    Ok(())
}

/// Probe10's 14:10Z stamps (raw records, 2026-11-17): page 1 14:01:42.474Z, page 2 13:55:47.638Z.
const STAMP_1410_PAGE_1: &str = "2026-11-17T14:01:42.474Z";
const STAMP_1410_PAGE_2: &str = "2026-11-17T13:55:47.638Z";

/// A poll's continuity, its envelope `boundaries` and the day's ledger boundary counts.
type Labelled = (Option<String>, Value, Vec<[i64; 4]>);

/// 14:05Z, then 14:10Z's two pages stamped `page_1` and `page_2` (`None`: no header): 14:10Z's
/// continuity, its envelope `boundaries`, and the day's ledger counts in `Generation::ALL` order.
fn labelled_1410(page_1: Option<&str>, page_2: Option<&str>) -> Result<Labelled, Box<dyn Error>> {
    let root = TempDir::new()?;
    let answer = stamped_1410(root.path(), page_1, page_2)?;
    Ok((
        poll_continuity(&answer).1,
        answer.envelope["continuity"][0]["boundaries"].clone(),
        ledgered_boundaries(root.path())?,
    ))
}

/// 14:05Z, then 14:10Z's two pages stamped `page_1` and `page_2`, in a fresh store at `root`.
fn stamped_1410(
    root: &Path,
    page_1: Option<&str>,
    page_2: Option<&str>,
) -> Result<cmd_collect::Answer, Box<dyn Error>> {
    seed::refresh_committed(root, ROBOTS_EARLIER)?;
    paged_poll(root, PROBE10_1405, &[(PRN_SITEMAP, "sitemap_p1_1405.xml")])?;
    let stamps: Vec<(&str, &str)> = [(PRN_SITEMAP, page_1), (PRN_SITEMAP_PAGE_2, page_2)]
        .into_iter()
        .filter_map(|(url, stamp)| Some((url, stamp?)))
        .collect();
    let exchange = Gnw::paged(&[
        (PRN_SITEMAP, "sitemap_p1_1410.xml"),
        (PRN_SITEMAP_PAGE_2, "sitemap_p2_1410.xml"),
    ])?
    .stamped(&stamps);
    let (answer, _) = paged_poll_over(root, PROBE10_1410, exchange)?;
    Ok(answer)
}

/// `[same, older, newer, unknown]` as the envelope's `boundaries` object for one broken boundary.
fn one_boundary(labelled: [u64; 4]) -> Value {
    let [same, older, newer, unknown] = labelled;
    serde_json::json!({
        "same_generation": same,
        "older_generation": older,
        "newer_generation": newer,
        "generation_unknown": unknown,
    })
}

/// Equal stamps: one generation, so no row fell between the pages; the poll stays
/// `boundary_no_shared_key`, which the label never decides.
#[test]
fn a_broken_boundary_between_pages_of_one_generation_is_same_generation() -> TestResult {
    assert_eq!(
        labelled_1410(Some(STAMP_1410_PAGE_1), Some(STAMP_1410_PAGE_1))?,
        (
            Some("boundary_no_shared_key".to_owned()),
            one_boundary([1, 0, 0, 0]),
            vec![[1, 0, 0, 0]]
        )
    );
    Ok(())
}

/// Probe10's 14:10Z: page 2 from an older generation, a real gap.
#[test]
fn a_broken_boundary_whose_later_page_is_older_is_older_generation() -> TestResult {
    assert_eq!(
        labelled_1410(Some(STAMP_1410_PAGE_1), Some(STAMP_1410_PAGE_2))?,
        (
            Some("boundary_no_shared_key".to_owned()),
            one_boundary([0, 1, 0, 0]),
            vec![[0, 1, 0, 0]]
        )
    );
    Ok(())
}

/// The stamps reversed: page 2 newer than page 1, by the millisecond part as well.
#[test]
fn a_broken_boundary_whose_later_page_is_newer_is_newer_generation() -> TestResult {
    assert_eq!(
        labelled_1410(Some(STAMP_1410_PAGE_2), Some("2026-11-17T13:55:47.639Z"))?,
        (
            Some("boundary_no_shared_key".to_owned()),
            one_boundary([0, 0, 1, 0]),
            vec![[0, 0, 1, 0]]
        )
    );
    Ok(())
}

/// A stamp absent on page 2, or on page 1 in `Last-Modified`'s form, which drops the
/// milliseconds: `generation_unknown` both times, and the poll as it was before the header.
#[test]
fn a_broken_boundary_missing_or_misreading_a_stamp_is_generation_unknown() -> TestResult {
    let unknown = (
        Some("boundary_no_shared_key".to_owned()),
        one_boundary([0, 0, 0, 1]),
        vec![[0, 0, 0, 1]],
    );
    assert_eq!(
        (
            labelled_1410(Some(STAMP_1410_PAGE_1), None)?,
            labelled_1410(
                Some("Tue, 17 Nov 2026 14:01:42 GMT"),
                Some(STAMP_1410_PAGE_2)
            )?,
        ),
        (unknown.clone(), unknown)
    );
    Ok(())
}

/// The human `continuity` line names each label's own count. Review 2026-11-17: one label's
/// count printed under all four was green before this.
#[test]
fn the_human_continuity_line_prints_each_label_with_its_own_count() -> TestResult {
    let root = TempDir::new()?;
    let answer = stamped_1410(
        root.path(),
        Some(STAMP_1410_PAGE_1),
        Some(STAMP_1410_PAGE_2),
    )?;
    let mut human = Vec::new();
    cmd_collect::render_human(&answer.envelope, &mut human)?;
    let human = String::from_utf8(human)?;
    assert_eq!(
        human
            .lines()
            .find(|line| line.starts_with("continuity"))
            .map(str::to_owned),
        Some(format!(
            "continuity       {PRN_SITEMAP} pages 2 boundary_no_shared_key same_generation 0 \
             older_generation 1 newer_generation 0 generation_unknown 0"
        )),
        "{human}"
    );
    Ok(())
}

/// A stored `payloads.headers` text that does not parse is `store_unreadable`, never an empty
/// header set, which would label the boundary `generation_unknown` with no error. A trigger
/// corrupts page 2's text as it is filed.
#[test]
fn an_unreadable_stored_header_text_is_store_unreadable_and_records_no_continuity() -> TestResult {
    let root = TempDir::new()?;
    seed::refresh_committed(root.path(), ROBOTS_EARLIER)?;
    paged_poll(
        root.path(),
        PROBE10_1405,
        &[(PRN_SITEMAP, "sitemap_p1_1405.xml")],
    )?;
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    db.execute_batch(&format!(
        "CREATE TRIGGER corrupt_headers AFTER INSERT ON payloads WHEN NEW.url = '{PRN_SITEMAP_PAGE_2}'
         BEGIN UPDATE payloads SET headers = 'not json' WHERE rowid = NEW.rowid; END;"
    ))?;
    drop(db);
    let exchange = Gnw::paged(&[
        (PRN_SITEMAP, "sitemap_p1_1410.xml"),
        (PRN_SITEMAP_PAGE_2, "sitemap_p2_1410.xml"),
    ])?
    .stamped(&[
        (PRN_SITEMAP, STAMP_1410_PAGE_1),
        (PRN_SITEMAP_PAGE_2, STAMP_1410_PAGE_2),
    ]);
    let (answer, _) = paged_poll_over(root.path(), PROBE10_1410, exchange)?;
    let poll = &answer.envelope["continuity"][0];
    assert_eq!(
        (
            poll["error"]["code"].as_str(),
            poll["continuity"].as_str(),
            poll["boundaries"].clone(),
            ledgered_boundaries(root.path())?,
        ),
        (Some("store_unreadable"), None, Value::Null, vec![[0; 4]])
    );
    Ok(())
}

/// The table `MIGRATIONS[12]` adds, which a v12 store lacks.
const DEFERRED_TABLE: &str = "DROP TABLE deferred;";
/// The columns `MIGRATIONS[11]` adds, which a v11 store lacks.
const WALK_COLUMNS: &str = "ALTER TABLE attempts DROP COLUMN walk_through;
     ALTER TABLE attempts DROP COLUMN walk_gone;
     ALTER TABLE attempts DROP COLUMN walk_other_language;
     ALTER TABLE attempts DROP COLUMN walk_unread;";
/// The columns `MIGRATIONS[10]` adds, which a v10 store lacks.
const BOUNDARY_COLUMNS: &str = "ALTER TABLE attempts DROP COLUMN boundary_same_generation;
     ALTER TABLE attempts DROP COLUMN boundary_older_generation;
     ALTER TABLE attempts DROP COLUMN boundary_newer_generation;
     ALTER TABLE attempts DROP COLUMN boundary_generation_unknown;";

/// The first run over an older store reads that store's basis: `basis_pages` runs before the run
/// migrates it. Review 2026-11-17: a v11 build over a v10 store read none and recorded `first_poll`.
#[test]
fn a_v10_v11_or_v12_store_keeps_its_basis_for_the_run_that_migrates_it() -> TestResult {
    let mut kept = Vec::new();
    for (version, dropped) in [
        (
            10,
            format!("{DEFERRED_TABLE}{WALK_COLUMNS}{BOUNDARY_COLUMNS}"),
        ),
        (11, format!("{DEFERRED_TABLE}{WALK_COLUMNS}")),
        (12, DEFERRED_TABLE.to_owned()),
    ] {
        let root = TempDir::new()?;
        seed::refresh_committed(root.path(), ROBOTS_EARLIER)?;
        paged_poll(
            root.path(),
            PROBE10_1200,
            &[(PRN_SITEMAP, "sitemap_p1_1200.xml")],
        )?;
        let db = Connection::open(root.path().join(COMMIT_LOG))?;
        db.execute_batch(&format!("{dropped} PRAGMA user_version = {version};"))?;
        drop(db);
        let (second, _) = paged_poll(
            root.path(),
            PROBE10_1205,
            &[(PRN_SITEMAP, "sitemap_p1_1205.xml")],
        )?;
        kept.push(poll_continuity(&second));
    }
    assert_eq!(kept, vec![(vec![1], Some("complete".to_owned())); 3]);
    Ok(())
}

/// A poll whose page 1 never answered records no continuity, so it is "not looked", never
/// complete, and the next poll anchors to the poll before it.
#[test]
fn a_poll_whose_page_one_failed_records_no_continuity() -> TestResult {
    let root = TempDir::new()?;
    seed::refresh_committed(root.path(), ROBOTS_EARLIER)?;
    paged_poll(
        root.path(),
        PROBE10_1200,
        &[(PRN_SITEMAP, "sitemap_p1_1200.xml")],
    )?;
    let exchange = Gnw::paged(&[])?.answering(PRN_SITEMAP, &[UNAVAILABLE; 3]);
    let ask = Ask {
        hosts: Some(seed::committed_hosts()),
        at: instant(PROBE10_1205),
        schedule: Some("probe10".to_owned()),
        ..ask(root.path(), cmd_collect::TODAY)
    };
    let opened = cmd_collect::open_provider(&ask)?;
    let failed = cmd_collect::run(&ask, &opened, &exchange, &At(instant(PROBE10_1205)))?;
    let (next, _) = paged_poll(
        root.path(),
        PROBE10_1405,
        &[(PRN_SITEMAP, "sitemap_p1_1205.xml")],
    )?;
    assert_eq!(
        (
            poll_continuity(&failed),
            poll_continuity(&next),
            ledgered_continuity(root.path())?,
        ),
        (
            (Vec::new(), None),
            (vec![1], Some("complete".to_owned())),
            vec![[1, 0, 0, 1, 0]],
        )
    );
    Ok(())
}

/// A page 2 that failed leaves 13:51-13:59Z unread, so 14:10Z is `next_page_unread` and no basis:
/// 14:15Z reads back to 14:05Z, and 14:20Z anchors to both 14:15Z pages. Mutations 2026-11-17,
/// the next-page check dropped from `continuity` and the basis built from page 1 only: each red.
#[test]
fn a_poll_whose_page_two_failed_is_no_basis_and_the_next_reads_back_across_it() -> TestResult {
    let root = TempDir::new()?;
    seed::refresh_committed(root.path(), ROBOTS_EARLIER)?;
    let joined = [
        (PRN_SITEMAP, "sitemap_p1_1410.xml"),
        (PRN_SITEMAP_PAGE_2, "sitemap_p2_joined.xml"),
    ];
    paged_poll(
        root.path(),
        PROBE10_1405,
        &[(PRN_SITEMAP, "sitemap_p1_1405.xml")],
    )?;
    let failing = Gnw::paged(&[(PRN_SITEMAP, "sitemap_p1_1410.xml")])?
        .answering(PRN_SITEMAP_PAGE_2, &[UNAVAILABLE; 3]);
    let (failed, failed_sent) = paged_poll_over(root.path(), PROBE10_1410, failing)?;
    let (after, after_sent) = paged_poll(root.path(), PROBE10_1415, &joined)?;
    let (anchored, _) = paged_poll(
        root.path(),
        PROBE10_1420,
        &[(PRN_SITEMAP, "sitemap_p2_1410.xml")],
    )?;
    assert_eq!(
        (
            poll_continuity(&failed),
            sends_of(&failed_sent, PRN_SITEMAP_PAGE_2),
            poll_continuity(&after),
            sends_of(&after_sent, PRN_SITEMAP_PAGE_2),
            poll_continuity(&anchored),
            ledgered_continuity(root.path())?,
        ),
        (
            (vec![1], Some("next_page_unread".to_owned())),
            3,
            (vec![1, 2], Some("complete".to_owned())),
            1,
            (vec![1], Some("complete".to_owned())),
            vec![[2, 0, 0, 1, 1]],
        )
    );
    Ok(())
}

/// Page 2 answered with PRN's real page past the end (probe 11: page 24, 200, an empty `urlset`):
/// the walk ran out, the run exits 0, 14:05Z is `no_previous_key` and the basis of 14:10Z, not
/// 12:00Z. Mutations 2026-11-17, `ran_out` ignored and the basis ordered oldest first: each red.
#[test]
fn a_poll_whose_next_page_is_past_the_end_ran_out_and_is_the_basis_of_the_next() -> TestResult {
    let root = TempDir::new()?;
    seed::refresh_committed(root.path(), ROBOTS_EARLIER)?;
    let ran_out = [
        (PRN_SITEMAP, "sitemap_p1_1405.xml"),
        (PRN_SITEMAP_PAGE_2, "sitemap_past_end_p24.xml"),
    ];
    paged_poll(
        root.path(),
        PROBE10_1200,
        &[(PRN_SITEMAP, "sitemap_p1_1200.xml")],
    )?;
    let (empty, _) = paged_poll(root.path(), PROBE10_1405, &ran_out)?;
    let (next, next_sent) = paged_poll(
        root.path(),
        PROBE10_1410,
        &[(PRN_SITEMAP, "sitemap_p1_1405.xml")],
    )?;
    assert_eq!(
        (
            poll_continuity(&empty),
            empty.exit,
            poll_continuity(&next),
            sends_of(&next_sent, PRN_SITEMAP_PAGE_2),
        ),
        (
            (vec![1], Some("no_previous_key".to_owned())),
            Exit::Ok,
            (vec![1], Some("complete".to_owned())),
            0,
        )
    );
    Ok(())
}

// ---- GNW id walk (section 11.2, owner decisions PG-D2 and PG-D4, 2026-11-17) ----

/// The GNW row with its id walk, alone.
const WALK_HOSTS: &str = "gnw_walked.json";
/// Probe10's GNW polls of 2026-11-17 (section 11.6): 12:00Z cut to 3404422 (en, the frontier),
/// 3404419 (en), 3404417 (he) and 3404415 (en); 12:00Z cut to 3404422 and 3404421; 12:05Z cut
/// to 3404462 (en), 3404462 (fr) and 3404424 (en).
const RSS_1200: &str = "rssfeed_1200_4_items.xml";
const RSS_1200_TWO: &str = "rssfeed_1200_2_items.xml";
const RSS_1205: &str = "rssfeed_1205_3_items.xml";
/// Probe10's walked page of 3404438, `og:article:published_time` 2026-11-17T12:00:00Z.
const WALKED_PAGE: &str = "walked_3404438.html";
const RELEASE_PREFIX: &str = "https://rss.globenewswire.com/news-release/";
const LOCATION: &str = "location";
const GONE: u16 = 410;
const WALK_FIRST: &str = "2026-11-17T12:00:30Z";
const WALK_SECOND: &str = "2026-11-17T12:05:30Z";
const WALK_THIRD: &str = "2026-11-17T12:10:30Z";
/// 3404415, the lowest id `RSS_1200` lists, less one; and its highest.
const BELOW_THE_FIRST_POLL: u64 = 3_404_414;
const FIRST_FRONTIER: u64 = 3_404_422;
const SECOND_FRONTIER: u64 = 3_404_462;

fn walk_url(id: u64) -> String {
    format!("{RELEASE_PREFIX}2026/11/17/{id}/0/en/x.html")
}

/// A live collect of the GNW walk row at `at`, keyed by its own schedule.
fn walked_poll(
    store: &Path,
    hosts: PathBuf,
    at: &str,
    exchange: &Gnw,
) -> Result<cmd_collect::Answer, Box<dyn Error>> {
    let ask = Ask {
        hosts: Some(hosts),
        at: instant(at),
        schedule: Some("walk".to_owned()),
        ..ask(store, cmd_collect::TODAY)
    };
    let opened = cmd_collect::open_provider(&ask)?;
    Ok(cmd_collect::run(&ask, &opened, exchange, &At(instant(at)))?)
}

/// A store whose first walked poll read `RSS_1200`, every release answered.
fn after_a_first_walked_poll() -> Result<TempDir, Box<dyn Error>> {
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts_fixture(WALK_HOSTS), ROBOTS_EARLIER)?;
    walked_poll(
        root.path(),
        hosts_fixture(WALK_HOSTS),
        WALK_FIRST,
        &Gnw::walked(RSS_1200)?,
    )?;
    Ok(root)
}

/// The ids a run walked, in the order sent, each once per send.
fn walked_ids(sent: &[String]) -> Vec<u64> {
    sent.iter()
        .filter_map(|url| {
            url.strip_prefix(RELEASE_PREFIX)?
                .strip_prefix("2026/11/17/")?
                .strip_suffix("/0/en/x.html")?
                .parse()
                .ok()
        })
        .collect()
}

/// `(after, through, date, asked, continuity, unread, gone, other_language)` of the run's walk.
type WalkPoll = (Value, Value, Value, Value, Value, Value, Value, Value);

fn walk_poll(answer: &cmd_collect::Answer) -> WalkPoll {
    let poll = &answer.envelope["walk"][0];
    (
        poll["after"].clone(),
        poll["through"].clone(),
        poll["date"].clone(),
        poll["asked"].clone(),
        poll["continuity"].clone(),
        poll["unread"].clone(),
        poll["gone"].clone(),
        poll["other_language"].clone(),
    )
}

/// `(continuity, walk_through, walk_gone, walk_other_language, walk_unread)` of the run's index
/// attempt, as stored.
type WalkColumns = (
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

fn walk_columns(store: &Path, answer: &cmd_collect::Answer) -> Result<WalkColumns, Box<dyn Error>> {
    let attempt = answer.envelope["walk"][0]["attempt_id"]
        .as_str()
        .ok_or("no walked attempt")?;
    let db = Connection::open_with_flags(store.join(COMMIT_LOG), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(db.query_row(
        "SELECT continuity, walk_through, walk_gone, walk_other_language, walk_unread
           FROM attempts WHERE attempt_id = ?1",
        [attempt],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?)
}

/// Per source: walked polls `complete`, `first_poll`, `id_unread`, `range_unread`, then ids gone,
/// moved, unread.
type WalkLedger = Vec<[i64; 7]>;

/// The day's walk counts per source, from the JSON report and from its human line.
fn ledgered_walk(store: &Path) -> Result<(WalkLedger, String), Box<dyn Error>> {
    let report = ledgered(store)?;
    let counts = report
        .days
        .iter()
        .flat_map(|one| &one.per_source)
        .map(|row| {
            let walk = row.walk;
            [
                walk.complete,
                walk.first_poll,
                walk.id_unread,
                walk.range_unread,
                walk.gone,
                walk.other_language,
                walk.unread,
            ]
        })
        .collect();
    let mut human = Vec::new();
    newsvault::cmd_ledger::render_human(&report, &mut human)?;
    Ok((counts, String::from_utf8(human)?))
}

/// No earlier answer of the index: no frontier, so nothing is walked and the poll is the basis
/// the next one walks from (section 11.2). Mutation 2026-11-17, `range` read without a basis:
/// see `a_second_poll_walks_below_the_previous_frontier_only_and_records_each_answer`.
#[test]
fn a_first_walked_poll_asks_no_id_and_records_first_poll() -> TestResult {
    let root = TempDir::new()?;
    seed_gnw(root.path(), &hosts_fixture(WALK_HOSTS), ROBOTS_EARLIER)?;
    let exchange = Gnw::walked(RSS_1200)?;
    let first = walked_poll(
        root.path(),
        hosts_fixture(WALK_HOSTS),
        WALK_FIRST,
        &exchange,
    )?;
    let (sent, _) = exchange.sent();
    assert_eq!(
        (
            walked_ids(&sent),
            sent.len(),
            walk_poll(&first).4,
            walk_columns(root.path(), &first)?,
            ledgered_walk(root.path())?.0,
        ),
        (
            Vec::new(),
            1 + 4,
            Value::from("first_poll"),
            (
                Some("first_poll".to_owned()),
                None,
                Some(0),
                Some(0),
                Some(0)
            ),
            vec![[0, 1, 0, 0, 0, 0, 0]],
        )
    );
    Ok(())
}

/// The second poll walks below the FIRST poll's frontier only (PG-D2) and records each answer: a
/// 301 to Hebrew not followed (PG-D4), each 410 asked once and counted apart from the 301, a
/// walked page dated by its own time. A 410 is an answer: `complete`, and exit 8 as any 4xx.
#[test]
fn a_second_poll_walks_below_the_previous_frontier_only_and_records_each_answer() -> TestResult {
    let root = after_a_first_walked_poll()?;
    let moved = "/news-release/2026/11/17/3404417/0/he/lepas-aura.html";
    let exchange = Gnw::walked(RSS_1205)?
        .moved(&walk_url(3_404_417), moved)
        .answering(&walk_url(3_404_418), &[GONE])
        .answering(&walk_url(3_404_420), &[GONE]);
    let second = walked_poll(
        root.path(),
        hosts_fixture(WALK_HOSTS),
        WALK_SECOND,
        &exchange,
    )?;
    let (sent, _) = exchange.sent();
    assert_eq!(
        (
            walked_ids(&sent),
            sent.iter().filter(|url| url.contains("/he/")).count(),
            second.exit,
        ),
        (
            vec![3_404_416, 3_404_417, 3_404_418, 3_404_420, 3_404_421],
            0,
            Exit::PartialCollect,
        )
    );
    assert_eq!(
        walk_poll(&second),
        (
            Value::from(BELOW_THE_FIRST_POLL),
            Value::from(FIRST_FRONTIER),
            Value::from("2026/11/17"),
            Value::from(5),
            Value::from("complete"),
            serde_json::json!([]),
            serde_json::json!([3_404_418, 3_404_420]),
            serde_json::json!([{"id": 3_404_417, "language": "he"}]),
        )
    );
    assert_eq!(
        (
            walk_columns(root.path(), &second)?,
            ledgered_walk(root.path())?.0,
        ),
        (
            (
                Some("complete".to_owned()),
                Some(i64::try_from(FIRST_FRONTIER)?),
                Some(2),
                Some(1),
                Some(0)
            ),
            vec![[1, 1, 0, 0, 2, 1, 0]],
        )
    );
    let human = ledgered_walk(root.path())?.1;
    assert!(
        human.contains("walk_complete 1 walk_first_poll 1 walk_id_unread 0 walk_range_unread 0 walk_gone 2 walk_other_language 1 walk_unread 0"),
        "{human}"
    );
    let db = Connection::open_with_flags(
        root.path().join(COMMIT_LOG),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let walked = digest(canon::digest::story_id(
        SOURCE,
        "rss.globenewswire.com/news-release/3404416/en",
    ))
    .ok_or("story id")?;
    let stored: (String, String, String, String, String) = db.query_row(
        "SELECT i.valid_minute, i.valid_raw, i.valid_field, i.observed_at, a.received_at
           FROM items i JOIN attempts a ON a.attempt_id = i.attempt_id WHERE i.story_id = ?1",
        [&walked],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    assert_eq!(
        stored,
        (
            "2026-11-17T12:00:00.000000Z".to_owned(),
            "2026-11-17T12:00:00Z".to_owned(),
            "og:article:published_time".to_owned(),
            "2026-11-17T12:05:29.000000Z".to_owned(),
            "2026-11-17T12:05:29.000000Z".to_owned(),
        )
    );
    // The RSS index's `held` counts its own rows only: none of its 3 was held.
    let held: Option<i64> = db.query_row(
        "SELECT held FROM attempts WHERE attempt_id = ?1",
        [second.envelope["walk"][0]["attempt_id"]
            .as_str()
            .ok_or("no attempt")?],
        |row| row.get(0),
    )?;
    assert_eq!(held, Some(0));
    Ok(())
}

/// The poll after a `complete` walk walks from the id it walked through, so the 410 and the 301
/// are never asked again (owner decision 2026-11-17: a 410 is not re-asked under another slug).
#[test]
fn a_poll_after_a_complete_walk_starts_at_the_id_it_walked_through() -> TestResult {
    let root = after_a_first_walked_poll()?;
    let second = Gnw::walked(RSS_1205)?
        .moved(
            &walk_url(3_404_417),
            "/news-release/2026/11/17/3404417/0/he/lepas-aura.html",
        )
        .answering(&walk_url(3_404_418), &[GONE]);
    walked_poll(root.path(), hosts_fixture(WALK_HOSTS), WALK_SECOND, &second)?;
    let exchange = Gnw::walked(RSS_1205)?;
    let third = walked_poll(
        root.path(),
        hosts_fixture(WALK_HOSTS),
        WALK_THIRD,
        &exchange,
    )?;
    let (sent, _) = exchange.sent();
    let expected: Vec<u64> = (FIRST_FRONTIER + 1..=SECOND_FRONTIER)
        .filter(|id| *id != 3_404_424 && *id != SECOND_FRONTIER)
        .collect();
    assert_eq!(
        (walked_ids(&sent), walk_poll(&third).0, walk_poll(&third).4),
        (
            expected,
            Value::from(FIRST_FRONTIER),
            Value::from("complete")
        )
    );
    Ok(())
}

/// An id still failing after its retries leaves the poll `id_unread`, no basis, so the next poll
/// walks from the first poll again and asks that id once more and no id already answered: a 301
/// to no other language included, which is permanent and not held (§10) yet answered.
#[test]
fn an_id_unread_after_its_retries_is_walked_again_by_the_next_poll() -> TestResult {
    let root = after_a_first_walked_poll()?;
    let failing = Gnw::walked(RSS_1205)?
        .answering(&walk_url(3_404_420), &[UNAVAILABLE; 3])
        .moved(&walk_url(3_404_421), "/elsewhere");
    let second = walked_poll(
        root.path(),
        hosts_fixture(WALK_HOSTS),
        WALK_SECOND,
        &failing,
    )?;
    let (failed_sent, _) = failing.sent();
    let exchange = Gnw::walked(RSS_1205)?;
    let third = walked_poll(
        root.path(),
        hosts_fixture(WALK_HOSTS),
        WALK_THIRD,
        &exchange,
    )?;
    let (sent, _) = exchange.sent();
    let mut expected = vec![3_404_420];
    expected.extend((FIRST_FRONTIER + 1..SECOND_FRONTIER).filter(|id| *id != 3_404_424));
    assert_eq!(
        (
            sends_of(&failed_sent, &walk_url(3_404_420)),
            walk_poll(&second).4,
            walk_poll(&second).5,
            walk_columns(root.path(), &second)?.4,
            walked_ids(&sent),
            walk_poll(&third).0,
            walk_poll(&third).4,
            ledgered_walk(root.path())?.0,
        ),
        (
            3,
            Value::from("id_unread"),
            serde_json::json!([3_404_420]),
            Some(1),
            expected,
            Value::from(BELOW_THE_FIRST_POLL),
            Value::from("complete"),
            vec![[1, 1, 1, 0, 0, 0, 1]],
        )
    );
    Ok(())
}

/// An id this run's index lists in the walk's language is fetched once, from its listed url, and
/// is one story with the walk's url of it (`(dc:identifier, language)`, section 11.2).
#[test]
fn an_id_the_index_lists_inside_the_range_is_fetched_once_from_its_listed_url() -> TestResult {
    let root = after_a_first_walked_poll()?;
    let exchange = Gnw::walked(RSS_1200_TWO)?;
    walked_poll(
        root.path(),
        hosts_fixture(WALK_HOSTS),
        WALK_SECOND,
        &exchange,
    )?;
    let (sent, _) = exchange.sent();
    let listed = "https://rss.globenewswire.com/news-release/2026/11/17/3404421/0/en/man-group-plc-form-8-3-rhi-magnesita-n-v.html";
    let db = Connection::open_with_flags(
        root.path().join(COMMIT_LOG),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let story = digest(canon::digest::story_id(
        SOURCE,
        "rss.globenewswire.com/news-release/3404421/en",
    ))
    .ok_or("story id")?;
    let items: i64 = db.query_row(
        "SELECT COUNT(*) FROM items WHERE story_id = ?1",
        [&story],
        |row| row.get(0),
    )?;
    assert_eq!(
        (
            walked_ids(&sent),
            sends_of(&sent, listed),
            sends_of(&sent, &walk_url(3_404_421)),
            items,
        ),
        (vec![3_404_416, 3_404_417, 3_404_418, 3_404_420], 1, 0, 1)
    );
    Ok(())
}

/// A walked id whose url the pinned robots disallows is never asked, so it has no answer and
/// the poll is `id_unread` (owner decision 2026-11-17, option B).
#[test]
fn a_walked_id_robots_disallow_leaves_the_poll_no_basis() -> TestResult {
    let dir = TempDir::new()?;
    let body = b"User-agent: *\nDisallow: /news-release/2026/11/17/3404420/\n";
    let hosts = dir.path().join("hosts.json");
    let store = dir.path().join("store");
    let file = serde_json::json!({
        "schema": "open-web-feed.hosts/2",
        "hosts": [{"host": "rss.globenewswire.com", "status": "admitted", "index_url": INDEX,
                   "rule": "gnw", "id_walk": {"url": format!("{RELEASE_PREFIX}{{date}}/{{id}}/0/en/x.html")},
                   "robots": canon::FileDigest::of(body).hex()}],
    });
    std::fs::write(&hosts, serde_json::to_vec(&file)?)?;
    seed::refresh(&store, &hosts, ROBOTS_EARLIER, (OK, body))?;
    walked_poll(&store, hosts.clone(), WALK_FIRST, &Gnw::walked(RSS_1200)?)?;
    let exchange = Gnw::walked(RSS_1205)?;
    let second = walked_poll(&store, hosts, WALK_SECOND, &exchange)?;
    let (sent, _) = exchange.sent();
    assert_eq!(
        (
            walked_ids(&sent),
            walk_poll(&second).4,
            walk_poll(&second).5
        ),
        (
            vec![3_404_416, 3_404_417, 3_404_418, 3_404_421],
            Value::from("id_unread"),
            serde_json::json!([3_404_420]),
        )
    );
    Ok(())
}

const WALK_FOURTH: &str = "2026-11-17T12:15:30Z";

/// `RSS_1205` with every release link moved under `/en/`, a shape no release is read from.
fn shifted_index() -> Result<Gnw, Box<dyn Error>> {
    let mut exchange = Gnw::walked(RSS_1205)?;
    let listed = exchange.pages.get(INDEX).ok_or("no index page")?;
    let moved = String::from_utf8_lossy(listed).replace(
        "globenewswire.com/news-release/",
        "globenewswire.com/en/news-release/",
    );
    exchange.pages.insert(INDEX.to_owned(), moved.into_bytes());
    Ok(exchange)
}

/// A previous answer that lists no release leaves the range unknown: `range_unread`, no basis and
/// no `walk_through`, so the next poll walks from the basis before it. PR #49 review, 2026-11-18:
/// recorded `first_poll`, the next poll started at 3404423 and never asked 3404423.
#[test]
fn a_poll_whose_previous_answer_lists_no_release_is_range_unread_and_keeps_the_basis() -> TestResult
{
    let root = after_a_first_walked_poll()?;
    let hosts = || hosts_fixture(WALK_HOSTS);
    walked_poll(root.path(), hosts(), WALK_SECOND, &shifted_index()?)?;
    let exchange = Gnw::walked(RSS_1205)?;
    let third = walked_poll(root.path(), hosts(), WALK_THIRD, &exchange)?;
    let (third_sent, _) = exchange.sent();
    let fourth = walked_poll(root.path(), hosts(), WALK_FOURTH, &exchange)?;
    let (fourth_sent, _) = exchange.sent();
    assert_eq!(
        (
            walked_ids(&third_sent),
            walk_poll(&third).0,
            walk_poll(&third).4,
            walk_columns(root.path(), &third)?,
            walk_poll(&fourth).0,
            walked_ids(&fourth_sent).first().copied(),
            walk_poll(&fourth).4,
            ledgered_walk(root.path())?.0,
        ),
        (
            Vec::new(),
            Value::Null,
            Value::from("range_unread"),
            (
                Some("range_unread".to_owned()),
                None,
                Some(0),
                Some(0),
                Some(0)
            ),
            Value::from(FIRST_FRONTIER),
            Some(FIRST_FRONTIER + 1),
            Value::from("complete"),
            vec![[2, 1, 0, 1, 0, 0, 0]],
        )
    );
    Ok(())
}

/// A previous answer whose highest id is below the basis's `walk_through` walks nothing and keeps
/// that `walk_through` (`max(after, through)`, §3.2), so no answered id is walked again.
#[test]
fn a_poll_whose_frontier_fell_below_its_basis_keeps_the_id_the_basis_walked_through() -> TestResult
{
    let root = after_a_first_walked_poll()?;
    let hosts = || hosts_fixture(WALK_HOSTS);
    walked_poll(root.path(), hosts(), WALK_SECOND, &Gnw::walked(RSS_1205)?)?;
    walked_poll(
        root.path(),
        hosts(),
        WALK_THIRD,
        &Gnw::walked(RSS_1200_TWO)?,
    )?;
    let exchange = Gnw::walked(RSS_1205)?;
    let fourth = walked_poll(root.path(), hosts(), WALK_FOURTH, &exchange)?;
    let (sent, _) = exchange.sent();
    let poll = walk_poll(&fourth);
    assert_eq!(
        (
            walked_ids(&sent),
            poll.0,
            poll.1,
            poll.3,
            poll.4,
            walk_columns(root.path(), &fourth)?.1,
        ),
        (
            Vec::new(),
            Value::from(SECOND_FRONTIER),
            Value::from(FIRST_FRONTIER),
            Value::from(0),
            Value::from("complete"),
            Some(i64::try_from(SECOND_FRONTIER)?),
        )
    );
    Ok(())
}

/// A real store whose held set also names `also`: a story committed by another run after
/// `open_provider` read the walk's answered ids and before this run's index was extracted.
struct HoldingToo {
    store: Committer,
    also: std::collections::BTreeSet<String>,
}

impl Sink for HoldingToo {
    fn write(&mut self, event: &AttemptEvent) -> Result<(), LedgerError> {
        self.store.write(event)
    }
}

impl Archive for HoldingToo {
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
        let mut held = Archive::held_story_ids(&self.store, source)?;
        held.extend(self.also.iter().cloned());
        Ok(held)
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

    fn record_deferred(&mut self, deferred: &news_provider::Deferred) -> Result<(), ArchiveError> {
        Archive::record_deferred(&mut self.store, deferred)
    }
}

/// The driver drops a walked id whose story became held after `open_provider` read the answered
/// ids (`news_provider::collect::attempt_once`); the feed's own filter cannot see it.
#[test]
fn a_walked_id_held_after_the_range_was_read_is_not_asked() -> TestResult {
    let root = after_a_first_walked_poll()?;
    let ask = Ask {
        hosts: Some(hosts_fixture(WALK_HOSTS)),
        at: instant(WALK_SECOND),
        schedule: Some("walk".to_owned()),
        ..ask(root.path(), cmd_collect::TODAY)
    };
    let opened = cmd_collect::open_provider(&ask)?;
    let run = RunId::new("held-after-open");
    let held = digest(canon::digest::story_id(
        SOURCE,
        "rss.globenewswire.com/news-release/3404416/en",
    ))
    .ok_or("story id")?;
    let mut archive = HoldingToo {
        store: Committer::open(root.path(), &run)?,
        also: std::collections::BTreeSet::from([held]),
    };
    let exchange = Gnw::walked(RSS_1205)?;
    cmd_collect::drive(
        &ask,
        opened.provider.as_ref(),
        &mut archive,
        &exchange,
        &At(instant(WALK_SECOND)),
        &run,
    )?;
    let (sent, _) = exchange.sent();
    assert_eq!(
        walked_ids(&sent),
        vec![3_404_417, 3_404_418, 3_404_420, 3_404_421]
    );
    Ok(())
}

const CNW_RELEASE_PREFIX: &str = "https://www.newswire.ca/news-releases/";
/// The five-host run's document follow-ups before any synthetic CNW release: GNW 3, PRN 4, CNW 3,
/// the Fed 3 and Yahoo 2 (`an_open_web_run_collects_a_gnw_a_prn_a_cnw_a_fed_and_a_yahoo_day_end_to_end`).
const FIVE_HOST_FOLLOW_UPS: usize = 15;
const FOLLOW_UP_CAP: usize = 150;

/// `cnw/sitemap_news_3_urls.xml` with `extra` synthetic releases after its three, each dated as
/// its first; the synthetic urls in sitemap order.
fn cnw_sitemap_with(extra: usize) -> Result<(Vec<u8>, Vec<String>), Box<dyn Error>> {
    let fixture =
        std::fs::read_to_string(provider_dir().join("tests/fixtures/cnw/sitemap_news_3_urls.xml"))?;
    let urls: Vec<String> = (0..extra)
        .map(|nth| format!("{CNW_RELEASE_PREFIX}capped-release-{nth}-7{nth:08}.html"))
        .collect();
    let entries: String = urls
        .iter()
        .map(|url| {
            format!(
                "<url><loc>{url}</loc><news:news><news:publication><news:name>Canada NewsWire\
                 </news:name><news:language>en</news:language></news:publication>\
                 <news:publication_date>2026-11-16T02:22:00-05:00</news:publication_date>\
                 <news:title>A capped release</news:title></news:news></url>\n"
            )
        })
        .collect();
    let body = fixture.replacen("</urlset>", &format!("{entries}</urlset>"), 1);
    Ok((body.into_bytes(), urls))
}

type CappedRun = (TempDir, cmd_collect::Answer, Gnw, Vec<String>);

/// The committed five hosts on Monday on a fresh store, CNW's sitemap listing `extra` more
/// releases, each answered with a kept CNW page.
fn capped_run(extra: usize) -> Result<CappedRun, Box<dyn Error>> {
    let root = TempDir::new()?;
    seed::refresh_committed(root.path(), ROBOTS_EARLIER)?;
    let (sitemap, urls) = cnw_sitemap_with(extra)?;
    let mut exchange = Gnw::five_hosts()?;
    exchange.pages.insert(CNW_INDEX.to_owned(), sitemap);
    exchange.fallback = Some((
        CNW_RELEASE_PREFIX.to_owned(),
        std::fs::read(provider_dir().join("tests/fixtures/cnw/article_805423040.html"))?,
    ));
    let ask = Ask {
        hosts: Some(seed::committed_hosts()),
        at: instant(MONDAY_AT),
        ..ask(root.path(), cmd_collect::TODAY)
    };
    let opened = cmd_collect::open_provider(&ask)?;
    let answer = cmd_collect::run(&ask, &opened, &exchange, &Monday)?;
    Ok((root, answer, exchange, urls))
}

fn document_attempts(envelope: &Value) -> usize {
    envelope["attempts"].as_array().map_or(0, |rows| {
        rows.iter().filter(|row| row["phase"] == "document").count()
    })
}

/// One `deferred` row as stored, every column in table order.
type DeferredRow = (
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
);

fn stored_deferred(store: &Path) -> Result<Vec<DeferredRow>, Box<dyn Error>> {
    let db = Connection::open_with_flags(store.join(COMMIT_LOG), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut query = db.prepare(
        "SELECT run_id, source, target_window, host, url, phase, story_id, listed_date, reason,
                deferred_at
           FROM deferred ORDER BY rowid",
    )?;
    let rows = query
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
            ))
        })?
        .collect::<Result<Vec<DeferredRow>, _>>()?;
    Ok(rows)
}

/// S7-D6 at its boundary through `collect`: 150 document follow-ups are all asked, no row is
/// deferred and the run exits 0.
#[test]
fn a_run_of_exactly_150_follow_ups_asks_every_one_and_defers_none() -> TestResult {
    let (root, answer, _, _) = capped_run(FOLLOW_UP_CAP - FIVE_HOST_FOLLOW_UPS)?;
    assert_eq!(
        (
            answer.exit,
            document_attempts(&answer.envelope),
            answer.envelope["deferred"].as_array().map(Vec::len),
            stored_deferred(root.path())?.len(),
        ),
        (Exit::Ok, FOLLOW_UP_CAP, Some(0), 0)
    );
    Ok(())
}

/// S7-D4 and S7-D6 through `collect`: the 151st follow-up, CNW's last in round-robin order, is
/// never sent and is one `deferred` row naming it, in the envelope and the ledger; the run exits
/// 8 `partial_collect` (S7-D8).
#[test]
fn the_151st_follow_up_is_deferred_recorded_and_never_sent_and_the_run_exits_8() -> TestResult {
    let (root, answer, exchange, urls) = capped_run(FOLLOW_UP_CAP + 1 - FIVE_HOST_FOLLOW_UPS)?;
    let last = urls.last().cloned().ok_or("no synthetic release")?;
    let story = digest(canon::digest::story_id(
        SOURCE,
        last.trim_start_matches("https://"),
    ));
    let envelope = &answer.envelope;
    let (sent, _) = exchange.sent();
    let run_id = envelope["run_id"].as_str().ok_or("run_id")?.to_owned();
    let listed = "2026-11-16T02:22:00-05:00";
    let expected_row: DeferredRow = (
        run_id,
        SOURCE.to_owned(),
        "2026-11-16".to_owned(),
        Some("www.newswire.ca".to_owned()),
        last.clone(),
        "document".to_owned(),
        story.clone(),
        Some(listed.to_owned()),
        "follow_up_cap".to_owned(),
        "2026-11-16T09:30:30.000000Z".to_owned(),
    );
    assert_eq!(
        (
            answer.exit.code(),
            answer.exit.error_code(),
            document_attempts(envelope),
            sent.contains(&last),
            stored_deferred(root.path())?,
        ),
        (
            8,
            Some("partial_collect"),
            FOLLOW_UP_CAP,
            false,
            vec![expected_row]
        )
    );
    assert_eq!(
        (
            envelope["deferred"].clone(),
            envelope["per_source"][0]["deferred"].as_u64(),
        ),
        (
            serde_json::json!([{
                "source": SOURCE,
                "target_window": "2026-11-16",
                "host": "www.newswire.ca",
                "url": last,
                "phase": "document",
                "story_id": story,
                "listed_date": listed,
                "reason": "follow_up_cap",
            }]),
            Some(1)
        )
    );
    let mut human = Vec::new();
    cmd_collect::render_human(envelope, &mut human)?;
    let line = String::from_utf8(human)?
        .lines()
        .find(|line| line.starts_with("deferred"))
        .map(str::to_owned);
    let report = newsvault::cmd_ledger::read(
        root.path(),
        &newsvault::cmd_ledger::Ask {
            at: instant("2026-11-17T00:00:00Z"),
            from: "2026-11-16".parse()?,
            to: "2026-11-16".parse()?,
            source: None,
            expected_runs: None,
            limit: None,
            check: false,
        },
    )?;
    let ledgered: Vec<i64> = report
        .days
        .iter()
        .flat_map(|day| &day.per_source)
        .map(|row| row.deferred.follow_up_cap)
        .collect();
    assert_eq!(
        (line.as_deref(), ledgered),
        (Some("deferred         1 follow_up_cap 1"), vec![1])
    );
    Ok(())
}

/// `RSS_1205` with `extra` synthetic English releases after its three, ids from 3405000 up.
fn gnw_rss_with(extra: u64) -> Result<Vec<u8>, Box<dyn Error>> {
    let fixture =
        std::fs::read_to_string(provider_dir().join("tests/fixtures/gnw").join(RSS_1205))?;
    let items: String = (0..extra)
        .map(|nth| {
            let id = 3_405_000 + nth;
            let url = format!(
                "https://www.globenewswire.com/news-release/2026/11/17/{id}/0/en/capped.html"
            );
            format!(
                "<item><guid isPermaLink=\"true\">{url}</guid><link>{url}</link>\
                 <title>A capped release</title><pubDate>Tue, 17 Nov 2026 12:03 GMT</pubDate>\
                 <dc:identifier>{id}</dc:identifier><dc:language>en</dc:language></item>\n"
            )
        })
        .collect();
    Ok(fixture
        .replacen("</channel>", &format!("{items}</channel>"), 1)
        .into_bytes())
}

/// S7-D6's exception on the real path: a walked poll whose listed follow-ups pass the cap still
/// sends every walk id and defers listed releases only; a deferral takes no ordinal, so the walk
/// ids asked after it keep dense `attempt_id`s.
#[test]
fn a_walked_poll_past_the_cap_sends_every_walk_id_and_defers_listed_releases_only() -> TestResult {
    let root = after_a_first_walked_poll()?;
    let mut exchange = Gnw::walked(RSS_1205)?;
    exchange.pages.insert(
        INDEX.to_owned(),
        gnw_rss_with(u64::try_from(FOLLOW_UP_CAP)?)?,
    );
    let second = walked_poll(
        root.path(),
        hosts_fixture(WALK_HOSTS),
        WALK_SECOND,
        &exchange,
    )?;
    let (sent, _) = exchange.sent();
    let deferred: Vec<String> = stored_deferred(root.path())?
        .into_iter()
        .map(|row| row.4)
        .collect();
    let walked = [3_404_416, 3_404_417, 3_404_418, 3_404_420, 3_404_421];
    assert_eq!(
        (
            walked_ids(&sent),
            deferred.len(),
            deferred
                .iter()
                .all(|url| url.ends_with("/0/en/capped.html")),
            second.exit.error_code(),
        ),
        (walked.to_vec(), 3, true, Some("partial_collect"))
    );
    let run = second.envelope["run_id"].as_str().ok_or("run_id")?;
    let window = second.envelope["window"].as_str().ok_or("window")?;
    let ids: Vec<Option<String>> = second.envelope["attempts"]
        .as_array()
        .ok_or("attempts")?
        .iter()
        .map(|row| row["attempt_id"].as_str().map(str::to_owned))
        .collect();
    let dense: Vec<Option<String>> = (0..u32::try_from(ids.len())?)
        .map(|ordinal| digest(canon::digest::attempt_id(run, SOURCE, window, ordinal)))
        .collect();
    assert_eq!(ids, dense);
    Ok(())
}
