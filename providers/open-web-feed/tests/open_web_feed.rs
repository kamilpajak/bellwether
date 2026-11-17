//! Against committed fixtures and never the network. The GNW and PRN files are trimmed from the
//! 2026-11-14 and 2026-11-16 probes, the CNW, Fed and Yahoo files from the 2026-11-16 probes
//! (architecture.md section 11.6); every `synthetic_` file says so in its head.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use canon::digest::story_id;
use canon::FileDigest;
use chrono::{DateTime, Utc};
use news_provider::conformance::{breaches, RecordingSink, Subject};
use news_provider::provider::{
    precision_of, Cadence, Calendar, CollectedItem, DecidedBy, Extracted, Listing, PlanRefusal,
    Provenance, Provider, RawBlob, UnusableReason, UnusableRow, ValidPrecision, ValidSource,
};
use news_provider::request::{HeaderValue, Request, RequestPhase};
use news_provider::{
    AttemptSlot, CredentialRef, RawResponse, RunId, TargetWindow, TransportError, TransportKind,
    Verdict,
};
use open_web_feed::{
    AdmittedPath, CrossChecker, DirectAnswer, HostPin, HostRobots, OpenWebFeed, Refusal, RobotsPin,
    RobotsRules, CONTACT_CREDENTIAL, DIRECT_LEAD, LONGEST_POLL_PERIOD, SOURCE,
};
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
const PRN_INDEX: &str = "https://www.prnewswire.com/rss/news-releases-list.rss";
const PRN_HOST: &str = "www.prnewswire.com";
/// Items 2-4 of the 20 in the 2026-11-14 19:21:04 GMT feed. The first is a correction notice
/// listed under release link 302913655 (§8 row R3).
const PRN_REAL_INDEX: &str = "rss_3_items.xml";
const PRN_LINKS: [&str; 3] = [
    "https://www.prnewswire.com/news-releases/north-carolinas-electric-cooperatives-launches-evconnectnc-powered-by-weavegrid-to-maximize-charging-during-lower-demand-and-power-cost-periods-302913655.html",
    PRN_ARTICLE_URL,
    PRN_SECOND_ARTICLE_URL,
];
const PRN_PUB_DATES: [&str; 3] = [
    "Sat, 14 Nov 2026 17:48:00 +0000",
    "Sat, 14 Nov 2026 17:15:00 +0000",
    "Sat, 14 Nov 2026 17:01:00 +0000",
];
const PRN_MINUTES: [&str; 3] = [
    "2026-11-14T17:48:00Z",
    "2026-11-14T17:15:00Z",
    "2026-11-14T17:01:00Z",
];
/// A second inside the minute, another offset, a two-digit year: none is a PRN minute.
const PRN_NOT_A_MINUTE_PUB_DATES: [&str; 3] = [
    "Sat, 14 Nov 2026 17:15:30 +0000",
    "Sat, 14 Nov 2026 17:15:00 -0500",
    "Sat, 14 Nov 26 17:15:00 +0000",
];
const PRN_ARTICLE: &str = "article_302939553.html";
const PRN_ARTICLE_URL: &str = "https://www.prnewswire.com/news-releases/novos-wegovy-was-associated-with-a-reduction-in-food-noise-according-to-real-world-studies-at-obesityweek-2026-302939553.html";
const PRN_ARTICLE_KEY: &str = "www.prnewswire.com/news-releases/novos-wegovy-was-associated-with-a-reduction-in-food-noise-according-to-real-world-studies-at-obesityweek-2026-302939553.html";
const PRN_ARTICLE_TITLE: &str = "Novo's Wegovy® was associated with a reduction in food noise according to real-world studies at ObesityWeek® 2026";
const PRN_SECOND_ARTICLE: &str = "article_302938569.html";
const PRN_SECOND_ARTICLE_URL: &str = "https://www.prnewswire.com/news-releases/lilly-to-present-new-data-across-zepbound-foundayo-eloratzp-and-retatrutide-at-obesityweek-2026-moving-obesity-care-beyond-the-scale-302938569.html";
const PRN_SECOND_ARTICLE_TITLE: &str = "Lilly to present new data across Zepbound, Foundayo, EloraTZP and retatrutide at ObesityWeek 2026, moving obesity care beyond the scale";
const PRN_NO_RELEASE_BODY_PAGE: &str = "synthetic_no_release_body.html";
const PRN_LINE_BREAK_PAGE: &str = "synthetic_heading_line_break.html";
const PRN_LISTS_PAGE: &str = "synthetic_lists.html";
const PRN_ADDRESSES_PAGE: &str = "synthetic_addresses.html";
const PRN_JOINED_ADDRESS_PAGE: &str = "synthetic_joined_address.html";
const PRN_TABLES_PAGE: &str = "synthetic_tables.html";
/// Release 302939742 (2026-11-16 09:42Z fetch) cut to its lead, two of its four gallery figures
/// and one later paragraph; the two figures naming a person were left out.
const PRN_FIGURE_PAGE: &str = "article_302939742.html";
const GNW_ONE_PATH_PAGE: &str = "synthetic_lists_tables_and_addresses.html";
/// Releases 3403177 and 3403225 (2026-11-16 09:42Z and 09:44Z fetches) cut to a contact block,
/// names, phone digits and addresses replaced: the address in link text, then in plain text.
const GNW_LINKED_ADDRESS_PAGE: &str = "article_3403177.html";
const GNW_PLAIN_ADDRESS_PAGE: &str = "article_3403225.html";
/// Section 11.1's P5: every page below, its committed body digest, and the extraction version
/// that produced them. Rewritten by hand when an extraction change is meant.
const GOLDEN_BODIES: &str = "golden_bodies.json";
const GOLDEN_GNW_PAGES: [&str; 7] = [
    REAL_ARTICLE,
    "synthetic_canonical_differs.html",
    "synthetic_relative_canonical.html",
    PROSE_PAGE,
    GNW_ONE_PATH_PAGE,
    GNW_LINKED_ADDRESS_PAGE,
    GNW_PLAIN_ADDRESS_PAGE,
];
const GOLDEN_PRN_PAGES: [&str; 7] = [
    PRN_ARTICLE,
    PRN_SECOND_ARTICLE,
    PRN_LINE_BREAK_PAGE,
    PRN_LISTS_PAGE,
    PRN_ADDRESSES_PAGE,
    PRN_TABLES_PAGE,
    PRN_FIGURE_PAGE,
];
const PRN_OFF_PREFIX_URL: &str =
    "https://www.prnewswire.com/apac/news-releases/a-release-off-the-prefix-302999998.html";
const PRN_ARTICLE_OPENS: &str =
    "PLAINSBORO, N.J. and BAGSVÆRD, Denmark, Nov. 14, 2026 /PRNewswire/ -- Today, Novo Nordisk";
/// The first of the 3 `<li>` that open `section.release-body` before its first `<p>`.
const PRN_SUMMARY_OPENS: &str = "Statistically significant reductions in food noise scores";
const PRN_SUMMARY_ITEMS: usize = 3;
/// Text the trimmed page holds outside `section.release-body`: the dateline `<p>`, the share
/// `<p>`, the ld+json `<script>`, and a `<p>` of `section.nav-main`, one of the 4 other
/// `<section>` elements of the untrimmed page. `<article>` holds the first two.
const PRN_OUTSIDE_THE_SECTION: [&str; 4] = [
    "12:15 ET",
    "Share this article",
    "NewsArticle",
    "Searching for your content",
];
const CNW_INDEX: &str = "https://www.newswire.ca/sitemap-news.xml?page=1";
const CNW_HOST: &str = "www.newswire.ca";
/// Urls 1-3 of the 100 in the 2026-11-16 08:04:39Z news sitemap (§11.6 weekday probe, p0800).
const CNW_REAL_INDEX: &str = "sitemap_news_3_urls.xml";
const CNW_LINKS: [&str; 3] = [
    CNW_ARTICLE_URL,
    CNW_SECOND_ARTICLE_URL,
    "https://www.newswire.ca/news-releases/hkex-launches-offshore-rmb-bond-index-807505308.html",
];
const CNW_PUBLICATION_DATES: [&str; 3] = [
    "2026-11-16T02:22:00-05:00",
    "2026-11-16T01:47:00-05:00",
    "2026-11-16T01:33:00-05:00",
];
const CNW_MINUTES: [&str; 3] = [
    "2026-11-16T07:22:00Z",
    "2026-11-16T06:47:00Z",
    "2026-11-16T06:33:00Z",
];
/// Daylight time's offset, UTC's, and an evening whose UTC day is the next (13 of 246 keys on
/// 2026-11-16): each is the instant its own offset names.
const CNW_OTHER_OFFSETS: [(&str, &str); 3] = [
    ("2026-07-16T03:22:00-04:00", "2026-07-16T07:22:00Z"),
    ("2026-11-16T07:22:00+00:00", "2026-11-16T07:22:00Z"),
    ("2026-11-16T20:30:00-05:00", "2026-11-17T01:30:00Z"),
];
/// A second inside the minute, `Z` for the offset, no seconds field, a two-digit year.
const CNW_NOT_A_MINUTE: [&str; 4] = [
    "2026-11-16T02:22:30-05:00",
    "2026-11-16T07:22:00Z",
    "2026-11-16T02:22-05:00",
    "26-11-16T02:22:00-05:00",
];
/// Release 805423040 (p0800 fetch) cut to its lead, two of its three gallery figures, four later
/// paragraphs and its contact line, whose name, phone digits and encoded address are replaced.
const CNW_ARTICLE: &str = "article_805423040.html";
const CNW_ARTICLE_URL: &str = "https://www.newswire.ca/news-releases/global-voltage-travel-ready-protection-elegrp-powers-dyson-supersonic-travel-hair-dryer-805423040.html";
const CNW_ARTICLE_KEY: &str = "www.newswire.ca/news-releases/global-voltage-travel-ready-protection-elegrp-powers-dyson-supersonic-travel-hair-dryer-805423040.html";
const CNW_ARTICLE_TITLE: &str =
    "Global Voltage, Travel-Ready Protection: ELEGRP Powers Dyson Supersonic Travel hair dryer";
/// Release 823884942 (p0800 fetch) cut to the paragraphs naming no driver; its encoded address
/// is replaced.
const CNW_SECOND_ARTICLE: &str = "article_823884942.html";
const CNW_SECOND_ARTICLE_URL: &str = "https://www.newswire.ca/news-releases/segway-powersports-secures-class-2-one-two-finish-on-score-baja-1000-debut-823884942.html";
const CNW_SECOND_ARTICLE_TITLE: &str =
    "Segway Powersports Secures Class 2 One-Two Finish on SCORE Baja 1000 Debut";
const CNW_NO_RELEASE_BODY_PAGE: &str = "synthetic_no_release_body.html";
/// The shape of the page's own organisation link, `/news/segway-powersports/`.
const CNW_OFF_PREFIX_URL: &str = "https://www.newswire.ca/news/an-organisation/";
/// The trimmed 805423040 body, one line per `p`, `li` or `figcaption`.
const CNW_ARTICLE_LINES: [&str; 4] = [
    "LAS VEGAS, Nov. 16, 2026 /CNW/ -- With carry-on space at a premium",
    "ELEGRP Global Voltage Travel-Ready Protection",
    "ELEGRP 100V–240V Wide-Voltage Travel Safety",
    "The Dyson Supersonic Travel hair dryer is 32% smaller",
];
/// What the trimmed 805423040 holds outside `section.release-body`, as PRN's page does.
const CNW_OUTSIDE_THE_SECTION: [&str; 4] = [
    "02:22 ET",
    "Share this article",
    "NewsArticle",
    "Searching for your content",
];
const GOLDEN_CNW_PAGES: [&str; 2] = [CNW_ARTICLE, CNW_SECOND_ARTICLE];
const FED_INDEX: &str = "https://www.federalreserve.gov/feeds/press_all.xml";
const FED_HOST: &str = "www.federalreserve.gov";
/// Items 1, 3 and 5 of the 20 in the 2026-11-16 08:04:39Z feed (§11.6 weekday probe, p0800),
/// byte for byte: a byte order mark first, CRLF line ends, `link` and `pubDate` in CDATA.
const FED_REAL_INDEX: &str = "press_all_3_items.xml";
const FED_LINKS: [&str; 3] = [
    FED_ARTICLE_URL,
    "https://www.federalreserve.gov/newsevents/pressreleases/monetary20261112a.htm",
    FED_SECOND_ARTICLE_URL,
];
/// A day before the 10th is printed with no leading zero (9 of 20 items, 2026-11-16).
const FED_PUB_DATES: [&str; 3] = [
    "Fri, 13 Nov 2026 21:15:00 GMT",
    "Thu, 12 Nov 2026 17:45:00 GMT",
    "Mon, 9 Nov 2026 21:00:00 GMT",
];
const FED_MINUTES: [&str; 3] = [
    "2026-11-13T21:15:00Z",
    "2026-11-12T17:45:00Z",
    "2026-11-09T21:00:00Z",
];
/// A zero-padded day, a second inside the minute, no seconds field, another zone spelling, a
/// two-digit year: none is a Fed minute.
const FED_NOT_A_MINUTE: [&str; 5] = [
    "Mon, 09 Nov 2026 21:00:00 GMT",
    "Fri, 13 Nov 2026 21:15:30 GMT",
    "Fri, 13 Nov 2026 21:15 GMT",
    "Fri, 13 Nov 2026 21:15:00 +0000",
    "Fri, 13 Nov 26 21:15:00 GMT",
];
/// Release bcreg20261113a (p0800 fetch) cut to its head, one `nav` paragraph, the breadcrumb and
/// `div#article`; the four media contacts' names and phone numbers are replaced.
const FED_ARTICLE: &str = "article_bcreg20261113a.html";
const FED_ARTICLE_URL: &str =
    "https://www.federalreserve.gov/newsevents/pressreleases/bcreg20261113a.htm";
const FED_ARTICLE_KEY: &str = "www.federalreserve.gov/newsevents/pressreleases/bcreg20261113a.htm";
const FED_ARTICLE_TITLE: &str =
    "Agencies extend comment period for proposed third-party risk management guidance";
/// Release orders20261109a (p0800 fetch) cut the same way; it names no person.
const FED_SECOND_ARTICLE: &str = "article_orders20261109a.html";
const FED_SECOND_ARTICLE_URL: &str =
    "https://www.federalreserve.gov/newsevents/pressreleases/orders20261109a.htm";
const FED_SECOND_ARTICLE_TITLE: &str = "Federal Reserve Board announces approval of two applications by Mutual Federal Bancorp, MHC, and MFB Bancorp, Inc.";
const FED_NO_ARTICLE_BLOCK_PAGE: &str = "synthetic_no_article_block.html";
const FED_OFF_PREFIX_URL: &str =
    "https://www.federalreserve.gov/newsevents/speech/a-speech-20261113a.htm";
/// The press-release listing page: it shares the prefix's letters but not its trailing `/`.
const FED_PREFIX_WITHOUT_SLASH_URL: &str =
    "https://www.federalreserve.gov/newsevents/pressreleases.htm";
/// A canonical link that is neither the requested url nor the page's `og:url`.
const FED_SYNTHETIC_CANONICAL: &str =
    "https://www.federalreserve.gov/newsevents/pressreleases/bcreg20261113b.htm";
/// The trimmed bcreg20261113a body: four release paragraphs, then the two attachment links.
const FED_ARTICLE_LINES: [&str; 6] = [
    "The Federal Deposit Insurance Corporation, the Federal Reserve Board, the National Credit",
    "The proposed guidance reflects the agencies' supervisory experience",
    "When finalized, the federal bank regulatory agencies plan to rescind",
    "Also today, the Federal Reserve Board separately announced",
    "Federal Register notice: Proposed Third-Party Risk Management Guidance; Extension of Comment Period (PDF)",
    "Federal Register notice: Proposed Third-Party Risk Management Guide for Traditional Community Banking Organizations (PDF)",
];
/// What the trimmed bcreg20261113a holds outside the release column: the heading block (date,
/// issuing agencies, release time, share), the Related Information panel, the nav, the
/// breadcrumb, the page title and the media contacts' cells.
const FED_OUTSIDE_THE_COLUMN: [&str; 10] = [
    "November 13, 2026",
    "Board of Governors of the Federal Reserve System",
    "For release at",
    "Share",
    "Agencies seek comment on proposed third-party risk management guidance",
    "Federal Open Market Committee",
    "Press Releases",
    "Joint Press Release",
    "Media Contacts",
    "Jane Example",
];
/// The trimmed orders20261109a body, whole; its address is a Cloudflare placeholder (P1).
const FED_SECOND_ARTICLE_BODY: [&str; 4] = [
    "The Federal Reserve Board on Monday announced its approval of two applications by Mutual Federal Bancorp, MHC, and MFB Bancorp, Inc., both of Chicago, Illinois.",
    "These approvals allow the banking organization to have a new top-tier holding company, MFB Bancorp, Inc., which would become a savings and loan holding company by acquiring Mutual Federal Bank, of Chicago, Illinois.",
    "For media inquiries, please email or call (202) 452-2955.",
    "Order (PDF)",
];
const GOLDEN_FED_PAGES: [&str; 2] = [FED_ARTICLE, FED_SECOND_ARTICLE];
const YAHOO_INDEX: &str = "https://finance.yahoo.com/news-sitemap.xml";
const YAHOO_HOST: &str = "finance.yahoo.com";
/// 8 of the 1,000 `url` rows of the 2026-11-16 09:30:00Z sitemap (§11.6 weekday probe, p0930),
/// byte for byte: keys `motleyfool.com`, none, `business-wire.com`, `newsfile_64`,
/// `globenewswire.com`, `prnewswire.com`, `accesswire.ca` and `prnewswire.com`, in that order.
const YAHOO_REAL_INDEX: &str = "news_sitemap_8_urls.xml";
const YAHOO_ROWS: u32 = 8;
/// The `business-wire.com` row, then the `accesswire.ca` row: the two the key filter follows.
const YAHOO_LINKS: [&str; 2] = [
    YAHOO_ARTICLE_URL,
    "https://finance.yahoo.com/technology/ai/articles/reo-dev-launches-100-ai-084000068.html",
];
const YAHOO_PUBLICATION_DATES: [&str; 2] = ["2026-11-16T09:17:00Z", "2026-11-16T08:40:00Z"];
const YAHOO_MINUTES: [&str; 2] = ["2026-11-16T09:17:00Z", "2026-11-16T08:40:00Z"];
/// An outlet row's second (`motleyfool.com`, 09:22:02Z), an offset, no seconds field, a space
/// for the `T`, a two-digit year: none is a Yahoo minute.
const YAHOO_NOT_A_MINUTE: [&str; 5] = [
    "2026-11-16T09:22:02Z",
    "2026-11-16T09:17:00+00:00",
    "2026-11-16T09:17Z",
    "2026-11-16 09:17:00Z",
    "26-11-16T09:17:00Z",
];
/// The `business-wire.com` reprint (probe7 fetch 2026-11-16 10:53Z) cut to its head, the paid
/// release disclaimer, the `h1` and `div[data-testid="article-body"]` whole; the contact's name
/// and address replaced.
const YAHOO_ARTICLE: &str = "article_laverock_091700844.html";
const YAHOO_ARTICLE_URL: &str = "https://finance.yahoo.com/healthcare/articles/laverock-therapeutics-appoints-parker-moss-091700844.html";
const YAHOO_ARTICLE_KEY: &str = "finance.yahoo.com/healthcare/articles/laverock-therapeutics-appoints-parker-moss-091700844.html";
const YAHOO_ARTICLE_TITLE: &str = "Laverock Therapeutics Appoints Parker Moss as Chief Executive Officer and Paul J. Sekhri as Executive Chairman";
/// A `prnewswire.com` reprint (probe7 fetch 2026-11-16 10:52Z), cut the same way: its logo
/// caption, `Story Continues` button and Cision trailer. That key is cross-checked, not followed.
const YAHOO_PRN_REPRINT: &str = "article_horizon_090000908.html";
const YAHOO_NO_ARTICLE_BODY_PAGE: &str = "synthetic_no_article_body.html";
/// The Laverock body opens each line so, in order; line 9 is the first after `Story Continues`.
const YAHOO_ARTICLE_LINES: [&str; 15] = [
    "Appointment of experienced transatlantic leadership team",
    "Interim Chair Lisa Deschamps to transition",
    "David Venables will step down as Chief Executive Officer on 31 December following a period of transition",
    "LONDON, November 16, 2026--(BUSINESS WIRE)--Laverock Therapeutics",
    "Parker Moss is a life sciences and technology leader",
    "Paul J. Sekhri brings nearly four decades",
    "Laverock's platform technology enables",
    "Paul J. Sekhri, Executive Chairman of Laverock Therapeutics, said:",
    "Parker Moss, Chief Executive Officer of Laverock Therapeutics, added:",
    "Lisa Deschamps, Non-Executive Director",
    "David Venables will step down as Chief Executive Officer on 31 December following a period of transition. Lisa",
    "For more information, please visit www.laverocktx.com",
    "View source version on businesswire.com: https://www.businesswire.com/news/home/20261116863333/en/",
    "Contacts",
    "Zyme Communications Dr Jane Example",
];
/// The button, the disclaimer, the page chrome around the body, the `h1`, the body's JSON
/// `script`, and the contact address decision r removes.
const YAHOO_OUTSIDE_THE_BODY: [&str; 7] = [
    "Story Continues",
    "This is a paid press release",
    "Yahoo Finance navigation",
    "Related story outside the body",
    YAHOO_ARTICLE_TITLE,
    "contentId",
    "jane.example@example.com",
];
const GOLDEN_YAHOO_PAGES: [&str; 2] = [YAHOO_ARTICLE, YAHOO_PRN_REPRINT];
/// What the real sitemap's `image:loc` reads before the key, and one image per key or shape.
const YAHOO_IMAGE: &str = "https://s.yimg.com/ny/mysterio/api/0/ynews/resizefill_w1200;quality_80;format_webp/https://media.zenfs.com/en/";
/// The CNW fixture's first title, which the real Yahoo row 7 carries under `prnewswire.com`.
const YAHOO_ROW_LISTED_BY_CNW: &str = "https://finance.yahoo.com/technology/articles/global-voltage-travel-ready-protection-072200770.html";
const YAHOO_GNW_ROW: &str =
    "https://finance.yahoo.com/energy/articles/geely-holding-announces-evsa-global-090500744.html";
const YAHOO_PRN_ROW: &str =
    "https://finance.yahoo.com/economy/articles/kyiv-based-horizon-capital-reaches-090000908.html";
const FIRST_ATTEMPT: u32 = 0;
const OK: u16 = 200;
const REQUEST_TIMEOUT: u16 = 408;
/// RFC 9110's 3xx codes but 305 (deprecated) and 306 (unused); a `Refuse` profile hands each to
/// `classify` as an answer.
const REDIRECTS: [u16; 7] = [300, 301, 302, 303, 304, 307, 308];

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixtures() -> PathBuf {
    crate_dir().join("tests/fixtures")
}

fn gnw(name: &str) -> Vec<u8> {
    std::fs::read(fixtures().join("gnw").join(name)).unwrap_or_default()
}

fn prn(name: &str) -> Vec<u8> {
    std::fs::read(fixtures().join("prn").join(name)).unwrap_or_default()
}

fn cnw(name: &str) -> Vec<u8> {
    std::fs::read(fixtures().join("cnw").join(name)).unwrap_or_default()
}

fn fed(name: &str) -> Vec<u8> {
    std::fs::read(fixtures().join("fed").join(name)).unwrap_or_default()
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

/// The committed feed, every admitted host's robots handed as `Absent`, so none disallows a path.
fn provider() -> Result<OpenWebFeed, Refusal> {
    Ok(handed(
        OpenWebFeed::open(&hosts_path(), &rules_path(), CONTACT)?,
        |_| HostRobots::Absent,
    ))
}

fn handed(feed: OpenWebFeed, robots: impl Fn(&str) -> HostRobots) -> OpenWebFeed {
    let handed: BTreeMap<String, HostRobots> = feed
        .host_pins()
        .into_iter()
        .filter(|row| row.admitted)
        .map(|row| {
            let one = robots(&row.host);
            (row.host, one)
        })
        .collect();
    feed.with_robots(handed)
}

/// Disallows every path under `prefix` and records each path it was asked about.
#[derive(Debug)]
struct Disallowing {
    prefix: String,
    asked: Mutex<Vec<String>>,
}

impl RobotsRules for Disallowing {
    fn allows(&self, path: &str) -> bool {
        if let Ok(mut asked) = self.asked.lock() {
            asked.push(path.to_owned());
        }
        !path.starts_with(&self.prefix)
    }
}

/// The committed feed, GNW's robots disallowing `prefix`; the rules, to read what they were asked.
fn gnw_disallowing(prefix: &str) -> Result<(OpenWebFeed, Arc<Disallowing>), Refusal> {
    let rules = Arc::new(Disallowing {
        prefix: prefix.to_owned(),
        asked: Mutex::new(Vec::new()),
    });
    let shared: Arc<dyn RobotsRules> = rules.clone();
    let feed = handed(
        OpenWebFeed::open(&hosts_path(), &rules_path(), CONTACT)?,
        |host| {
            if host == GNW_HOST {
                HostRobots::Rules(shared.clone())
            } else {
                HostRobots::Absent
            }
        },
    );
    Ok((feed, rules))
}

fn asked(rules: &Disallowing) -> Vec<String> {
    rules
        .asked
        .lock()
        .map(|asked| asked.clone())
        .unwrap_or_default()
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

fn prn_index_request(provider: &OpenWebFeed) -> Result<Request, Box<dyn std::error::Error>> {
    let plan = provider.plan(&TargetWindow::new(RUN_DAY), None, None, at(RUN_AT))?;
    plan.requests
        .into_iter()
        .find(|request| request.url == PRN_INDEX)
        .ok_or_else(|| "the run day planned no PR Newswire index".into())
}

/// The follow-up the real PRN index names at `position`, with its real `pubDate`.
fn prn_follow_up(
    provider: &OpenWebFeed,
    position: usize,
) -> Result<Request, Box<dyn std::error::Error>> {
    let index = provider.extract(&prn_index_request(provider)?, &blob(prn(PRN_REAL_INDEX)));
    index
        .follow_ups
        .get(position)
        .cloned()
        .ok_or_else(|| format!("the real PRN index yielded no follow-up {position}").into())
}

/// Release 302939553, the second item of the real PRN index.
fn prn_article_request(provider: &OpenWebFeed) -> Result<Request, Box<dyn std::error::Error>> {
    prn_follow_up(provider, 1)
}

fn cnw_index_request(provider: &OpenWebFeed) -> Result<Request, Box<dyn std::error::Error>> {
    let plan = provider.plan(&TargetWindow::new(RUN_DAY), None, None, at(RUN_AT))?;
    plan.requests
        .into_iter()
        .find(|request| request.url == CNW_INDEX)
        .ok_or_else(|| "the run day planned no Canada Newswire index".into())
}

/// The follow-up the real CNW sitemap names at `position`, with its real publication date.
fn cnw_follow_up(
    provider: &OpenWebFeed,
    position: usize,
) -> Result<Request, Box<dyn std::error::Error>> {
    let index = provider.extract(&cnw_index_request(provider)?, &blob(cnw(CNW_REAL_INDEX)));
    index
        .follow_ups
        .get(position)
        .cloned()
        .ok_or_else(|| format!("the real CNW sitemap yielded no follow-up {position}").into())
}

/// One synthetic news sitemap holding each `(link, publication date)` as one `url`.
fn cnw_sitemap(rows: &[(&str, &str)]) -> Vec<u8> {
    let urls: String = rows
        .iter()
        .map(|(link, date)| {
            format!(
                "<url><loc>{link}</loc><news:news><news:publication_date>{date}\
                 </news:publication_date></news:news></url>"
            )
        })
        .collect();
    format!(
        "<?xml version=\"1.0\"?><urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\" \
         xmlns:news=\"http://www.google.com/schemas/sitemap-news/0.9\">{urls}</urlset>"
    )
    .into_bytes()
}

fn fed_index_request(provider: &OpenWebFeed) -> Result<Request, Box<dyn std::error::Error>> {
    let plan = provider.plan(&TargetWindow::new(RUN_DAY), None, None, at(RUN_AT))?;
    plan.requests
        .into_iter()
        .find(|request| request.url == FED_INDEX)
        .ok_or_else(|| "the run day planned no Federal Reserve index".into())
}

/// The follow-up the real Fed feed names at `position`, with its real `pubDate`.
fn fed_follow_up(
    provider: &OpenWebFeed,
    position: usize,
) -> Result<Request, Box<dyn std::error::Error>> {
    let index = provider.extract(&fed_index_request(provider)?, &blob(fed(FED_REAL_INDEX)));
    index
        .follow_ups
        .get(position)
        .cloned()
        .ok_or_else(|| format!("the real Fed feed yielded no follow-up {position}").into())
}

/// One synthetic RSS feed holding each `(link, pubDate)` as one `item`.
fn fed_rss(rows: &[(&str, &str)]) -> Vec<u8> {
    let items: String = rows
        .iter()
        .map(|(link, date)| format!("<item><link>{link}</link><pubDate>{date}</pubDate></item>"))
        .collect();
    format!("<?xml version=\"1.0\"?><rss version=\"2.0\"><channel>{items}</channel></rss>")
        .into_bytes()
}

fn yahoo(name: &str) -> Vec<u8> {
    std::fs::read(fixtures().join("yahoo").join(name)).unwrap_or_default()
}

fn yahoo_index_request(provider: &OpenWebFeed) -> Result<Request, Box<dyn std::error::Error>> {
    let plan = provider.plan(&TargetWindow::new(RUN_DAY), None, None, at(RUN_AT))?;
    plan.requests
        .into_iter()
        .find(|request| request.url == YAHOO_INDEX)
        .ok_or_else(|| "the run day planned no Yahoo index".into())
}

/// The follow-up the real Yahoo sitemap names at `position`, with its real publication date.
fn yahoo_follow_up(
    provider: &OpenWebFeed,
    position: usize,
) -> Result<Request, Box<dyn std::error::Error>> {
    let index = provider.extract(
        &yahoo_index_request(provider)?,
        &blob(yahoo(YAHOO_REAL_INDEX)),
    );
    index
        .follow_ups
        .get(position)
        .cloned()
        .ok_or_else(|| format!("the real Yahoo sitemap yielded no follow-up {position}").into())
}

/// One synthetic news sitemap holding each `(link, publication date, image:loc)` as one `url`.
fn yahoo_sitemap(rows: &[(&str, &str, Option<&str>)]) -> Vec<u8> {
    let urls: String = rows
        .iter()
        .map(|(link, date, image)| {
            let image = image.map_or_else(String::new, |loc| {
                format!("<image:image><image:loc>{loc}</image:loc></image:image>")
            });
            format!(
                "<url><loc>{link}</loc><news:news><news:publication_date>{date}\
                 </news:publication_date><news:title>{link}</news:title></news:news>{image}</url>"
            )
        })
        .collect();
    format!(
        "<?xml version=\"1.0\"?><urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\" \
         xmlns:news=\"http://www.google.com/schemas/sitemap-news/0.9\" \
         xmlns:image=\"http://www.google.com/schemas/sitemap-image/1.1\">{urls}</urlset>"
    )
    .into_bytes()
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
        None,
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
    assert_eq!(plan.requests.len(), 5);
    assert_eq!(plan.not_planned, Vec::new());
    Ok(())
}

/// Section 11.2's host order, one index each. Mutations 2026-11-16, the PRN row `candidate` and
/// the GNW and PRN rows swapped: each red; 2026-11-17, the CNW, Fed and Yahoo rows `candidate`
/// and CNW's index the RSS feed: each red.
#[test]
fn the_committed_hosts_file_plans_gnw_then_prn_then_cnw_then_the_fed_then_yahoo() -> Outcome {
    let plan = provider()?.plan(&TargetWindow::new(RUN_DAY), None, None, at(RUN_AT))?;
    let urls: Vec<&str> = plan
        .requests
        .iter()
        .map(|request| request.url.as_str())
        .collect();
    assert_eq!(
        urls,
        vec![GNW_INDEX, PRN_INDEX, CNW_INDEX, FED_INDEX, YAHOO_INDEX]
    );
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

/// Owner decision of 2026-11-15 (§10): a 408 is retried in-run and never held.
#[test]
fn a_408_request_timeout_is_transient() -> Outcome {
    let provider = provider()?;
    for request in [index_request(&provider)?, article_request(&provider)?] {
        assert_eq!(
            verdict(&provider, &request, REQUEST_TIMEOUT, b"")?,
            Verdict::TransientFailure
        );
    }
    Ok(())
}

/// `Wire` hands every 3xx to `classify` as the answer (§11.2, §8 row R19); none is retried.
#[test]
fn a_redirect_is_permanent_never_transient() -> Outcome {
    let provider = provider()?;
    for request in [index_request(&provider)?, article_request(&provider)?] {
        for status in REDIRECTS {
            assert_eq!(
                verdict(&provider, &request, status, b"")?,
                Verdict::PermanentFailure,
                "{status}"
            );
        }
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
        None,
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
        None,
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
    let with_a_candidate = handed(
        OpenWebFeed::open(
            &hosts_fixture("one_admitted_one_candidate.json"),
            &rules_path(),
            CONTACT,
        )?,
        |_| HostRobots::Absent,
    );
    for (provider, url) in [
        (provider()?, "https://www.businesswire.com/news/home/a.html"),
        (
            with_a_candidate,
            "https://www.prnewswire.com/news-releases/a.html",
        ),
    ] {
        let mut request = article_request(&provider)?;
        request.url = url.to_owned();
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

/// Owner decision 2026-11-17 (section 11.5 step 7): a row whose url the host's robots.txt
/// disallows is not followed and is counted apart from `filtered_out`; its siblings are followed.
#[test]
fn a_follow_up_whose_url_the_robots_disallows_is_not_planned_and_counted_apart() -> Outcome {
    let (feed, rules) = gnw_disallowing("/news-release/2026/11/14/3403075/")?;
    let extracted = follow_ups(&feed, &gnw(REAL_INDEX))?;
    let kept: Vec<&str> = extracted
        .follow_ups
        .iter()
        .map(|request| request.url.as_str())
        .collect();
    assert_eq!(
        (
            extracted.vendor_rows,
            extracted.filtered_out,
            extracted.robots_disallowed,
            kept.len(),
            kept.contains(&ARTICLE_URL),
            asked(&rules).len(),
        ),
        (REAL_ROWS, 0, 1, 2, false, 3)
    );
    Ok(())
}

/// Decision 4: robots.txt is asked about the fetched url's path as written: percent escapes
/// untouched in either case, and no query, because the fetched url carries none.
#[test]
fn the_path_handed_to_robots_is_the_fetched_urls_path_as_written() -> Outcome {
    let item = |link: &str| {
        format!("<item><link>{link}</link><pubDate>Sat, 14 Nov 2026 19:00 GMT</pubDate></item>")
    };
    let index = format!(
        "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel>{}{}{}</channel></rss>",
        item("https://rss.globenewswire.com/news-release/1/caf%C3%A9.html"),
        item("https://rss.globenewswire.com/news-release/2/caf%c3%a9.html"),
        item("https://rss.globenewswire.com/news-release/3/x.html?pdf=1"),
    );
    let (feed, rules) = gnw_disallowing("/news-release/1/")?;
    let extracted = follow_ups(&feed, index.as_bytes())?;
    let kept: Vec<&str> = extracted
        .follow_ups
        .iter()
        .map(|request| request.url.as_str())
        .collect();
    assert_eq!(
        (asked(&rules), kept, extracted.robots_disallowed),
        (
            vec![
                "/news-release/1/caf%C3%A9.html".to_owned(),
                "/news-release/2/caf%c3%a9.html".to_owned(),
                "/news-release/3/x.html".to_owned(),
            ],
            vec![
                "https://rss.globenewswire.com/news-release/2/caf%c3%a9.html",
                "https://rss.globenewswire.com/news-release/3/x.html",
            ],
            1
        )
    );
    Ok(())
}

/// robots.cc reads a url only %-encoded (robots.h:153) and matches it as written, so a raw
/// non-ASCII, space or control byte would be checked unescaped; `%2e` segments would walk.
#[test]
fn a_document_path_robots_cannot_read_as_written_is_filtered_out_and_never_asked() -> Outcome {
    let item = |link: &str| {
        format!("<item><link>{link}</link><pubDate>Sat, 14 Nov 2026 19:00 GMT</pubDate></item>")
    };
    let index = format!(
        "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel>{}{}{}{}{}{}</channel></rss>",
        item("https://rss.globenewswire.com/news-release/1/caf\u{e9}.html"),
        item("https://rss.globenewswire.com/news-release/2/a b.html"),
        item("https://rss.globenewswire.com/news-release/3/%2e%2e/x.html"),
        item("https://rss.globenewswire.com/news-release/4/.%2E/x.html"),
        item("https://rss.globenewswire.com/news-release/5/%2E/x.html"),
        item("https://rss.globenewswire.com/news-release/6/%2e%2e.html"),
    );
    let (feed, rules) = gnw_disallowing("/never/")?;
    let extracted = follow_ups(&feed, index.as_bytes())?;
    let kept: Vec<&str> = extracted
        .follow_ups
        .iter()
        .map(|request| request.url.as_str())
        .collect();
    assert_eq!(
        (
            asked(&rules),
            kept,
            extracted.filtered_out,
            extracted.robots_disallowed
        ),
        (
            vec!["/news-release/6/%2e%2e.html".to_owned()],
            vec!["https://rss.globenewswire.com/news-release/6/%2e%2e.html"],
            5,
            0
        )
    );
    Ok(())
}

/// Fail closed: a host whose robots were never handed over follows nothing, and every row it
/// would have followed is counted as disallowed rather than dropped.
#[test]
fn a_host_whose_robots_were_never_handed_over_follows_nothing() -> Outcome {
    let feed = OpenWebFeed::open(&hosts_path(), &rules_path(), CONTACT)?;
    let extracted = follow_ups(&feed, &gnw(REAL_INDEX))?;
    assert_eq!(
        (
            extracted.follow_ups.len(),
            extracted.filtered_out,
            extracted.robots_disallowed
        ),
        (0, 0, REAL_ROWS)
    );
    Ok(())
}

/// The seam's counts add up with a disallowed row: it is one of the five, not a lost row.
#[test]
fn an_index_with_a_disallowed_row_breaks_no_claim_of_the_seam() -> Outcome {
    let (feed, _) = gnw_disallowing("/news-release/2026/11/14/3403075/")?;
    let payloads = [(index_request(&feed)?, blob(gnw(REAL_INDEX)))];
    let subject = Subject {
        target: TargetWindow::new(RUN_DAY),
        calendar: None,
        listing: None,
        at: at(RUN_AT),
        payloads: &payloads,
    };
    assert_eq!(breaches(&feed, &subject), Vec::new());
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

/// Section 11.1: a follow-up with no `story_of` is issued even beside a held one, built by hand
/// because `index_rows_of` emits none. Mutation 2026-11-15, `is_none_or` made `is_some_and`: red.
#[test]
fn a_follow_up_with_no_story_is_issued_and_a_held_one_dropped() -> Outcome {
    let provider = provider()?;
    let held_one = article_request(&provider)?;
    let storyless = Request {
        url: held_one
            .url
            .replacen(GNW_HOST, &format!("{GNW_HOST}:443"), 1),
        ..held_one.clone()
    };
    let story = provider
        .story_of(&held_one)
        .ok_or("the article has no story")?;
    let held = BTreeSet::from([story.hex().to_owned()]);
    let issued = provider.follow_ups_to_issue(vec![held_one, storyless.clone()], &held);
    assert_eq!(
        (provider.story_of(&storyless), issued),
        (None, vec![storyless])
    );
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

/// Section 11.1, owner decision of 2026-11-15: the listed time came from the index row, so a later
/// listing can correct it; the four page reasons cannot be. A bad listing outranks a bad page.
#[test]
fn an_unparseable_listed_time_is_decided_by_the_index_row_and_a_page_reason_by_the_page() -> Outcome
{
    let provider = provider()?;
    let request = article_request(&provider)?;
    let unlisted = Request {
        listed_date: Some(NOT_A_MINUTE_PUB_DATES[0].to_owned()),
        ..request.clone()
    };
    let mut too_deep = "<html><body>".to_owned();
    too_deep.push_str(&"<div>".repeat(DEEPEST_PAGE));
    let mut not_utf8 = gnw(REAL_ARTICLE);
    not_utf8.extend_from_slice(&[0xFF, 0xFE]);
    let decided = |request: &Request, page: Vec<u8>| -> Result<_, Box<dyn std::error::Error>> {
        let row = only_unusable(&provider.extract(request, &blob(page)))?;
        Ok((row.reason, row.decided_by))
    };
    assert_eq!(
        [
            decided(&unlisted, gnw(REAL_ARTICLE))?,
            decided(&request, gnw(NO_CONTAINER_PAGE))?,
            decided(&request, not_utf8)?,
            decided(&request, too_deep.into_bytes())?,
            decided(&request, gnw(TWO_CONTAINERS_PAGE))?,
            decided(&unlisted, gnw(NO_CONTAINER_PAGE))?,
        ],
        [
            (UnusableReason::NoParseableValidTime, DecidedBy::IndexRow),
            (UnusableReason::NoArticleBody, DecidedBy::Page),
            (UnusableReason::PageNotUtf8, DecidedBy::Page),
            (UnusableReason::PageTooDeep, DecidedBy::Page),
            (UnusableReason::ArticleContainerAmbiguous, DecidedBy::Page),
            (UnusableReason::NoParseableValidTime, DecidedBy::IndexRow),
        ]
    );
    Ok(())
}

/// Section 11.1: a live follow-up whose story is held is dropped. Mutation 2026-11-15, the filter
/// removed: red.
#[test]
fn a_live_follow_up_whose_story_is_held_is_dropped_and_an_unheld_one_kept() -> Outcome {
    let provider = provider()?;
    let all = follow_ups(&provider, &gnw(REAL_INDEX))?.follow_ups;
    let Some((first, rest)) = all.split_first() else {
        return Err("the real index names no follow-up".into());
    };
    let held: BTreeSet<String> = provider
        .story_of(first)
        .map(|story| story.hex().to_owned())
        .into_iter()
        .collect();
    assert_eq!(held.len(), 1);
    assert_eq!(provider.follow_ups_to_issue(all.clone(), &held), rest);
    Ok(())
}

// ---- PR Newswire ----

/// No rewrite: each link stays on `www.prnewswire.com` and passes `/news-releases/` (plural).
/// Mutation 2026-11-16, the prefix GNW's `/news-release/`: red, all 3 filtered out.
#[test]
fn the_real_pr_newswire_index_yields_one_follow_up_per_release_with_its_raw_pub_date() -> Outcome {
    let provider = provider()?;
    let extracted = provider.extract(&prn_index_request(&provider)?, &blob(prn(PRN_REAL_INDEX)));
    assert_eq!(
        (extracted.vendor_rows, extracted.filtered_out),
        (REAL_ROWS, 0)
    );
    let kept: Vec<(&str, Option<&str>, RequestPhase)> = extracted
        .follow_ups
        .iter()
        .map(|request| {
            (
                request.url.as_str(),
                request.listed_date.as_deref(),
                request.phase,
            )
        })
        .collect();
    let expected: Vec<(&str, Option<&str>, RequestPhase)> = PRN_LINKS
        .iter()
        .zip(PRN_PUB_DATES)
        .map(|(link, pub_date)| (*link, Some(pub_date), RequestPhase::Document))
        .collect();
    assert_eq!(kept, expected);
    Ok(())
}

/// The 40 real links of 2026-11-14 and 2026-11-16 are all under `/news-releases/`, so only a
/// synthetic link tests the prefix from the wide side. Mutation 2026-11-16, the prefix `/`: red.
#[test]
fn a_pr_newswire_link_outside_news_releases_is_filtered_out_and_counted() -> Outcome {
    let provider = provider()?;
    let item = |link: &str| {
        format!(
            "<item><link>{link}</link><pubDate>Sat, 14 Nov 2026 17:15:00 +0000</pubDate></item>"
        )
    };
    let index = format!(
        "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel>{}{}</channel></rss>",
        item(PRN_ARTICLE_URL),
        item(PRN_OFF_PREFIX_URL),
    );
    let extracted = provider.extract(&prn_index_request(&provider)?, &blob(index.into_bytes()));
    let kept: Vec<&str> = extracted
        .follow_ups
        .iter()
        .map(|request| request.url.as_str())
        .collect();
    assert_eq!(
        (extracted.vendor_rows, kept, extracted.filtered_out),
        (2, vec![PRN_ARTICLE_URL], 1)
    );
    Ok(())
}

/// Section 11.1: every observed PRN item `pubDate` ends `:00 +0000` (40 of 40, feeds of
/// 2026-11-14 and 2026-11-16) while the channel `pubDate` carries real seconds, so the item value
/// is a minute. Mutations 2026-11-16: `%S` for `:00`, the loader's seconds check dropped and
/// `minute_of` truncating to the minute, red (17:15:30 read as 17:15); the round trip removed, red.
#[test]
fn a_pr_newswire_pubdate_is_a_minute_and_never_an_instant() -> Outcome {
    let provider = provider()?;
    let request = prn_article_request(&provider)?;
    let extract = |raw: &str| {
        let dated = Request {
            listed_date: Some(raw.to_owned()),
            ..request.clone()
        };
        provider.extract(&dated, &blob(prn(PRN_ARTICLE)))
    };
    for (raw, minute) in PRN_PUB_DATES.iter().zip(PRN_MINUTES) {
        let item = only_item(&extract(raw))?;
        assert_eq!(
            (&item.valid, precision_of(&item.valid), item.valid_source),
            (
                &Published::AtMinute(Minute::new(minute.parse()?)?),
                ValidPrecision::Minute,
                ValidSource::VendorPublished
            ),
            "{raw}"
        );
    }
    for raw in PRN_NOT_A_MINUTE_PUB_DATES {
        let row = only_unusable(&extract(raw))?;
        assert_eq!(
            (row.reason, row.decided_by, &row.item.valid),
            (
                UnusableReason::NoParseableValidTime,
                DecidedBy::IndexRow,
                &Published::Unknown
            ),
            "{raw}"
        );
    }
    Ok(())
}

/// §8 row R17 for PR Newswire: `p, li` text inside `section.release-body`, the 3 summary `<li>`
/// first (decision 8). `<article>` would add the dateline and the share line (2 of 42 `<p>` on the
/// untrimmed page, 2026-11-14 probe). Mutations 2026-11-16, the container `body`, `article` and
/// `section`: each red.
#[test]
fn the_extracted_body_is_article_text_and_never_page_html_on_a_pr_newswire_page() -> Outcome {
    let provider = provider()?;
    let item =
        only_item(&provider.extract(&prn_article_request(&provider)?, &blob(prn(PRN_ARTICLE))))?;
    let body = item.body.ok_or("no body")?.text;
    let lines: Vec<&str> = body.lines().collect();
    assert!(lines[0].starts_with(PRN_SUMMARY_OPENS), "{body}");
    assert!(
        lines[PRN_SUMMARY_ITEMS].starts_with(PRN_ARTICLE_OPENS),
        "{body}"
    );
    assert_eq!(
        lines.len(),
        PRN_SUMMARY_ITEMS + ARTICLE_PARAGRAPHS,
        "{body}"
    );
    assert!(!body.contains('<'), "{body}");
    for outside in PRN_OUTSIDE_THE_SECTION {
        assert!(
            !body.contains(outside),
            "{outside} reached the body: {body}"
        );
    }
    Ok(())
}

/// `body_ref` is an `item_id` input, so fused words would fix every affected id. Mutation
/// 2026-11-16, the `br` arm removed: red ("About SIGNALSIGNAL is").
#[test]
fn a_line_break_inside_a_paragraph_is_read_as_a_space() -> Outcome {
    let provider = provider()?;
    let page = blob(prn(PRN_LINE_BREAK_PAGE));
    let item = only_item(&provider.extract(&prn_article_request(&provider)?, &page))?;
    assert_eq!(
        item.body.map(|body| body.text).as_deref(),
        Some("About SIGNAL SIGNAL is a retrospective cohort study.\nLine one line two.")
    );
    Ok(())
}

/// A page without `section.release-body` is the page's fault, so it is held (section 11.1).
/// Mutation 2026-11-16, the container `article`: red.
#[test]
fn a_pr_newswire_page_with_no_release_body_is_unusable_as_no_article_body() -> Outcome {
    let provider = provider()?;
    let page = blob(prn(PRN_NO_RELEASE_BODY_PAGE));
    let row = only_unusable(&provider.extract(&prn_article_request(&provider)?, &page))?;
    assert_eq!(
        (
            row.reason,
            row.decided_by,
            row.raw_blob.as_str(),
            row.item.body.as_ref()
        ),
        (
            UnusableReason::NoArticleBody,
            DecidedBy::Page,
            page.digest.as_str(),
            None
        )
    );
    assert_eq!(row.item.title, "A Release Without Its Body");
    Ok(())
}

/// The key is the link's own host and path; both probe pages' canonical and `og:url` are that
/// link. Mutation 2026-11-16, a rewrite `www.prnewswire.com` to `prnewswire.com`: red.
#[test]
fn the_real_pr_newswire_article_carries_its_title_its_canonical_link_and_its_www_item_key(
) -> Outcome {
    let provider = provider()?;
    assert_eq!(
        provider.item_key(PRN_ARTICLE_URL).as_deref(),
        Some(PRN_ARTICLE_KEY)
    );
    let mut read = Vec::new();
    for (position, page) in [(1, PRN_ARTICLE), (2, PRN_SECOND_ARTICLE)] {
        let request = prn_follow_up(&provider, position)?;
        let item = only_item(&provider.extract(&request, &blob(prn(page))))?;
        read.push((
            item.title,
            item.canonical_url,
            item.source_item_id,
            item.observed,
        ));
    }
    let key_of = |url: &str| url.trim_start_matches("https://").to_owned();
    assert_eq!(
        read,
        vec![
            (
                PRN_ARTICLE_TITLE.to_owned(),
                PRN_ARTICLE_URL.to_owned(),
                PRN_ARTICLE_KEY.to_owned(),
                None
            ),
            (
                PRN_SECOND_ARTICLE_TITLE.to_owned(),
                PRN_SECOND_ARTICLE_URL.to_owned(),
                key_of(PRN_SECOND_ARTICLE_URL),
                None
            ),
        ]
    );
    Ok(())
}

// ---- Canada Newswire ----

/// The news sitemap's `loc` and `news:publication_date`, no rewrite, `/news-releases/`.
/// Mutation 2026-11-17, the prefix GNW's `/news-release/`: red, all 3 filtered out.
#[test]
fn the_real_canada_newswire_sitemap_yields_one_follow_up_per_release_with_its_raw_date() -> Outcome
{
    let provider = provider()?;
    let extracted = provider.extract(&cnw_index_request(&provider)?, &blob(cnw(CNW_REAL_INDEX)));
    assert_eq!(
        (extracted.vendor_rows, extracted.filtered_out),
        (REAL_ROWS, 0)
    );
    let kept: Vec<(&str, Option<&str>, RequestPhase)> = extracted
        .follow_ups
        .iter()
        .map(|request| {
            (
                request.url.as_str(),
                request.listed_date.as_deref(),
                request.phase,
            )
        })
        .collect();
    let expected: Vec<(&str, Option<&str>, RequestPhase)> = CNW_LINKS
        .iter()
        .zip(CNW_PUBLICATION_DATES)
        .map(|(link, date)| (*link, Some(date), RequestPhase::Document))
        .collect();
    assert_eq!(kept, expected);
    Ok(())
}

/// The 2,600 real rows of 2026-11-16 are all under `/news-releases/`, so only a synthetic link
/// tests the prefix from the wide side. Mutation 2026-11-17, the prefix `/`: red.
#[test]
fn a_canada_newswire_link_outside_news_releases_is_filtered_out_and_counted() -> Outcome {
    let provider = provider()?;
    let index = cnw_sitemap(&[
        (CNW_ARTICLE_URL, CNW_PUBLICATION_DATES[0]),
        (CNW_OFF_PREFIX_URL, CNW_PUBLICATION_DATES[0]),
    ]);
    let extracted = provider.extract(&cnw_index_request(&provider)?, &blob(index));
    let kept: Vec<&str> = extracted
        .follow_ups
        .iter()
        .map(|request| request.url.as_str())
        .collect();
    assert_eq!(
        (extracted.vendor_rows, kept, extracted.filtered_out),
        (2, vec![CNW_ARTICLE_URL], 1)
    );
    Ok(())
}

/// Elements are matched by namespace: an `image:loc` and a `news:publication_date` of another
/// namespace placed first are never read. Mutation 2026-11-17, the namespace test dropped: red.
#[test]
fn a_sitemap_element_named_like_a_field_in_another_namespace_is_never_read() -> Outcome {
    let provider = provider()?;
    let index = format!(
        "<?xml version=\"1.0\"?><urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\" \
         xmlns:news=\"http://www.google.com/schemas/sitemap-news/0.9\" \
         xmlns:image=\"http://www.google.com/schemas/sitemap-image/1.1\" \
         xmlns:other=\"urn:example:other\"><url>\
         <image:loc>https://www.newswire.ca/news-releases/an-image-1.html</image:loc>\
         <loc>{CNW_ARTICLE_URL}</loc><news:news>\
         <other:publication_date>2026-11-16T03:00:00-05:00</other:publication_date>\
         <news:publication_date>{}</news:publication_date></news:news></url></urlset>",
        CNW_PUBLICATION_DATES[0]
    );
    let extracted = provider.extract(&cnw_index_request(&provider)?, &blob(index.into_bytes()));
    let kept: Vec<(&str, Option<&str>)> = extracted
        .follow_ups
        .iter()
        .map(|request| (request.url.as_str(), request.listed_date.as_deref()))
        .collect();
    assert_eq!(
        kept,
        vec![(CNW_ARTICLE_URL, Some(CNW_PUBLICATION_DATES[0]))]
    );
    Ok(())
}

/// A `loc` and a date wrapped in whitespace are read trimmed; a date outside `news:news` is never
/// read; a `url` inside a `url` is no row. Mutations 2026-11-17: `.trim()` dropped, the field read
/// anywhere under `url`, `children()` read as `descendants()`: each red.
#[test]
fn a_sitemap_row_is_a_top_level_url_with_its_trimmed_loc_and_its_news_child_only() -> Outcome {
    let provider = provider()?;
    let nested = "https://www.newswire.ca/news-releases/a-nested-url-100000001.html";
    let index = format!(
        "<?xml version=\"1.0\"?><urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\" \
         xmlns:news=\"http://www.google.com/schemas/sitemap-news/0.9\">\
         <url><loc>\n  {}  \n</loc><news:news><news:publication_date>\n {} \n\
         </news:publication_date></news:news></url>\
         <url><loc>{}</loc><news:publication_date>{}</news:publication_date></url>\
         <url><loc>{}</loc><url><loc>{nested}</loc><news:news><news:publication_date>{}\
         </news:publication_date></news:news></url></url></urlset>",
        CNW_LINKS[0],
        CNW_PUBLICATION_DATES[0],
        CNW_LINKS[1],
        CNW_PUBLICATION_DATES[1],
        CNW_LINKS[2],
        CNW_PUBLICATION_DATES[2],
    );
    let extracted = provider.extract(&cnw_index_request(&provider)?, &blob(index.into_bytes()));
    let kept: Vec<(&str, Option<&str>)> = extracted
        .follow_ups
        .iter()
        .map(|request| (request.url.as_str(), request.listed_date.as_deref()))
        .collect();
    assert_eq!(
        (kept, extracted.vendor_rows, extracted.filtered_out),
        (
            vec![
                (CNW_LINKS[0], Some(CNW_PUBLICATION_DATES[0])),
                (CNW_LINKS[1], None),
                (CNW_LINKS[2], None),
            ],
            3,
            0
        )
    );
    Ok(())
}

/// Section 11.1: CNW's valid time is the sitemap's `news:publication_date`, so its index is the
/// sitemap and RSS is a permanent failure there; the sitemap is one on an RSS host, and so is a
/// sitemap index or a `urlset` outside the sitemaps.org namespace. Mutations 2026-11-17: the `cnw`
/// rule reading `rss`, red; the root check reading only "not `rss`", red.
#[test]
fn the_canada_newswire_index_is_its_news_sitemap_and_an_rss_answer_is_permanent() -> Outcome {
    let provider = provider()?;
    let request = cnw_index_request(&provider)?;
    let empty = cnw_sitemap(&[]);
    let sitemap_index = format!(
        "<?xml version=\"1.0\"?><sitemapindex xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\
         <sitemap><loc>{CNW_INDEX}</loc></sitemap></sitemapindex>"
    );
    let no_namespace =
        format!("<?xml version=\"1.0\"?><urlset><url><loc>{CNW_ARTICLE_URL}</loc></url></urlset>");
    assert_eq!(
        (
            verdict(&provider, &request, OK, &cnw(CNW_REAL_INDEX))?,
            verdict(&provider, &request, OK, &empty)?,
            verdict(&provider, &request, OK, &prn(PRN_REAL_INDEX))?,
            verdict(
                &provider,
                &prn_index_request(&provider)?,
                OK,
                &cnw(CNW_REAL_INDEX)
            )?,
            verdict(&provider, &request, OK, sitemap_index.as_bytes())?,
            verdict(&provider, &request, OK, no_namespace.as_bytes())?,
        ),
        (
            Verdict::Usable,
            Verdict::Empty,
            Verdict::PermanentFailure,
            Verdict::PermanentFailure,
            Verdict::PermanentFailure,
            Verdict::PermanentFailure
        )
    );
    Ok(())
}

/// Section 11.1: every observed CNW value ends `:00` and an offset (2,600 of 2,600, 2026-11-16),
/// so it is a minute at that offset. Mutations 2026-11-17: the offset dropped from `minute_of`,
/// red (02:22 read as 02:22Z); the round trip removed, red.
#[test]
fn a_canada_newswire_publication_date_is_a_minute_at_its_own_offset_and_never_an_instant() -> Outcome
{
    let provider = provider()?;
    let request = cnw_follow_up(&provider, 0)?;
    let extract = |raw: &str| {
        let dated = Request {
            listed_date: Some(raw.to_owned()),
            ..request.clone()
        };
        provider.extract(&dated, &blob(cnw(CNW_ARTICLE)))
    };
    let read = CNW_PUBLICATION_DATES
        .iter()
        .copied()
        .zip(CNW_MINUTES)
        .chain(CNW_OTHER_OFFSETS);
    for (raw, minute) in read {
        let item = only_item(&extract(raw))?;
        assert_eq!(
            (
                &item.valid,
                precision_of(&item.valid),
                item.valid_source,
                item.valid_raw.as_str(),
                item.valid_field.as_str()
            ),
            (
                &Published::AtMinute(Minute::new(minute.parse()?)?),
                ValidPrecision::Minute,
                ValidSource::VendorPublished,
                raw,
                "news:publication_date"
            ),
            "{raw}"
        );
    }
    for raw in CNW_NOT_A_MINUTE {
        let row = only_unusable(&extract(raw))?;
        assert_eq!(
            (row.reason, row.decided_by, &row.item.valid),
            (
                UnusableReason::NoParseableValidTime,
                DecidedBy::IndexRow,
                &Published::Unknown
            ),
            "{raw}"
        );
    }
    Ok(())
}

/// §8 row R17 for Canada Newswire: `p, li, figcaption` in `section.release-body`, captions in
/// document order; `article` adds the dateline and the share line (§11.1). Mutations 2026-11-17,
/// the container `body`, `article` and `section`: each red.
#[test]
fn the_extracted_body_is_article_text_and_never_page_html_on_a_canada_newswire_page() -> Outcome {
    let provider = provider()?;
    let item =
        only_item(&provider.extract(&cnw_follow_up(&provider, 0)?, &blob(cnw(CNW_ARTICLE))))?;
    let body = item.body.ok_or("no body")?.text;
    let lines: Vec<&str> = body.lines().collect();
    let opened: Vec<bool> = [0, 2, 3, 4]
        .iter()
        .zip(CNW_ARTICLE_LINES)
        .map(|(line, opens)| lines.get(*line).is_some_and(|text| text.starts_with(opens)))
        .collect();
    assert_eq!(opened, vec![true; CNW_ARTICLE_LINES.len()], "{body}");
    assert_eq!(
        lines.last().copied(),
        Some("Jane Example, , 00-00000000000"),
        "{body}"
    );
    assert!(!body.contains('<') && !body.contains('@'), "{body}");
    for outside in CNW_OUTSIDE_THE_SECTION
        .into_iter()
        .chain(["Continue Reading"])
    {
        assert!(
            !body.contains(outside),
            "{outside} reached the body: {body}"
        );
    }
    Ok(())
}

/// A page without `section.release-body` is the page's fault, so it is held (section 11.1). The
/// synthetic page holds a bare `div.release-content`, the container nested in the section on every
/// real page. Mutations 2026-11-17, the container `article` and `div.release-content`: each red.
#[test]
fn a_canada_newswire_page_with_no_release_body_is_unusable_as_no_article_body() -> Outcome {
    let provider = provider()?;
    let page = blob(cnw(CNW_NO_RELEASE_BODY_PAGE));
    let row = only_unusable(&provider.extract(&cnw_follow_up(&provider, 2)?, &page))?;
    assert_eq!(
        (
            row.reason,
            row.decided_by,
            row.raw_blob.as_str(),
            row.item.body.as_ref(),
            row.item.title.as_str()
        ),
        (
            UnusableReason::NoArticleBody,
            DecidedBy::Page,
            page.digest.as_str(),
            None,
            "A Release Without Its Body"
        )
    );
    Ok(())
}

/// The key is the link's own host and path; on all 7 CNW urls fetched 2026-11-16 the canonical
/// link and `og:url` are that link. Mutation 2026-11-17, a rewrite `www.newswire.ca` to
/// `newswire.ca`: red.
#[test]
fn the_real_canada_newswire_article_carries_its_title_its_canonical_link_and_its_item_key(
) -> Outcome {
    let provider = provider()?;
    assert_eq!(
        provider.item_key(CNW_ARTICLE_URL).as_deref(),
        Some(CNW_ARTICLE_KEY)
    );
    let mut read = Vec::new();
    for (position, page) in [(0, CNW_ARTICLE), (1, CNW_SECOND_ARTICLE)] {
        let request = cnw_follow_up(&provider, position)?;
        let item = only_item(&provider.extract(&request, &blob(cnw(page))))?;
        read.push((
            item.title,
            item.canonical_url,
            item.source_item_id,
            item.observed,
        ));
    }
    let key_of = |url: &str| url.trim_start_matches("https://").to_owned();
    assert_eq!(
        read,
        vec![
            (
                CNW_ARTICLE_TITLE.to_owned(),
                CNW_ARTICLE_URL.to_owned(),
                CNW_ARTICLE_KEY.to_owned(),
                None
            ),
            (
                CNW_SECOND_ARTICLE_TITLE.to_owned(),
                CNW_SECOND_ARTICLE_URL.to_owned(),
                key_of(CNW_SECOND_ARTICLE_URL),
                None
            ),
        ]
    );
    Ok(())
}

/// Section 11.2: what the committed CNW row asks of `www.newswire.ca`, which its robots.txt must
/// allow; the sitemap's query is part of it.
#[test]
fn the_committed_canada_newswire_row_asks_its_sitemap_with_its_query_and_its_prefix() -> Outcome {
    let asked: Vec<String> = provider()?
        .admitted_paths()
        .into_iter()
        .filter(|path| path.host == CNW_HOST)
        .map(|path| path.path)
        .collect();
    assert_eq!(asked, vec!["/sitemap-news.xml?page=1", "/news-releases/"]);
    Ok(())
}

/// A rule that names an index shape the reader does not know, or a news sitemap field outside
/// `news:news`, would read no row; both are refused before any request.
#[test]
fn an_unknown_index_or_a_sitemap_field_outside_news_is_refused() {
    for name in [
        "rules_unknown_index.json",
        "rules_sitemap_field_not_news.json",
    ] {
        assert!(
            matches!(
                refusal_of(&hosts_path(), &rules_fixture(name), CONTACT),
                Some(Refusal::RulesUnparseable { .. })
            ),
            "{name}"
        );
    }
}

// ---- Federal Reserve ----

/// The real feed's `link` and `pubDate`, no rewrite, `/newsevents/pressreleases/`; its byte order
/// mark, CRLF line ends and CDATA are read. Mutation 2026-11-17, the prefix PRN's `/news-releases/`:
/// red, all 3 filtered out.
#[test]
fn the_real_federal_reserve_feed_yields_one_follow_up_per_release_with_its_raw_pub_date() -> Outcome
{
    let provider = provider()?;
    let extracted = provider.extract(&fed_index_request(&provider)?, &blob(fed(FED_REAL_INDEX)));
    assert_eq!(
        (extracted.vendor_rows, extracted.filtered_out),
        (REAL_ROWS, 0)
    );
    let kept: Vec<(&str, Option<&str>, RequestPhase)> = extracted
        .follow_ups
        .iter()
        .map(|request| {
            (
                request.url.as_str(),
                request.listed_date.as_deref(),
                request.phase,
            )
        })
        .collect();
    let expected: Vec<(&str, Option<&str>, RequestPhase)> = FED_LINKS
        .iter()
        .zip(FED_PUB_DATES)
        .map(|(link, date)| (*link, Some(date), RequestPhase::Document))
        .collect();
    assert_eq!(kept, expected);
    Ok(())
}

/// The 20 real links of 2026-11-16 are all under `/newsevents/pressreleases/`, so only synthetic
/// links test the prefix from the wide side. Mutations 2026-11-17, the prefix `/`, and the prefix
/// without its trailing `/`: each red.
#[test]
fn a_federal_reserve_link_outside_press_releases_is_filtered_out_and_counted() -> Outcome {
    let provider = provider()?;
    let index = fed_rss(&[
        (FED_ARTICLE_URL, FED_PUB_DATES[0]),
        (FED_OFF_PREFIX_URL, FED_PUB_DATES[0]),
        (FED_PREFIX_WITHOUT_SLASH_URL, FED_PUB_DATES[0]),
    ]);
    let extracted = provider.extract(&fed_index_request(&provider)?, &blob(index));
    let kept: Vec<&str> = extracted
        .follow_ups
        .iter()
        .map(|request| request.url.as_str())
        .collect();
    assert_eq!(
        (extracted.vendor_rows, kept, extracted.filtered_out),
        (3, vec![FED_ARTICLE_URL], 2)
    );
    Ok(())
}

/// Section 11.1: every observed Fed `pubDate` is `GMT` with second `00`, its day unpadded below
/// the 10th (20 of 20 items; one body in all 26 polls of 2026-11-16). Mutations 2026-11-17: `%d`
/// for `%-d`, red (the real `9 Nov` refused); the round trip removed, red (`09 Nov` read).
#[test]
fn a_federal_reserve_pubdate_is_a_minute_with_an_unpadded_day_and_never_an_instant() -> Outcome {
    let provider = provider()?;
    let request = fed_follow_up(&provider, 0)?;
    let extract = |raw: &str| {
        let dated = Request {
            listed_date: Some(raw.to_owned()),
            ..request.clone()
        };
        provider.extract(&dated, &blob(fed(FED_ARTICLE)))
    };
    for (raw, minute) in FED_PUB_DATES.iter().copied().zip(FED_MINUTES) {
        let item = only_item(&extract(raw))?;
        assert_eq!(
            (
                &item.valid,
                precision_of(&item.valid),
                item.valid_source,
                item.valid_raw.as_str(),
                item.valid_field.as_str()
            ),
            (
                &Published::AtMinute(Minute::new(minute.parse()?)?),
                ValidPrecision::Minute,
                ValidSource::VendorPublished,
                raw,
                "pubDate"
            ),
            "{raw}"
        );
    }
    for raw in FED_NOT_A_MINUTE {
        let row = only_unusable(&extract(raw))?;
        assert_eq!(
            (row.reason, row.decided_by, &row.item.valid),
            (
                UnusableReason::NoParseableValidTime,
                DecidedBy::IndexRow,
                &Published::Unknown
            ),
            "{raw}"
        );
    }
    Ok(())
}

/// §8 row R17 for the Fed: `p, li, figcaption` in the release column, the direct child of
/// `div#article` that is not its heading block. Mutations 2026-11-17, the container `body`,
/// `div#article`, the column without `:not(.heading)`, and without `div#article >`: each red.
#[test]
fn the_extracted_body_is_article_text_and_never_page_html_on_a_federal_reserve_page() -> Outcome {
    let provider = provider()?;
    let item =
        only_item(&provider.extract(&fed_follow_up(&provider, 0)?, &blob(fed(FED_ARTICLE))))?;
    let body = item.body.ok_or("no body")?.text;
    let opened: Vec<bool> = body
        .lines()
        .zip(FED_ARTICLE_LINES)
        .map(|(line, opens)| line.starts_with(opens))
        .collect();
    assert_eq!(opened, vec![true; FED_ARTICLE_LINES.len()], "{body}");
    assert_eq!(body.lines().count(), FED_ARTICLE_LINES.len(), "{body}");
    assert!(!body.contains('<'), "{body}");
    for outside in FED_OUTSIDE_THE_COLUMN {
        assert!(
            !body.contains(outside),
            "{outside} reached the body: {body}"
        );
    }
    let second = only_item(&provider.extract(
        &fed_follow_up(&provider, 2)?,
        &blob(fed(FED_SECOND_ARTICLE)),
    ))?;
    assert_eq!(
        second.body.ok_or("no body")?.text,
        FED_SECOND_ARTICLE_BODY.join("\n")
    );
    Ok(())
}

/// A page with no release column under `div#article` is the page's fault, so it is held (section
/// 11.1). The synthetic page holds the heading and the column under another `div`. Mutation
/// 2026-11-17, the container without `div#article >`: red.
#[test]
fn a_federal_reserve_page_with_no_article_block_is_unusable_as_no_article_body() -> Outcome {
    let provider = provider()?;
    let page = blob(fed(FED_NO_ARTICLE_BLOCK_PAGE));
    let row = only_unusable(&provider.extract(&fed_follow_up(&provider, 1)?, &page))?;
    assert_eq!(
        (
            row.reason,
            row.decided_by,
            row.raw_blob.as_str(),
            row.item.body.as_ref(),
            row.item.title.as_str()
        ),
        (
            UnusableReason::NoArticleBody,
            DecidedBy::Page,
            page.digest.as_str(),
            None,
            "A release with no article block"
        )
    );
    Ok(())
}

/// The key is the link's own host and path. No Fed page names a canonical link (9 of 9 files,
/// 2026-11-16), so `doc_id` is the requested url, which `og:url` equals on all 9. Mutation
/// 2026-11-17, a rewrite `www.federalreserve.gov` to `federalreserve.gov`: red.
#[test]
fn the_real_federal_reserve_release_carries_its_title_its_own_url_and_its_item_key() -> Outcome {
    let provider = provider()?;
    assert_eq!(
        provider.item_key(FED_ARTICLE_URL).as_deref(),
        Some(FED_ARTICLE_KEY)
    );
    let mut read = Vec::new();
    for (position, page) in [(0, FED_ARTICLE), (2, FED_SECOND_ARTICLE)] {
        let request = fed_follow_up(&provider, position)?;
        let item = only_item(&provider.extract(&request, &blob(fed(page))))?;
        read.push((
            item.title,
            item.canonical_url,
            item.source_item_id,
            item.observed,
        ));
    }
    let key_of = |url: &str| url.trim_start_matches("https://").to_owned();
    assert_eq!(
        read,
        vec![
            (
                FED_ARTICLE_TITLE.to_owned(),
                FED_ARTICLE_URL.to_owned(),
                FED_ARTICLE_KEY.to_owned(),
                None
            ),
            (
                FED_SECOND_ARTICLE_TITLE.to_owned(),
                FED_SECOND_ARTICLE_URL.to_owned(),
                key_of(FED_SECOND_ARTICLE_URL),
                None
            ),
        ]
    );
    Ok(())
}

/// §8 row R3 for the Fed: a page naming `link[rel=canonical]` is stored under that link, never its
/// requested url or `og:url`. No real Fed page names one, so the real release gets one in its head.
/// Mutations 2026-11-17, the Fed canonical selector `meta[property="og:url"]` and `link[rel="x"]`: red.
#[test]
fn a_federal_reserve_page_naming_a_canonical_link_is_stored_under_it() -> Outcome {
    let provider = provider()?;
    let real = String::from_utf8(fed(FED_ARTICLE))?;
    let named = format!("<link rel=\"canonical\" href=\"{FED_SYNTHETIC_CANONICAL}\" />\n</head>");
    let page = real.replacen("</head>", &named, 1);
    let item = only_item(&provider.extract(
        &fed_follow_up(&provider, 0)?,
        &blob(page.clone().into_bytes()),
    ))?;
    assert_eq!(
        (
            page.contains(FED_SYNTHETIC_CANONICAL),
            item.canonical_url.as_str(),
            item.url.as_str(),
            item.source_item_id.as_str(),
        ),
        (
            true,
            FED_SYNTHETIC_CANONICAL,
            FED_ARTICLE_URL,
            FED_ARTICLE_KEY
        )
    );
    Ok(())
}

/// Section 11.2: what the committed Fed row asks of `www.federalreserve.gov`; its robots.txt is a
/// 404, so no rule forbids either path.
#[test]
fn the_committed_federal_reserve_row_asks_its_feed_and_its_prefix() -> Outcome {
    let asked: Vec<String> = provider()?
        .admitted_paths()
        .into_iter()
        .filter(|path| path.host == FED_HOST)
        .map(|path| path.path)
        .collect();
    assert_eq!(
        asked,
        vec!["/feeds/press_all.xml", "/newsevents/pressreleases/"]
    );
    Ok(())
}

// ---- Yahoo Finance ----

/// Section 11.2: of the real sitemap's 8 rows only the `business-wire.com` and `accesswire.ca`
/// ones are followed, with their raw `news:publication_date`; the 6 others are counted.
#[test]
fn the_real_yahoo_sitemap_follows_only_its_business_wire_and_accesswire_rows() -> Outcome {
    let provider = provider()?;
    let extracted = provider.extract(
        &yahoo_index_request(&provider)?,
        &blob(yahoo(YAHOO_REAL_INDEX)),
    );
    let kept: Vec<(&str, Option<&str>, RequestPhase)> = extracted
        .follow_ups
        .iter()
        .map(|request| {
            (
                request.url.as_str(),
                request.listed_date.as_deref(),
                request.phase,
            )
        })
        .collect();
    let expected: Vec<(&str, Option<&str>, RequestPhase)> = YAHOO_LINKS
        .iter()
        .zip(YAHOO_PUBLICATION_DATES)
        .map(|(link, date)| (*link, Some(date), RequestPhase::Document))
        .collect();
    assert_eq!(
        (extracted.vendor_rows, kept, extracted.filtered_out),
        (YAHOO_ROWS, expected, YAHOO_ROWS - 2)
    );
    Ok(())
}

/// Section 11.2: a key is matched exactly, never as a substring and never in another case; a mark
/// with no segment after it, another mark and no image are no key. Mutations 2026-11-17, the
/// match by `contains` either way and by `eq_ignore_ascii_case`: each red.
#[test]
fn a_provider_key_is_matched_whole_and_never_as_a_substring() -> Outcome {
    let provider = provider()?;
    let link = |n: u8| format!("https://finance.yahoo.com/markets/stocks/articles/a-{n}.html");
    let images = [
        Some(format!("{YAHOO_IMAGE}business-wire.com/a.jpg")),
        Some(format!("{YAHOO_IMAGE}business-wire.com.au/a.jpg")),
        Some(format!("{YAHOO_IMAGE}xbusiness-wire.com/a.jpg")),
        Some(format!("{YAHOO_IMAGE}accesswire.ca_9/a.jpg")),
        Some(format!("{YAHOO_IMAGE}Business-Wire.com/a.jpg")),
        Some(format!("{YAHOO_IMAGE}wire.com/a.jpg")),
        Some(format!("{YAHOO_IMAGE}business-wire.com")),
        Some("https://media.zenfs.com/fr/business-wire.com/a.jpg".to_owned()),
        Some(format!("{YAHOO_IMAGE}prnewswire.com/a.jpg")),
        None,
    ];
    let links: Vec<String> = (0..).take(images.len()).map(link).collect();
    let rows: Vec<(&str, &str, Option<&str>)> = links
        .iter()
        .zip(&images)
        .map(|(link, image)| (link.as_str(), YAHOO_PUBLICATION_DATES[0], image.as_deref()))
        .collect();
    let extracted = provider.extract(
        &yahoo_index_request(&provider)?,
        &blob(yahoo_sitemap(&rows)),
    );
    let kept: Vec<&str> = extracted
        .follow_ups
        .iter()
        .map(|request| request.url.as_str())
        .collect();
    assert_eq!(
        (extracted.vendor_rows, kept, extracted.filtered_out),
        (10, vec![links[0].as_str()], 9)
    );
    Ok(())
}

/// Section 11.1: every `business-wire.com` and `accesswire.ca` row of the 26 polls of 2026-11-16
/// is `Z` with second `00` (2,017 of 2,017); outlet rows carry seconds (12,247 of 23,983 others)
/// and are never followed. Mutation 2026-11-17, `minute_of`'s round trip removed: red.
#[test]
fn a_yahoo_publication_date_is_a_minute_and_never_an_instant() -> Outcome {
    let provider = provider()?;
    let request = yahoo_follow_up(&provider, 0)?;
    let extract = |raw: &str| {
        let dated = Request {
            listed_date: Some(raw.to_owned()),
            ..request.clone()
        };
        provider.extract(&dated, &blob(yahoo(YAHOO_ARTICLE)))
    };
    for (raw, minute) in YAHOO_PUBLICATION_DATES.iter().copied().zip(YAHOO_MINUTES) {
        let item = only_item(&extract(raw))?;
        assert_eq!(
            (
                &item.valid,
                item.valid_source,
                item.valid_raw.as_str(),
                item.valid_field.as_str()
            ),
            (
                &Published::AtMinute(Minute::new(minute.parse()?)?),
                ValidSource::VendorPublished,
                raw,
                "news:publication_date"
            ),
            "{raw}"
        );
    }
    for raw in YAHOO_NOT_A_MINUTE {
        let row = only_unusable(&extract(raw))?;
        assert_eq!(
            (row.reason, row.decided_by),
            (UnusableReason::NoParseableValidTime, DecidedBy::IndexRow),
            "{raw}"
        );
    }
    Ok(())
}

/// §8 row R17 for Yahoo, P6: `Story Continues` is a `button` and an ad slot holds only `div`s, so
/// neither yields a line; the text after the button does (16 of 16 pages, 2026-11-17). Mutations
/// 2026-11-17, the container `body` and `div.bodyItems-wrapper`: each red.
#[test]
fn the_extracted_body_is_article_text_and_never_page_html_on_a_yahoo_page() -> Outcome {
    let provider = provider()?;
    let item =
        only_item(&provider.extract(&yahoo_follow_up(&provider, 0)?, &blob(yahoo(YAHOO_ARTICLE))))?;
    let body = item.body.ok_or("no body")?.text;
    let opened: Vec<bool> = body
        .lines()
        .zip(YAHOO_ARTICLE_LINES)
        .map(|(line, opens)| line.starts_with(opens))
        .collect();
    assert_eq!(opened, vec![true; YAHOO_ARTICLE_LINES.len()], "{body}");
    assert_eq!(body.lines().count(), YAHOO_ARTICLE_LINES.len(), "{body}");
    assert!(!body.contains('<'), "{body}");
    for outside in YAHOO_OUTSIDE_THE_BODY {
        assert!(
            !body.contains(outside),
            "{outside} reached the body: {body}"
        );
    }
    let reprint = only_item(&provider.extract(
        &yahoo_follow_up(&provider, 0)?,
        &blob(yahoo(YAHOO_PRN_REPRINT)),
    ))?;
    let reprinted = reprint.body.ok_or("no body")?.text;
    let added: Vec<bool> = ["Horizon Capital logo", "Cision", "Contact: Jane Doe"]
        .iter()
        .map(|line| reprinted.lines().any(|read| read == *line))
        .collect();
    assert_eq!(
        (added, reprinted.contains("Story Continues")),
        (vec![true; 3], false),
        "{reprinted}"
    );
    Ok(())
}

/// A page with no `div[data-testid="article-body"]` is the page's fault, so it is held (section
/// 11.1). Mutations 2026-11-17, the container `article`, `body` and `div.body`: each red.
#[test]
fn a_yahoo_page_with_no_article_body_is_unusable_as_no_article_body() -> Outcome {
    let provider = provider()?;
    let page = blob(yahoo(YAHOO_NO_ARTICLE_BODY_PAGE));
    let row = only_unusable(&provider.extract(&yahoo_follow_up(&provider, 1)?, &page))?;
    assert_eq!(
        (
            row.reason,
            row.decided_by,
            row.raw_blob.as_str(),
            row.item.title.as_str()
        ),
        (
            UnusableReason::NoArticleBody,
            DecidedBy::Page,
            page.digest.as_str(),
            "A Reprint Without Its Body"
        )
    );
    Ok(())
}

/// The key is the Yahoo link's own host and path; a reprint is never keyed by the wire release it
/// copies. Mutation 2026-11-17, a rewrite `finance.yahoo.com` to `yahoo.com`: red.
#[test]
fn the_real_yahoo_reprint_carries_its_title_its_canonical_link_and_its_item_key() -> Outcome {
    let provider = provider()?;
    let item =
        only_item(&provider.extract(&yahoo_follow_up(&provider, 0)?, &blob(yahoo(YAHOO_ARTICLE))))?;
    assert_eq!(
        (
            item.title.as_str(),
            item.canonical_url.as_str(),
            item.source_item_id.as_str(),
            item.observed
        ),
        (
            YAHOO_ARTICLE_TITLE,
            YAHOO_ARTICLE_URL,
            YAHOO_ARTICLE_KEY,
            None
        )
    );
    Ok(())
}

/// Section 11.2: Yahoo's article paths start in 9 sections (`/technology/`, `/markets/`, ...; 26
/// polls of 2026-11-16), so the path filter is `/` and the key filter narrows. Mutation
/// 2026-11-17, the prefix `/markets/`: red.
#[test]
fn the_committed_yahoo_row_asks_its_sitemap_and_every_path() -> Outcome {
    let asked: Vec<String> = provider()?
        .admitted_paths()
        .into_iter()
        .filter(|path| path.host == YAHOO_HOST)
        .map(|path| path.path)
        .collect();
    assert_eq!(asked, vec!["/news-sitemap.xml", "/"]);
    Ok(())
}

/// A key filter reads `image:loc`, which only a news sitemap has; a key both followed and
/// cross-checked would be stored and compared; a list holding a `/` cannot be one segment.
#[test]
fn a_provider_key_filter_nothing_can_read_whole_is_refused() {
    for name in [
        "rules_provider_keys_on_rss.json",
        "rules_provider_keys_followed_and_cross_checked.json",
        "rules_provider_keys_following_none.json",
        "rules_provider_keys_mark_without_slash.json",
        "rules_provider_keys_key_with_slash.json",
    ] {
        assert!(
            matches!(
                refusal_of(&hosts_path(), &rules_fixture(name), CONTACT),
                Some(Refusal::RulesUnparseable { .. })
            ),
            "{name}"
        );
    }
}

/// A `cross_check` key's host must be another admitted row, or its rows are never looked for:
/// GNW absent from the CNW-and-Yahoo hosts file, and a key mapped to Yahoo itself. Mutations
/// 2026-11-17, the check removed and the own-host clause dropped: each red.
#[test]
fn a_cross_check_key_whose_host_no_other_admitted_row_holds_is_refused() {
    let refused = |hosts: PathBuf, rules: PathBuf| match refusal_of(&hosts, &rules, CONTACT) {
        Some(Refusal::CrossCheckHostNotAdmitted {
            rule, key, host, ..
        }) => Some((rule, key, host)),
        _ => None,
    };
    let named = |key: &str, host: &str| Some(("yahoo".to_owned(), key.to_owned(), host.to_owned()));
    assert_eq!(
        (
            refused(hosts_fixture("cnw_and_yahoo_admitted.json"), rules_path()),
            refused(
                hosts_path(),
                rules_fixture("rules_cross_check_to_its_own_host.json")
            ),
        ),
        (
            named("globenewswire.com", "rss.globenewswire.com"),
            named("cnwgroup.com", YAHOO_HOST)
        )
    );
}

/// `cross_check` is a key-to-host object: the pre-map list, a key mapped to `""` and one mapped to
/// `null` are each `RulesUnparseable`. Mutations 2026-11-17, a non-object read as no key and the
/// empty-host check removed: each red (green before this test).
#[test]
fn a_cross_check_that_maps_no_key_to_a_host_is_unparseable() {
    for name in [
        "rules_cross_check_a_list.json",
        "rules_cross_check_to_an_empty_host.json",
        "rules_cross_check_to_a_non_string.json",
    ] {
        assert!(
            matches!(
                refusal_of(&hosts_path(), &rules_fixture(name), CONTACT),
                Some(Refusal::RulesUnparseable { .. })
            ),
            "{name}"
        );
    }
}

/// The hosts file's hosts are lowercased, so a `cross_check` host is too: `WWW.PRNEWSWIRE.COM`
/// names the admitted PRN row. Mutation 2026-11-17, the rules host kept as written: red (green
/// before this test).
#[test]
fn a_cross_check_host_in_upper_case_names_its_admitted_row() {
    assert!(refusal_of(
        &hosts_path(),
        &rules_fixture("rules_cross_check_host_in_upper_case.json"),
        CONTACT
    )
    .is_none());
}

// ---- Yahoo cross-check (section 11.2, option C) ----

fn checker() -> Result<CrossChecker, Box<dyn std::error::Error>> {
    provider()?
        .cross_checker()
        .ok_or_else(|| "the committed files cross-check nothing".into())
}

fn instant(text: &str) -> Option<DateTime<Utc>> {
    text.parse().ok()
}

/// After every keyed row of the real Yahoo sitemap closed.
const LOOKED_AT: &str = "2026-11-16T09:30:29Z";
const EMPTY_RSS: &[u8] = b"<rss version=\"2.0\"><channel></channel></rss>";
const EMPTY_SITEMAP: &[u8] = b"<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\"/>";

fn answer(url: &str, at: &str, bytes: Vec<u8>) -> Result<DirectAnswer, Box<dyn std::error::Error>> {
    Ok(DirectAnswer {
        url: url.to_owned(),
        received_at: instant(at).ok_or("not an instant")?,
        bytes,
    })
}

/// Each committed direct index answered at `at`: by `given` when named there, else an empty index.
fn every_index_at(
    at: &str,
    given: &[(&str, Vec<u8>)],
) -> Result<Vec<DirectAnswer>, Box<dyn std::error::Error>> {
    [GNW_INDEX, PRN_INDEX, CNW_INDEX, FED_INDEX]
        .iter()
        .map(|url| {
            let empty = if *url == CNW_INDEX {
                EMPTY_SITEMAP
            } else {
                EMPTY_RSS
            };
            let bytes = given
                .iter()
                .find(|(named, _)| named == url)
                .map_or_else(|| empty.to_vec(), |(_, bytes)| bytes.clone());
            answer(url, at, bytes)
        })
        .collect()
}

/// The committed files: Yahoo is the keyed index, and the other four admitted indexes are direct.
#[test]
fn the_committed_hosts_file_cross_checks_yahoo_against_the_other_four_indexes() -> Outcome {
    let checker = checker()?;
    let direct: Vec<bool> = [GNW_INDEX, PRN_INDEX, CNW_INDEX, FED_INDEX, YAHOO_INDEX]
        .iter()
        .map(|url| checker.is_direct_index(url))
        .collect();
    assert_eq!(
        (checker.index_url(), direct),
        (YAHOO_INDEX, vec![true, true, true, true, false])
    );
    Ok(())
}

/// Option C: a `cross_check` row whose title no direct index listed is named, one whose title the
/// CNW sitemap listed is counted, and the other rows are counted by key. Mutations 2026-11-17,
/// every title read as listed, and a cross-check row counted as followed: each red.
#[test]
fn a_cross_check_row_no_direct_index_listed_is_named_and_a_listed_one_counted() -> Outcome {
    let checked = checker()?
        .check(
            &yahoo(YAHOO_REAL_INDEX),
            &every_index_at(LOOKED_AT, &[(CNW_INDEX, cnw(CNW_REAL_INDEX))])?,
            instant("2026-11-16T07:00:00Z"),
        )
        .ok_or("the real sitemap did not parse")?;
    let unlisted: Vec<(&str, &str, &str)> = checked
        .unlisted
        .iter()
        .map(|row| (row.key.as_str(), row.url.as_str(), row.listed_raw.as_str()))
        .collect();
    assert_eq!(
        (
            [
                checked.followed,
                checked.cross_checked,
                checked.other_key,
                checked.no_key
            ],
            checked.not_compared,
            checked.not_looked,
            checked.listed_directly,
            unlisted,
            checked.direct_titles
        ),
        (
            [2, 3, 2, 1],
            0,
            0,
            1,
            vec![
                ("globenewswire.com", YAHOO_GNW_ROW, "2026-11-16T09:05:00Z"),
                ("prnewswire.com", YAHOO_PRN_ROW, "2026-11-16T09:00:00Z"),
            ],
            3
        )
    );
    Ok(())
}

/// A row listed before the first direct answer could not have been seen directly, so it is not
/// compared; with no direct answer at all, none is. Mutation 2026-11-17, `>=` as `>`: red.
#[test]
fn a_cross_check_row_dated_before_the_first_direct_answer_is_not_compared() -> Outcome {
    let checker = checker()?;
    let direct = every_index_at(LOOKED_AT, &[(CNW_INDEX, cnw(CNW_REAL_INDEX))])?;
    let index = yahoo(YAHOO_REAL_INDEX);
    let compared = |from: Option<DateTime<Utc>>| {
        checker.check(&index, &direct, from).map(|checked| {
            let urls: Vec<String> = checked.unlisted.into_iter().map(|row| row.url).collect();
            (checked.not_compared, checked.listed_directly, urls)
        })
    };
    assert_eq!(
        (
            compared(instant("2026-11-16T09:01:00Z")),
            compared(instant("2026-11-16T09:00:00Z")),
            compared(None)
        ),
        (
            Some((2, 0, vec![YAHOO_GNW_ROW.to_owned()])),
            Some((
                1,
                0,
                vec![YAHOO_GNW_ROW.to_owned(), YAHOO_PRN_ROW.to_owned()]
            )),
            Some((3, 0, Vec::new()))
        )
    );
    Ok(())
}

/// Section 11.6's title match: case and every run of punctuation fold away, and a title is never
/// matched by a prefix of it. A payload under a url no admitted row names adds no title.
/// Mutations 2026-11-17, the case kept, a prefix match and any rule that parses it: each red.
#[test]
fn titles_match_after_case_and_punctuation_fold_and_never_by_a_prefix() -> Outcome {
    let checker = checker()?;
    let rss = |titles: &[&str]| -> Vec<u8> {
        let items: String = titles
            .iter()
            .map(|title| format!("<item><title>{title}</title></item>"))
            .collect();
        format!("<rss version=\"2.0\"><channel>{items}</channel></rss>").into_bytes()
    };
    let mut direct = every_index_at(
        LOOKED_AT,
        &[(
            PRN_INDEX,
            rss(&[
                "kyiv based horizon capital reaches 200 million at second closing of catalyst fund",
                "Geely Holding Announces EVSA",
            ]),
        )],
    )?;
    direct.push(answer(
        "https://www.prnewswire.com/rss/elsewhere.rss",
        LOOKED_AT,
        cnw(CNW_REAL_INDEX),
    )?);
    let checked = checker
        .check(
            &yahoo(YAHOO_REAL_INDEX),
            &direct,
            instant("2026-11-16T07:00:00Z"),
        )
        .ok_or("the real sitemap did not parse")?;
    let unlisted: Vec<&str> = checked
        .unlisted
        .iter()
        .map(|row| row.url.as_str())
        .collect();
    assert_eq!(
        (checked.listed_directly, unlisted),
        (1, vec![YAHOO_GNW_ROW, YAHOO_ROW_LISTED_BY_CNW])
    );
    assert_eq!(
        open_web_feed::normalised("Share Buy-back Programme – Transactions Week 46"),
        "share buy back programme transactions week 46"
    );
    Ok(())
}

/// Collect reads direct answers from `DIRECT_LEAD`, 90 min, before the oldest `cross_check` row's
/// minute; rows of another key, a followed key or none do not move it. Mutations 2026-11-17, `min`
/// as `max`, the minimum over every row, and the lead 89 or 91 min: each red.
#[test]
fn the_earliest_cross_check_minute_is_the_oldest_cross_check_row() -> Outcome {
    let index = yahoo_sitemap(&[
        (
            "https://finance.yahoo.com/a",
            "2026-11-16T08:00:00Z",
            Some("https://media.zenfs.com/en/prnewswire.com/a.jpg"),
        ),
        (
            "https://finance.yahoo.com/b",
            "2026-11-16T07:30:00Z",
            Some("https://media.zenfs.com/en/globenewswire.com/b.jpg"),
        ),
        (
            "https://finance.yahoo.com/c",
            "2026-11-16T06:00:00Z",
            Some("https://media.zenfs.com/en/newsfile_64/c.jpg"),
        ),
        (
            "https://finance.yahoo.com/d",
            "2026-11-16T05:00:00Z",
            Some("https://media.zenfs.com/en/business-wire.com/d.jpg"),
        ),
        ("https://finance.yahoo.com/e", "2026-11-16T04:00:00Z", None),
    ]);
    let checker = checker()?;
    assert_eq!(
        (
            checker.earliest_cross_checked(&index),
            checker.read_from(&index),
            checker.earliest_cross_checked(&yahoo(YAHOO_REAL_INDEX)),
        ),
        (
            instant("2026-11-16T07:30:00Z"),
            instant("2026-11-16T06:00:00Z"),
            instant("2026-11-16T07:22:00Z")
        )
    );
    Ok(())
}

/// Owner decision 2026-11-17: the lead is section 11.2's longest poll period, not a number fitted
/// to a measurement. Mutation 2026-11-17, `DIRECT_LEAD` 2 h again: red.
#[test]
fn the_direct_lead_is_the_longest_poll_period() {
    assert_eq!(
        (DIRECT_LEAD, LONGEST_POLL_PERIOD.num_minutes()),
        (LONGEST_POLL_PERIOD, 90)
    );
}

/// Yahoo dated a PRN row 72 min after a PRN answer listed it (2026-11-16): an answer 72 min before
/// the earliest `cross_check` minute (07:22Z) is read and lists its row. Mutation 2026-11-17,
/// `DIRECT_LEAD` zero: red.
#[test]
fn a_direct_answer_received_before_the_keyed_minute_still_lists_the_row() -> Outcome {
    let checker = checker()?;
    let index = yahoo(YAHOO_REAL_INDEX);
    let earlier = answer(
        PRN_INDEX,
        "2026-11-16T06:10:00Z",
        rss_titles(&[
            "Kyiv-based Horizon Capital reaches $200 million at second closing of Catalyst Fund",
        ]),
    )?;
    let read_from = checker.read_from(&index).ok_or("no cross-check row")?;
    let mut direct = every_index_at(LOOKED_AT, &[])?;
    direct.push(earlier.clone());
    let checked = checker
        .check(&index, &direct, instant("2026-11-16T07:00:00Z"))
        .ok_or("the real sitemap did not parse")?;
    let unlisted: Vec<String> = checked.unlisted.into_iter().map(|row| row.url).collect();
    assert_eq!(
        (
            earlier.received_at >= read_from,
            checked.listed_directly,
            unlisted
        ),
        (
            true,
            1,
            vec![YAHOO_GNW_ROW.to_owned(), YAHOO_ROW_LISTED_BY_CNW.to_owned()]
        )
    );
    Ok(())
}

/// A row is compared only when its key's own index parsed an answer after the row's minute
/// closed: PRN absent, a PRN 503 body, or a PRN answer inside the minute (09:00:30Z for Horizon's
/// 09:00) leaves the PRN rows "not looked", never "unlisted"; one at 09:01:00Z looks. Review
/// 2026-11-17: a CNW 503 run named three rows unlisted. Mutation 2026-11-17, `>=` as `>`: red.
#[test]
fn a_row_whose_own_index_did_not_answer_after_is_not_looked_and_never_unlisted() -> Outcome {
    let checker = checker()?;
    let index = yahoo(YAHOO_REAL_INDEX);
    let looked = |direct: &[DirectAnswer]| {
        checker
            .check(&index, direct, instant("2026-11-16T07:00:00Z"))
            .map(|checked| {
                (
                    checked.not_looked,
                    checked.listed_directly + u32::try_from(checked.unlisted.len()).unwrap_or(0),
                    checked.direct_parsed,
                )
            })
    };
    let all = every_index_at(LOOKED_AT, &[(CNW_INDEX, cnw(CNW_REAL_INDEX))])?;
    let without_prn = all_but(&all, PRN_INDEX);
    let mut prn_503 = without_prn.clone();
    prn_503.push(answer(
        PRN_INDEX,
        LOOKED_AT,
        b"Service Unavailable".to_vec(),
    )?);
    let mut prn_early = without_prn.clone();
    prn_early.push(answer(
        PRN_INDEX,
        "2026-11-16T09:00:30Z",
        EMPTY_RSS.to_vec(),
    )?);
    let mut prn_at_close = without_prn.clone();
    prn_at_close.push(answer(
        PRN_INDEX,
        "2026-11-16T09:01:00Z",
        EMPTY_RSS.to_vec(),
    )?);
    assert_eq!(
        (
            looked(&all),
            looked(&without_prn),
            looked(&prn_503),
            looked(&prn_early),
            looked(&prn_at_close)
        ),
        (
            Some((0, 3, 4)),
            Some((2, 1, 3)),
            Some((2, 1, 3)),
            Some((1, 2, 4)),
            Some((0, 3, 4))
        )
    );
    Ok(())
}

/// Owner decision 2026-11-17: each `cross_check` key waits for its own index only (PRN, GNW, CNW;
/// the Fed is none's). One row per key, each index absent in turn: only that index's key is not
/// compared. Mutations 2026-11-17, the map ignored and two hosts swapped in the rules: each red.
#[test]
fn each_cross_check_key_waits_for_its_own_index_and_no_other() -> Outcome {
    let checker = checker()?;
    let keys = ["prnewswire.com", "globenewswire.com", "cnwgroup.com"];
    let rows: Vec<(String, String)> = keys
        .iter()
        .map(|key| {
            (
                format!("https://finance.yahoo.com/{key}"),
                format!("https://media.zenfs.com/en/{key}/a.jpg"),
            )
        })
        .collect();
    let listed: Vec<(&str, &str, Option<&str>)> = rows
        .iter()
        .map(|(link, image)| (link.as_str(), "2026-11-16T09:00:00Z", Some(image.as_str())))
        .collect();
    let index = yahoo_sitemap(&listed);
    let all = every_index_at(LOOKED_AT, &[])?;
    let compared = |absent: &str| {
        checker
            .check(
                &index,
                &all_but(&all, absent),
                instant("2026-11-16T07:00:00Z"),
            )
            .map(|checked| {
                let keys: Vec<String> = checked.unlisted.into_iter().map(|row| row.key).collect();
                (checked.not_looked, keys)
            })
    };
    let but = |skipped: &str| -> Option<(u32, Vec<String>)> {
        let kept = keys.iter().filter(|key| **key != skipped);
        Some((
            u32::from(!skipped.is_empty()),
            kept.map(|key| (*key).to_owned()).collect(),
        ))
    };
    assert_eq!(
        [PRN_INDEX, GNW_INDEX, CNW_INDEX, FED_INDEX].map(compared),
        [
            but("prnewswire.com"),
            but("globenewswire.com"),
            but("cnwgroup.com"),
            but("")
        ]
    );
    Ok(())
}

fn all_but(answers: &[DirectAnswer], url: &str) -> Vec<DirectAnswer> {
    answers
        .iter()
        .filter(|answer| answer.url != url)
        .cloned()
        .collect()
}

/// A title or a key that folds to nothing matches nothing: an empty title is never "listed", and
/// an image with an empty key segment has no key. Review 2026-11-17: both caught only by golden.
#[test]
fn an_empty_title_or_an_empty_key_matches_nothing() -> Outcome {
    let checker = checker()?;
    let index = b"<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\" \
         xmlns:news=\"http://www.google.com/schemas/sitemap-news/0.9\" \
         xmlns:image=\"http://www.google.com/schemas/sitemap-image/1.1\">\
         <url><loc>https://finance.yahoo.com/a</loc><news:news><news:publication_date>\
         2026-11-16T09:00:00Z</news:publication_date><news:title>?!</news:title></news:news>\
         <image:image><image:loc>https://media.zenfs.com/en/prnewswire.com/a.jpg</image:loc>\
         </image:image></url>\
         <url><loc>https://finance.yahoo.com/b</loc><news:news><news:publication_date>\
         2026-11-16T09:00:00Z</news:publication_date><news:title>b</news:title></news:news>\
         <image:image><image:loc>https://media.zenfs.com/en//b.jpg</image:loc>\
         </image:image></url></urlset>"
        .to_vec();
    let direct = every_index_at(LOOKED_AT, &[(PRN_INDEX, rss_titles(&["--"]))])?;
    let checked = checker
        .check(&index, &direct, instant("2026-11-16T07:00:00Z"))
        .ok_or("the sitemap did not parse")?;
    assert_eq!(
        (
            checked.no_key,
            checked.other_key,
            checked.listed_directly,
            checked.unlisted.len(),
            checked.direct_titles
        ),
        (1, 0, 0, 1, 0)
    );
    Ok(())
}

fn rss_titles(titles: &[&str]) -> Vec<u8> {
    let items: String = titles
        .iter()
        .map(|title| format!("<item><title>{title}</title></item>"))
        .collect();
    format!("<rss version=\"2.0\"><channel>{items}</channel></rss>").into_bytes()
}

// ---- transport ----

/// Owner decision 1, 2026-11-17: each host its own 10 s queue, and no per-IP queue across hosts.
#[test]
fn the_profile_paces_each_host_at_one_request_per_ten_seconds_and_no_shared_ip_queue() -> Outcome {
    let profile = provider()?.transport();
    let ten = std::time::Duration::from_secs(10);
    assert_eq!(
        (profile.budget.min_interval, profile.budget.per_host),
        (None, Some(ten))
    );
    assert_eq!(
        profile.user_agent,
        HeaderValue::FromCredential(CredentialRef::new(CONTACT_CREDENTIAL))
    );
    Ok(())
}

/// Section 11.2: the index url's path with its query, then the rule's `path_prefix`, per admitted
/// row; a candidate row asks nothing.
#[test]
fn the_admitted_paths_are_each_admitted_index_with_its_query_and_its_rule_prefix() -> Outcome {
    let paths = OpenWebFeed::open(
        &hosts_fixture("one_admitted_one_candidate_index_query.json"),
        &rules_path(),
        CONTACT,
    )?
    .admitted_paths();
    let expected = ["/rssfeed/?lang=en", "/news-release/"].map(|path| AdmittedPath {
        host: GNW_HOST.to_owned(),
        path: path.to_owned(),
    });
    assert_eq!(paths, expected.to_vec());
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
            rule: "no_such_rule".to_owned()
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

/// Owner decision 4 (2026-11-17): robots.txt is matched against the index target as written.
#[test]
fn an_index_target_holding_a_percent_a_non_ascii_or_a_control_byte_is_refused() {
    let read = [
        ("index_target_percent.json", "/rssfeed/?page=%31"),
        ("index_target_non_ascii.json", "/rssfeed/ä"),
        ("index_target_control.json", "/rssfeed/\u{7f}"),
    ]
    .map(|(name, target)| {
        let path = hosts_fixture(name);
        let refused = Refusal::IndexTargetNotPlain {
            path: path.clone(),
            host: GNW_HOST.to_owned(),
            target: target.to_owned(),
        };
        (refusal_of(&path, &rules_path(), CONTACT), Some(refused))
    });
    let (got, wanted): (Vec<_>, Vec<_>) = read.into_iter().unzip();
    assert_eq!(got, wanted);
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

/// Section 11.1: an admitted host's robots.txt is pinned, so a host nobody fetched robots for is
/// not admitted. A candidate row may pin nothing (`only_admitted_hosts_are_planned` reads one).
#[test]
fn an_admitted_hosts_row_without_a_robots_pin_is_refused() {
    let path = hosts_fixture("no_robots_pin.json");
    assert_eq!(
        refusal_of(&path, &rules_path(), CONTACT),
        Some(Refusal::HostsUnparseable {
            path,
            why: format!("{GNW_HOST} is admitted and pins no robots")
        })
    );
}

#[test]
fn a_robots_pin_that_is_neither_64_lowercase_hex_nor_absent_is_refused() -> Outcome {
    let refused: Vec<bool> = [
        "robots_pin_upper_case.json",
        "robots_pin_short.json",
        "robots_pin_null.json",
    ]
    .iter()
    .map(|name| {
        matches!(
            refusal_of(&hosts_fixture(name), &rules_path(), CONTACT),
            Some(Refusal::HostsUnparseable { why, .. }) if why.contains("pins robots to")
        )
    })
    .collect();
    assert_eq!(refused, [true; 3]);
    let absent = OpenWebFeed::open(
        &hosts_fixture("robots_pin_absent.json"),
        &rules_path(),
        CONTACT,
    )?;
    assert_eq!(
        absent
            .host_pins()
            .first()
            .and_then(|row| row.robots.clone()),
        Some(RobotsPin::Absent)
    );
    Ok(())
}

/// Each pin is its committed robots body's sha256 (`newsvault`'s `tests/robots_refresh.rs`), or
/// `absent` for the Fed's 404. Mutations 2026-11-16 and 2026-11-17, one hex digit of the PRN, CNW
/// and Yahoo pins changed, and the Fed pin a digest: each red.
#[test]
fn the_committed_hosts_file_pins_the_committed_robots_body() -> Outcome {
    let pin = |body: Vec<u8>| Some(RobotsPin::Sha256(FileDigest::of(&body).hex().to_owned()));
    assert_eq!(
        provider()?.host_pins(),
        vec![
            HostPin {
                host: GNW_HOST.to_owned(),
                admitted: true,
                robots: pin(gnw("robots_rss_globenewswire_com.txt")),
            },
            HostPin {
                host: PRN_HOST.to_owned(),
                admitted: true,
                robots: pin(prn("robots_www_prnewswire_com.txt")),
            },
            HostPin {
                host: CNW_HOST.to_owned(),
                admitted: true,
                robots: pin(cnw("robots_www_newswire_ca.txt")),
            },
            HostPin {
                host: FED_HOST.to_owned(),
                admitted: true,
                robots: Some(RobotsPin::Absent),
            },
            HostPin {
                host: YAHOO_HOST.to_owned(),
                admitted: true,
                robots: pin(yahoo("robots_finance_yahoo_com.txt")),
            },
        ]
    );
    Ok(())
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

/// Owner decision 4 (2026-11-17), as for an index target; also a `#`, which Google's parser cuts
/// from a url and an index target never holds (`host_and_target` cuts it).
#[test]
fn a_path_filter_holding_a_percent_a_non_ascii_or_a_control_byte_is_refused() {
    let read = [
        ("rules_path_prefix_percent.json", "/news-r%65lease/"),
        ("rules_path_prefix_non_ascii.json", "/news-release/ä/"),
        ("rules_path_prefix_control.json", "/news-release/\t"),
        ("rules_path_prefix_fragment.json", "/news-release/#x"),
    ]
    .map(|(name, prefix)| {
        let path = rules_fixture(name);
        let refused = Refusal::PathPrefixNotPlain {
            path: path.clone(),
            rule: "gnw".to_owned(),
            prefix: prefix.to_owned(),
        };
        (refusal_of(&hosts_path(), &path, CONTACT), Some(refused))
    });
    let (got, wanted): (Vec<_>, Vec<_>) = read.into_iter().unzip();
    assert_eq!(got, wanted);
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
    let prn_article = prn_article_request(&provider)?;
    let payloads = [
        (index_request(&provider)?, blob(gnw(REAL_INDEX))),
        (article.clone(), blob(gnw(REAL_ARTICLE))),
        (synthetic, blob(gnw(SYNTHETIC_PAGE))),
        (article, blob(gnw(NO_CONTAINER_PAGE))),
        (prn_index_request(&provider)?, blob(prn(PRN_REAL_INDEX))),
        (prn_article.clone(), blob(prn(PRN_ARTICLE))),
        (prn_follow_up(&provider, 2)?, blob(prn(PRN_SECOND_ARTICLE))),
        (prn_article, blob(prn(PRN_NO_RELEASE_BODY_PAGE))),
        (cnw_index_request(&provider)?, blob(cnw(CNW_REAL_INDEX))),
        (cnw_follow_up(&provider, 0)?, blob(cnw(CNW_ARTICLE))),
        (cnw_follow_up(&provider, 1)?, blob(cnw(CNW_SECOND_ARTICLE))),
        (
            cnw_follow_up(&provider, 2)?,
            blob(cnw(CNW_NO_RELEASE_BODY_PAGE)),
        ),
        (fed_index_request(&provider)?, blob(fed(FED_REAL_INDEX))),
        (fed_follow_up(&provider, 0)?, blob(fed(FED_ARTICLE))),
        (
            fed_follow_up(&provider, 1)?,
            blob(fed(FED_NO_ARTICLE_BLOCK_PAGE)),
        ),
        (fed_follow_up(&provider, 2)?, blob(fed(FED_SECOND_ARTICLE))),
        (
            yahoo_index_request(&provider)?,
            blob(yahoo(YAHOO_REAL_INDEX)),
        ),
        (yahoo_follow_up(&provider, 0)?, blob(yahoo(YAHOO_ARTICLE))),
        (
            yahoo_follow_up(&provider, 1)?,
            blob(yahoo(YAHOO_NO_ARTICLE_BODY_PAGE)),
        ),
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
    assert_eq!(produced.len(), 9);
    assert!(
        produced.iter().all(|precision| *precision == ceiling),
        "{produced:?} under {ceiling:?}"
    );
    Ok(())
}

fn prn_body(page: &str) -> Result<String, Box<dyn std::error::Error>> {
    let provider = provider()?;
    let item = only_item(&provider.extract(&prn_article_request(&provider)?, &blob(prn(page))))?;
    Ok(item.body.ok_or("no body")?.text)
}

/// Decision 8 (section 11.1): `p, li`, the outermost match only, so an `li` holding a `p` and a
/// nested list are each one line and no text is read twice; an inline `b` adds no space.
#[test]
fn a_list_item_holding_a_paragraph_or_a_nested_list_is_one_line_read_once() -> Outcome {
    assert_eq!(
        prn_body(PRN_LISTS_PAGE)?,
        "First summary point\nAmbre Example +45 0000 0000\nOpening paragraph.\n\
         Outer item inner one inner two\nSecond outer item\nBoldtail block after"
    );
    Ok(())
}

/// P1 and decision r (section 11.1): neither Cloudflare shape nor an address reaches the body; a
/// `mailto:` link's name, a phone number, a label and a web link do.
#[test]
fn a_cloudflare_placeholder_and_an_address_never_reach_the_body_and_a_mailto_name_does() -> Outcome
{
    assert_eq!(
        prn_body(PRN_ADDRESSES_PAGE)?,
        "Media: today\nInvestors:\nPress: or , see example.com.\n\
         Contact Jane Example, +45 0000 0000, E-mail:\nDesk: daily"
    );
    Ok(())
}

/// Decision r's measured limit (section 11.1): inline elements join with no space, so the
/// regex reads letters joined to an address as part of it; with spaces, the words stay.
#[test]
fn letters_joined_to_an_address_with_no_space_are_removed_with_it() -> Outcome {
    assert_eq!(
        prn_body(PRN_JOINED_ADDRESS_PAGE)?,
        "Media\n: +1 000 000 0000\nMedia Contact Tel: +1 000 000 0000"
    );
    Ok(())
}

fn gnw_body(page: &str) -> Result<String, Box<dyn std::error::Error>> {
    let provider = provider()?;
    let item = only_item(&provider.extract(&article_request(&provider)?, &blob(gnw(page))))?;
    Ok(item.body.ok_or("no body")?.text)
}

/// Decision r (section 11.1) on trimmed real GNW pages: an address as a Tracker link's text and as
/// plain text is removed; the name, the phone number and the `E-mail:` label stay.
#[test]
fn an_address_in_link_text_or_plain_text_leaves_a_globenewswire_body_and_the_contact_stays(
) -> Outcome {
    let linked = gnw_body(GNW_LINKED_ADDRESS_PAGE)?;
    let plain = gnw_body(GNW_PLAIN_ADDRESS_PAGE)?;
    assert_eq!(
        (linked.lines().nth(3), linked.lines().nth(4)),
        (
            Some("Ann Example Philips Global External Relations Tel.: +31 0 0000 0000 E-mail:"),
            Some("Bo Example Philips Investor Relations Tel.: +31 00 00 00000 E-mail:")
        ),
        "{linked}"
    );
    assert_eq!(
        plain.lines().last(),
        Some("Niu Technologies E-mail:"),
        "{plain}"
    );
    assert!(
        !linked.contains('@') && !plain.contains('@'),
        "{linked}\n{plain}"
    );
    Ok(())
}

/// P2 (section 11.1): one line per `tr` in document order between paragraphs, non-empty cells
/// joined by ` | `, a cell's `p` read once as part of its row, a nested table flattened into its
/// outer cell, a caption one line, and a row of empty or no-break-space cells dropped.
#[test]
fn a_table_is_read_one_line_per_row_and_a_nested_table_inside_its_outer_cell() -> Outcome {
    assert_eq!(
        prn_body(PRN_TABLES_PAGE)?,
        "Before the tables.\nResults (unaudited)\n(in millions) | Q3 2026 | Q3 2025\nRevenue | 1,234 | 1,100\nIR\n\
         Between the tables.\nAmbre Example +45 0000 0000 | Liz Example (US) +1 000 000 0000\n\
         Outer inner a inner b inner c | right\nAfter the tables.\nListed:\nCost | 4\nafter the listed table"
    );
    Ok(())
}

/// P6 (section 11.1): GNW reads lists, tables, addresses and a `figcaption` through PRN's path;
/// only the container differs.
#[test]
fn a_globenewswire_page_reads_lists_tables_and_addresses_through_the_same_path() -> Outcome {
    let provider = provider()?;
    let page = blob(gnw(GNW_ONE_PATH_PAGE));
    let item = only_item(&provider.extract(&article_request(&provider)?, &page))?;
    assert_eq!(
        item.body.map(|body| body.text).as_deref(),
        Some("Opening paragraph.\nPoint one\nPoint two\nContact | or\nA caption on GNW")
    );
    Ok(())
}

/// `figcaption` is a paragraph on both hosts (§11.1): each caption is one line in document order,
/// between the lead and the paragraph after the gallery; the `Continue Reading` link is not read.
#[test]
fn a_figure_caption_is_one_line_in_document_order_on_a_pr_newswire_page() -> Outcome {
    let body = prn_body(PRN_FIGURE_PAGE)?;
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(
        (lines.len(), lines.get(2..4)),
        (
            5,
            Some(&["Stark Future HQ - Barcelona", "Stark VARG SM_Lineup"][..])
        ),
        "{body}"
    );
    assert!(
        lines[1].starts_with("BARCELONA, Spain, Nov. 16, 2026"),
        "{body}"
    );
    assert!(lines[4].starts_with("Founded in 2020"), "{body}");
    assert!(!body.contains("Continue Reading"), "{body}");
    Ok(())
}

/// Every request the feed builds names the version its answer is read under (section 2).
#[test]
fn every_planned_request_and_follow_up_carries_the_extraction_version() -> Outcome {
    let provider = provider()?;
    let version = Some(provider.extract_version());
    let index = index_request(&provider)?;
    let document = article_request(&provider)?;
    assert_eq!(
        (
            index.extract_version.as_ref(),
            document.extract_version.as_ref()
        ),
        (version, version)
    );
    Ok(())
}

/// The committed golden file as JSON, and the same shape computed from the fixtures now.
fn golden_bodies() -> Result<(serde_json::Value, serde_json::Value), Box<dyn std::error::Error>> {
    let provider = provider()?;
    let mut bodies = serde_json::Map::new();
    let gnw_request = article_request(&provider)?;
    let prn_request = prn_article_request(&provider)?;
    let cnw_request = cnw_follow_up(&provider, 0)?;
    let fed_request = fed_follow_up(&provider, 0)?;
    let yahoo_request = yahoo_follow_up(&provider, 0)?;
    let pages = GOLDEN_GNW_PAGES
        .iter()
        .map(|page| ("gnw", *page, gnw(page), &gnw_request))
        .chain(
            GOLDEN_PRN_PAGES
                .iter()
                .map(|page| ("prn", *page, prn(page), &prn_request)),
        )
        .chain(
            GOLDEN_CNW_PAGES
                .iter()
                .map(|page| ("cnw", *page, cnw(page), &cnw_request)),
        )
        .chain(
            GOLDEN_FED_PAGES
                .iter()
                .map(|page| ("fed", *page, fed(page), &fed_request)),
        )
        .chain(
            GOLDEN_YAHOO_PAGES
                .iter()
                .map(|page| ("yahoo", *page, yahoo(page), &yahoo_request)),
        );
    for (host, page, bytes, request) in pages {
        let item = only_item(&provider.extract(request, &blob(bytes)))?;
        let text = item.body.ok_or("no body")?.text;
        bodies.insert(
            format!("{host}/{page}"),
            FileDigest::of(text.as_bytes()).hex().into(),
        );
    }
    let computed = serde_json::json!({
        "extract_version": provider.extract_version().hex(),
        "bodies": bodies,
    });
    let committed = serde_json::from_slice(&std::fs::read(fixtures().join(GOLDEN_BODIES))?)?;
    Ok((committed, computed))
}

/// P5 (section 11.1): a change to the extraction code, the rules file or any body moves this
/// file's `extract_version` or a digest, and stays red until the file is rewritten by hand.
#[test]
fn every_golden_page_extracts_to_its_committed_body_under_the_committed_version() -> Outcome {
    let (committed, computed) = golden_bodies()?;
    assert_eq!(
        committed,
        computed,
        "rewrite tests/fixtures/{GOLDEN_BODIES} if the change is meant:\n{}",
        serde_json::to_string_pretty(&computed)?
    );
    Ok(())
}
