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
use sec_edgar::{body_of_200, filing_of, index_lines, index_row, listed_days, Quarter, HOST};

use crate::error::FilingsError;
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

/// An index's verdict and its rows of a listed form. A body that is no index, and a line of a
/// listed form that is no whole filing (`sec_edgar::filing_of`), are permanent failures, never a
/// quiet day or a dropped row.
#[must_use]
pub fn index_read(
    answer: &Result<RawResponse, TransportError>,
    forms: &[String],
) -> (Verdict, Vec<Listed>) {
    let body = match body_of_200(answer) {
        Ok(body) => body,
        Err(verdict) => return (verdict, Vec::new()),
    };
    let Some(lines) = index_lines(&body) else {
        return (Verdict::PermanentFailure, Vec::new());
    };
    let mut listed = Vec::new();
    for line in lines {
        let form = line.split_whitespace().next().unwrap_or_default();
        if !forms.iter().any(|listed_form| listed_form == form) {
            continue;
        }
        let Some(filing) = index_row(line).as_ref().and_then(filing_of) else {
            return (Verdict::PermanentFailure, Vec::new());
        };
        listed.push(Listed {
            cik: filing.cik,
            accession: filing.accession,
            form: form.to_owned(),
            filed_day: filing.filed,
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
/// quarter of the fill window while no listing of it was received after its last day. A step back
/// that does not go back, or more quarters than the window holds, is an error and never a loop.
pub fn quarters_to_ask(
    fill_start: NaiveDate,
    current: Quarter,
    received_after_end: &BTreeSet<Quarter>,
) -> Result<Vec<Quarter>, FilingsError> {
    walk_back(fill_start, current, received_after_end, Quarter::previous)
}

/// The shortest quarter, a non-leap first one, in days.
const SHORTEST_QUARTER_DAYS: i64 = 90;

fn walk_back(
    fill_start: NaiveDate,
    current: Quarter,
    received_after_end: &BTreeSet<Quarter>,
    step: impl Fn(Quarter) -> Quarter,
) -> Result<Vec<Quarter>, FilingsError> {
    let oldest = Quarter::of(fill_start);
    let most = (current.first_day() - fill_start).num_days().max(0) / SHORTEST_QUARTER_DAYS + 2;
    let mut quarters = vec![current];
    let mut quarter = current;
    let mut steps = 0_i64;
    loop {
        let next = step(quarter);
        steps += 1;
        if next >= quarter || steps > most {
            return Err(FilingsError::FillWindow(format!(
                "the quarter before {} is {}, and the window from {fill_start} holds at most \
                 {most}",
                quarter.label(),
                next.label()
            )));
        }
        if next < oldest {
            return Ok(quarters);
        }
        if !received_after_end.contains(&next) {
            quarters.push(next);
        }
        quarter = next;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn day(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap_or_default()
    }

    /// A step that does not go back ends the walk with an error. On 2026-11-27, `Quarter::of`
    /// mutated to `month % 12` grew the walk to 87 GB resident and panicked the host kernel.
    #[test]
    fn a_quarter_step_that_does_not_go_back_is_an_error_and_never_a_loop() {
        let current = Quarter::of(day("2026-11-12"));
        let fill_start = day("2025-08-13");
        let none = BTreeSet::new();
        let labels = |walked: Result<Vec<Quarter>, FilingsError>| {
            walked.map(|quarters| quarters.into_iter().map(Quarter::label).collect::<Vec<_>>())
        };
        let stuck =
            walk_back(fill_start, current, &none, |quarter| quarter).map_err(|why| why.code());
        assert_eq!(
            (
                labels(quarters_to_ask(fill_start, current, &none)).ok(),
                stuck
            ),
            (
                Some(
                    [
                        "2026/QTR4",
                        "2026/QTR3",
                        "2026/QTR2",
                        "2026/QTR1",
                        "2025/QTR4",
                        "2025/QTR3"
                    ]
                    .map(str::to_owned)
                    .to_vec()
                ),
                Err(crate::error::ErrorCode::InputUnparseable)
            )
        );
    }

    fn answered(body: &str) -> Result<RawResponse, TransportError> {
        let at = pit::ObservationTime::new(chrono::DateTime::UNIX_EPOCH);
        let attempt = news_provider::AttemptId::recorded("a");
        Ok(RawResponse::new(
            attempt,
            OK,
            body.as_bytes().to_vec(),
            at,
            at,
        ))
    }

    const HEAD: &str = "Form Type   Company Name   CIK   Date Filed  File Name\n-----------\n";
    const GOOD: &str =
        "10-K  ACME CORP  320193  20261110  edgar/data/320193/0000320193-26-000071.txt";

    /// Each listed row is a filing with every field read, or the day fails: never a made-up CIK,
    /// a dropped row or a quiet day (review of 2026-11-27, finding F2). Mutation 2026-11-27, a
    /// row whose path names no accession dropped: red.
    #[test]
    fn a_listed_index_row_is_read_whole_or_the_day_fails() {
        let forms = ["10-K".to_owned(), "10-K/A".to_owned()];
        let cases = [
            (
                format!("{HEAD}{GOOD}\n8-K  X\n"),
                "usable",
                vec!["320193|0000320193-26-000071"],
            ),
            (
                format!("{HEAD}{}", GOOD.replace("320193  ", "0000320193  ")),
                "usable",
                vec!["320193|0000320193-26-000071"],
            ),
            (
                format!("{HEAD}8-K  ACME  1  20261110  edgar/data/1/0000000001-26-000001.txt\n"),
                "empty",
                vec![],
            ),
            (
                format!("{HEAD}{GOOD}\n10-K BETA INC 1234\n"),
                "permanent_failure",
                vec![],
            ),
            (format!("{HEAD}10-K x y\n"), "permanent_failure", vec![]),
            (
                format!("{HEAD}{}", GOOD.replace("20261110", "2026111")),
                "permanent_failure",
                vec![],
            ),
            (
                format!("{HEAD}{}", GOOD.replace("data/320193/", "data/320194/")),
                "permanent_failure",
                vec![],
            ),
            (
                format!("{HEAD}{}", GOOD.replace("-26-000071.txt", "-26-71.txt")),
                "permanent_failure",
                vec![],
            ),
            (
                format!("{HEAD}{}", GOOD.replace("0000320193-26-000071.txt", "")),
                "permanent_failure",
                vec![],
            ),
        ];
        for (body, verdict, rows) in cases {
            let (read, listed) = index_read(&answered(&body), &forms);
            let listed: Vec<String> = listed
                .iter()
                .map(|row| format!("{}|{}", row.cik, row.accession))
                .collect();
            assert_eq!(
                (read.name(), listed),
                (verdict, rows.into_iter().map(str::to_owned).collect()),
                "{body}"
            );
        }
    }

    /// A listing is a `directory.item` array; one naming no day is `empty`. Mutations 2026-11-27,
    /// each red here: a body that does not parse read `empty`; an empty listing read `usable`.
    #[test]
    fn a_listing_that_is_no_listing_fails_and_one_naming_no_day_is_empty() {
        let read = [
            "{}",
            "<html>",
            r#"{"directory": {"item": []}}"#,
            r#"{"directory": {"item": [{"name": "form.20261110.idx"}]}}"#,
        ]
        .map(|body| {
            let (verdict, days) = listing_read(&answered(body));
            (verdict.name(), days.len())
        });
        assert_eq!(
            read,
            [
                ("permanent_failure", 0),
                ("permanent_failure", 0),
                ("empty", 0),
                ("usable", 1)
            ]
        );
    }
}
