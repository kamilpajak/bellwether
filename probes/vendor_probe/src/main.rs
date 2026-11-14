//! The news-vendor probe (Rust port of the former `probe.py`). Its four questions (Q1 volume, Q2
//! stability, Q3 body, Q4 lag) and their decision rules are in `PREREGISTRATION.md` beside this crate.
//!
//! A standalone crate (its own `[workspace]`): `repo_invariants::clients` forbids an HTTP client in
//! any workspace member but news-provider, and a probe talks to vendor URLs directly.
//!
//! Reads no outcome and spends only vendor quota.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, NaiveDateTime};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const OUT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/vendor_probe_results.json");
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
const KEY_HINT: &str =
    "keys: NEWSAPI_AI_KEY (newsapi.ai), NEWSCATCHER_API_KEY (newscatcherapi.com), \
                        WEBZ_API_KEY (api.webz.io), PERIGON_API_KEY (perigon.io)";
/// Exit status for a usage or setup error; nothing is requested and nothing is written.
const EXIT_USAGE: u8 = 2;

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

fn key_present(name: &str) -> bool {
    env::var(name).is_ok_and(|v| !v.is_empty())
}

/// Missing input aborts; an empty fallback is forbidden (R6).
fn need(name: &str) -> Result<String, String> {
    match env::var(name) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(format!("missing key {name}; {KEY_HINT}")),
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

/// The UTC day after `day`, as YYYY-MM-DD.
fn next_day(day: &str) -> Result<String, String> {
    Ok(NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .map_err(|e| e.to_string())?
        .succ_opt()
        .ok_or("date overflow")?
        .format("%Y-%m-%d")
        .to_string())
}

/// Paging is exhausted when every reported result was fetched; with no reported total, the
/// adapter's own end-of-results signal decides.
fn exhausted(fetched: usize, total: Option<u64>, no_total_signal: bool) -> bool {
    match total {
        Some(t) => fetched as u64 >= t,
        None => no_total_signal,
    }
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
    /// The vendor-reported result count for the query: Q1's daily volume.
    total: Option<u64>,
    exhausted: bool,
    items: Vec<Item>,
}

type Probe = fn(&str) -> Result<VendorResult, String>;

// --------------------------------------------------------------------------- vendors

/// Event Registry / newsapi.ai. Broad query: English news for one UTC day.
fn probe_newsapi_ai(day: &str) -> Result<VendorResult, String> {
    let key = need("NEWSAPI_AI_KEY")?;
    // The key goes in the POST body, never the URL: ureq (like httpx) can put the request URL into
    // an error, and an error is written to the results file. Sequential: 5-simultaneous limit.
    let url = "https://eventregistry.org/api/v1/article/getArticles";
    let a = agent();
    let mut raw: Vec<Value> = Vec::new();
    let mut pages = 0;
    let mut total: Option<u64> = None;
    let mut last_short = false;
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
        if let Some(t) = block.get("totalResults").and_then(|x| x.as_u64()) {
            total = Some(t);
        }
        let n = got.len();
        raw.extend(got);
        last_short = n < 100;
        if last_short {
            break;
        }
    }
    let items = raw
        .iter()
        .map(|a| Item {
            id: str_at(a, "uri")
                .or_else(|| str_at(a, "url"))
                .unwrap_or_default(),
            // No fallback to `dateTime`: that is the index time, and lag would read 0 s.
            published: str_at(a, "dateTimePub"),
            indexed: str_at(a, "dateTime"),
            body_len: body_len(a, "body"),
        })
        .collect::<Vec<_>>();
    Ok(VendorResult {
        vendor: "newsapi.ai",
        day: day.to_string(),
        pages,
        note: total.map(|t| format!("totalResults={t}")),
        exhausted: exhausted(items.len(), total, last_short),
        total,
        items,
    })
}

/// NewsCatcher legacy News API (`x-api-key` header). Measured 2026-11-14: the key gets 401 from the v3
/// host; this endpoint has no `by_parse_date`, refuses a match-all `q`, and on this plan returns
/// `total_hits` with an empty `articles` array.
fn probe_newscatcher(day: &str) -> Result<VendorResult, String> {
    let key = need("NEWSCATCHER_API_KEY")?;
    let url = "https://api.newscatcherapi.com/news-api/articles";
    let to = next_day(day)?;
    let a = agent();
    let mut raw: Vec<Value> = Vec::new();
    let mut pages = 0;
    let mut total: Option<u64> = None;
    let mut no_more = false;
    let mut refused_deep = None;
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
        let resp = match a.post(url).set("x-api-key", &key).send_json(body) {
            Ok(r) => r,
            // The documented deep-page refusal (page * page_size > 1000) ends paging, unexhausted.
            Err(ureq::Error::Status(422, r)) if pages > 0 => {
                refused_deep = Some(err_text(ureq::Error::Status(422, r)));
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
            total = j.get("total_hits").and_then(|x| x.as_u64());
        }
        let total_pages = j.get("total_pages").and_then(|x| x.as_u64()).unwrap_or(0);
        pages += 1;
        let n = got.len();
        raw.extend(got);
        no_more = n < 100 || (total_pages != 0 && page as u64 >= total_pages);
        if no_more {
            break;
        }
    }
    let mut note = format!(
        "total_hits={} legacy news-api; count-only plan (articles empty); no by_parse_date",
        total.map_or("null".to_string(), |t| t.to_string())
    );
    if let Some(r) = &refused_deep {
        note.push_str(&format!("; page {} refused: {r}", pages + 1));
    }
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
        .collect::<Vec<_>>();
    Ok(VendorResult {
        vendor: "newscatcher",
        day: day.to_string(),
        pages,
        note: Some(note),
        exhausted: refused_deep.is_none() && exhausted(items.len(), total, no_more),
        total,
        items,
    })
}

/// Webz.io News API, token in a Bearer header. `ts` filters by CRAWL time with no end bound, and a
/// window older than 31 days needs `allowNewsHistory=true` (3 credits/request at 3-6 months).
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
    let mut total: Option<u64> = None;
    let mut more: Option<i64> = None;
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
        let j: Value = send
            .map_err(err_text)?
            .into_json()
            .map_err(|e| e.to_string())?;
        let posts = j
            .get("posts")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        if total.is_none() {
            total = j.get("total_results").and_then(|x| x.as_u64());
        }
        pages += 1;
        // Every post in the ts-window (crawl >= day start), not one calendar day: no end bound.
        raw.extend(posts.iter().cloned());
        more = j.get("more_results_available").and_then(|x| x.as_i64());
        next = j.get("next").and_then(|x| x.as_str()).map(str::to_string);
        if posts.is_empty() || more == Some(0) || next.is_none() {
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
        .collect::<Vec<_>>();
    let exhausted = match more {
        Some(m) => m == 0,
        None => exhausted(items.len(), total, false),
    };
    Ok(VendorResult {
        vendor: "webz.io",
        day: day.to_string(),
        pages,
        note,
        total,
        exhausted,
        items,
    })
}

/// Perigon `articles/all` (`x-api-key` header), one publication day, `to` exclusive; `pubDate` and
/// `addDate` make Q4 direct, `numResults` caps at 10000. Measured 2026-11-14: bodies carry raw
/// control characters (invalid strict JSON) in a one-line response, so they become spaces.
fn probe_perigon(day: &str) -> Result<VendorResult, String> {
    let key = need("PERIGON_API_KEY")?;
    let to = next_day(day)?;
    let a = agent();
    // Per-key request quota. One request suffices: numResults is Q1, and its 100 articles are the Q3
    // and Q4 samples.
    sleep(Duration::from_millis(1100));
    let text = a
        .get("https://api.perigon.io/v1/articles/all")
        .set("x-api-key", &key)
        .query("language", "en")
        .query("from", day)
        .query("to", &to)
        .query("sortBy", "date")
        .query("size", "100")
        .call()
        .map_err(err_text)?
        .into_string()
        .map_err(|e| e.to_string())?;
    let clean: String = text
        .chars()
        .map(|c| if (c as u32) < 0x20 { ' ' } else { c })
        .collect();
    let j: Value = serde_json::from_str(&clean).map_err(|e| e.to_string())?;
    let total = j.get("numResults").and_then(|x| x.as_u64());
    let got = j
        .get("articles")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let note = Some(format!(
        "numResults={} (caps at 10000); pubDate+addDate exposed (index-time); addDateFrom/To filter available",
        total.map_or("null".to_string(), |t| t.to_string())
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
        .collect::<Vec<_>>();
    Ok(VendorResult {
        vendor: "perigon",
        day: day.to_string(),
        pages: 1,
        note,
        exhausted: exhausted(items.len(), total, got.len() < 100),
        total,
        items,
    })
}

/// (vendor name, the environment key it needs, its probe).
fn vendors() -> Vec<(&'static str, &'static str, Probe)> {
    vec![
        ("newsapi.ai", "NEWSAPI_AI_KEY", probe_newsapi_ai),
        ("newscatcher", "NEWSCATCHER_API_KEY", probe_newscatcher),
        ("webz.io", "WEBZ_API_KEY", probe_webz),
        ("perigon", "PERIGON_API_KEY", probe_perigon),
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
    let no_pub = res.items.iter().filter(|i| i.published.is_none()).count();
    json!({
        "vendor": res.vendor,
        "day": res.day,
        "pages": res.pages,
        "note": res.note,
        "total_reported": res.total,
        "articles": res.items.len(),
        "distinct_ids": ids.len(),
        "id_digest": id_digest,
        "paging_exhausted": res.exhausted,
        "body_len_median": median_usize(&bodies),
        "body_len_p90": quantile(&bodies.iter().map(|b| *b as i64).collect::<Vec<_>>(), 0.9),
        "body_empty_share": round4(empty as f64 / bodies.len().max(1) as f64),
        "published_missing": no_pub,
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
    let known: Vec<String> = vendors().iter().map(|(n, _, _)| n.to_string()).collect();
    let mut days = Vec::new();
    let mut vendors = known.clone();
    let mut compare = None;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--days" => {
                i += 1;
                list_after(&argv, &mut i, &mut days);
            }
            "--vendors" => {
                i += 1;
                let mut got = Vec::new();
                list_after(&argv, &mut i, &mut got);
                if got.is_empty() {
                    return Err(format!("--vendors needs at least one of {known:?}"));
                }
                for v in &got {
                    if !known.contains(v) {
                        return Err(format!("unknown vendor {v}; choose from {known:?}"));
                    }
                }
                vendors = got;
            }
            "--compare" => {
                i += 1;
                match argv.get(i) {
                    Some(p) if !p.starts_with("--") => compare = Some(p.clone()),
                    _ => return Err("--compare needs a baseline file path".to_string()),
                }
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
fn list_after(argv: &[String], i: &mut usize, out: &mut Vec<String>) {
    while *i < argv.len() && !argv[*i].starts_with("--") {
        out.push(argv[*i].clone());
        *i += 1;
    }
}

/// Every key the selected vendors need, checked before the first request.
fn check_keys(selected: &[String]) -> Result<(), String> {
    let missing: Vec<&str> = vendors()
        .into_iter()
        .filter(|(n, k, _)| selected.iter().any(|s| s == n) && !key_present(k))
        .map(|(_, k, _)| k)
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "missing keys in the environment: {}; {KEY_HINT}",
            missing.join(", ")
        ))
    }
}

/// The output directory must exist and accept a file before any request is spent.
fn check_out_writable() -> Result<(), String> {
    let dir = Path::new(OUT).parent().ok_or("output path has no parent")?;
    let probe = dir.join(".vendor_probe_write_check");
    fs::write(&probe, b"").map_err(|e| format!("cannot write to {}: {e}", dir.display()))?;
    fs::remove_file(&probe).map_err(|e| format!("cannot remove {}: {e}", probe.display()))
}

/// Q2's baseline, read and refused before the first request when it is this run's own output.
fn load_baseline(cmp: &str) -> Result<Value, String> {
    let cmp_abs = fs::canonicalize(cmp).map_err(|_| format!("no baseline file: {cmp}"))?;
    if fs::canonicalize(OUT).is_ok_and(|o| o == cmp_abs) {
        return Err(format!(
            "--compare points at this run's own output ({OUT}). Copy it to another name."
        ));
    }
    let text = fs::read_to_string(&cmp_abs).map_err(|e| format!("cannot read {cmp}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("cannot parse baseline {cmp}: {e}"))
}

/// A row's field as display text: a string without quotes, anything else as JSON.
fn cell(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(x) => x.to_string(),
        None => String::new(),
    }
}

fn main() -> ExitCode {
    let setup = parse_args().and_then(|args| {
        check_keys(&args.vendors)?;
        check_out_writable()?;
        let baseline = match &args.compare {
            Some(cmp) => Some(load_baseline(cmp)?),
            None => None,
        };
        Ok((args, baseline))
    });
    let (args, baseline) = match setup {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(EXIT_USAGE);
        }
    };

    let table = vendors();
    let mut rows: Vec<Value> = Vec::new();
    for v in &args.vendors {
        let probe = table.iter().find(|(n, _, _)| n == v).map(|(_, _, f)| *f);
        let Some(probe) = probe else { continue };
        for day in &args.days {
            match probe(day) {
                Ok(res) => {
                    let s = summarise(&res);
                    println!(
                        "  {v} {day}: {} art. of {} reported, {} pages, body median {} chars, \
                         lag median {} s, {} without publication time",
                        s["articles"],
                        s["total_reported"],
                        s["pages"],
                        s["body_len_median"],
                        s["lag_seconds_median"],
                        s["published_missing"]
                    );
                    rows.push(s);
                }
                Err(e) => {
                    // The scrubbed, short reason only. A vendor error can carry the key; this file
                    // is committed in a public repository.
                    let detail = scrub(&e);
                    println!("  {v} {day}: FAILED {}", short(&detail));
                    rows.push(json!({
                        "vendor": v,
                        "day": day,
                        "error": short(&detail),
                        "paging_exhausted": false,
                    }));
                }
            }
        }
    }

    let stamp = chrono::Utc::now().to_rfc3339();
    let out = json!({"fetched_at": stamp, "rows": rows});
    if let Err(e) = fs::write(OUT, serde_json::to_string_pretty(&out).unwrap_or_default()) {
        eprintln!("cannot write {OUT}: {e}");
        return ExitCode::from(EXIT_USAGE);
    }

    println!(
        "\n{:14} {:12} {:>6} {:>9} {:>11} {:>12} {:>11} {:>7}",
        "vendor", "day", "art.", "exhaust", "body med.", "lag med.", "lag p90", "no pub"
    );
    for r in &rows {
        if let Some(err) = r.get("error").and_then(|x| x.as_str()) {
            println!(
                "{:14} {:12} ERROR: {}",
                cell(r, "vendor"),
                cell(r, "day"),
                short(err)
            );
            continue;
        }
        println!(
            "{:14} {:12} {:>6} {:>9} {:>11} {:>12} {:>11} {:>7}",
            cell(r, "vendor"),
            cell(r, "day"),
            cell(r, "articles"),
            cell(r, "paging_exhausted"),
            cell(r, "body_len_median"),
            cell(r, "lag_seconds_median"),
            cell(r, "lag_seconds_p90"),
            cell(r, "published_missing")
        );
    }

    // Q1: the vendor-reported daily volume; a cost only for newsapi.ai, whose pricing the formula is.
    println!(
        "\nQ1 daily volume (vendor-reported total). Cost of a year for newsapi.ai only, at its\n   \
         pricing: 5 tokens/searched year, 100 articles/search, overage $0.015/token:"
    );
    for r in &rows {
        if r.get("error").is_some() {
            continue;
        }
        let (vendor, day) = (cell(r, "vendor"), cell(r, "day"));
        let Some(volume) = r.get("total_reported").and_then(|x| x.as_u64()) else {
            println!("   {vendor:14} {day}: no total reported");
            continue;
        };
        if vendor != "newsapi.ai" {
            println!("   {vendor:14} {day}: {volume} articles/day");
            continue;
        }
        let searches = volume.div_ceil(100);
        let tokens_year = searches * 5 * 365;
        println!(
            "   {vendor:14} {day}: {volume} articles/day -> {searches} searches/day -> \
             {tokens_year} tokens/year -> ${:.0}",
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
                let key = (cell(r, "vendor"), cell(r, "day"));
                let prior = old
                    .iter()
                    .find(|o| (cell(o, "vendor"), cell(o, "day")) == key);
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
