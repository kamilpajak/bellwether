//! Step 2 of the successor memo: the three-day probe at news vendors (Rust port of the former
//! `probe.py`). It answers four questions no documentation can, because they depend on OUR broad
//! query rather than on the vendor's feature list:
//!
//!   Q1 VOLUME     how many articles a day does a BROAD query return? This is the whole cost of
//!                 history. The query is deliberately NOT narrowed to our bracket - a vendor-side
//!                 filter would become an unauditable part of the measurement, the same reason a
//!                 market-cap condition never goes into a prompt.
//!   Q2 STABILITY  does the same past-day query, repeated later, return the same set? One run
//!                 cannot answer this; this run records a digest of the id set, and a later run
//!                 with --compare answers it.
//!   Q3 BODY       a full article body, or a snippet? Measured as the length distribution of
//!                 whatever text field the vendor provides.
//!   Q4 LAG        the distribution of (index time - publication time), the input to R11.
//!
//! Scratchpad code on purpose, and a STANDALONE crate (its own workspace): a probe talks to a
//! vendor URL directly, which production code here may not, so it keeps its own HTTP client
//! outside the workspace's one-client invariant (repo_invariants::clients). If a vendor is adopted
//! its calls move behind news-provider's canonical client; this stays as the record of why.
//!
//! Reads no outcome and spends only vendor quota.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, NaiveDateTime};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const OUT: &str = "probes/vendor_probe/vendor_probe_results.json";
/// A day needing more than 1,200 articles answers Q1 already.
const PAGE_CAP: usize = 12;
/// Measured 2026-11-14: 9 Webz.io News API requests cost 4.59 USD (~0.51 each). One page already
/// gives Q1 (`total_results`) and the Q3/Q4 samples, so deeper pages buy nothing at that price.
const WEBZ_PAGE_CAP: usize = 1;
const TIMEOUT: Duration = Duration::from_secs(60);
const KEY_NAMES: [&str; 4] = [
    "NEWSAPI_AI_KEY",
    "NEWSCATCHER_API_KEY",
    "WEBZ_API_KEY",
    "PERIGON_API_KEY",
];

/// Replace any configured key found in `text`. Used before anything is printed or written, because
/// this script's output lands in a PUBLIC repository and an error echo can carry a key.
fn scrub(text: &str) -> String {
    let mut out = text.to_string();
    for name in KEY_NAMES {
        if let Ok(v) = env::var(name) {
            if v.len() >= 8 {
                out = out.replace(&v, &format!("<{name}>"));
            }
        }
    }
    out
}

/// Missing input aborts; an empty fallback is forbidden (R6).
fn need(name: &str) -> Result<String, String> {
    match env::var(name) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(format!(
            "missing key {name} in the environment.\n  \
             NEWSAPI_AI_KEY      -> free plan at newsapi.ai (2000 searches)\n  \
             NEWSCATCHER_API_KEY -> free plan at newscatcherapi.com\n  \
             WEBZ_API_KEY        -> api.webz.io token"
        )),
    }
}

fn short(text: &str) -> String {
    text.chars().take(200).collect()
}

/// A ureq error reduced to a scrubbed, short string. The full message can carry the request URL,
/// body or a response echo, so it is scrubbed before it can reach the results file.
fn err_text(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            scrub(&format!("http {code}: {}", short(&body)))
        }
        ureq::Error::Transport(t) => scrub(&format!("transport: {t}")),
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(TIMEOUT).build()
}

fn str_at(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::to_string)
}

fn body_len(v: &Value, key: &str) -> usize {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.chars().count())
        .unwrap_or(0)
}

/// Parse a vendor timestamp into epoch seconds. Vendors differ: NewsAPI.ai and Webz give RFC3339,
/// NewsCatcher gives "YYYY-MM-DD HH:MM:SS" (a space, no zone, UTC). Returns None if none fit.
fn parse_ts(s: &str) -> Option<i64> {
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.timestamp());
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y/%m/%d %H:%M:%S",
    ] {
        if let Ok(t) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(t.and_utc().timestamp());
        }
    }
    None
}

/// One normalised article: only the four fields every question needs.
struct Item {
    id: String,
    published: Option<String>,
    indexed: Option<String>,
    body_len: usize,
}

struct VendorResult {
    vendor: &'static str,
    day: String,
    pages: usize,
    note: Option<String>,
    items: Vec<Item>,
}

type Probe = fn(&str) -> Result<VendorResult, String>;

// --------------------------------------------------------------------------- vendors

/// Event Registry / newsapi.ai. Broad query: English news for one UTC day.
fn probe_newsapi_ai(day: &str) -> Result<VendorResult, String> {
    let key = need("NEWSAPI_AI_KEY")?;
    // The key goes in the POST body, never the URL: ureq (like httpx) can put the request URL into
    // an error, and this script writes errors into a PUBLIC results file; a key in the query string
    // would be published by one 401. Requests stay sequential (documented 5-simultaneous limit).
    let url = "https://eventregistry.org/api/v1/article/getArticles";
    let a = agent();
    let mut raw: Vec<Value> = Vec::new();
    let mut pages = 0;
    let mut note = None;
    for page in 1..=PAGE_CAP {
        let body = json!({
            "apiKey": key,
            "resultType": "articles",
            "lang": "eng",
            "dateStart": day,
            "dateEnd": day,
            "articlesPage": page,
            "articlesCount": 100,
            "articlesSortBy": "date",
            "isDuplicateFilter": "keepAll", // their dedup is THEIR editorial call
            "includeArticleBody": true,
        });
        let resp = a.post(url).send_json(body).map_err(err_text)?;
        let j: Value = resp.into_json().map_err(|e| e.to_string())?;
        let block = j.get("articles").cloned().unwrap_or(Value::Null);
        let got = block
            .get("results")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        pages += 1;
        if let Some(total) = block.get("totalResults") {
            note = Some(format!("totalResults={total}"));
        }
        let n = got.len();
        raw.extend(got);
        if n < 100 {
            break;
        }
    }
    let items = raw
        .iter()
        .map(|a| Item {
            id: str_at(a, "uri")
                .or_else(|| str_at(a, "url"))
                .unwrap_or_default(),
            published: str_at(a, "dateTimePub").or_else(|| str_at(a, "dateTime")),
            indexed: str_at(a, "dateTime"),
            body_len: body_len(a, "body"),
        })
        .collect();
    Ok(VendorResult {
        vendor: "newsapi.ai",
        day: day.to_string(),
        pages,
        note,
        items,
    })
}

/// NewsCatcher legacy "News API" (POST https://api.newscatcherapi.com/news-api/articles, header
/// `x-api-key`). Diagnosed live 2026-11-14: this key authenticates on THIS endpoint and is rejected
/// by the v3 host (v3-api.newscatcherapi.com, 401). Three limits of this endpoint/plan, each a
/// finding rather than a bug: it has NO `by_parse_date` (no index-time filter, so Q4 has no index
/// timestamp); `q` rejects a match-all (a bare `*` is refused and stopwords are stripped), so the
/// query is a broad set of common content terms rather than "everything"; and this plan is
/// COUNT-ONLY - it returns `total_hits` (so Q1 volume is measurable) but an EMPTY `articles` array,
/// so Q3 (body) and Q4 (lag) and the section-6 index filter all need a higher plan.
fn probe_newscatcher(day: &str) -> Result<VendorResult, String> {
    let key = need("NEWSCATCHER_API_KEY")?;
    let url = "https://api.newscatcherapi.com/news-api/articles";
    let to = NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .map_err(|e| e.to_string())?
        .succ_opt()
        .ok_or("date overflow")?
        .format("%Y-%m-%d")
        .to_string();
    let a = agent();
    let mut raw: Vec<Value> = Vec::new();
    let mut pages = 0;
    let mut total: Option<Value> = None;
    for page in 1..=PAGE_CAP.min(10) {
        // page * 100 <= 1000; pace to stay under the ~1 req/s concurrency limit (429 otherwise)
        sleep(Duration::from_millis(1100));
        let body = json!({
            "q": "news OR market OR company OR people OR year OR world OR government OR time",
            "languages": "en",
            "from_date": day,
            "to_date": to,
            "sort_by": "published_at",
            "page": page,
            "page_size": 100,
        });
        let send = a.post(url).set("x-api-key", &key).send_json(body);
        let resp = match send {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                if pages == 0 {
                    let b = r.into_string().unwrap_or_default();
                    return Ok(VendorResult {
                        vendor: "newscatcher",
                        day: day.to_string(),
                        pages,
                        note: Some(scrub(&format!("http {code}: {}", short(&b)))),
                        items: Vec::new(),
                    });
                }
                break;
            }
            Err(e) => return Err(err_text(e)),
        };
        let j: Value = resp.into_json().map_err(|e| e.to_string())?;
        let got = j
            .get("articles")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        if total.is_none() {
            total = j.get("total_hits").cloned();
        }
        let total_pages = j.get("total_pages").and_then(|x| x.as_u64()).unwrap_or(0);
        pages += 1;
        let n = got.len();
        raw.extend(got);
        if n < 100 || (total_pages != 0 && page as u64 >= total_pages) {
            break;
        }
    }
    let note = Some(format!(
        "total_hits={} legacy news-api; count-only plan (articles empty); no by_parse_date",
        total.unwrap_or(Value::Null)
    ));
    let items = raw
        .iter()
        .map(|a| Item {
            id: str_at(a, "_id")
                .or_else(|| str_at(a, "id"))
                .or_else(|| str_at(a, "link"))
                .unwrap_or_default(),
            published: str_at(a, "published_date"),
            indexed: None, // this endpoint exposes no index/parse timestamp
            body_len: {
                let s = body_len(a, "summary");
                if s > 0 {
                    s
                } else {
                    body_len(a, "excerpt")
                }
            },
        })
        .collect();
    Ok(VendorResult {
        vendor: "newscatcher",
        day: day.to_string(),
        pages,
        note,
        items,
    })
}

/// Webz.io News API (api.webz.io/api/news). Added 2026-11-14 (see the pre-registration's dated
/// amendment). The token goes in an Authorization: Bearer header, never the URL (public results
/// file). `ts` filters by CRAWL time; a window older than 31 days needs allowNewsHistory=true and
/// the default ascending crawl sort. A 2026-07 day is a 3-6 month window (3 credits/request) within
/// the ~1-year news history; the deeper archive is a sales-gated ordered dataset, not this endpoint.
/// An empty or 4xx result IS the Q1/archive-access answer for Webz (R6), not a crash.
fn probe_webz(day: &str) -> Result<VendorResult, String> {
    let key = need("WEBZ_API_KEY")?;
    let start = NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .map_err(|e| e.to_string())?
        .and_hms_opt(0, 0, 0)
        .ok_or("bad time")?
        .and_utc()
        .timestamp();
    let ts_ms = start * 1000;
    let base = "https://api.webz.io";
    let bearer = format!("Bearer {key}");
    let a = agent();
    let mut raw: Vec<Value> = Vec::new();
    let mut pages = 0;
    let mut total: Option<Value> = None;
    let mut next: Option<String> = None;
    for _ in 0..WEBZ_PAGE_CAP {
        sleep(Duration::from_millis(1100)); // Webz documents ~1 request/second
        let send = match &next {
            None => a
                .get(&format!("{base}/api/news"))
                .set("Authorization", &bearer)
                .query("q", "language:english")
                .query("ts", &ts_ms.to_string())
                .query("allowNewsHistory", "true")
                .query("size", "100")
                .query("sort", "crawled")
                .query("order", "asc")
                .call(),
            Some(n) => a
                .get(&format!("{base}{n}"))
                .set("Authorization", &bearer)
                .call(),
        };
        let j: Value = match send {
            Ok(r) => r.into_json().map_err(|e| e.to_string())?,
            // A 4xx/429 on the history window IS the archive-access finding, not a crash.
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                return Ok(VendorResult {
                    vendor: "webz.io",
                    day: day.to_string(),
                    pages,
                    note: Some(scrub(&format!("http {code}: {}", short(&body)))),
                    items: Vec::new(),
                });
            }
            Err(e) => return Err(err_text(e)),
        };
        let posts = j
            .get("posts")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        if total.is_none() {
            total = j.get("total_results").cloned();
        }
        pages += 1;
        // Keep every post in the ts-window (crawl >= day start); the note records that this is
        // "from the day onward", not one calendar day, because the News API has no end bound.
        raw.extend(posts.iter().cloned());
        let more = j
            .get("more_results_available")
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        next = j.get("next").and_then(|x| x.as_str()).map(str::to_string);
        if posts.is_empty() || more == 0 || next.is_none() {
            break;
        }
    }
    let note = total.map(|t| {
        format!(
            "total_results={t} ts={ts_ms} allowNewsHistory crawl_sort(asc) window_3-6mo=3cr/req"
        )
    });
    let items = raw
        .iter()
        .map(|p| Item {
            id: str_at(p, "uuid")
                .or_else(|| str_at(p, "url"))
                .unwrap_or_default(),
            published: str_at(p, "published"),
            indexed: str_at(p, "crawled"),
            body_len: body_len(p, "text"),
        })
        .collect();
    Ok(VendorResult {
        vendor: "webz.io",
        day: day.to_string(),
        pages,
        note,
        items,
    })
}

/// Perigon (GET https://api.perigon.io/v1/articles/all, header `x-api-key`). Added 2026-11-14 (see
/// the pre-registration amendment). Broad English news for one publication day (`from`/`to`, where
/// `to` is exclusive). It exposes BOTH `pubDate` (publication) and `addDate` (first ingestion), so
/// Q4 lag is measurable directly - the point-in-time feature memo section 6 valued - and
/// `addDateFrom`/`addDateTo` are an index-time FILTER if the vendor is adopted. Historical depth may
/// be a paid tier, so a 2026-07 query may return nothing on this key, which is itself the Q1 /
/// archive-access finding. `numResults` is the volume (it caps at 10000). The response can carry raw
/// control characters inside article bodies (invalid strict JSON), so it is sanitised before parsing.
fn probe_perigon(day: &str) -> Result<VendorResult, String> {
    let key = need("PERIGON_API_KEY")?;
    let to = NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .map_err(|e| e.to_string())?
        .succ_opt()
        .ok_or("date overflow")?
        .format("%Y-%m-%d")
        .to_string();
    let a = agent();
    // Per-key request quota. One request suffices: numResults is Q1, and its 100 articles are the Q3
    // and Q4 samples.
    sleep(Duration::from_millis(1100));
    let resp = a
        .get("https://api.perigon.io/v1/articles/all")
        .set("x-api-key", &key)
        .query("language", "en")
        .query("from", day)
        .query("to", &to)
        .query("sortBy", "date")
        .query("size", "100")
        .call();
    let text = match resp {
        Ok(r) => r.into_string().map_err(|e| e.to_string())?,
        Err(ureq::Error::Status(code, r)) => {
            let b = r.into_string().unwrap_or_default();
            return Ok(VendorResult {
                vendor: "perigon",
                day: day.to_string(),
                pages: 0,
                note: Some(scrub(&format!("http {code}: {}", short(&b)))),
                items: Vec::new(),
            });
        }
        Err(e) => return Err(err_text(e)),
    };
    // Perigon emits raw control characters inside article bodies, which is invalid strict JSON; the
    // response is a single line, so replacing every control character with a space fixes it without
    // touching structure.
    let clean: String = text
        .chars()
        .map(|c| if (c as u32) < 0x20 { ' ' } else { c })
        .collect();
    let j: Value = serde_json::from_str(&clean).map_err(|e| e.to_string())?;
    let total = j.get("numResults").cloned();
    let got = j
        .get("articles")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let note = Some(format!(
        "numResults={} (caps at 10000); pubDate+addDate exposed (index-time); addDateFrom/To filter available",
        total.unwrap_or(Value::Null)
    ));
    let items = got
        .iter()
        .map(|p| Item {
            id: str_at(p, "articleId")
                .or_else(|| str_at(p, "url"))
                .unwrap_or_default(),
            published: str_at(p, "pubDate"),
            indexed: str_at(p, "addDate"),
            body_len: body_len(p, "content"),
        })
        .collect();
    Ok(VendorResult {
        vendor: "perigon",
        day: day.to_string(),
        pages: 1,
        note,
        items,
    })
}

fn vendors() -> Vec<(&'static str, Probe)> {
    vec![
        ("newsapi.ai", probe_newsapi_ai),
        ("newscatcher", probe_newscatcher),
        ("webz.io", probe_webz),
        ("perigon", probe_perigon),
    ]
}

// --------------------------------------------------------------------------- readout

fn quantile(xs: &[i64], f: f64) -> Option<i64> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_unstable();
    let idx = ((f * v.len() as f64) as usize).min(v.len() - 1);
    Some(v[idx])
}

fn median_usize(xs: &[usize]) -> i64 {
    if xs.is_empty() {
        return 0;
    }
    let mut v = xs.to_vec();
    v.sort_unstable();
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2] as i64
    } else {
        ((v[n / 2 - 1] + v[n / 2]) / 2) as i64
    }
}

/// The per-(vendor, day) answer, as a JSON object to be written and compared.
fn summarise(res: &VendorResult) -> Value {
    let ids: BTreeSet<&str> = res
        .items
        .iter()
        .filter(|i| !i.id.is_empty())
        .map(|i| i.id.as_str())
        .collect();
    let bodies: Vec<usize> = res.items.iter().map(|i| i.body_len).collect();
    let mut lags: Vec<i64> = Vec::new();
    for i in &res.items {
        if let (Some(p), Some(x)) = (&i.published, &i.indexed) {
            if let (Some(pt), Some(xt)) = (parse_ts(p), parse_ts(x)) {
                lags.push(xt - pt);
            }
        }
    }
    // None, not the digest of "": an empty day would otherwise hash identically for every vendor
    // and date, and two such days would compare as "the same set".
    let id_digest = if ids.is_empty() {
        Value::Null
    } else {
        let joined = ids.iter().copied().collect::<Vec<_>>().join("\n");
        let mut h = Sha256::new();
        h.update(joined.as_bytes());
        Value::String(format!("{:x}", h.finalize()))
    };
    let empty = bodies.iter().filter(|b| **b == 0).count();
    let neg = lags.iter().filter(|l| **l < 0).count();
    json!({
        "vendor": res.vendor,
        "day": res.day,
        "pages": res.pages,
        "note": res.note,
        "articles": res.items.len(),
        "distinct_ids": ids.len(),
        "id_digest": id_digest,
        "paging_exhausted": res.pages < PAGE_CAP,
        "body_len_median": median_usize(&bodies),
        "body_len_p90": quantile(&bodies.iter().map(|b| *b as i64).collect::<Vec<_>>(), 0.9),
        "body_empty_share": round4(empty as f64 / bodies.len().max(1) as f64),
        "lag_measured_for": lags.len(),
        "lag_seconds_median": quantile(&lags, 0.5),
        "lag_seconds_p90": quantile(&lags, 0.9),
        "lag_negative_share": round4(neg as f64 / lags.len().max(1) as f64),
    })
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

struct Args {
    days: Vec<String>,
    vendors: Vec<String>,
    compare: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let argv: Vec<String> = env::args().skip(1).collect();
    let known: Vec<String> = vendors().iter().map(|(n, _)| n.to_string()).collect();
    let mut days = Vec::new();
    let mut vendors = known.clone();
    let mut compare = None;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--days" => {
                i += 1;
                vendors_list(&argv, &mut i, &mut days);
            }
            "--vendors" => {
                i += 1;
                let mut got = Vec::new();
                vendors_list(&argv, &mut i, &mut got);
                for v in &got {
                    if !known.contains(v) {
                        return Err(format!("unknown vendor {v}; choose from {known:?}"));
                    }
                }
                vendors = got;
            }
            "--compare" => {
                i += 1;
                compare = argv.get(i).cloned();
                i += 1;
            }
            other => return Err(format!("unexpected argument {other}")),
        }
    }
    if days.is_empty() {
        return Err("--days is required (UTC days, YYYY-MM-DD)".to_string());
    }
    Ok(Args {
        days,
        vendors,
        compare,
    })
}

/// Consume the run of non-flag tokens following a list flag into `out`.
fn vendors_list(argv: &[String], i: &mut usize, out: &mut Vec<String>) {
    while *i < argv.len() && !argv[*i].starts_with("--") {
        out.push(argv[*i].clone());
        *i += 1;
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    let table = vendors();
    let mut rows: Vec<Value> = Vec::new();
    for v in &args.vendors {
        let probe = table.iter().find(|(n, _)| n == v).map(|(_, f)| *f);
        let Some(probe) = probe else { continue };
        for day in &args.days {
            match probe(day) {
                Ok(res) => {
                    let s = summarise(&res);
                    println!(
                        "  {v} {day}: {} art., {} pages, body median {} chars, lag median {} s",
                        s["articles"], s["pages"], s["body_len_median"], s["lag_seconds_median"]
                    );
                    rows.push(s);
                }
                Err(e) => {
                    // The scrubbed, short reason only. A vendor error can carry the key; this file
                    // is committed in a public repository.
                    let detail = scrub(&e);
                    println!("  {v} {day}: FAILED {}", short(&detail));
                    rows.push(json!({"vendor": v, "day": day, "error": short(&detail)}));
                }
            }
        }
    }

    // Q2 is answered BEFORE the results are written: writing first would overwrite the baseline
    // that --compare reads, and a run compared against its own fresh output always reports "same".
    let mut baseline: Option<Value> = None;
    if let Some(cmp) = &args.compare {
        let cmp_abs = fs::canonicalize(cmp).unwrap_or_else(|_| cmp.into());
        let out_abs = fs::canonicalize(OUT).unwrap_or_else(|_| OUT.into());
        if cmp_abs == out_abs {
            eprintln!(
                "--compare points at this run's own output ({OUT}). Copy it to another name."
            );
            return ExitCode::from(2);
        }
        match fs::read_to_string(cmp) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(j) => baseline = Some(j),
                Err(e) => {
                    eprintln!("cannot parse baseline {cmp}: {e}");
                    return ExitCode::from(2);
                }
            },
            Err(_) => {
                eprintln!("no baseline file: {cmp}");
                return ExitCode::from(2);
            }
        }
    }

    let stamp = chrono::Utc::now().to_rfc3339();
    let out = json!({"fetched_at": stamp, "rows": rows});
    if let Err(e) = fs::write(OUT, serde_json::to_string_pretty(&out).unwrap_or_default()) {
        eprintln!("cannot write {OUT}: {e}");
        return ExitCode::from(2);
    }

    println!(
        "\n{:14} {:12} {:>6} {:>9} {:>11} {:>12} {:>11}",
        "vendor", "day", "art.", "exhaust", "body med.", "lag med.", "lag p90"
    );
    for r in &rows {
        if let Some(err) = r.get("error").and_then(|x| x.as_str()) {
            println!(
                "{:14} {:12} ERROR: {}",
                s(r, "vendor"),
                s(r, "day"),
                short(err)
            );
            continue;
        }
        println!(
            "{:14} {:12} {:>6} {:>9} {:>11} {:>12} {:>11}",
            s(r, "vendor"),
            s(r, "day"),
            r["articles"],
            r["paging_exhausted"],
            r["body_len_median"],
            r["lag_seconds_median"],
            r["lag_seconds_p90"]
        );
    }

    // Q1: cost of a year, from newsapi.ai's published token pricing.
    println!(
        "\nQ1 cost of a year of history (newsapi.ai pricing: 5 tokens/searched year,\n   \
         100 articles/search, overage $0.015/token):"
    );
    for r in &rows {
        let articles = r.get("articles").and_then(|x| x.as_u64()).unwrap_or(0);
        if r.get("error").is_some() || articles == 0 {
            continue;
        }
        let searches = articles.div_ceil(100);
        let tokens_year = searches * 5 * 365;
        println!(
            "   {:14} {}: {} searches/day -> {} tokens/year -> ${:.0}",
            s(r, "vendor"),
            s(r, "day"),
            searches,
            tokens_year,
            tokens_year as f64 * 0.015
        );
    }

    match baseline {
        Some(b) => {
            let old = b
                .get("rows")
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default();
            println!("\nQ2 archive stability (same set over time?):");
            for r in &rows {
                let key = (s(r, "vendor"), s(r, "day"));
                let prior = old.iter().find(|o| (s(o, "vendor"), s(o, "day")) == key);
                let Some(prior) = prior else { continue };
                if r.get("error").is_some() {
                    continue;
                }
                let a = prior.get("id_digest");
                let b = r.get("id_digest");
                if a == Some(&Value::Null) || b == Some(&Value::Null) {
                    println!(
                        "   {:14} {}: UNDECIDED (an empty day on one side)",
                        key.0, key.1
                    );
                } else if a == b {
                    println!("   {:14} {}: SAME set", key.0, key.1);
                } else {
                    println!("   {:14} {}: SET CHANGED", key.0, key.1);
                }
            }
        }
        None => {
            println!("\nQ2 stability: id digests saved. One run cannot decide this. Copy");
            println!("   {OUT} to another name, wait at least 7 days, and re-run with");
            println!("   --compare pointing at THAT copy (comparing against this run's own");
            println!("   output is refused).");
        }
    }
    println!("\nwrote {OUT}");
    ExitCode::SUCCESS
}

fn s(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}
