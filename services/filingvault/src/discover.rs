//! Discovery by SEC's daily form index (architecture.md section 12.3): the requests, how each
//! answer reads, and which quarters and days a run asks. Pure: no store and no socket.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use canon::FileDigest;
use chrono::NaiveDate;
use news_provider::robots::Robots;
use news_provider::{
    Provenance, RawResponse, Request, RequestPhase, SourceId, TargetWindow, TransportError, Verdict,
};
use sec_edgar::{accession_of, body_of_200, index_rows, listed_days, Quarter, HOST, INDEX_DATE};

use crate::store::{Ask, IndexAttempt, Kind, Listed};

/// The ledger's source of every filings request; the `Wire`'s per-IP bucket is SEC's shared one.
pub const SOURCE: &str = "edgar_filings";
/// 64 MiB: the largest primary document seen is 23.71 MiB (architecture.md section 12.3).
pub const BODY_LIMIT: u64 = 64 * 1_024 * 1_024;
/// The newest listed days every run asks again, so a day written late is read (section 12.2).
pub const TRAILING_DAYS: usize = 3;
const ROBOTS_PATH: &str = "/robots.txt";
const OK: u16 = 200;
const NOT_FOUND: u16 = 404;

/// A request of the filings context: `<kind>/<target>` is the only field that carries the kind;
/// the news-only fields hold the constants section 12.1's table names.
#[must_use]
pub fn request(ask: &Ask, provenance: Provenance) -> Request {
    Request {
        source: SourceId::new(SOURCE),
        target_window: TargetWindow::new(ask.window()),
        phase: RequestPhase::Index,
        provenance,
        url: ask.url.clone(),
        headers: Vec::new(),
        params: Vec::new(),
        universe_version: FileDigest::of(&[]),
        listed_date: None,
        names_ticker: false,
        hosts_version: None,
        extract_version: None,
    }
}

fn ask(kind: Kind, target: String, url: String, forms: Option<Vec<String>>) -> Ask {
    Ask {
        kind,
        target,
        url,
        forms,
        body_limit: BODY_LIMIT,
    }
}

#[must_use]
pub fn robots_ask() -> Ask {
    ask(
        Kind::Robots,
        HOST.to_owned(),
        format!("https://{HOST}{ROBOTS_PATH}"),
        None,
    )
}

#[must_use]
pub fn listing_ask(quarter: Quarter) -> Ask {
    ask(
        Kind::Listing,
        quarter.label(),
        sec_edgar::listing_url(quarter.first_day()),
        None,
    )
}

#[must_use]
pub fn index_ask(day: NaiveDate, forms: &[String]) -> Ask {
    ask(
        Kind::Index,
        crate::store::day_text(day),
        sec_edgar::index_url(day),
        Some(forms.to_vec()),
    )
}

/// Why robots.txt stops a run; `details.reason` of `robots_stopped`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RobotsStop {
    Disallowed,
    Unparseable,
    CrawlDelayOverInterval,
    NotAnswered,
}

impl RobotsStop {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Disallowed => "disallowed",
            Self::Unparseable => "unparseable",
            Self::CrawlDelayOverInterval => "crawl_delay_over_interval",
            Self::NotAnswered => "not_answered",
        }
    }
}

/// 200 is usable and 404 empty (no rules); every other answer as SEC's statuses read.
#[must_use]
pub fn robots_verdict(answer: &Result<RawResponse, TransportError>) -> Verdict {
    match answer {
        Ok(response) if response.status() == NOT_FOUND => Verdict::Empty,
        _ => body_of_200(answer).map_or_else(|failed| failed, |_| Verdict::Usable),
    }
}

/// The rules the run is held to, `None` for a 404, or why the run asks nothing more: a body that
/// does not parse, a Crawl-delay over our interval, or any other answer (section 12.3).
pub fn robots_rules(
    answer: &Result<RawResponse, TransportError>,
    interval: Duration,
) -> Result<Option<Robots>, RobotsStop> {
    let response = match answer {
        Ok(response) if response.status() == NOT_FOUND => return Ok(None),
        Ok(response) if response.status() == OK => response,
        _ => return Err(RobotsStop::NotAnswered),
    };
    let rules = Robots::parse(response.body()).map_err(|_| RobotsStop::Unparseable)?;
    if rules.crawl_delay().is_some_and(|delay| delay > interval) {
        return Err(RobotsStop::CrawlDelayOverInterval);
    }
    Ok(Some(rules))
}

/// Whether `url`'s path may be asked under `rules`; no rules allow every path.
#[must_use]
pub fn allowed(rules: Option<&Robots>, url: &str) -> bool {
    let path = url.split_once(HOST).map_or(url, |(_, path)| path);
    rules.is_none_or(|rules| rules.allows(path))
}

/// A listing's verdict and the days it names: a listing naming no day is `empty`, one that is no
/// directory listing a permanent failure with its body kept.
#[must_use]
pub fn listing_read(answer: &Result<RawResponse, TransportError>) -> (Verdict, Vec<NaiveDate>) {
    match body_of_200(answer) {
        Err(verdict) => (verdict, Vec::new()),
        Ok(body) => match listed_days(body.as_bytes()) {
            Ok(days) if days.is_empty() => (Verdict::Empty, days),
            Ok(days) => (Verdict::Usable, days),
            Err(_) => (Verdict::PermanentFailure, Vec::new()),
        },
    }
}

/// An index's verdict and its rows of a listed form. A body that is no index, and a listed row
/// whose path names no accession, are permanent failures, never a quiet day or a dropped row.
#[must_use]
pub fn index_read(
    answer: &Result<RawResponse, TransportError>,
    forms: &[String],
) -> (Verdict, Vec<Listed>) {
    let body = match body_of_200(answer) {
        Ok(body) => body,
        Err(verdict) => return (verdict, Vec::new()),
    };
    let Some(rows) = index_rows(&body) else {
        return (Verdict::PermanentFailure, Vec::new());
    };
    let mut listed = Vec::new();
    for row in rows.into_iter().filter(|row| forms.contains(&row.form)) {
        let Some(accession) = accession_of(&row.path) else {
            return (Verdict::PermanentFailure, Vec::new());
        };
        listed.push(Listed {
            filed_day: NaiveDate::parse_from_str(&row.filed, INDEX_DATE).ok(),
            cik: row.cik,
            accession,
            form: row.form,
        });
    }
    if listed.is_empty() {
        (Verdict::Empty, listed)
    } else {
        (Verdict::Usable, listed)
    }
}

/// What the ledger knows of an index day (architecture.md section 12.2). Every state but `Asked`
/// is "unasked" there; the split keeps "we looked and failed" apart from "we did not look".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DayState {
    /// A usable or empty answer under a form list holding every form of the current one.
    Asked,
    /// Such an answer only under a narrower list.
    Narrower,
    /// Cut at a limit at least the one in force: asked again only under a higher one.
    Truncated,
    /// Sent, and no answer above.
    Failed,
    /// No attempt was sent.
    Unasked,
}

impl DayState {
    pub const ALL: [Self; 5] = [
        Self::Asked,
        Self::Narrower,
        Self::Truncated,
        Self::Failed,
        Self::Unasked,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Asked => "asked",
            Self::Narrower => "narrower",
            Self::Truncated => "truncated",
            Self::Failed => "failed",
            Self::Unasked => "unasked",
        }
    }
}

/// The state of each day any attempt names, under the current form list and body limit.
#[must_use]
pub fn day_states(
    attempts: &[IndexAttempt],
    forms: &[String],
    limit: u64,
) -> BTreeMap<NaiveDate, DayState> {
    let mut states: BTreeMap<NaiveDate, DayState> = BTreeMap::new();
    for attempt in attempts {
        let answered = matches!(attempt.verdict.as_deref(), Some("usable" | "empty"));
        let state = if answered && forms.iter().all(|form| attempt.forms.contains(form)) {
            DayState::Asked
        } else if answered {
            DayState::Narrower
        } else if attempt.truncated && attempt.body_limit >= limit {
            DayState::Truncated
        } else if attempt.outcome == "failed" {
            DayState::Failed
        } else {
            DayState::Unasked
        };
        let kept = states.entry(attempt.day).or_insert(state);
        *kept = (*kept).min(state);
    }
    states
}

/// The quarters a run asks for a listing, newest first: the current one always, and each other
/// quarter of the fill window while no listing of it was received after its last day.
#[must_use]
pub fn quarters_to_ask(
    fill_start: NaiveDate,
    current: Quarter,
    received_after_end: &BTreeSet<Quarter>,
) -> Vec<Quarter> {
    let oldest = Quarter::of(fill_start);
    let mut quarters = vec![current];
    let mut quarter = current.previous();
    while quarter >= oldest {
        if !received_after_end.contains(&quarter) {
            quarters.push(quarter);
        }
        quarter = quarter.previous();
    }
    quarters
}

/// The index days a run asks, newest first: the trailing [`TRAILING_DAYS`] listed days before
/// the run's date, then every other listed day of the fill window not asked or cut.
#[must_use]
pub fn days_to_ask(
    listed: &BTreeSet<NaiveDate>,
    states: &BTreeMap<NaiveDate, DayState>,
    fill_start: NaiveDate,
    run_date: NaiveDate,
) -> Vec<NaiveDate> {
    let window: Vec<NaiveDate> = listed
        .iter()
        .rev()
        .copied()
        .filter(|day| *day < run_date && *day >= fill_start)
        .collect();
    let (trailing, older) = window.split_at(window.len().min(TRAILING_DAYS));
    let state = |day: &NaiveDate| states.get(day).copied().unwrap_or(DayState::Unasked);
    trailing
        .iter()
        .copied()
        .filter(|day| state(day) != DayState::Truncated)
        .chain(
            older
                .iter()
                .copied()
                .filter(|day| !matches!(state(day), DayState::Asked | DayState::Truncated)),
        )
        .collect()
}
