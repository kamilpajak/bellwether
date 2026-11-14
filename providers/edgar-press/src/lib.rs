//! Our EDGAR press-release collector, the first [`Provider`] and the only crate a buy deletes.
//! It declares no HTTP dependency and holds no clock and no `Sink`.
//!
//! Measured on six consecutive dates 2026-11-12 with a contact-shaped User-Agent: EDGAR answers a
//! Saturday 403/216 B, a Sunday 403/212 B, a federal holiday 403/219 B and an unpublished day
//! 403/219 B, all three bodies the same generic S3 `<Error><Code>AccessDenied</Code>`. A verdict
//! cannot tell those apart, so the question is PLANNING: `plan` emits no request for a non-session
//! day and asks only for days the quarter listing names. "Not available yet" is a planning fact
//! with a row, never a vendor verdict, and there is no `Verdict::Premature` to reach.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use canon::{Digest, FileDigest};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use news_provider::budget::{BudgetScope, RequestBudget};
use news_provider::provider::{
    precision_of, probed_lag, Calendar, CollectedItem, Extracted, ExtractedBody, Listing,
    NotPlanned, Plan, PlanRefusal, Provenance, Provider, RawBlob, UnusableReason, UnusableRow,
    ValidPrecision, ValidSource,
};
use news_provider::request::{days_of, HeaderValue, Request, RequestPhase};
use news_provider::wire::{RawResponse, Timeouts, TransportError, TransportProfile};
use news_provider::{CredentialRef, SourceId, TargetWindow, Verdict};
use pit::datedness::{end_of_day, Published};
use pit::{MeasuredLag, ObservationTime, PitError, ValidTime, Visibility};

/// The adapter's own name, which is the only thing that tells ours from a purchased one besides
/// the [`Provenance`] it returns.
pub const SOURCE: &str = "edgar_press";
/// The credential a run must hold. SEC answers a User-Agent with no contact address with 403 and
/// one with a contact with 200, re-measured 2026-11-12, and the address may never reach a plan or
/// a ledger row (architecture.md section 3.2's `filter_params` row), so it travels by NAME.
pub const CONTACT_CREDENTIAL: &str = "sec_contact";

const ARCHIVES: &str = "https://www.sec.gov/Archives/";
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
/// Where the day-precision fallback's value comes from: the one index column carrying a date and
/// no hour. Taken from [`COLUMNS`] so the heading has one home.
const DATE_FILED: &str = COLUMNS[3];
/// The one zone name this binary knows, because `Published::On` carries a `&'static str` and a
/// zone read out of a file is not static. EDGAR files in Eastern time.
const KNOWN_ZONES: [&str; 1] = ["America/New_York"];
const RULES_SCHEMA: &str = "edgar-press.rules/1";
const HEADER_END: &str = "</SEC-HEADER>";
const ACCESS_DENIED: &str = "<Code>AccessDenied</Code>";
const TOO_MANY_REQUESTS: u16 = 429;
const SERVER_ERRORS: std::ops::RangeInclusive<u16> = 500..=599;
const OK: u16 = 200;
const FORBIDDEN: u16 = 403;

/// Every timeout, as one value. An ASSUMPTION taken by default and not a measurement: nothing in
/// this tree has timed an EDGAR response, so there is no quantile to quote. `transport()` below is
/// the one place to change it.
const VENDOR_PATIENCE: std::time::Duration = std::time::Duration::from_secs(30);
/// Construction: the 2026-11-10 daily index holds 5,429 filing rows (measured by download
/// 2026-11-12) at the 110-byte fixed width the committed fixture's header carries, so a full index
/// is about 600 KB. The order of magnitude of room is an assumption, like the timeout above.
const BODY_LIMIT: u64 = 8 * 1_024 * 1_024;
/// 1 s / 10 requests. SEC's fair-access ceiling is 10 requests a second per IP as the
/// predecessor's vendor-quota table states it (read 2026-11-13 from
/// `apps/alphalens-pipeline/CLAUDE.md`); nothing here has measured it, and this paces TO the
/// ceiling with no margin under it, which row R19 names as the open question it is.
const SEC_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Why no request may be made at all. Every one of these is raised BEFORE the first request, so a
/// broken input is never a vendor event in the coverage ledger.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Refusal {
    /// Registry code `roster_absent`, exit 4. R6: a missing dictionary once produced an audit
    /// result of 0, indistinguishable from a clean pass.
    #[error("no roster at {0}")]
    RosterAbsent(PathBuf),
    /// Registry code `input_unparseable`, exit 2. The predecessor's `_load_cik_to_ticker` catches
    /// this and returns `{}` with a warning, and the run then reports a quiet day.
    #[error("the roster at {path} is present and is not a CIK map: {why}")]
    RosterUnparseable { path: PathBuf, why: String },
    /// Registry code `roster_absent`, exit 4. An empty roster keeps every row and is the same
    /// silent zero as an absent one.
    #[error("the roster at {0} names no CIK")]
    RosterEmpty(PathBuf),
    #[error("no extraction rules at {0}")]
    RulesAbsent(PathBuf),
    #[error("the rules at {path} are present and are not {RULES_SCHEMA}: {why}")]
    RulesUnparseable { path: PathBuf, why: String },
    #[error("the rules at {path} name zone {zone}, which this binary does not know")]
    RulesZoneUnknown { path: PathBuf, zone: String },
    /// Registry code `input_unparseable`, exit 2. A listing nothing can read would plan zero
    /// requests, which reads as a closed market.
    #[error("the quarter listing is present and is not a directory listing: {0}")]
    ListingUnparseable(String),
    /// Registry code `credential_contact_missing`, exit 2. A startup refusal and never a vendor
    /// failure recorded in the coverage ledger (design.md section 3).
    #[error("the {CONTACT_CREDENTIAL} credential carries no contact address")]
    ContactMissing,
}

/// The in-universe CIK set as read, with the digest every request carries.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Roster {
    ciks: BTreeSet<u64>,
    version: FileDigest,
}

/// The hashed rules as parsed. Every field below is read by `extract`, so a rules edit that
/// changes the extracted bytes changes `body_ref` and therefore `item_id`, while one that leaves
/// the bytes identical changes only this crate's `extract_rules_version`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Parsed {
    form_types: BTreeSet<String>,
    drop_elements: Vec<String>,
    strip_tags: bool,
    collapse_whitespace: bool,
    entities: BTreeMap<String, String>,
    acceptance_field: String,
    acceptance_format: String,
    zone: &'static str,
    accession_field: String,
    company_field: String,
    form_field: String,
    host_independent: bool,
}

#[derive(Debug)]
pub struct EdgarPress {
    roster: Roster,
    /// sha256 of the rules file as read, which architecture.md section 2 calls `rule_version`.
    /// The bytes are not kept: `Parsed` below is what `extract` reads, and a second copy of the
    /// file nothing reads is the shape CONTRIBUTING rule 2 names.
    rules_version: FileDigest,
    parsed: Parsed,
    lag: Option<MeasuredLag>,
}

impl EdgarPress {
    /// Reads the roster and the rules and refuses before any request exists. The contact is
    /// checked and NOT stored: nothing in this crate can put an address into a request.
    pub fn open(roster: &Path, rules: &Path, contact: &str) -> Result<Self, Refusal> {
        usable_contact(contact)?;
        let roster_bytes =
            std::fs::read(roster).map_err(|_| Refusal::RosterAbsent(roster.to_path_buf()))?;
        let read: BTreeMap<String, String> =
            serde_json::from_slice(&roster_bytes).map_err(|why| Refusal::RosterUnparseable {
                path: roster.to_path_buf(),
                why: why.to_string(),
            })?;
        let ciks: BTreeSet<u64> = read.keys().filter_map(|cik| cik.parse().ok()).collect();
        if ciks.is_empty() {
            return Err(Refusal::RosterEmpty(roster.to_path_buf()));
        }
        let rules_bytes =
            std::fs::read(rules).map_err(|_| Refusal::RulesAbsent(rules.to_path_buf()))?;
        let parsed = parse_rules(&rules_bytes, rules)?;
        Ok(Self {
            roster: Roster {
                ciks,
                version: FileDigest::of(&roster_bytes),
            },
            rules_version: FileDigest::of(&rules_bytes),
            parsed,
            lag: None,
        })
    }

    /// The probed lag a `vendor_archive` window is dated through. Without one such a window is
    /// refused: decision D5.
    #[must_use]
    pub fn with_observation_lag(mut self, lag: MeasuredLag) -> Self {
        self.lag = Some(lag);
        self
    }

    /// The quarter listing's url for a day, which is what `plan` is asked to consult.
    #[must_use]
    pub fn listing_url(day: NaiveDate) -> String {
        format!(
            "{ARCHIVES}{DAILY_INDEX}/{}/QTR{}/index.json",
            day.format("%Y"),
            quarter(day)
        )
    }

    fn index_request(&self, day: NaiveDate, provenance: Provenance) -> Request {
        Request {
            source: SourceId::new(SOURCE),
            target_window: TargetWindow::new(day.to_string()),
            phase: RequestPhase::Index,
            provenance,
            url: format!(
                "{ARCHIVES}{DAILY_INDEX}/{}/QTR{}/form.{}.idx",
                day.format("%Y"),
                quarter(day),
                day.format("%Y%m%d")
            ),
            headers: Vec::new(),
            params: Vec::new(),
            universe_version: self.roster.version.clone(),
        }
    }

    /// A follow-up inherits its index request's provenance: the two read the same window under the
    /// same retrieval model, and R10's rule is that two retrieval models are two populations.
    fn document_request(&self, request: &Request, path: &str) -> Request {
        Request {
            source: SourceId::new(SOURCE),
            target_window: request.target_window.clone(),
            phase: RequestPhase::Document,
            provenance: request.provenance,
            url: format!("{ARCHIVES}{path}"),
            headers: Vec::new(),
            params: Vec::new(),
            universe_version: self.roster.version.clone(),
        }
    }

    /// The daily index: the follow-ups are the in-universe subset, and every other row is counted.
    fn index_rows_of(&self, request: &Request, raw: &RawBlob) -> Extracted {
        // An unreadable payload is `classify`'s refusal, not a count of zero: a 200 whose body is
        // not an index closes `PermanentFailure` and nothing calls this.
        let rows = index_rows(&String::from_utf8_lossy(&raw.bytes)).unwrap_or_default();
        let mut follow_ups = Vec::new();
        let mut filtered_out = 0;
        for row in &rows {
            let in_form = self.parsed.form_types.contains(&row.form);
            let in_universe = row
                .cik
                .parse()
                .is_ok_and(|cik| self.roster.ciks.contains(&cik));
            if in_form && in_universe {
                follow_ups.push(self.document_request(request, &row.path));
            } else {
                filtered_out += 1;
            }
        }
        Extracted {
            items: Vec::new(),
            follow_ups,
            unusable: Vec::new(),
            vendor_rows: u32::try_from(rows.len()).unwrap_or(u32::MAX),
            filtered_out,
        }
    }

    /// One filing document, which is the one row a document payload holds.
    fn document_of(&self, request: &Request, raw: &RawBlob) -> Extracted {
        let text = String::from_utf8_lossy(&raw.bytes).to_string();
        let accession = labelled(&text, &self.parsed.accession_field).unwrap_or_default();
        let company = labelled(&text, &self.parsed.company_field).unwrap_or_default();
        let form = labelled(&text, &self.parsed.form_field).unwrap_or_default();
        let (valid, valid_raw, valid_field) = self.publication(&text, &request.target_window);
        let url = request.url.clone();
        let observation = self.backfill_observation(request.provenance, valid);
        let item = CollectedItem {
            source: SourceId::new(SOURCE),
            source_item_id: accession.clone(),
            canonical_url: self.canonical(&url),
            url,
            title: format!("{company} {form}").trim().to_owned(),
            valid,
            valid_raw,
            valid_field,
            valid_source: ValidSource::VendorPublished,
            provenance: request.provenance,
            observed: observation.as_ref().map_or(None, |placed| *placed),
            body: self.body(&text),
        };
        let unusable = if accession.trim().is_empty() {
            Some(UnusableReason::BlankSourceItemId)
        } else if precision_of(&item.valid) == ValidPrecision::Unknown {
            Some(UnusableReason::NoParseableValidTime)
        } else if observation.is_err() {
            Some(UnusableReason::NoRepresentableObservation)
        } else {
            None
        };
        match unusable {
            Some(reason) => Extracted {
                items: Vec::new(),
                follow_ups: Vec::new(),
                unusable: vec![UnusableRow {
                    reason,
                    item,
                    raw_blob: raw.digest.clone(),
                }],
                vendor_rows: 1,
                filtered_out: 0,
            },
            None => Extracted {
                items: vec![item],
                follow_ups: Vec::new(),
                unusable: Vec::new(),
                vendor_rows: 1,
                filtered_out: 0,
            },
        }
    }

    /// The acceptance instant when it parses, and the index's own `Date Filed` DAY when it does
    /// not - never a fabricated midnight, which is what carried 13 h of lookahead through the
    /// predecessor's lake (architecture.md section 3.1's `valid_precision` row).
    fn publication(&self, text: &str, window: &TargetWindow) -> (Published, String, String) {
        if let Some(raw) = tagged(text, &self.parsed.acceptance_field) {
            if let Some(at) = self.instant(&raw) {
                return (
                    Published::At(ValidTime::new(at)),
                    raw,
                    self.parsed.acceptance_field.clone(),
                );
            }
        }
        match days_of(window).and_then(|days| days.first().copied()) {
            Some(day) => (
                Published::On {
                    day,
                    tz: self.parsed.zone,
                },
                day.to_string(),
                DATE_FILED.to_owned(),
            ),
            None => (
                Published::Unknown,
                window.as_str().to_owned(),
                DATE_FILED.to_owned(),
            ),
        }
    }

    /// Where a backfilled item's observation goes: its own publication plus the probed lag, via
    /// [`Visibility::from_backfill`], which is the only constructor that takes a [`MeasuredLag`].
    /// `Ok(None)` for a live ingest, whose observation is the fetch instant the driver holds.
    ///
    /// A DATE has no instant, so its publication bound is the END of that day in its own calendar
    /// and the lag is added to that: an upper bound on top of an upper bound, which can only make
    /// an item less visible. Which `Ok(None)` arms an archive item can reach: section 8 row R11.
    fn backfill_observation(
        &self,
        provenance: Provenance,
        valid: Published,
    ) -> Result<Option<ObservationTime>, PitError> {
        if provenance != Provenance::VendorArchive {
            return Ok(None);
        }
        let Some(lag) = self.lag.as_ref() else {
            return Ok(None);
        };
        let bound = match valid {
            Published::At(instant) => instant,
            Published::On { day, tz } => match end_of_day(day, tz) {
                Some(ends) => ValidTime::new(ends),
                None => return Ok(None),
            },
            Published::Unknown => return Ok(None),
        };
        Visibility::from_backfill(bound, lag).map(|visible| Some(visible.observed()))
    }

    fn instant(&self, raw: &str) -> Option<DateTime<Utc>> {
        let local =
            NaiveDateTime::parse_from_str(raw.trim(), &self.parsed.acceptance_format).ok()?;
        let zone: Tz = self.parsed.zone.parse().ok()?;
        Some(
            zone.from_local_datetime(&local)
                .earliest()?
                .with_timezone(&Utc),
        )
    }

    /// `doc_id`'s input. EDGAR serves one document from more than one host, so the canonical form
    /// drops the scheme and the host and keeps the archive path.
    fn canonical(&self, url: &str) -> String {
        if !self.parsed.host_independent {
            return url.to_owned();
        }
        match url
            .split_once("//")
            .and_then(|(_, rest)| rest.split_once('/'))
        {
            Some((_, path)) => format!("/{path}"),
            None => url.to_owned(),
        }
    }

    /// Article body text, never page HTML: requirement R17 matches over the body.
    fn body(&self, text: &str) -> Option<ExtractedBody> {
        let document = match text.split_once(HEADER_END) {
            Some((_, after)) => after,
            None => text,
        };
        let mut body = document.to_owned();
        for element in &self.parsed.drop_elements {
            body = without_element(&body, element);
        }
        if self.parsed.strip_tags {
            body = without_tags(&body);
        }
        for (entity, replacement) in &self.parsed.entities {
            body = body.replace(entity, replacement);
        }
        if self.parsed.collapse_whitespace {
            body = body.split_whitespace().collect::<Vec<&str>>().join(" ");
        }
        let body = body.trim().to_owned();
        (!body.is_empty()).then(|| ExtractedBody::of(body))
    }
}

impl Provider for EdgarPress {
    fn source(&self) -> SourceId {
        SourceId::new(SOURCE)
    }

    fn transport(&self) -> TransportProfile {
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
            body_limit: BODY_LIMIT,
            // Per IP: the contact above identifies this caller and does not scope the quota, so
            // every process on this host shares one bucket however many of them there are.
            budget: RequestBudget {
                min_interval: SEC_MIN_INTERVAL,
                scope: BudgetScope::PerIp,
            },
        }
    }

    /// Per REQUEST. A day the run is in is a live ingest; an earlier day is read after the fact
    /// and its observation is publication plus a lag somebody has to have measured. The boundary
    /// is the run's own UTC date and no other number, because no measurement places one.
    fn provenance(&self, window: &TargetWindow, at: pit::ObservationTime) -> Provenance {
        match days_of(window).and_then(|days| days.last().copied()) {
            Some(last) if last >= at.inner().date_naive() => Provenance::LiveIngest,
            // A window nothing can read takes the arm that refuses rather than the one that asks.
            Some(_) | None => Provenance::VendorArchive,
        }
    }

    fn precision_ceiling(&self) -> ValidPrecision {
        ValidPrecision::Instant
    }

    fn universe_version(&self) -> FileDigest {
        self.roster.version.clone()
    }

    fn plan(
        &self,
        target: &TargetWindow,
        calendar: &Calendar,
        listing: &Listing,
        at: pit::ObservationTime,
    ) -> Result<Plan, PlanRefusal> {
        let Some(days) = days_of(target) else {
            return Err(PlanRefusal::TargetWindowUnreadable(
                target.as_str().to_owned(),
            ));
        };
        if days.is_empty() {
            return Ok(Plan {
                requests: Vec::new(),
                not_planned: vec![(target.clone(), NotPlanned::PlanEmpty)],
            });
        }
        let mut requests = Vec::new();
        let mut not_planned = Vec::new();
        for day in days {
            let window = TargetWindow::new(day.to_string());
            if !calendar.is_session(day) {
                not_planned.push((window, NotPlanned::CalendarClosed));
                continue;
            }
            if !listing.names(day) {
                not_planned.push((window, NotPlanned::WindowNotPublished));
                continue;
            }
            let provenance = self.provenance(&window, at);
            if provenance == Provenance::VendorArchive {
                probed_lag(&self.source(), &window, self.lag.as_ref())?;
            }
            requests.push(self.index_request(day, provenance));
        }
        Ok(Plan {
            requests,
            not_planned,
        })
    }

    /// From the BODY as well as the status. The shared-IP traffic 403 is TRANSIENT because the
    /// predecessor measured it clearing by waiting (its issues #379, #382, #383); only the generic
    /// S3 `AccessDenied` is permanent, and only because `plan` asked for a day the listing named.
    fn classify(&self, request: &Request, answer: &Result<RawResponse, TransportError>) -> Verdict {
        let Ok(response) = answer else {
            return Verdict::TransientFailure;
        };
        let body = String::from_utf8_lossy(response.body());
        match response.status() {
            OK => match request.phase {
                RequestPhase::Index => match index_rows(&body) {
                    None => Verdict::PermanentFailure,
                    Some(rows) if rows.is_empty() => Verdict::Empty,
                    Some(_) => Verdict::Usable,
                },
                RequestPhase::Document => {
                    if body.contains(HEADER_END) {
                        Verdict::Usable
                    } else {
                        Verdict::PermanentFailure
                    }
                }
            },
            FORBIDDEN if body.contains(ACCESS_DENIED) => Verdict::PermanentFailure,
            FORBIDDEN | TOO_MANY_REQUESTS => Verdict::TransientFailure,
            status if SERVER_ERRORS.contains(&status) => Verdict::TransientFailure,
            _ => Verdict::PermanentFailure,
        }
    }

    fn extract(&self, request: &Request, raw: &RawBlob) -> Extracted {
        match request.phase {
            RequestPhase::Index => self.index_rows_of(request, raw),
            RequestPhase::Document => self.document_of(request, raw),
        }
    }

    fn extract_rules_version(&self) -> FileDigest {
        self.rules_version.clone()
    }

    /// A `vendor_archive` follow-up whose story the store already holds a committed item for is
    /// dropped, so a re-run fetches only the uncovered accessions and converges on full coverage.
    /// A `live_ingest` follow-up is always issued, because a live sweep must poll every matched
    /// row (§8 row R16): the covered set is consulted for the archive arm alone. A follow-up whose
    /// accession the url does not carry is never dropped - a skip we cannot justify is a hole.
    fn follow_ups_to_issue(
        &self,
        follow_ups: Vec<Request>,
        covered: &BTreeSet<String>,
    ) -> Vec<Request> {
        follow_ups
            .into_iter()
            .filter(|follow_up| {
                if follow_up.provenance != Provenance::VendorArchive {
                    return true;
                }
                match story_of(follow_up) {
                    Some(story) => !covered.contains(story.hex()),
                    None => true,
                }
            })
            .collect()
    }
}

/// The story a follow-up document request belongs to, taken over the accession its url carries.
/// `story_id` is `canon` over the vendor row identity, which for EDGAR is the accession, so the
/// covered set is matched by computing the digest from the index-derived url rather than by
/// storing it. `None` when the url carries no accession.
fn story_of(follow_up: &Request) -> Option<Digest> {
    let accession = accession_of(&follow_up.url)?;
    canon::digest::story_id(SOURCE, &accession).ok()
}

/// The accession an EDGAR document url ends in:
/// `.../edgar/data/320193/0000320193-26-000071.txt` -> `0000320193-26-000071`. The same string
/// the filing header's `ACCESSION NUMBER` carries, so a follow-up's story matches the committed
/// item's.
fn accession_of(url: &str) -> Option<String> {
    let last = url.rsplit('/').next()?;
    let stem = last.strip_suffix(".txt").unwrap_or(last);
    (!stem.is_empty()).then(|| stem.to_owned())
}

/// The quarter's listing, read into the seam's [`Listing`]. Only `form.<date>.idx` names count: a
/// quarter directory also holds `company.`, `master.` and `index.json` itself.
pub fn read_listing(bytes: &[u8]) -> Result<Listing, Refusal> {
    let read: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|why| Refusal::ListingUnparseable(why.to_string()))?;
    let days: Vec<NaiveDate> = read["directory"]["item"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| day_of_index_name(item["name"].as_str()?))
                .collect()
        })
        .unwrap_or_default();
    Ok(Listing::of_days(&days))
}

fn day_of_index_name(name: &str) -> Option<NaiveDate> {
    let digits = name.strip_prefix("form.")?.strip_suffix(".idx")?;
    NaiveDate::parse_from_str(digits, "%Y%m%d").ok()
}

/// SEC asks for a name and an address. Checked at startup and never stored, so a run with no
/// contact opens no attempt row at all.
fn usable_contact(contact: &str) -> Result<(), Refusal> {
    let addressed = contact.split_whitespace().any(|token| {
        token
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
    });
    if addressed {
        Ok(())
    } else {
        Err(Refusal::ContactMissing)
    }
}

fn quarter(day: NaiveDate) -> u32 {
    (chrono::Datelike::month(&day) - 1) / 3 + 1
}

fn parse_rules(bytes: &[u8], path: &Path) -> Result<Parsed, Refusal> {
    let unparseable = |why: String| Refusal::RulesUnparseable {
        path: path.to_path_buf(),
        why,
    };
    let read: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|why| unparseable(why.to_string()))?;
    let schema = read["schema"].as_str().unwrap_or_default();
    if schema != RULES_SCHEMA {
        return Err(unparseable(format!("schema {schema}")));
    }
    let zone_name = read["acceptance"]["tz"].as_str().unwrap_or_default();
    let Some(zone) = KNOWN_ZONES.into_iter().find(|known| *known == zone_name) else {
        return Err(Refusal::RulesZoneUnknown {
            path: path.to_path_buf(),
            zone: zone_name.to_owned(),
        });
    };
    Ok(Parsed {
        form_types: strings(&read["form_types"]).into_iter().collect(),
        drop_elements: strings(&read["body"]["drop_elements"]),
        strip_tags: read["body"]["strip_tags"].as_bool().unwrap_or(false),
        collapse_whitespace: read["body"]["collapse_whitespace"]
            .as_bool()
            .unwrap_or(false),
        entities: read["body"]["entities"]
            .as_object()
            .map(|table| {
                table
                    .iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default(),
        acceptance_field: read["acceptance"]["field"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        acceptance_format: read["acceptance"]["format"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        zone,
        accession_field: read["fields"]["accession_number"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        company_field: read["fields"]["company_name"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        form_field: read["fields"]["form_type"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        host_independent: read["url_canon"]["host_independent"]
            .as_bool()
            .unwrap_or(false),
    })
}

fn strings(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .map(|members| {
            members
                .iter()
                .filter_map(|member| Some(member.as_str()?.to_owned()))
                .collect()
        })
        .unwrap_or_default()
}

/// One row of a daily index.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexRow {
    form: String,
    cik: String,
    /// The index's own `Date Filed`, which is `YYYYMMDD` and not ISO - measured on the real file
    /// 2026-11-13, where the hand-written fixture had `2026-11-10`.
    filed: String,
    path: String,
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
fn index_rows(payload: &str) -> Option<Vec<IndexRow>> {
    let separator_at = payload
        .lines()
        .position(|line| !line.trim().is_empty() && line.trim().chars().all(|c| c == DASH))?;
    let rows = payload
        .lines()
        .skip(separator_at + 1)
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // Form type, then a company name of unknown word count, then CIK, date and path.
            let [form, .., cik, date, path] = fields.as_slice() else {
                return None;
            };
            Some(IndexRow {
                form: (*form).to_owned(),
                cik: (*cik).to_owned(),
                filed: (*date).to_owned(),
                path: (*path).to_owned(),
            })
        })
        .collect();
    Some(rows)
}

/// A line of the form `<TAG>value`, which is how the filing header carries its acceptance instant.
fn tagged(text: &str, tag: &str) -> Option<String> {
    let opening = format!("<{tag}>");
    text.lines()
        .find_map(|line| line.trim().strip_prefix(&opening))
        .map(|value| value.trim().to_owned())
}

/// A line of the form `LABEL:<whitespace>value`, which is how it carries the rest.
fn labelled(text: &str, label: &str) -> Option<String> {
    let opening = format!("{label}:");
    text.lines()
        .find_map(|line| line.trim().strip_prefix(&opening))
        .map(|value| value.trim().to_owned())
}

/// One element and its content gone, which is the difference between article text and page HTML:
/// a `<script>` holding a JSON data layer reads as prose once its tags are stripped.
fn without_element(text: &str, element: &str) -> String {
    let (open, close) = (format!("<{element}"), format!("</{element}>"));
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(&open) {
        out.push_str(rest.get(..start).unwrap_or_default());
        let after = rest.get(start..).unwrap_or_default();
        match after.find(&close) {
            Some(end) => rest = after.get(end + close.len()..).unwrap_or_default(),
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn without_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for character in text.chars() {
        match character {
            '<' => inside = true,
            '>' => {
                inside = false;
                out.push(' ');
            }
            _ if !inside => out.push(character),
            _ => {}
        }
    }
    out
}
