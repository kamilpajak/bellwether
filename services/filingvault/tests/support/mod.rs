//! A stub `Exchange` that answers each url as a test sets it, a clock that moves one second per
//! read, and SEC's daily-index bodies cut from a real one. No test here sends a request to SEC.

#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::path::Path;

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use filingvault::collect::{self, Ask, Report};
use filingvault::error::FilingsError;
use filingvault::rules::Rules;
use filingvault::store::LOG;
use news_provider::{
    BudgetError, Clock, Exchange, Headers, RawResponse, Request, Reservation, SendFailure,
    SendPermit, TransportError, TransportKind, Unreserved,
};
use pit::ObservationTime;
use rusqlite::Connection;
use vault_core::sqlite::read_only;

pub type TestResult = Result<(), Box<dyn Error>>;
pub type OnSend = Box<dyn Fn(&str)>;

pub const RUN: &str = "2026-11-12T06:10:00Z";
pub const NEXT_RUN: &str = "2026-11-13T06:10:00Z";
pub const ROBOTS_URL: &str = "https://www.sec.gov/robots.txt";
/// SEC's robots.txt shape on 2026-11-23: `*` disallows paths, none under the daily index.
pub const ROBOTS: &[u8] = b"User-agent: *\nDisallow: /cgi-bin\nDisallow: /Archives/bin\n";
pub const INDEX: &str = include_str!("../fixtures/form.20261110.idx");
/// The fixture's own day, as its rows' `Date Filed` spells it.
const FIXTURE_DAY: &str = "20261110";
/// The fixture's `Last Data Received`, as SEC spells it.
const FIXTURE_RECEIVED: &str = "Nov 10, 2026";
/// QTR4 2026 as the tests list it; the trailing three of a run on 2026-11-12 are the last three.
pub const LISTED: [&str; 5] = [
    "2026-11-05",
    "2026-11-06",
    "2026-11-09",
    "2026-11-10",
    "2026-11-11",
];
pub const ANNUAL: [&str; 2] = ["10-K", "10-K/A"];
/// The committed rules file's budget: no stub run lists as many filings.
pub const FILING_BUDGET: u32 = 1_800;
pub const OK: u16 = 200;

pub fn day(text: &str) -> NaiveDate {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap_or_default()
}

pub fn index_url(text: &str) -> String {
    sec_edgar::index_url(day(text))
}

pub fn listing_url(text: &str) -> String {
    sec_edgar::listing_url(day(text))
}

/// A quarter listing naming `days`, in the shape SEC's `index.json` has: each index last
/// written at 22:00 New York on its own day, as 29 of 29 of 2026 Q4 were (section 12.2).
pub fn listing(days: &[&str]) -> Vec<u8> {
    let written: Vec<(&str, String)> = days
        .iter()
        .map(|text| {
            let [year, month, on] = [&text[..4], &text[5..7], &text[8..]];
            (*text, format!("{month}/{on}/{year} 10:00:00 PM"))
        })
        .collect();
    listing_written(&written)
}

/// A quarter listing naming each day with its `last-modified`, New York time as SEC writes it.
pub fn listing_written<T: AsRef<str>>(days: &[(&str, T)]) -> Vec<u8> {
    let items: Vec<String> = days
        .iter()
        .map(|(text, written)| {
            format!(
                r#"{{"last-modified": "{}", "name": "form.{}.idx", "type": "file"}}"#,
                written.as_ref(),
                text.replace('-', "")
            )
        })
        .collect();
    format!(r#"{{"directory": {{"item": [{}]}}}}"#, items.join(", ")).into_bytes()
}

/// The real index's rows, filed on `text`'s day, under a `Last Data Received` naming it.
pub fn index_of(text: &str) -> Vec<u8> {
    let received = day(text).format("%b %d, %Y").to_string();
    INDEX
        .replace(FIXTURE_DAY, &text.replace('-', ""))
        .replace(FIXTURE_RECEIVED, &received)
        .into_bytes()
}

/// robots.txt, the listing of `days`'s quarter naming them, and each day's index.
pub fn listing_of(days: &[&str]) -> Stub {
    let stub = Stub::default()
        .answering(ROBOTS_URL, Reply::Status(OK, ROBOTS.to_vec()))
        .answering(&listing_url(days[0]), Reply::Status(OK, listing(days)));
    days.iter().fold(stub, |stub, text| {
        stub.answering(&index_url(text), Reply::Status(OK, index_of(text)))
    })
}

pub fn rules(fill_days: u32) -> Rules {
    Rules {
        forms: ANNUAL.map(str::to_owned).to_vec(),
        fill_days,
        filing_budget: FILING_BUDGET,
    }
}

/// The committed holiday list, as `ledger check --holidays` reads it.
pub fn holidays() -> Result<filingvault::holidays::Holidays, FilingsError> {
    filingvault::holidays::read(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/rules/holidays.json"
    )))
}

pub fn ask(store: &Path) -> Ask {
    Ask {
        store: store.to_path_buf(),
        rules: rules(10),
        deadline: None,
        dry_run: false,
    }
}

/// A clock that moves on one second each time it is read.
pub struct Tick(Cell<DateTime<Utc>>);

impl Tick {
    pub fn at(text: &str) -> Result<Self, Box<dyn Error>> {
        Ok(Self(Cell::new(text.parse()?)))
    }
}

impl Clock for Tick {
    fn now(&self) -> DateTime<Utc> {
        let now = self.0.get();
        self.0.set(now + TimeDelta::seconds(1));
        now
    }
}

#[derive(Clone)]
pub enum Reply {
    Status(u16, Vec<u8>),
    /// A head with `Last-Modified`, as SEC sends on an index.
    Dated(u16, Vec<u8>, &'static str),
    Transport,
    /// A body over the limit, cut at it.
    OverLimit(Vec<u8>),
    /// A head of this status, then the connection lost with these bytes of the body read.
    HeadThenLost(u16, Vec<u8>),
    GateHeld,
    /// A pace file the gate cannot read: a budget error that is no gate refusal.
    Budget,
    /// The gate owed a wait past the run's deadline.
    GateOwed,
    NoCredential,
}

/// Each url answered as set, the last reply repeating; a url it does not hold fails in transport.
#[derive(Default)]
pub struct Stub {
    replies: RefCell<BTreeMap<String, Vec<Reply>>>,
    pub asked: RefCell<Vec<String>>,
    past_deadline: BTreeSet<String>,
    /// Called at each send with the attempt id, before the answer is made.
    pub on_send: Option<OnSend>,
}

impl Stub {
    pub fn answering(self, url: &str, reply: Reply) -> Self {
        self.replies
            .borrow_mut()
            .entry(url.to_owned())
            .or_default()
            .push(reply);
        self
    }

    pub fn past_deadline(mut self, url: &str) -> Self {
        self.past_deadline.insert(url.to_owned());
        self
    }

    /// robots.txt as SEC serves it, QTR4's listing of [`LISTED`], and each listed day's index.
    pub fn sec() -> Self {
        let mut stub = Self::default()
            .answering(ROBOTS_URL, Reply::Status(OK, ROBOTS.to_vec()))
            .answering(&listing_url(LISTED[0]), Reply::Status(OK, listing(&LISTED)));
        for text in LISTED {
            stub = stub.answering(&index_url(text), Reply::Status(OK, index_of(text)));
        }
        stub
    }

    /// Replaces every reply of `url`.
    pub fn replacing(self, url: &str, reply: Reply) -> Self {
        self.replies.borrow_mut().remove(url);
        self.answering(url, reply)
    }

    pub fn asked(&self) -> Vec<String> {
        self.asked.borrow().clone()
    }
}

impl Exchange for Stub {
    fn reserve(
        &self,
        request: &Request,
        clock: &dyn Clock,
        _deadline: Option<DateTime<Utc>>,
    ) -> Result<Reservation, Unreserved> {
        if self.past_deadline.contains(&request.url) {
            return Ok(Reservation::PastDeadline {
                go_at: clock.now(),
                by_server: false,
            });
        }
        Ok(Reservation::Go)
    }

    fn send(
        &self,
        permit: SendPermit,
        request: &Request,
        clock: &dyn Clock,
    ) -> Result<RawResponse, SendFailure> {
        self.asked.borrow_mut().push(request.url.clone());
        let attempt = permit.attempt_id().clone();
        if let Some(check) = &self.on_send {
            check(attempt.hex());
        }
        let at = ObservationTime::new(clock.now());
        let back = ObservationTime::new(clock.now());
        let failed = || TransportError::new(attempt.clone(), at, back, "stub".to_owned());
        let reply = {
            let mut replies = self.replies.borrow_mut();
            replies.get_mut(&request.url).and_then(|queue| {
                if queue.len() > 1 {
                    Some(queue.remove(0))
                } else {
                    queue.first().cloned()
                }
            })
        };
        match reply {
            Some(Reply::Status(status, body)) => {
                Ok(RawResponse::new(attempt, status, body, at, back))
            }
            Some(Reply::Dated(status, body, modified)) => {
                Ok(
                    RawResponse::new(attempt, status, body, at, back).with_headers(Headers::new(
                        vec![("last-modified".to_owned(), modified.to_owned())],
                    )),
                )
            }
            Some(Reply::OverLimit(cut)) => Err(SendFailure::Transport(
                failed()
                    .with_status(OK)
                    .with_cut_body(cut)
                    .of_kind(TransportKind::BodyOverLimit),
            )),
            Some(Reply::HeadThenLost(status, cut)) => Err(SendFailure::Transport(
                failed().with_status(status).with_cut_body(cut),
            )),
            Some(Reply::GateHeld) => Err(SendFailure::Budget(BudgetError::GateHeld {
                path: "pace/www.sec.gov.per-host.sent".into(),
                waited: std::time::Duration::from_secs(16),
            })),
            Some(Reply::Budget) => Err(SendFailure::Budget(BudgetError::NoPace)),
            Some(Reply::GateOwed) => Err(SendFailure::Budget(BudgetError::PastDeadline {
                owed: std::time::Duration::from_secs(1),
                left: std::time::Duration::ZERO,
            })),
            Some(Reply::NoCredential) => {
                Err(SendFailure::CredentialMissing("sec_contact".to_owned()))
            }
            Some(Reply::Transport) | None => Err(SendFailure::Transport(failed())),
        }
    }
}

pub fn run(
    store: &Path,
    stub: &Stub,
    at: &str,
) -> Result<Result<Report, FilingsError>, Box<dyn Error>> {
    Ok(collect::run(&ask(store), stub, &Tick::at(at)?))
}

pub fn run_ask(
    asked: &Ask,
    stub: &Stub,
    at: &str,
) -> Result<Result<Report, FilingsError>, Box<dyn Error>> {
    Ok(collect::run(asked, stub, &Tick::at(at)?))
}

pub fn log(store: &Path) -> Result<Connection, Box<dyn Error>> {
    Ok(read_only(&store.join(LOG))?)
}

/// Every row `sql` returns, each column read as text and joined with `|`.
pub fn rows(store: &Path, sql: &str) -> Result<Vec<String>, Box<dyn Error>> {
    let db = log(store)?;
    let mut statement = db.prepare(sql)?;
    let width = statement.column_count();
    let found = statement
        .query_map([], |row| {
            (0..width)
                .map(|at| {
                    row.get::<_, rusqlite::types::Value>(at)
                        .map(|value| match value {
                            rusqlite::types::Value::Null => "null".to_owned(),
                            rusqlite::types::Value::Integer(n) => n.to_string(),
                            rusqlite::types::Value::Real(n) => n.to_string(),
                            rusqlite::types::Value::Text(text) => text,
                            rusqlite::types::Value::Blob(_) => "blob".to_owned(),
                        })
                })
                .collect::<Result<Vec<String>, _>>()
                .map(|columns| columns.join("|"))
        })?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(found)
}

/// The index days asked, in order.
pub fn asked_days(stub: &Stub) -> Vec<String> {
    stub.asked()
        .iter()
        .filter_map(|url| {
            let name = url.rsplit('/').next()?;
            let digits = name.strip_prefix("form.")?.strip_suffix(".idx")?;
            NaiveDate::parse_from_str(digits, "%Y%m%d")
                .ok()
                .map(|day| day.format("%Y-%m-%d").to_string())
        })
        .collect()
}
