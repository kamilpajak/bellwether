//! Against committed fixtures and never the network. The GNW files are trimmed from the
//! 2026-11-14 probes (architecture.md section 11.6); every `synthetic_` file says so in its head.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use canon::digest::story_id;
use canon::FileDigest;
use chrono::{DateTime, Utc};
use news_provider::conformance::{breaches, RecordingSink, Subject};
use news_provider::provider::{
    precision_of, Cadence, Calendar, CollectedItem, Extracted, Listing, PlanRefusal, Provenance,
    Provider, RawBlob, UnusableReason, UnusableRow, ValidPrecision, ValidSource,
};
use news_provider::request::{HeaderValue, Request, RequestPhase};
use news_provider::{
    AttemptSlot, CredentialRef, RawResponse, RunId, TargetWindow, TransportError, TransportKind,
    Verdict,
};
use open_web_feed::{OpenWebFeed, Refusal, CONTACT_CREDENTIAL, SOURCE};
use pit::datedness::{Minute, Published};
use pit::ObservationTime;

type Outcome = Result<(), Box<dyn std::error::Error>>;

const CONTACT: &str = "bellwether-feed/0.1 (+contact@bellwether.test)";
const NO_CONTACT: &str = "bellwether-feed/0.1";
/// Inside Saturday 2026-11-14, the day the probes ran.
const RUN_AT: &str = "2026-11-14T19:30:00Z";
const RUN: &str = "run-2026-11-14T19:30:00Z";
const RUN_DAY: &str = "2026-11-14";
const GNW_INDEX: &str = "https://rss.globenewswire.com/rssfeed/";
const GNW_HOST: &str = "rss.globenewswire.com";
const REAL_INDEX: &str = "rssfeed_3_items.xml";
const REAL_ROWS: u32 = 3;
const REAL_ARTICLE: &str = "article_3403075.html";
const ARTICLE_URL: &str = "https://rss.globenewswire.com/news-release/2026/11/14/3403075/0/en/usta-florida-releases-2026-state-of-florida-tennis-report.html";
const ARTICLE_WWW_URL: &str = "https://www.globenewswire.com/news-release/2026/11/14/3403075/0/en/usta-florida-releases-2026-state-of-florida-tennis-report.html";
const ARTICLE_UPPER_CASE_URL: &str = "https://WWW.GlobeNewswire.com/news-release/2026/11/14/3403075/0/en/usta-florida-releases-2026-state-of-florida-tennis-report.html";
const ARTICLE_KEY: &str = "rss.globenewswire.com/news-release/2026/11/14/3403075/0/en/usta-florida-releases-2026-state-of-florida-tennis-report.html";
const ARTICLE_TITLE: &str = "USTA FLORIDA RELEASES 2026 STATE OF FLORIDA TENNIS REPORT";
/// The three real `pubDate` values, minutes only, as the 2026-11-14 19:22:19Z feed spelled them.
const REAL_PUB_DATES: [&str; 3] = [
    "Sat, 14 Nov 2026 19:00 GMT",
    "Sat, 14 Nov 2026 18:45 GMT",
    "Sat, 14 Nov 2026 18:38 GMT",
];
const ARTICLE_MINUTE: &str = "2026-11-14T19:00:00Z";
/// Each read by the rule's format into a value that is not a minute of the release.
const NOT_A_MINUTE_PUB_DATES: [&str; 3] = [
    "Sat, 14 Nov 2026 19:00:05 GMT",
    // chrono's `%Y` reads it as the year 26; review 2026-11-15.
    "Sat, 14 Nov 26 19:00 GMT",
    "Sat, 14 Nov 2026 9:05 GMT",
];
const SYNTHETIC_INDEX: &str = "synthetic_index_one_release.xml";
const SYNTHETIC_PAGE: &str = "synthetic_canonical_differs.html";
const SYNTHETIC_CANONICAL: &str =
    "https://www.globenewswire.com/en/news-release/2026/11/14/9999999/0/en/a-renamed-slug.html";
const RELATIVE_CANONICAL_PAGE: &str = "synthetic_relative_canonical.html";
const RELATIVE_CANONICAL: &str = "/en/news-release/2026/11/14/3403075/0/en/a-relative-slug.html";
const SCHEME_RELATIVE_CANONICAL: &str =
    "//rss.globenewswire.com/en/news-release/2026/11/14/3403075/0/en/a-relative-slug.html";
const RESOLVED_CANONICAL: &str =
    "https://rss.globenewswire.com/en/news-release/2026/11/14/3403075/0/en/a-relative-slug.html";
const EMPTY_PARAGRAPHS_PAGE: &str = "synthetic_empty_paragraphs.html";
const NO_CONTAINER_PAGE: &str = "synthetic_no_article_container.html";
const BOT_PROTECTION_PAGE: &str = "akamai_403_access_denied.html";
const PROSE_PAGE: &str = "synthetic_script_and_symbol_paragraphs.html";
const TWO_CONTAINERS_PAGE: &str = "synthetic_two_containers.html";
/// `parse::DEEPEST_PAGE`, and the `html`, `body`, container and `<p>` around the nested divs.
const DEEPEST_PAGE: usize = 112;
const PAGE_DEPTH_AROUND_THE_WRAPPERS: usize = 4;
const ARTICLE_PARAGRAPHS: usize = 3;
const ARTICLE_OPENS: &str = "ORLANDO, FLORIDA, Nov. 14, 2026 (GLOBE NEWSWIRE) -- USTA Florida";
/// Text the trimmed page holds OUTSIDE its article container: a `<p>` (the source line), two
/// `<script>` payloads, and the tag list. A selector wider than the container reaches the first.
const OUTSIDE_THE_CONTAINER: [&str; 5] = [
    "Source:",
    "November 14, 2026 14:00 ET",
    "dataLayer",
    "pnrApplicationSettings",
    "Tennis Courts Florida",
];
const FIRST_ATTEMPT: u32 = 0;
const OK: u16 = 200;

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixtures() -> PathBuf {
    crate_dir().join("tests/fixtures")
}

fn gnw(name: &str) -> Vec<u8> {
    std::fs::read(fixtures().join("gnw").join(name)).unwrap_or_default()
}

fn hosts_path() -> PathBuf {
    crate_dir().join("hosts/open_web_wires.hosts.json")
}

fn rules_path() -> PathBuf {
    crate_dir().join("rules/open-web-feed.rules.json")
}

fn hosts_fixture(name: &str) -> PathBuf {
    fixtures().join("hosts").join(name)
}

fn rules_fixture(name: &str) -> PathBuf {
    fixtures().join("rules").join(name)
}

fn provider() -> Result<OpenWebFeed, Refusal> {
    OpenWebFeed::open(&hosts_path(), &rules_path(), CONTACT)
}

fn refusal_of(hosts: &Path, rules: &Path, contact: &str) -> Option<Refusal> {
    OpenWebFeed::open(hosts, rules, contact).err()
}

fn at(text: &str) -> ObservationTime {
    ObservationTime::new(text.parse().unwrap_or(DateTime::<Utc>::UNIX_EPOCH))
}

fn blob(bytes: Vec<u8>) -> RawBlob {
    RawBlob {
        digest: FileDigest::of(&bytes).hex().to_owned(),
        bytes,
    }
}

fn index_request(provider: &OpenWebFeed) -> Result<Request, Box<dyn std::error::Error>> {
    let plan = provider.plan(&TargetWindow::new(RUN_DAY), None, None, at(RUN_AT))?;
    plan.requests
        .first()
        .cloned()
        .ok_or_else(|| "the run day planned no request".into())
}

fn follow_ups(
    provider: &OpenWebFeed,
    index: &[u8],
) -> Result<Extracted, Box<dyn std::error::Error>> {
    Ok(provider.extract(&index_request(provider)?, &blob(index.to_vec())))
}

/// The follow-up the real index names for release 3403075, with its real `pubDate`.
fn article_request(provider: &OpenWebFeed) -> Result<Request, Box<dyn std::error::Error>> {
    follow_ups(provider, &gnw(REAL_INDEX))?
        .follow_ups
        .first()
        .cloned()
        .ok_or_else(|| "the real index yielded no follow-up".into())
}

fn only_item(extracted: &Extracted) -> Result<CollectedItem, Box<dyn std::error::Error>> {
    match extracted.items.as_slice() {
        [item] => Ok(item.clone()),
        _ => Err(format!("expected one item, got {extracted:?}").into()),
    }
}

fn only_unusable(extracted: &Extracted) -> Result<UnusableRow, Box<dyn std::error::Error>> {
    match (extracted.items.as_slice(), extracted.unusable.as_slice()) {
        ([], [row]) => Ok(row.clone()),
        _ => Err(format!("expected one unusable row, got {extracted:?}").into()),
    }
}

/// The verdict for one answer, opened through a real slot so the attempt id is the ledger's.
fn verdict(
    provider: &OpenWebFeed,
    request: &Request,
    status: u16,
    body: &[u8],
) -> Result<Verdict, Box<dyn std::error::Error>> {
    let mut sink = RecordingSink::default();
    let slot = AttemptSlot::open(
        &mut sink,
        request,
        &RunId::new(RUN),
        FIRST_ATTEMPT,
        at(RUN_AT),
    )?;
    let answer = Ok(RawResponse::new(
        slot.attempt_id().clone(),
        status,
        body.to_vec(),
        at(RUN_AT),
        at(RUN_AT),
    ));
    Ok(provider.classify(request, &answer))
}

// ---- plan ----

/// §8 row R10 and section 11.1: live only, so no `vendor_archive` request can exist.
#[test]
fn a_target_other_than_the_run_day_is_refused_before_any_request() -> Outcome {
    let provider = provider()?;
    for window in [
        "2026-11-13",
        "2026-11-15",
        "2026-11-13..2026-11-14",
        "2026-11-15..2026-11-14",
    ] {
        assert_eq!(
            provider
                .plan(&TargetWindow::new(window), None, None, at(RUN_AT))
                .err(),
            Some(PlanRefusal::NotTheRunDay {
                source_id: SOURCE.to_owned(),
                window: window.to_owned(),
                run_day: RUN_DAY.parse()?,
            }),
            "{window}"
        );
    }
    let plan = provider.plan(&TargetWindow::new(RUN_DAY), None, None, at(RUN_AT))?;
    assert_eq!(plan.requests.len(), 1);
    assert_eq!(plan.not_planned, Vec::new());
    Ok(())
}

/// Every driver refuses the mismatch first, so only this test reaches the check in `plan`
/// (review 2026-11-15: that line removed, the whole suite stayed green).
#[test]
fn a_calendar_or_a_listing_handed_to_plan_is_refused() -> Outcome {
    let provider = provider()?;
    let calendar = Calendar::read(br#"{"schema":"calendar/1","sessions":["2026-11-14"]}"#)?;
    let listing = Listing::of_days(&[RUN_DAY.parse()?]);
    for (calendar, listing, why) in [
        (Some(&calendar), None, "was handed a calendar"),
        (None, Some(&listing), "was handed a listing"),
    ] {
        assert_eq!(
            provider
                .plan(&TargetWindow::new(RUN_DAY), calendar, listing, at(RUN_AT))
                .err(),
            Some(PlanRefusal::InputsDoNotMatchCadence {
                source_id: SOURCE.to_owned(),
                cadence: Cadence::EveryUtcDay.name(),
                why,
            }),
            "{why}"
        );
    }
    Ok(())
}

#[test]
fn a_window_nothing_can_read_is_refused_rather_than_planned_as_nothing() -> Outcome {
    assert_eq!(
        provider()?
            .plan(&TargetWindow::new("2026-QTR4"), None, None, at(RUN_AT))
            .err(),
        Some(PlanRefusal::TargetWindowUnreadable("2026-QTR4".to_owned()))
    );
    Ok(())
}

/// §8 row R19(a). The fixture's second row is a synthetic `candidate`.
#[test]
fn only_admitted_hosts_are_planned() -> Outcome {
    let provider = OpenWebFeed::open(
        &hosts_fixture("one_admitted_one_candidate.json"),
        &rules_path(),
        CONTACT,
    )?;
    let plan = provider.plan(&TargetWindow::new(RUN_DAY), None, None, at(RUN_AT))?;
    let urls: Vec<&str> = plan
        .requests
        .iter()
        .map(|request| request.url.as_str())
        .collect();
    assert_eq!(urls, vec![GNW_INDEX]);
    Ok(())
}

#[test]
fn every_planned_request_is_a_live_index_carrying_the_hosts_version() -> Outcome {
    let provider = provider()?;
    let request = index_request(&provider)?;
    assert_eq!(
        (
            request.phase,
            request.provenance,
            request.target_window.as_str(),
            request.hosts_version.as_ref(),
            request.listed_date.as_ref(),
        ),
        (
            RequestPhase::Index,
            Provenance::LiveIngest,
            RUN_DAY,
            Some(provider.hosts_version()),
            None
        )
    );
    assert_eq!(
        provider.hosts_version(),
        &FileDigest::of(&std::fs::read(hosts_path())?)
    );
    Ok(())
}

#[test]
fn the_roster_is_the_empty_one_and_every_request_carries_it() -> Outcome {
    let provider = provider()?;
    let empty = FileDigest::of(&[]);
    assert_eq!(provider.universe_version(), empty);
    assert_eq!(index_request(&provider)?.universe_version, empty);
    assert_eq!(article_request(&provider)?.universe_version, empty);
    Ok(())
}

// ---- classify ----

/// §8 row R21(b) and section 11.1: GNW's own Akamai 403 page, from the 2026-11-14 robots fetch.
#[test]
fn a_bot_protection_page_is_permanent_never_transient() -> Outcome {
    let provider = provider()?;
    let page = gnw(BOT_PROTECTION_PAGE);
    for request in [index_request(&provider)?, article_request(&provider)?] {
        assert_eq!(
            verdict(&provider, &request, 403, &page)?,
            Verdict::PermanentFailure
        );
    }
    Ok(())
}

#[test]
fn every_other_client_error_is_permanent_and_a_rate_limit_or_server_error_is_transient() -> Outcome
{
    let provider = provider()?;
    let request = article_request(&provider)?;
    for (status, expected) in [
        (401, Verdict::PermanentFailure),
        (404, Verdict::PermanentFailure),
        (410, Verdict::PermanentFailure),
        (301, Verdict::PermanentFailure),
        (429, Verdict::TransientFailure),
        (500, Verdict::TransientFailure),
        (503, Verdict::TransientFailure),
    ] {
        assert_eq!(
            verdict(&provider, &request, status, b"")?,
            expected,
            "{status}"
        );
    }
    Ok(())
}

#[test]
fn a_lost_connection_is_transient() -> Outcome {
    let provider = provider()?;
    let request = index_request(&provider)?;
    let mut sink = RecordingSink::default();
    let slot = AttemptSlot::open(
        &mut sink,
        &request,
        &RunId::new(RUN),
        FIRST_ATTEMPT,
        at(RUN_AT),
    )?;
    let lost = Err(TransportError::new(
        slot.attempt_id().clone(),
        at(RUN_AT),
        at(RUN_AT),
        "connection reset".to_owned(),
    ));
    assert_eq!(
        provider.classify(&request, &lost),
        Verdict::TransientFailure
    );
    Ok(())
}

/// `Wire` reads `BODY_LIMIT` bytes and refuses the rest, so a refetch fails the same way.
#[test]
fn a_body_over_the_limit_is_permanent() -> Outcome {
    let provider = provider()?;
    let request = index_request(&provider)?;
    let mut sink = RecordingSink::default();
    let slot = AttemptSlot::open(
        &mut sink,
        &request,
        &RunId::new(RUN),
        FIRST_ATTEMPT,
        at(RUN_AT),
    )?;
    let too_large = Err(TransportError::new(
        slot.attempt_id().clone(),
        at(RUN_AT),
        at(RUN_AT),
        "the response body is larger than the limit".to_owned(),
    )
    .of_kind(TransportKind::BodyOverLimit));
    assert_eq!(
        provider.classify(&request, &too_large),
        Verdict::PermanentFailure
    );
    Ok(())
}

#[test]
fn an_index_is_usable_with_rows_empty_without_and_permanent_when_it_is_not_rss() -> Outcome {
    let provider = provider()?;
    let request = index_request(&provider)?;
    let empty =
        b"<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>t</title></channel></rss>";
    assert_eq!(
        verdict(&provider, &request, OK, &gnw(REAL_INDEX))?,
        Verdict::Usable
    );
    assert_eq!(verdict(&provider, &request, OK, empty)?, Verdict::Empty);
    assert_eq!(
        verdict(&provider, &request, OK, &gnw(REAL_ARTICLE))?,
        Verdict::PermanentFailure
    );
    Ok(())
}

/// `classify`'s refusal of a host no row admits is what keeps `extract` from reading it: a host
/// the file never names, and a `candidate` row's host. Mutation 2026-11-15, `admitted_row`
/// matching any row: green before the candidate case.
#[test]
fn an_answer_from_a_host_no_row_admits_is_permanent() -> Outcome {
    let with_a_candidate = OpenWebFeed::open(
        &hosts_fixture("one_admitted_one_candidate.json"),
        &rules_path(),
        CONTACT,
    )?;
    for provider in [provider()?, with_a_candidate] {
        let mut request = article_request(&provider)?;
        request.url = "https://www.prnewswire.com/news-releases/a.html".to_owned();
        assert_eq!(
            verdict(&provider, &request, OK, &gnw(REAL_ARTICLE))?,
            Verdict::PermanentFailure
        );
        let extracted = provider.extract(&request, &blob(gnw(REAL_ARTICLE)));
        assert_eq!((extracted.items.len(), extracted.unusable.len()), (0, 0));
    }
    Ok(())
}

// ---- index ----

#[test]
fn the_real_index_yields_one_follow_up_per_release_on_the_rss_host_with_its_raw_pub_date() -> Outcome
{
    let provider = provider()?;
    let extracted = follow_ups(&provider, &gnw(REAL_INDEX))?;
    assert_eq!(
        (extracted.vendor_rows, extracted.filtered_out),
        (REAL_ROWS, 0)
    );
    let dates: Vec<Option<&str>> = extracted
        .follow_ups
        .iter()
        .map(|request| request.listed_date.as_deref())
        .collect();
    assert_eq!(dates, REAL_PUB_DATES.map(Some).to_vec());
    let first = extracted.follow_ups.first().ok_or("no follow-up")?;
    assert_eq!(
        (
            first.url.as_str(),
            first.phase,
            first.hosts_version.as_ref()
        ),
        (
            ARTICLE_URL,
            RequestPhase::Document,
            Some(provider.hosts_version())
        )
    );
    for request in &extracted.follow_ups {
        assert!(request
            .url
            .starts_with("https://rss.globenewswire.com/news-release/"));
    }
    Ok(())
}

#[test]
fn a_link_off_the_path_filter_off_the_host_or_absent_is_filtered_out_and_counted() -> Outcome {
    let item = |link: &str| {
        format!("<item><link>{link}</link><pubDate>Sat, 14 Nov 2026 19:00 GMT</pubDate></item>")
    };
    let index = format!(
        "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel>{}{}{}{}{}{}</channel></rss>",
        item(ARTICLE_WWW_URL),
        item("https://www.globenewswire.com/en/search/tag/tennis"),
        item("https://www.prnewswire.com/news-release/2026/11/14/1/a.html"),
        "<item><pubDate>Sat, 14 Nov 2026 19:00 GMT</pubDate></item>",
        item("https://rss.globenewswire.com/news-release/../rssfeed/"),
        item("https://rss.globenewswire.com/news-release/./x.html"),
    );
    let extracted = follow_ups(&provider()?, index.as_bytes())?;
    assert_eq!(
        (
            extracted.vendor_rows,
            extracted.follow_ups.len(),
            extracted.filtered_out
        ),
        (6, 1, 5)
    );
    Ok(())
}

/// Two follow-ups for one story would be two paced requests and two `listed_date` values.
#[test]
fn a_story_listed_twice_in_one_index_is_followed_up_once_and_the_repeat_counted() -> Outcome {
    let item = |link: &str, pub_date: &str| {
        format!("<item><link>{link}</link><pubDate>{pub_date}</pubDate></item>")
    };
    let index = format!(
        "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel>{}{}{}</channel></rss>",
        item(ARTICLE_URL, REAL_PUB_DATES[0]),
        item(ARTICLE_URL, REAL_PUB_DATES[1]),
        item(ARTICLE_WWW_URL, REAL_PUB_DATES[2]),
    );
    let extracted = follow_ups(&provider()?, index.as_bytes())?;
    let kept: Vec<(&str, Option<&str>)> = extracted
        .follow_ups
        .iter()
        .map(|request| (request.url.as_str(), request.listed_date.as_deref()))
        .collect();
    assert_eq!(
        (kept, extracted.vendor_rows, extracted.filtered_out),
        (vec![(ARTICLE_URL, Some(REAL_PUB_DATES[0]))], 3, 2)
    );
    Ok(())
}

/// RSS 2.0 elements have no namespace; a namespaced element of the same local name placed first
/// was read instead of the item's own `pubDate` or `link` (review 2026-11-15).
#[test]
fn a_namespaced_element_named_like_an_rss_field_is_never_read_as_that_field() -> Outcome {
    let other_release = ARTICLE_URL.replace("3403075", "3403076");
    let index = format!(
        "<?xml version=\"1.0\"?><rss version=\"2.0\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:atom=\"http://www.w3.org/2005/Atom\"><channel>\
         <item><dc:pubDate>Fri, 13 Nov 2026 01:00 GMT</dc:pubDate><link>{ARTICLE_URL}</link>\
         <pubDate>{}</pubDate></item>\
         <item><atom:link>https://www.prnewswire.com/news-release/a.html</atom:link>\
         <link>{other_release}</link><pubDate>{}</pubDate></item></channel></rss>",
        REAL_PUB_DATES[0], REAL_PUB_DATES[1],
    );
    let extracted = follow_ups(&provider()?, index.as_bytes())?;
    let kept: Vec<(&str, Option<&str>)> = extracted
        .follow_ups
        .iter()
        .map(|request| (request.url.as_str(), request.listed_date.as_deref()))
        .collect();
    assert_eq!(
        kept,
        vec![
            (ARTICLE_URL, Some(REAL_PUB_DATES[0])),
            (other_release.as_str(), Some(REAL_PUB_DATES[1])),
        ]
    );
    Ok(())
}

/// Section 11.2: the rewrite is a hashed rule, not a redirect, so both links key one story.
#[test]
fn www_and_rss_links_of_one_release_share_one_item_key() -> Outcome {
    let provider = provider()?;
    assert_eq!(
        provider.item_key(ARTICLE_WWW_URL).as_deref(),
        Some(ARTICLE_KEY)
    );
    assert_eq!(provider.item_key(ARTICLE_URL).as_deref(), Some(ARTICLE_KEY));
    // Mutation 2026-11-15, the host's case kept: green before this case.
    assert_eq!(
        provider.item_key(ARTICLE_UPPER_CASE_URL).as_deref(),
        Some(ARTICLE_KEY)
    );
    assert_eq!(
        provider
            .item_key(&format!("{ARTICLE_URL}?utm=x#top"))
            .as_deref(),
        Some(ARTICLE_KEY)
    );
    Ok(())
}

/// `item_key` is `story_id`'s input, so a spelling `HostKey` refuses would key a second story.
#[test]
fn a_url_with_a_port_or_a_userinfo_has_no_item_key() -> Outcome {
    let provider = provider()?;
    for url in [
        ARTICLE_URL.replacen(GNW_HOST, &format!("{GNW_HOST}:443"), 1),
        ARTICLE_URL.replacen(GNW_HOST, &format!("u@{GNW_HOST}"), 1),
    ] {
        assert_eq!(provider.item_key(&url), None, "{url}");
    }
    Ok(())
}

// ---- document ----

#[test]
fn a_globenewswire_pubdate_is_a_minute_and_never_an_instant() -> Outcome {
    let provider = provider()?;
    let request = article_request(&provider)?;
    let item = only_item(&provider.extract(&request, &blob(gnw(REAL_ARTICLE))))?;
    let minute = Minute::new(ARTICLE_MINUTE.parse()?)?;
    assert_eq!(
        (
            &item.valid,
            precision_of(&item.valid),
            item.valid_raw.as_str(),
            item.valid_field.as_str(),
            item.valid_source,
        ),
        (
            &Published::AtMinute(minute),
            ValidPrecision::Minute,
            REAL_PUB_DATES[0],
            "pubDate",
            ValidSource::VendorPublished,
        )
    );
    for raw in NOT_A_MINUTE_PUB_DATES {
        let unread = Request {
            listed_date: Some(raw.to_owned()),
            ..request.clone()
        };
        let row = only_unusable(&provider.extract(&unread, &blob(gnw(REAL_ARTICLE))))?;
        assert_eq!(
            (row.reason, &row.item.valid, row.item.valid_raw.as_str()),
            (
                UnusableReason::NoParseableValidTime,
                &Published::Unknown,
                raw
            )
        );
    }
    Ok(())
}

/// Pins today's reading of days 1-9 until a real GNW `pubDate` on one is recorded (§11.1, Unknown):
/// `%d` reads `1` as well as `01`, and only `01` prints back byte for byte.
#[test]
fn a_single_digit_day_is_a_minute_only_when_zero_padded() -> Outcome {
    let provider = provider()?;
    let request = article_request(&provider)?;
    let read = |raw: &str| -> Result<Published, Box<dyn std::error::Error>> {
        let dated = Request {
            listed_date: Some(raw.to_owned()),
            ..request.clone()
        };
        let extracted = provider.extract(&dated, &blob(gnw(REAL_ARTICLE)));
        Ok(match extracted.items.first() {
            Some(item) => item.valid,
            None => only_unusable(&extracted)?.item.valid,
        })
    };
    let first_of_november = Minute::new("2026-11-01T19:00:00Z".parse()?)?;
    assert_eq!(
        (
            read("Sun, 01 Nov 2026 19:00 GMT")?,
            read("Sun, 1 Nov 2026 19:00 GMT")?
        ),
        (Published::AtMinute(first_of_november), Published::Unknown)
    );
    Ok(())
}

/// §8 row R17 for GNW: `<p>` text inside `div.main-body-container` (section 11.6), and none of the
/// page's `<p>`, `<script>` or tag text outside it.
#[test]
fn the_extracted_body_is_article_text_and_never_page_html_on_a_globenewswire_page() -> Outcome {
    let provider = provider()?;
    let item =
        only_item(&provider.extract(&article_request(&provider)?, &blob(gnw(REAL_ARTICLE))))?;
    let body = item.body.ok_or("no body")?.text;
    assert!(body.starts_with(ARTICLE_OPENS), "{body}");
    assert_eq!(body.lines().count(), ARTICLE_PARAGRAPHS, "{body}");
    assert!(!body.contains('<'), "{body}");
    for outside in OUTSIDE_THE_CONTAINER {
        assert!(
            !body.contains(outside),
            "{outside} reached the body: {body}"
        );
    }
    Ok(())
}

#[test]
fn the_real_article_carries_its_title_and_its_canonical_link_and_no_observation() -> Outcome {
    let provider = provider()?;
    let item =
        only_item(&provider.extract(&article_request(&provider)?, &blob(gnw(REAL_ARTICLE))))?;
    assert_eq!(
        (
            item.title.as_str(),
            item.canonical_url.as_str(),
            item.source_item_id.as_str(),
            item.observed,
            item.provenance,
        ),
        (
            ARTICLE_TITLE,
            ARTICLE_URL,
            ARTICLE_KEY,
            None,
            Provenance::LiveIngest
        )
    );
    Ok(())
}

/// §8 row R3(a): the story a follow-up is held under before the fetch is the story its item
/// carries after it, on a page whose canonical link names another host and slug.
#[test]
fn the_follow_up_story_equals_the_item_story_on_a_page_whose_canonical_link_differs() -> Outcome {
    let provider = provider()?;
    let follow_up = follow_ups(&provider, &gnw(SYNTHETIC_INDEX))?
        .follow_ups
        .first()
        .cloned()
        .ok_or("the synthetic index yielded no follow-up")?;
    let item = only_item(&provider.extract(&follow_up, &blob(gnw(SYNTHETIC_PAGE))))?;
    assert_eq!(item.canonical_url, SYNTHETIC_CANONICAL);
    assert_ne!(item.canonical_url, item.url);
    assert!(provider.story_of(&follow_up).is_some());
    assert_eq!(
        provider.story_of(&follow_up),
        story_id(SOURCE, &item.source_item_id).ok()
    );
    Ok(())
}

/// Review 2026-11-15: a root-relative canonical href reached `doc_id` as the bare path, so two
/// hosts serving one path shared a `doc_id`. A path-relative or non-web href is the page's url.
#[test]
fn a_relative_canonical_link_is_resolved_against_the_page_url_before_it_reaches_doc_id() -> Outcome
{
    let provider = provider()?;
    let request = article_request(&provider)?;
    let page = String::from_utf8(gnw(RELATIVE_CANONICAL_PAGE))?;
    let mut canonicals = Vec::new();
    for href in [
        RELATIVE_CANONICAL,
        SCHEME_RELATIVE_CANONICAL,
        "a.html",
        "javascript:void(0)",
        A_CANONICAL_WITH_A_PORT,
        A_CANONICAL_ON_A_HOST_KEY_REFUSES,
        A_SCHEME_RELATIVE_CANONICAL_WITH_A_PORT,
        "https://",
        "https:///news-release/x.html",
    ] {
        let written = page.replace(RELATIVE_CANONICAL, href).into_bytes();
        canonicals.push(only_item(&provider.extract(&request, &blob(written)))?.canonical_url);
    }
    assert_eq!(
        canonicals,
        [
            RESOLVED_CANONICAL,
            RESOLVED_CANONICAL,
            ARTICLE_URL,
            ARTICLE_URL,
            A_CANONICAL_WITH_A_PORT,
            A_CANONICAL_ON_A_HOST_KEY_REFUSES,
            A_CANONICAL_WITH_A_PORT,
            ARTICLE_URL,
            ARTICLE_URL
        ]
    );
    Ok(())
}

/// Absolute hrefs `host_and_path` refuses; review 2026-11-15 found both replaced by the page url,
/// and the scheme-relative spelling still replaced after that fix (a second `doc_id` for one link).
const A_CANONICAL_WITH_A_PORT: &str = "https://www.globenewswire.com:443/news-release/x.html";
const A_CANONICAL_ON_A_HOST_KEY_REFUSES: &str = "https://other_host.example/x.html";
const A_SCHEME_RELATIVE_CANONICAL_WITH_A_PORT: &str =
    "//www.globenewswire.com:443/news-release/x.html";

/// The other half of `NoArticleBody` (section 11.5 step 2): a container holding only empty or
/// whitespace paragraphs. Mutation 2026-11-15, the empty-paragraph filter removed: green before.
#[test]
fn a_container_holding_only_empty_paragraphs_is_unusable_as_no_article_body() -> Outcome {
    let provider = provider()?;
    let page = blob(gnw(EMPTY_PARAGRAPHS_PAGE));
    let row = only_unusable(&provider.extract(&article_request(&provider)?, &page))?;
    assert_eq!(
        (row.reason, row.item.body.as_ref()),
        (UnusableReason::NoArticleBody, None)
    );
    Ok(())
}

/// Review 2026-11-15: a `<script>` or `<style>` inside a container `<p>` read as body text, and
/// a paragraph of U+200B only passed the emptiness filter.
#[test]
fn paragraph_text_skips_script_style_and_noscript_and_needs_a_letter_or_a_digit() -> Outcome {
    let provider = provider()?;
    let page = blob(gnw(PROSE_PAGE));
    let item = only_item(&provider.extract(&article_request(&provider)?, &page))?;
    assert_eq!(
        item.body.map(|body| body.text).as_deref(),
        Some("First paragraph.\nSecond paragraph.")
    );
    Ok(())
}

/// A lossy decode would store U+FFFD in place of the byte; the page is evidence, not text.
#[test]
fn a_page_that_is_not_utf8_is_unusable_and_never_decoded_lossily() -> Outcome {
    let provider = provider()?;
    let mut bytes = gnw(REAL_ARTICLE);
    bytes.extend_from_slice(&[0xFF, 0xFE]);
    let row = only_unusable(&provider.extract(&article_request(&provider)?, &blob(bytes)))?;
    assert_eq!(
        (row.reason, row.item.body.as_ref()),
        (UnusableReason::PageNotUtf8, None)
    );
    Ok(())
}

#[test]
fn a_page_holding_the_article_container_twice_is_unusable_as_ambiguous() -> Outcome {
    let provider = provider()?;
    let page = blob(gnw(TWO_CONTAINERS_PAGE));
    let row = only_unusable(&provider.extract(&article_request(&provider)?, &page))?;
    assert_eq!(
        (row.reason, row.item.body.as_ref()),
        (UnusableReason::ArticleContainerAmbiguous, None)
    );
    Ok(())
}

/// html5ever is superlinear in nesting depth (section 11.1), so the bound is read before parsing.
#[test]
fn a_page_nested_past_the_bound_is_unusable_and_one_at_the_bound_is_read() -> Outcome {
    let provider = provider()?;
    let request = article_request(&provider)?;
    let nested = |depth: usize| {
        let wrappers = depth - PAGE_DEPTH_AROUND_THE_WRAPPERS;
        blob(
            format!(
                "<html><body>{}<div class=\"main-body-container\"><p>Text.</p></div></body></html>",
                "<div>".repeat(wrappers)
            )
            .into_bytes(),
        )
    };
    let at_the_bound = provider.extract(&request, &nested(DEEPEST_PAGE));
    assert_eq!(
        only_item(&at_the_bound)?
            .body
            .map(|body| body.text)
            .as_deref(),
        Some("Text.")
    );
    let past = only_unusable(&provider.extract(&request, &nested(DEEPEST_PAGE + 1)))?;
    assert_eq!(past.reason, UnusableReason::PageTooDeep);
    Ok(())
}

#[test]
fn a_page_with_no_article_container_is_unusable_as_no_article_body() -> Outcome {
    let provider = provider()?;
    let page = blob(gnw(NO_CONTAINER_PAGE));
    let row = only_unusable(&provider.extract(&article_request(&provider)?, &page))?;
    assert_eq!(
        (row.reason, row.raw_blob.as_str(), row.item.body.as_ref()),
        (UnusableReason::NoArticleBody, page.digest.as_str(), None)
    );
    assert_eq!(row.item.title, "A Page Without Its Article");
    Ok(())
}

/// Section 11.5 step 4 builds the held filter; until then every live follow-up is issued.
#[test]
fn every_live_follow_up_is_issued_even_when_its_story_is_covered() -> Outcome {
    let provider = provider()?;
    let all = follow_ups(&provider, &gnw(REAL_INDEX))?.follow_ups;
    let covered: BTreeSet<String> = all
        .iter()
        .filter_map(|request| provider.story_of(request))
        .map(|story| story.hex().to_owned())
        .collect();
    assert_eq!(covered.len(), all.len());
    assert_eq!(provider.follow_ups_to_issue(all.clone(), &covered), all);
    Ok(())
}

// ---- transport ----

#[test]
fn the_profile_paces_every_host_and_our_address_at_one_request_per_ten_seconds() -> Outcome {
    let profile = provider()?.transport();
    let ten = std::time::Duration::from_secs(10);
    assert_eq!(
        (profile.budget.min_interval, profile.budget.per_host),
        (ten, Some(ten))
    );
    assert_eq!(
        profile.user_agent,
        HeaderValue::FromCredential(CredentialRef::new(CONTACT_CREDENTIAL))
    );
    Ok(())
}

// ---- open: every refusal is raised before any request ----

#[test]
fn a_user_agent_without_a_contact_is_refused_at_startup() {
    assert_eq!(
        refusal_of(&hosts_path(), &rules_path(), NO_CONTACT),
        Some(Refusal::ContactMissing)
    );
}

#[test]
fn an_absent_hosts_file_is_refused() {
    let absent = hosts_fixture("absent.json");
    assert_eq!(
        refusal_of(&absent, &rules_path(), CONTACT),
        Some(Refusal::HostsAbsent(absent))
    );
}

#[test]
fn a_truncated_hosts_file_is_refused_as_unparseable() {
    assert!(matches!(
        refusal_of(&hosts_fixture("truncated.json"), &rules_path(), CONTACT),
        Some(Refusal::HostsUnparseable { .. })
    ));
}

#[test]
fn a_hosts_row_holding_a_field_nothing_reads_is_refused() {
    let path = hosts_fixture("unknown_field.json");
    assert_eq!(
        refusal_of(&path, &rules_path(), CONTACT),
        Some(Refusal::UnknownField {
            path,
            field: "crawl_delay".to_owned()
        })
    );
}

/// Hosts are case-insensitive, so two spellings of one host are one host named twice.
#[test]
fn a_host_named_twice_is_refused() {
    let path = hosts_fixture("duplicate_host.json");
    assert_eq!(
        refusal_of(&path, &rules_path(), CONTACT),
        Some(Refusal::DuplicateHost {
            path,
            host: GNW_HOST.to_owned()
        })
    );
}

#[test]
fn a_host_no_pace_file_can_be_keyed_on_is_refused() {
    let path = hosts_fixture("not_a_host.json");
    assert_eq!(
        refusal_of(&path, &rules_path(), CONTACT),
        Some(Refusal::NotAHost {
            path,
            host: "[::1]".to_owned()
        })
    );
}

#[test]
fn a_hosts_row_naming_a_rule_the_rules_file_lacks_is_refused() {
    let path = hosts_fixture("unknown_rule.json");
    assert_eq!(
        refusal_of(&path, &rules_path(), CONTACT),
        Some(Refusal::UnknownRule {
            path,
            host: GNW_HOST.to_owned(),
            rule: "prn".to_owned()
        })
    );
}

#[test]
fn a_hosts_row_whose_index_is_on_another_host_is_refused() {
    let path = hosts_fixture("index_off_host.json");
    assert_eq!(
        refusal_of(&path, &rules_path(), CONTACT),
        Some(Refusal::IndexOffHost {
            path,
            host: GNW_HOST.to_owned(),
            url: "https://www.globenewswire.com/rssfeed/".to_owned()
        })
    );
}

/// R6: zero admitted rows would plan zero requests, which reads as a quiet day.
#[test]
fn a_hosts_file_admitting_no_host_is_refused() {
    let path = hosts_fixture("none_admitted.json");
    assert_eq!(
        refusal_of(&path, &rules_path(), CONTACT),
        Some(Refusal::NoHostAdmitted(path))
    );
}

#[test]
fn an_absent_rules_file_is_refused() {
    let absent = rules_fixture("absent.json");
    assert_eq!(
        refusal_of(&hosts_path(), &absent, CONTACT),
        Some(Refusal::RulesAbsent(absent))
    );
}

#[test]
fn a_rules_file_with_a_selector_nothing_can_parse_is_refused() {
    assert!(matches!(
        refusal_of(
            &hosts_path(),
            &rules_fixture("rules_bad_selector.json"),
            CONTACT
        ),
        Some(Refusal::RulesUnparseable { .. })
    ));
}

/// Otherwise `item_key` would take whichever rewrite the map kept last.
#[test]
fn a_host_two_rules_rewrite_to_two_hosts_is_refused() {
    let path = rules_fixture("rules_conflicting_rewrite.json");
    assert_eq!(
        refusal_of(&hosts_path(), &path, CONTACT),
        Some(Refusal::RulesUnparseable {
            path,
            why: "www.globenewswire.com is rewritten to two hosts".to_owned()
        })
    );
}

/// `serde_json::Value` keeps the last of two values, so a reader of the file sees the first.
#[test]
fn a_key_named_twice_in_one_object_of_either_file_is_refused() {
    let hosts = hosts_fixture("duplicate_key.json");
    assert_eq!(
        refusal_of(&hosts, &rules_path(), CONTACT),
        Some(Refusal::DuplicateKey {
            path: hosts,
            key: "status".to_owned()
        })
    );
    let rules = rules_fixture("rules_duplicate_key.json");
    assert_eq!(
        refusal_of(&hosts_path(), &rules, CONTACT),
        Some(Refusal::DuplicateKey {
            path: rules,
            key: "path_prefix".to_owned()
        })
    );
}

#[test]
fn a_rewrite_to_a_host_no_url_can_name_is_refused() {
    let path = rules_fixture("rules_rewrite_not_a_host.json");
    assert_eq!(
        refusal_of(&hosts_path(), &path, CONTACT),
        Some(Refusal::RewriteNotAHost {
            path,
            rule: "gnw".to_owned(),
            host: "rss.globenewswire.com/".to_owned()
        })
    );
}

/// Both spellings lowercase to one key, and the map kept whichever came last.
#[test]
fn a_host_one_rule_rewrites_twice_in_two_cases_is_refused() {
    let path = rules_fixture("rules_rewrite_named_twice.json");
    assert_eq!(
        refusal_of(&hosts_path(), &path, CONTACT),
        Some(Refusal::RewriteNamedTwice {
            path,
            rule: "gnw".to_owned(),
            host: "www.globenewswire.com".to_owned()
        })
    );
}

#[test]
fn a_path_filter_not_starting_with_a_slash_is_refused() {
    let path = rules_fixture("rules_path_prefix_unrooted.json");
    assert_eq!(
        refusal_of(&hosts_path(), &path, CONTACT),
        Some(Refusal::PathPrefixNotRooted {
            path,
            rule: "gnw".to_owned(),
            prefix: "news-release/".to_owned()
        })
    );
}

/// A format printing seconds reads only GNW values at second 0; every real one would be unusable.
#[test]
fn a_valid_format_that_does_not_read_back_a_minute_is_refused() {
    let path = rules_fixture("rules_format_prints_seconds.json");
    assert_eq!(
        refusal_of(&hosts_path(), &path, CONTACT),
        Some(Refusal::FormatReadsNoMinute {
            path,
            rule: "gnw".to_owned(),
            format: "%a, %d %b %Y %H:%M:%S GMT".to_owned()
        })
    );
}

/// Prints no second, so only the read-back half refuses it: the printed minute names no year.
#[test]
fn a_valid_format_whose_minute_does_not_parse_back_is_refused() {
    let path = rules_fixture("rules_format_prints_no_year.json");
    assert_eq!(
        refusal_of(&hosts_path(), &path, CONTACT),
        Some(Refusal::FormatReadsNoMinute {
            path,
            rule: "gnw".to_owned(),
            format: "%d %b %H:%M GMT".to_owned()
        })
    );
}

#[test]
fn a_rules_file_holding_a_field_nothing_reads_is_refused() {
    let path = rules_fixture("rules_unknown_field.json");
    assert_eq!(
        refusal_of(&hosts_path(), &path, CONTACT),
        Some(Refusal::UnknownField {
            path,
            field: "follow_redirects".to_owned()
        })
    );
}

// ---- the seam ----

/// The harness, plus the ceiling held tight: every item this source produces carries exactly
/// `Minute`, so a ceiling of `Instant` would claim a precision nothing here can deliver.
#[test]
fn the_open_web_feed_breaks_no_claim_of_the_seam() -> Outcome {
    let provider = provider()?;
    let article = article_request(&provider)?;
    let synthetic = follow_ups(&provider, &gnw(SYNTHETIC_INDEX))?
        .follow_ups
        .first()
        .cloned()
        .ok_or("no synthetic follow-up")?;
    let payloads = [
        (index_request(&provider)?, blob(gnw(REAL_INDEX))),
        (article.clone(), blob(gnw(REAL_ARTICLE))),
        (synthetic, blob(gnw(SYNTHETIC_PAGE))),
        (article, blob(gnw(NO_CONTAINER_PAGE))),
    ];
    let subject = Subject {
        target: TargetWindow::new(RUN_DAY),
        calendar: None,
        listing: None,
        at: at(RUN_AT),
        payloads: &payloads,
    };
    assert_eq!(breaches(&provider, &subject), Vec::new());
    assert_eq!(provider.cadence(), Cadence::EveryUtcDay);
    let ceiling = provider.precision_ceiling();
    let produced: Vec<ValidPrecision> = payloads
        .iter()
        .flat_map(|(request, raw)| provider.extract(request, raw).items)
        .map(|item| precision_of(&item.valid))
        .collect();
    assert_eq!(produced.len(), 2);
    assert!(
        produced.iter().all(|precision| *precision == ceiling),
        "{produced:?} under {ceiling:?}"
    );
    Ok(())
}
