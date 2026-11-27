//! What is true of SEC's EDGAR host for every caller of `www.sec.gov`: its paths, the bucket and
//! the contact name every caller shares, its transport profile with one interval per caller, and
//! how its daily index, quarter listing, filing header and statuses read. It holds no
//! `Provider`, no form list and no store, and declares no HTTP dependency (architecture.md
//! section 12.1).

use std::borrow::Cow;

use chrono::{DateTime, Datelike, Months, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use news_provider::request::HeaderValue;
use news_provider::wire::{RawResponse, Timeouts, TransportError, TransportKind, TransportProfile};
use news_provider::{CredentialRef, RequestBudget, Verdict};

/// The `SourceId` every SEC caller builds its `Wire` under, which names the per-IP pace file. SEC
/// counts requests per address, so the callers share one; the value is the file on disk since the
/// first run, `pace/edgar_press.per-ip` (architecture.md section 12.2).
pub const PER_IP_BUCKET: &str = "edgar_press";
/// The credential a run must hold. SEC answers a User-Agent with no contact address with 403 and
/// one with a contact with 200, re-measured 2026-11-12, and the address may never reach a plan or
/// a ledger row (architecture.md section 3.2's `filter_params` row), so it travels by NAME.
pub const CONTACT_CREDENTIAL: &str = "sec_contact";

/// The host every item here is a fact of. A caller's robots.txt fetch and the pace's per-host
/// bucket are keyed on it.
pub const HOST: &str = "www.sec.gov";
const ARCHIVES: &str = "/Archives/";
/// Where a full-submission text file's header ends and its documents start.
pub const HEADER_END: &str = "</SEC-HEADER>";
const DAILY_INDEX: &str = "edgar/daily-index";
/// The vendor's own column headings in `form.<date>.idx`. The file is FIXED WIDTH and its header
/// line is where the columns start: a whitespace split cannot read it, because `SC 13D` and every
/// multi-word company name hold spaces.
const COLUMNS: [&str; 5] = [
    "Form Type",
    "Company Name",
    "CIK",
    "Date Filed",
    "File Name",
];
/// The separator line the rows start after. Pure dashes, 141 of them on the real file.
const DASH: char = '-';
/// The one index column carrying a date and no hour. Taken from [`COLUMNS`] so the heading has
/// one home.
pub const DATE_FILED: &str = COLUMNS[3];
/// `Date Filed` and the index file name both read `YYYYMMDD`, measured on the real file 2026-11-13.
pub const INDEX_DATE: &str = "%Y%m%d";
const ACCESS_DENIED: &str = "<Code>AccessDenied</Code>";
const REQUEST_TIMEOUT: u16 = 408;
const TOO_MANY_REQUESTS: u16 = 429;
const SERVER_ERRORS: std::ops::RangeInclusive<u16> = 500..=599;
const OK: u16 = 200;
const FORBIDDEN: u16 = 403;

/// Every timeout, as one value. An ASSUMPTION taken by default and not a measurement: nothing in
/// this tree has timed an EDGAR response, so there is no quantile to quote. `transport()` below is
/// the one place to change it.
const VENDOR_PATIENCE: std::time::Duration = std::time::Duration::from_secs(30);
/// 110 ms, owner decision 2026-11-26: 10 intervals of it less a 100 ms stamp-to-arrival spread
/// still span SEC's 1 s for 10 requests per IP (predecessor table, read 2026-11-13). The spread
/// and what no margin covers: architecture.md section 12.6, the row on SEC's interval.
const SEC_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(110);
/// 500 ms, owner decision 2026-11-24: 2 requests a second, 20% of SEC's stated 10, for the one
/// caller whose nightly run asks thousands (architecture.md section 12.2).
const FILINGS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Who asks `www.sec.gov`. SEC counts every caller against one address, so how the ceiling is
/// split between them is one table, [`Caller::interval`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// The 8-K press adapter, `providers/edgar-press`.
    EdgarPress,
    /// `newsvault roster fetch` (architecture.md section 10, the roster).
    Roster,
    /// `filingvault filings collect` (architecture.md section 12.2).
    Filings,
}

impl Caller {
    /// The least time between two of this caller's requests, per IP and per host alike.
    #[must_use]
    pub const fn interval(self) -> std::time::Duration {
        match self {
            Self::EdgarPress => SEC_MIN_INTERVAL,
            Self::Roster => SEC_MIN_INTERVAL,
            Self::Filings => FILINGS_INTERVAL,
        }
    }
}

/// SEC's transport, one home for every caller of `www.sec.gov`. The pace's per-host bucket is
/// keyed on the host and its per-IP bucket on the `SourceId` a `Wire` is built with, so two SEC
/// callers share both only when each passes [`PER_IP_BUCKET`]. The body limit is the caller's.
#[must_use]
pub fn transport(caller: Caller, body_limit: u64) -> TransportProfile {
    TransportProfile {
        user_agent: HeaderValue::FromCredential(CredentialRef::new(CONTACT_CREDENTIAL)),
        timeouts: Timeouts {
            global: VENDOR_PATIENCE,
            per_call: VENDOR_PATIENCE,
            resolve: VENDOR_PATIENCE,
            connect: VENDOR_PATIENCE,
            send_request: VENDOR_PATIENCE,
            await_100: VENDOR_PATIENCE,
            send_body: VENDOR_PATIENCE,
            recv_response: VENDOR_PATIENCE,
            recv_body: VENDOR_PATIENCE,
        },
        body_limit,
        // Per IP, because the contact above does not scope the quota; per host as well, keyed
        // on `www.sec.gov` and not on a source, so a second SEC source shares the bucket.
        budget: RequestBudget {
            min_interval: Some(caller.interval()),
            per_host: Some(caller.interval()),
        },
    }
}

/// SEC asks for a name and an address: whether `contact` holds a token shaped like an address.
#[must_use]
pub fn contact_is_addressed(contact: &str) -> bool {
    contact.split_whitespace().any(|token| {
        token
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
    })
}

/// A calendar quarter, which is how SEC files its daily indexes: one directory each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Quarter {
    year: i32,
    number: u32,
}

const MONTHS_A_QUARTER: u32 = 3;

impl Quarter {
    #[must_use]
    pub fn of(day: NaiveDate) -> Self {
        Self {
            year: day.year(),
            number: (day.month() - 1) / MONTHS_A_QUARTER + 1,
        }
    }

    #[must_use]
    pub fn first_day(self) -> NaiveDate {
        NaiveDate::from_ymd_opt(self.year, (self.number - 1) * MONTHS_A_QUARTER + 1, 1)
            .unwrap_or(NaiveDate::MIN)
    }

    #[must_use]
    pub fn last_day(self) -> NaiveDate {
        self.first_day()
            .checked_add_months(Months::new(MONTHS_A_QUARTER))
            .and_then(|next| next.pred_opt())
            .unwrap_or(NaiveDate::MAX)
    }

    #[must_use]
    pub fn previous(self) -> Self {
        Self::of(self.first_day().pred_opt().unwrap_or(NaiveDate::MIN))
    }

    /// The directory's own name below `daily-index/`: `2026/QTR4`.
    #[must_use]
    pub fn label(self) -> String {
        format!("{}/QTR{}", self.year, self.number)
    }
}

/// The quarter listing's url for a day: the directory that names each daily index it holds.
#[must_use]
pub fn listing_url(day: NaiveDate) -> String {
    format!(
        "https://{HOST}{ARCHIVES}{DAILY_INDEX}/{}/index.json",
        Quarter::of(day).label()
    )
}

/// The url of a day's form index.
#[must_use]
pub fn index_url(day: NaiveDate) -> String {
    format!(
        "https://{HOST}{ARCHIVES}{DAILY_INDEX}/{}/form.{}.idx",
        Quarter::of(day).label(),
        day.format(INDEX_DATE)
    )
}

/// The url of a file an index row names in its `File Name` column.
#[must_use]
pub fn archive_url(path: &str) -> String {
    format!("https://{HOST}{ARCHIVES}{path}")
}

/// The accession an EDGAR document url ends in:
/// `.../edgar/data/320193/0000320193-26-000071.txt` -> `0000320193-26-000071`. The same string
/// the filing header's `ACCESSION NUMBER` carries, so a follow-up's story matches the committed
/// item's.
#[must_use]
pub fn accession_of(url: &str) -> Option<String> {
    let last = url.rsplit('/').next()?;
    let stem = last.strip_suffix(".txt").unwrap_or(last);
    (!stem.is_empty()).then(|| stem.to_owned())
}

/// A quarter listing nothing can read would plan zero requests, which reads as a closed market.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("the quarter listing is present and is not a directory listing: {0}")]
pub struct ListingUnparseable(pub String);

/// The days a quarter's listing holds a form index for. Only `form.<date>.idx` names count: a
/// quarter directory also holds `company.`, `master.` and `index.json` itself. A body with no
/// `directory.item` array, or a `form.*.idx` name that is no `YYYYMMDD` day, is refused, never
/// read as a listing of no day or of another day.
pub fn listed_days(bytes: &[u8]) -> Result<Vec<NaiveDate>, ListingUnparseable> {
    let read: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|why| ListingUnparseable(why.to_string()))?;
    let items = read["directory"]["item"]
        .as_array()
        .ok_or_else(|| ListingUnparseable("no `directory.item` array".to_owned()))?;
    let mut days = Vec::new();
    for name in items.iter().filter_map(|item| item["name"].as_str()) {
        if let Some(day) = day_of_index_name(name)? {
            days.push(day);
        }
    }
    Ok(days)
}

/// [`listed_days`] with the empty fallback `edgar-press` reads a listing by: JSON with no
/// `directory.item` array lists no day. Changing it changes an `edgar-press` verdict, which is the
/// owner's decision (architecture.md section 12.6).
pub fn listed_days_or_none(bytes: &[u8]) -> Result<Vec<NaiveDate>, ListingUnparseable> {
    match listed_days(bytes) {
        Err(_) if serde_json::from_slice::<serde_json::Value>(bytes).is_ok() => Ok(Vec::new()),
        read => read,
    }
}

/// `None` for a name of another file; a `form.*.idx` name must hold [`INDEX_DATE_WIDTH`] digits.
fn day_of_index_name(name: &str) -> Result<Option<NaiveDate>, ListingUnparseable> {
    let Some(digits) = name
        .strip_prefix("form.")
        .and_then(|rest| rest.strip_suffix(".idx"))
    else {
        return Ok(None);
    };
    let shaped = digits.len() == INDEX_DATE_WIDTH && digits.bytes().all(|b| b.is_ascii_digit());
    shaped
        .then(|| NaiveDate::parse_from_str(digits, INDEX_DATE).ok())
        .flatten()
        .map(Some)
        .ok_or_else(|| ListingUnparseable(format!("{name:?} names no YYYYMMDD day")))
}

/// One row of a daily index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRow {
    pub form: String,
    pub cik: String,
    /// The row's own `Date Filed`, which is `YYYYMMDD` and not ISO - measured on the real file
    /// 2026-11-13, where the hand-written fixture had `2026-11-10` until 2026-11-15.
    pub filed: String,
    pub path: String,
}

/// The rows of a daily index, or `None` when the payload is not one at all - which is what makes
/// a 200 carrying the vendor's own error text a permanent failure rather than a quiet day.
///
/// Anchored on the dashed separator and split from BOTH ENDS, never on column offsets taken from
/// a heading line. Measured 2026-11-13 on the real `form.20261110.idx` (1,057,192 bytes, captured
/// through the predecessor's own client): the vendor WRAPS its heading across two lines - `CIK`
/// ends line 9 and `Date Filed  File Name` is line 10 - so a reader requiring the five headings
/// on one line finds none, and this one returned `None` for a day holding 5,429 filing rows. Rows
/// are 194 bytes wide, not the 140 the hand-written fixture had. The predecessor has parsed this
/// file in production for months by splitting from both ends, which also survives a width change.
#[must_use]
pub fn index_rows(payload: &str) -> Option<Vec<IndexRow>> {
    Some(
        index_lines(payload)?
            .into_iter()
            .filter_map(index_row)
            .collect(),
    )
}

/// The non-blank lines after a daily index's dashed separator, or `None` when there is none.
#[must_use]
pub fn index_lines(payload: &str) -> Option<Vec<&str>> {
    let separator_at = payload
        .lines()
        .position(|line| !line.trim().is_empty() && line.trim().chars().all(|c| c == DASH))?;
    Some(
        payload
            .lines()
            .skip(separator_at + 1)
            .filter(|line| !line.trim().is_empty())
            .collect(),
    )
}

/// One index line read from both ends: form type, then a company name of unknown word count,
/// then CIK, date and path. `None` for a line of fewer than four fields.
#[must_use]
pub fn index_row(line: &str) -> Option<IndexRow> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let [form, .., cik, date, path] = fields.as_slice() else {
        return None;
    };
    Some(IndexRow {
        form: (*form).to_owned(),
        cik: (*cik).to_owned(),
        filed: (*date).to_owned(),
        path: (*path).to_owned(),
    })
}

/// A filing as an index row names it, every field checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filing {
    /// Decimal digits with no leading zero, so `0000320193` and `320193` are one filer.
    pub cik: String,
    pub accession: String,
    pub filed: NaiveDate,
}

/// `##########-##-######`, the accession's digit groups.
const ACCESSION_GROUPS: [usize; 3] = [10, 2, 6];
/// `YYYYMMDD`: chrono's `%m` and `%d` also take one digit, so `2026111` parses as 2026-11-01.
const INDEX_DATE_WIDTH: usize = 8;

/// The filing a row names, or `None` unless the CIK is digits, the date is `YYYYMMDD` and the
/// path is `edgar/data/<the same CIK>/<accession>.txt`: a cut or damaged line is never a filing.
#[must_use]
pub fn filing_of(row: &IndexRow) -> Option<Filing> {
    let number = |text: &str| {
        (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| text.parse::<u64>().ok())
            .flatten()
    };
    let cik = number(&row.cik)?;
    let ["edgar", "data", directory, file] = row.path.split('/').collect::<Vec<_>>()[..] else {
        return None;
    };
    let accession = file.strip_suffix(".txt")?;
    let groups: Vec<&str> = accession.split('-').collect();
    let shaped = groups.len() == ACCESSION_GROUPS.len()
        && groups.iter().zip(ACCESSION_GROUPS).all(|(group, width)| {
            group.len() == width && group.bytes().all(|byte| byte.is_ascii_digit())
        });
    let dated = row.filed.len() == INDEX_DATE_WIDTH;
    (shaped && dated && number(directory)? == cik).then_some(())?;
    Some(Filing {
        cik: cik.to_string(),
        accession: accession.to_owned(),
        filed: NaiveDate::parse_from_str(&row.filed, INDEX_DATE).ok()?,
    })
}

/// A line of the form `<TAG>value`, which is how the filing header carries its acceptance instant.
#[must_use]
pub fn tagged(text: &str, tag: &str) -> Option<String> {
    let opening = format!("<{tag}>");
    text.lines()
        .find_map(|line| line.trim().strip_prefix(&opening))
        .map(|value| value.trim().to_owned())
}

/// A line of the form `LABEL:<whitespace>value`, which is how it carries the rest.
#[must_use]
pub fn labelled(text: &str, label: &str) -> Option<String> {
    let opening = format!("{label}:");
    text.lines()
        .find_map(|line| line.trim().strip_prefix(&opening))
        .map(|value| value.trim().to_owned())
}

/// A filing's acceptance digits as an instant: `raw` read by `format` as local time in `zone`,
/// the earlier instant when the local time is ambiguous. `None` when either does not parse.
#[must_use]
pub fn acceptance_instant(raw: &str, format: &str, zone: &str) -> Option<DateTime<Utc>> {
    let local = NaiveDateTime::parse_from_str(raw.trim(), format).ok()?;
    let zone: Tz = zone.parse().ok()?;
    Some(
        zone.from_local_datetime(&local)
            .earliest()?
            .with_timezone(&Utc),
    )
}

/// The body of SEC's 200, for the caller to read, or the verdict every other answer closes with.
/// The shared-IP traffic 403 is TRANSIENT: it cleared by waiting (predecessor issues #379, #382,
/// #383). Permanent: the S3 403 `AccessDenied`, and a body over the limit whatever its status.
pub fn body_of_200(answer: &Result<RawResponse, TransportError>) -> Result<Cow<'_, str>, Verdict> {
    let response = match answer {
        Ok(response) => response,
        Err(failed) if failed.kind() == TransportKind::BodyOverLimit => {
            return Err(Verdict::PermanentFailure)
        }
        Err(_) => return Err(Verdict::TransientFailure),
    };
    let body = String::from_utf8_lossy(response.body());
    match response.status() {
        OK => Ok(body),
        FORBIDDEN if body.contains(ACCESS_DENIED) => Err(Verdict::PermanentFailure),
        FORBIDDEN | TOO_MANY_REQUESTS | REQUEST_TIMEOUT => Err(Verdict::TransientFailure),
        status if SERVER_ERRORS.contains(&status) => Err(Verdict::TransientFailure),
        _ => Err(Verdict::PermanentFailure),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(text: &str) -> Option<NaiveDate> {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()
    }

    /// Each quarter's first and last day. Mutation 2026-11-25, `month / 3 + 1`: red here, and
    /// 0 red of 635 before this test, whose callers' url tests all use a November day.
    #[test]
    fn a_day_is_filed_under_its_calendar_quarter() {
        let quarters = [
            ("2026-01-01", 1),
            ("2026-03-31", 1),
            ("2026-04-01", 2),
            ("2026-06-30", 2),
            ("2026-07-01", 3),
            ("2026-09-30", 3),
            ("2026-10-01", 4),
            ("2026-12-31", 4),
        ];
        for (text, quarter) in quarters {
            let listing = day(text).map(listing_url);
            let expected =
                format!("https://{HOST}/Archives/edgar/daily-index/2026/QTR{quarter}/index.json");
            assert_eq!(listing, Some(expected), "{text}");
        }
    }

    /// Mutations 2026-11-27, each red here: `last_day` one day short; `previous` returning the
    /// quarter itself.
    #[test]
    fn a_quarter_spans_its_three_months_and_the_previous_one_crosses_a_year() {
        let first = Quarter::of(day("2026-01-15").unwrap_or_default());
        assert_eq!(
            (
                first.label(),
                Some(first.first_day()),
                Some(first.last_day()),
                first.previous().label(),
                Some(first.previous().last_day()),
            ),
            (
                "2026/QTR1".to_owned(),
                day("2026-01-01"),
                day("2026-03-31"),
                "2025/QTR4".to_owned(),
                day("2025-12-31"),
            )
        );
    }

    /// Section 12.2's split of SEC's ceiling: the filings row is 500 ms per IP and per host, the
    /// other two 110 ms. Mutation 2026-11-27, `Caller::Filings` at `SEC_MIN_INTERVAL`: red here.
    #[test]
    fn the_filings_caller_paces_our_address_and_the_host_at_500_ms() {
        let paced = |caller| {
            let budget = transport(caller, 1).budget;
            (budget.min_interval, budget.per_host)
        };
        let filings = Some(std::time::Duration::from_millis(500));
        let others = Some(std::time::Duration::from_millis(110));
        assert_eq!(
            [Caller::Filings, Caller::Roster, Caller::EdgarPress].map(paced),
            [(filings, filings), (others, others), (others, others)]
        );
    }

    /// Mutation 2026-11-25, an address needing a local part OR a dotted domain: red here, and
    /// 0 red of 637 before this test.
    #[test]
    fn a_contact_is_addressed_by_a_token_with_a_local_part_and_a_dotted_domain() {
        assert!(contact_is_addressed("Example Research desk@example.test"));
        for unaddressed in [
            "",
            "collector/0.1",
            "desk@example",
            "@example.test",
            "a @ b.c",
        ] {
            assert!(!contact_is_addressed(unaddressed), "{unaddressed}");
        }
    }

    /// Mutation 2026-11-25, `company.` or `master.` names counted beside `form.`: red here, and
    /// 0 red of 635 before this test, whose fixture names no day `form.` does not name too.
    #[test]
    fn a_day_only_the_company_or_the_master_index_names_is_not_listed() {
        let listing = br#"{"directory": {"item": [
            {"name": "company.20261109.idx"}, {"name": "master.20261110.idx"},
            {"name": "form.20261112.idx"}, {"name": "index.json"}]}}"#;
        assert_eq!(
            listed_days(listing),
            Ok(vec![day("2026-11-12")].into_iter().flatten().collect())
        );
    }

    /// A `form.*.idx` name whose date is not eight digits of a real day refuses the listing:
    /// chrono reads `2026111` as 2026-11-01 (review of 2026-11-27). Mutation 2026-11-27, the
    /// width check dropped: red here.
    #[test]
    fn a_form_index_name_that_is_no_yyyymmdd_day_refuses_the_listing() {
        let named = |name: &str| {
            let body = format!(r#"{{"directory": {{"item": [{{"name": "{name}"}}]}}}}"#);
            listed_days(body.as_bytes()).map_err(|_| "unparseable")
        };
        assert_eq!(
            [
                "form.2026111.idx",
                "form.2026XX12.idx",
                "form.20261131.idx",
                "form.20261112.idx"
            ]
            .map(named),
            [
                Err("unparseable"),
                Err("unparseable"),
                Err("unparseable"),
                Ok(vec![day("2026-11-12")].into_iter().flatten().collect())
            ]
        );
    }

    /// A body with no `directory.item` array is no listing; an empty array is a listing of no
    /// day. Before 2026-11-27 `{}` read as no day, and filingvault held it as a complete quarter;
    /// `edgar-press`'s reading keeps that until the owner decides.
    #[test]
    fn a_body_with_no_item_array_is_unparseable_and_an_empty_one_lists_no_day() {
        let read = [
            &b"{}"[..],
            br#"{"directory": {"item": {}}}"#,
            br#"{"directory": {"item": []}}"#,
        ]
        .map(|body| listed_days(body).map_err(|_| "unparseable"));
        assert_eq!(
            (
                read,
                listed_days_or_none(b"{}"),
                listed_days_or_none(b"<html>").is_err()
            ),
            (
                [Err("unparseable"), Err("unparseable"), Ok(Vec::new())],
                Ok(Vec::new()),
                true
            )
        );
    }
}
