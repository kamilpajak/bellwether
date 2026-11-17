//! The adapter trait and the types a provider produces. Nothing in the workspace can tell our
//! collector from a purchased one except its [`SourceId`] and the [`Provenance`] it returns; no
//! method reads a clock, holds a [`crate::Sink`] or fetches. Which field means what:
//! `docs/design/architecture.md` sections 3.1 and 3.2.

use std::collections::BTreeSet;

use canon::{Digest, FileDigest};
use chrono::NaiveDate;
use pit::datedness::Published;
use pit::{MeasuredLag, ObservationTime};

use crate::request::{Request, TargetWindow};
use crate::wire::{RawResponse, TransportError, TransportProfile};
use crate::{SourceId, Verdict};

/// How an item reached us, which is architecture.md section 3.1's own enum: two sources with
/// different retrieval models are two populations and must never be silently pooled (R10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    LiveIngest,
    VendorArchive,
    /// No producer here. It stays because the enum is architecture.md section 3.1's contract and
    /// not this crate's; the first bulk dataset is its producer.
    BulkDataset,
}

impl Provenance {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::LiveIngest => "live_ingest",
            Self::VendorArchive => "vendor_archive",
            Self::BulkDataset => "bulk_dataset",
        }
    }
}

/// Where a publication value came from (architecture.md section 11.1's `ValidSource` per host).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidSource {
    /// The vendor stated it, in a field this provider names.
    VendorPublished,
    /// We read it out of the document under a hashed rule.
    OurExtraction,
}

impl ValidSource {
    /// The `items.valid_source` label, in one place.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::VendorPublished => "vendor_published",
            Self::OurExtraction => "our_extraction",
        }
    }
}

/// What architecture.md section 3.1's `valid_precision` column says, derived from the value rather
/// than carried beside it, so the two cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ValidPrecision {
    Instant,
    Minute,
    Day,
    Unknown,
}

impl ValidPrecision {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Instant => "instant",
            Self::Minute => "minute",
            Self::Day => "day",
            Self::Unknown => "unknown",
        }
    }
}

/// The publication value as `canon`'s stamp, which is `item_id`'s input. `None` for an unknown
/// value, because an item with no publication value has no identity to take over it - which is
/// why such a row is an [`UnusableRow`] and never an item.
#[must_use]
pub const fn stamp_of(published: &Published) -> Option<canon::Stamp> {
    match published {
        Published::At(valid) => Some(canon::Stamp::Instant(valid.inner())),
        Published::AtMinute(minute) => Some(canon::Stamp::Minute(minute.start())),
        Published::On { day, .. } => Some(canon::Stamp::Day(*day)),
        Published::Unknown => None,
    }
}

/// The precision of a publication value. `Ord` on [`ValidPrecision`] puts `Instant` first, so
/// "no finer than the ceiling" is `>=`.
#[must_use]
pub const fn precision_of(published: &Published) -> ValidPrecision {
    match published {
        Published::At(_) => ValidPrecision::Instant,
        Published::AtMinute(_) => ValidPrecision::Minute,
        Published::On { .. } => ValidPrecision::Day,
        Published::Unknown => ValidPrecision::Unknown,
    }
}

/// An extracted body and its state. There is no `Expired` arm: nothing expires a body until a
/// retention number exists (architecture.md section 3.9's L1 row), so it would be unconstructible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyState {
    Present { bytes: u64 },
}

/// Article body text, never page HTML: requirement R17 matches over the body, so what is stored
/// has to be the thing the matcher reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedBody {
    pub text: String,
    pub state: BodyState,
}

impl ExtractedBody {
    #[must_use]
    pub fn of(text: String) -> Self {
        let bytes = u64::try_from(text.len()).unwrap_or(u64::MAX);
        Self {
            text,
            state: BodyState::Present { bytes },
        }
    }
}

/// One item as a provider produces it, with the evidence columns beside the publication value so
/// a fabricated stamp has to forge evidence (architecture.md section 3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectedItem {
    pub source: SourceId,
    pub source_item_id: String,
    pub url: String,
    /// Canonicalised by a hashed rule, which is `doc_id`'s input (architecture.md section 2).
    pub canonical_url: String,
    pub title: String,
    pub valid: Published,
    /// The publication value as the payload spelled it, re-parseable by a replay.
    pub valid_raw: String,
    /// Which field of the payload `valid_raw` was read out of.
    pub valid_field: String,
    pub valid_source: ValidSource,
    pub provenance: Provenance,
    /// Where this item's own publication and the probed lag put its observation:
    /// [`pit::Visibility::from_backfill`]'s answer and nothing else. `None` for a live ingest,
    /// whose observation is the instant the fetch returned. A `vendor_archive` row carrying `None`
    /// is REFUSED by the store rather than falling back to that instant, which would date an
    /// archived item as though we had read it the moment it was published - §8 row R11's point.
    pub observed: Option<ObservationTime>,
    pub body: Option<ExtractedBody>,
}

/// Why a row cannot enter the archive as an item. A row is stored with its evidence rather than
/// dropped, so this is a label and not a discard (architecture.md section 5 row 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnusableReason {
    /// The predecessor's three real empty-string ids came back as ONE item with two publication
    /// disagreements; a blank id is not a dedup key (architecture.md section 8 row R3).
    BlankSourceItemId,
    /// No field of the payload yielded a publication value. Includes the sixtieth second, which
    /// `canon` refuses and Python's `fromisoformat` refuses too.
    NoParseableValidTime,
    /// A `vendor_archive` item whose publication plus the probed lag is past the last instant
    /// chrono holds, `pit::PitError::LagOutOfRange`: no observation can be placed for it.
    NoRepresentableObservation,
    /// A page whose article container is absent or yields zero paragraph text. The rule is fixed
    /// before its first measurement, as architecture.md section 11.3 requires of its threshold.
    NoArticleBody,
    /// A page whose bytes are not UTF-8; a lossy decode would store replacement characters.
    PageNotUtf8,
    /// A page nested deeper than its provider parses (architecture.md section 11.1).
    PageTooDeep,
    /// A page holding the article container more than once, so which one is the article is unknown.
    ArticleContainerAmbiguous,
}

/// Which input decided an [`UnusableReason`]. Not a method on the reason: `NoParseableValidTime`
/// is `IndexRow` in open-web and `Page` in edgar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecidedBy {
    /// The index row the request was planned from, which a later listing can correct: such a
    /// story is fetched again (architecture.md section 11.1).
    IndexRow,
    /// The fetched page itself, which a refetch of the same url would answer the same way.
    Page,
}

/// An unusable row carries the WHOLE item and the payload it came out of, so the evidence is in
/// the archive rather than in a log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnusableRow {
    pub reason: UnusableReason,
    pub decided_by: DecidedBy,
    pub item: CollectedItem,
    pub raw_blob: String,
}

/// What one payload held. The five counts add up: `vendor_rows` is what the payload carried and
/// every row of it is an item, a follow-up, an unusable row, a row one of our filters removed or a
/// row whose page the host's robots.txt disallows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted {
    pub items: Vec<CollectedItem>,
    pub follow_ups: Vec<Request>,
    pub unusable: Vec<UnusableRow>,
    /// Rows the payload HELD, before any filtering of ours.
    pub vendor_rows: u32,
    /// Rows our own filters removed - the roster and the form list, both hashed. The roster digest
    /// travels in `filter_params` (`Request::universe_version`).
    pub filtered_out: u32,
    /// Rows we would have followed whose url the host's pinned robots.txt disallows: never asked,
    /// and no attempt row (architecture.md section 11.1).
    pub robots_disallowed: u32,
    /// Document follow-ups no row of this payload lists: GNW's id walk (architecture.md section
    /// 11.2). Issued after `follow_ups` unless held or issued earlier in the run; never counted
    /// as matched, held or re-listed, and no vendor row.
    pub walk: Vec<Request>,
}

/// Why a target the window names was not asked for. Every target appears in a [`Plan`] as a
/// request or as one of these, so a calendar bug that drops every session day is a row per day
/// rather than an empty plan. `BudgetExhausted` is not here: no budget reserves anything yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotPlanned {
    /// The session calendar closes that day. EDGAR answers a weekend with 403 and a federal
    /// holiday with 403 carrying the same generic body as an unpublished day, measured on six
    /// dates 2026-11-12, so a verdict cannot tell them apart and the calendar has to.
    CalendarClosed,
    /// The vendor's own listing does not name that day yet.
    WindowNotPublished,
    /// The window names no day at all, which an inverted range does.
    PlanEmpty,
}

impl NotPlanned {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::CalendarClosed => "calendar_closed",
            Self::WindowNotPublished => "window_not_published",
            Self::PlanEmpty => "plan_empty",
        }
    }

    /// Whether not asking was the right answer. A clean reason keeps a run's exit at 0; an unclean
    /// one is incomplete work (architecture.md section 5.1's exit table).
    #[must_use]
    pub const fn clean(self) -> bool {
        match self {
            Self::CalendarClosed | Self::WindowNotPublished => true,
            Self::PlanEmpty => false,
        }
    }
}

/// What a run will ask for and what it will not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub requests: Vec<Request>,
    pub not_planned: Vec<(TargetWindow, NotPlanned)>,
}

/// A stored payload, as `extract` reads it back. Pure over (bytes, hashed rules), so
/// re-extraction under a new rules version is this same call over the same blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawBlob {
    pub digest: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CalendarError {
    #[error("the calendar states schema {0}, which this reader does not know")]
    UnknownSchema(String),
    #[error("the calendar is present and is not what the contract says: {0}")]
    Unparseable(String),
    /// R6: an empty fallback is forbidden, and a calendar naming no session would close every day.
    #[error("the calendar names no session")]
    NoSessions,
}

const CALENDAR_SCHEMA: &str = "calendar/1";

/// The session calendar, hashed. Its own file beside the rules file, so a calendar edit and a rule
/// edit are two diffs (architecture.md section 2's `calendar_version` row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Calendar {
    sessions: BTreeSet<NaiveDate>,
    version: FileDigest,
}

impl Calendar {
    /// The only constructor, so there is no second path that skips the schema and the R6 check.
    pub fn read(bytes: &[u8]) -> Result<Self, CalendarError> {
        let read: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|why| CalendarError::Unparseable(why.to_string()))?;
        let schema = read["schema"].as_str().unwrap_or_default();
        if schema != CALENDAR_SCHEMA {
            return Err(CalendarError::UnknownSchema(schema.to_owned()));
        }
        let sessions: BTreeSet<NaiveDate> = read["sessions"]
            .as_array()
            .map(|days| {
                days.iter()
                    .filter_map(|day| day.as_str()?.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        if sessions.is_empty() {
            return Err(CalendarError::NoSessions);
        }
        Ok(Self {
            sessions,
            version: FileDigest::of(bytes),
        })
    }

    #[must_use]
    pub fn is_session(&self, day: NaiveDate) -> bool {
        self.sessions.contains(&day)
    }

    #[must_use]
    pub const fn version(&self) -> &FileDigest {
        &self.version
    }
}

/// The days a vendor's own listing names. The provider parses its vendor's listing into one of
/// these; `plan` asks only for days it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    days: BTreeSet<NaiveDate>,
}

impl Listing {
    #[must_use]
    pub fn of_days(days: &[NaiveDate]) -> Self {
        Self {
            days: days.iter().copied().collect(),
        }
    }

    #[must_use]
    pub fn names(&self, day: NaiveDate) -> bool {
        self.days.contains(&day)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.days.len()
    }

    /// Here because `clippy::len_without_is_empty` is warn-by-default and CI runs `-D warnings`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.days.is_empty()
    }
}

/// Why no plan can be made. Not a [`NotPlanned`] row: a row is a target we chose not to ask for,
/// and these are states in which choosing is not possible.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanRefusal {
    #[error("target window {0} is neither a day nor a FROM..TO range")]
    TargetWindowUnreadable(String),
    /// Decision D5. `{probe_id: "", probes: 0, leaks: 0, seconds: 0}` is accepted by the type,
    /// `leak_rate()` returns `None`, and `from_backfill` then puts the observation at the
    /// publication instant - the leak section 8 row R11 records.
    #[error(
        "{source_id} has no probed observation lag, so window {window} cannot be dated: {why}"
    )]
    ObservationLagAbsent {
        source_id: String,
        window: String,
        why: &'static str,
    },
    /// The run's calendar and listing are not what the provider's [`Cadence`] takes. A calendar
    /// handed to an every-day provider would be silently ignored; without one a session-bound
    /// provider cannot plan.
    #[error("{source_id} is {cadence} and {why}")]
    InputsDoNotMatchCadence {
        source_id: String,
        cadence: &'static str,
        why: &'static str,
    },
    /// A live-only provider asked for any window but exactly the run's own UTC day
    /// (architecture.md section 11.1): no `vendor_archive` request can exist for it.
    #[error(
        "{source_id} collects only the run's UTC day {run_day}, and window {window} is not it"
    )]
    NotTheRunDay {
        source_id: String,
        window: String,
        run_day: NaiveDate,
    },
}

const HAS_NO_CALENDAR: &str = "has no calendar";
const HAS_NO_LISTING: &str = "has no listing";
const WAS_HANDED_A_CALENDAR: &str = "was handed a calendar";
const WAS_HANDED_A_LISTING: &str = "was handed a listing";

/// Which days a provider asks for, declared by the provider (architecture.md section 11.2's
/// conformance bullet). The driver refuses a run whose inputs do not match before anything is
/// planned, and the conformance harness holds only `SessionBound` to the listed-session check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    /// Only days a session calendar opens and the vendor's own listing names.
    SessionBound,
    /// Every UTC day, weekends included; takes no calendar and no listing.
    EveryUtcDay,
}

impl Cadence {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SessionBound => "session_bound",
            Self::EveryUtcDay => "every_utc_day",
        }
    }

    /// Refuses a calendar or listing this cadence does not take, and a missing one it needs.
    pub fn admits(
        self,
        source: &SourceId,
        calendar: Option<&Calendar>,
        listing: Option<&Listing>,
    ) -> Result<(), PlanRefusal> {
        match (self, calendar, listing) {
            (Self::SessionBound, _, _) => listed_sessions(source, calendar, listing).map(drop),
            (Self::EveryUtcDay, None, None) => Ok(()),
            (Self::EveryUtcDay, Some(_), _) => Err(mismatch(self, source, WAS_HANDED_A_CALENDAR)),
            (Self::EveryUtcDay, None, Some(_)) => Err(mismatch(self, source, WAS_HANDED_A_LISTING)),
        }
    }
}

fn mismatch(cadence: Cadence, source: &SourceId, why: &'static str) -> PlanRefusal {
    PlanRefusal::InputsDoNotMatchCadence {
        source_id: source.as_str().to_owned(),
        cadence: cadence.name(),
        why,
    }
}

/// The calendar and listing a [`Cadence::SessionBound`] plan reads, or the refusal naming which
/// one is missing.
pub fn listed_sessions<'a>(
    source: &SourceId,
    calendar: Option<&'a Calendar>,
    listing: Option<&'a Listing>,
) -> Result<(&'a Calendar, &'a Listing), PlanRefusal> {
    let session_bound = |why| mismatch(Cadence::SessionBound, source, why);
    let calendar = calendar.ok_or_else(|| session_bound(HAS_NO_CALENDAR))?;
    let listing = listing.ok_or_else(|| session_bound(HAS_NO_LISTING))?;
    Ok((calendar, listing))
}

const NO_LAG_AT_ALL: &str = "no lag entry";
const NO_PROBE_BEHIND_IT: &str = "no probe id";
const NO_PROBES_SO_NO_LEAK_RATE: &str = "zero probes, so no leak rate";
const A_LAG_OF_ZERO_IS_PUBLICATION_INSTANT_VISIBILITY: &str = "zero seconds";
const A_NEGATIVE_LAG_OBSERVES_BEFORE_PUBLICATION: &str = "negative seconds";
const A_LAG_NO_DURATION_HOLDS: &str = "more seconds than a duration holds";

/// The lag a `vendor_archive` window may be dated through, or why there is none. Called before the
/// first request, so an unprobed backfill is a refusal rather than a stored leak. Why here and not
/// in `LagPolicy::read`: architecture.md section 8 row R11.
pub fn probed_lag<'a>(
    source: &SourceId,
    window: &TargetWindow,
    lag: Option<&'a MeasuredLag>,
) -> Result<&'a MeasuredLag, PlanRefusal> {
    let refuse = |why: &'static str| PlanRefusal::ObservationLagAbsent {
        source_id: source.as_str().to_owned(),
        window: window.as_str().to_owned(),
        why,
    };
    let lag = lag.ok_or_else(|| refuse(NO_LAG_AT_ALL))?;
    if lag.probe_id.trim().is_empty() {
        return Err(refuse(NO_PROBE_BEHIND_IT));
    }
    if lag.leak_rate().is_none() {
        return Err(refuse(NO_PROBES_SO_NO_LEAK_RATE));
    }
    if lag.seconds == 0 {
        return Err(refuse(A_LAG_OF_ZERO_IS_PUBLICATION_INSTANT_VISIBILITY));
    }
    if lag.seconds < 0 {
        return Err(refuse(A_NEGATIVE_LAG_OBSERVES_BEFORE_PUBLICATION));
    }
    if chrono::TimeDelta::try_seconds(lag.seconds).is_none() {
        return Err(refuse(A_LAG_NO_DURATION_HOLDS));
    }
    Ok(lag)
}

/// One vendor adapter. No method is defaulted, none reads a clock, none holds a [`crate::Sink`]
/// and none fetches: the driver opens and closes every slot and [`crate::Wire`] owns the socket.
pub trait Provider: Send + Sync {
    fn source(&self) -> SourceId;

    /// The User-Agent template, the nine ureq timeouts and the body limit. The transport is this
    /// crate's; the profile is the provider's.
    fn transport(&self) -> TransportProfile;

    /// Per REQUEST, not per adapter: the same EDGAR adapter reading today's index is `live_ingest`
    /// and reading a 2024 index is `vendor_archive`. `at` is injected, because the two differ by
    /// where the window sits relative to the run and nothing here may read a clock.
    fn provenance(&self, window: &TargetWindow, at: ObservationTime) -> Provenance;

    /// The finest precision any item of this source can carry.
    fn precision_ceiling(&self) -> ValidPrecision;

    /// sha256 of the roster bytes as read, which every request this provider builds carries and
    /// every `Opened` row stores. On the trait because a run has to be able to name the roster it
    /// filtered by even when it planned no request at all (architecture.md section 8 row U1).
    fn universe_version(&self) -> FileDigest;

    /// Which days `plan` asks for, and so which of a calendar and a listing it takes. No default:
    /// a provider that forgot to declare would inherit a claim it never made.
    fn cadence(&self) -> Cadence;

    /// A [`Cadence::SessionBound`] plan takes the calendar AND the vendor's own listing, so a
    /// non-session day is zero requests and an unpublished day is a planning fact rather than a
    /// vendor verdict; [`listed_sessions`] refuses either one missing.
    fn plan(
        &self,
        target: &TargetWindow,
        calendar: Option<&Calendar>,
        listing: Option<&Listing>,
        at: ObservationTime,
    ) -> Result<Plan, PlanRefusal>;

    /// From the BODY as well as the status, and from the request's phase, because a payload read
    /// under the wrong parser yields zero rows and that is indistinguishable from a quiet day.
    fn classify(&self, request: &Request, answer: &Result<RawResponse, TransportError>) -> Verdict;

    /// Pure over (stored bytes, the provider's own hashed rules): a re-extraction under another
    /// version is this call on a provider opened with that file. The rules are not a parameter
    /// because re-parsing per call is a second parse path whose failure has no honest answer.
    fn extract(&self, request: &Request, raw: &RawBlob) -> Extracted;

    fn extract_rules_version(&self) -> FileDigest;

    /// Which follow-ups of an index attempt to issue, given a set of stories (by
    /// [`Provider::story_of`]): for a `vendor_archive` index the stories COVERED by a committed
    /// item under that one window, for a `live_ingest` index the stories HELD over every window
    /// (architecture.md section 11.1). Each provider decides what it drops (section 8 R16, R20).
    fn follow_ups_to_issue(
        &self,
        follow_ups: Vec<Request>,
        stories: &BTreeSet<String>,
    ) -> Vec<Request>;

    /// The `story_id` a follow-up's item will carry, from the request alone and so before any
    /// fetch; the provider's, because only it knows its vendor's row identity. `None` when the
    /// request does not name its story, and such a follow-up is never treated as covered.
    fn story_of(&self, follow_up: &Request) -> Option<Digest>;

    /// The publication value the index listed for a follow-up, read from the request alone under
    /// the rule `extract` dates its item by; `Unknown` when that rule reads none. The driver
    /// compares a held story's listing with its stored item (architecture.md section 8 row R15).
    fn listed_publication(&self, follow_up: &Request) -> Published;
}

#[cfg(test)]
mod tests {
    use super::{
        precision_of, probed_lag, stamp_of, Cadence, Calendar, CalendarError, Listing, PlanRefusal,
        TargetWindow, ValidPrecision,
    };
    use crate::SourceId;
    use chrono::{DateTime, Utc};
    use pit::datedness::{Minute, Published};
    use pit::MeasuredLag;

    const SOURCE: &str = "edgar_press";
    const WINDOW: &str = "2024-03-01";
    const GOOD_CALENDAR: &[u8] =
        br#"{"schema":"calendar/1","sessions":["2026-11-09","2026-11-10"]}"#;
    const NO_SESSIONS: &[u8] = br#"{"schema":"calendar/1","sessions":[]}"#;
    const WRONG_SCHEMA: &[u8] = br#"{"schema":"calendar/2","sessions":["2026-11-09"]}"#;
    const TRUNCATED: &[u8] = br#"{"schema":"calendar/1","sessi"#;
    const A_SESSION: &str = "2026-11-09";
    const A_WEEKEND: &str = "2026-11-07";

    fn source() -> SourceId {
        SourceId::new(SOURCE)
    }

    fn window() -> TargetWindow {
        TargetWindow::new(WINDOW)
    }

    fn refusal(why: &'static str) -> Option<PlanRefusal> {
        Some(PlanRefusal::ObservationLagAbsent {
            source_id: SOURCE.to_owned(),
            window: WINDOW.to_owned(),
            why,
        })
    }

    /// Each cadence over all four presences of (calendar, listing): exactly one is admitted.
    #[test]
    fn each_cadence_admits_exactly_the_inputs_it_declares_and_names_the_one_it_refuses() {
        let Ok(calendar) = Calendar::read(GOOD_CALENDAR) else {
            assert_eq!(GOOD_CALENDAR, b"a readable calendar");
            return;
        };
        let listing = Listing::of_days(&[]);
        let refused = |cadence: Cadence, why: &'static str| {
            Err(PlanRefusal::InputsDoNotMatchCadence {
                source_id: SOURCE.to_owned(),
                cadence: cadence.name(),
                why,
            })
        };
        let (session, every) = (Cadence::SessionBound, Cadence::EveryUtcDay);
        let cases = [
            (session, true, true, Ok(())),
            (session, false, true, refused(session, "has no calendar")),
            (session, true, false, refused(session, "has no listing")),
            (session, false, false, refused(session, "has no calendar")),
            (every, false, false, Ok(())),
            (every, true, false, refused(every, "was handed a calendar")),
            (every, false, true, refused(every, "was handed a listing")),
            (every, true, true, refused(every, "was handed a calendar")),
        ];
        for (cadence, with_calendar, with_listing, expected) in cases {
            let admitted = cadence.admits(
                &source(),
                with_calendar.then_some(&calendar),
                with_listing.then_some(&listing),
            );
            assert_eq!(
                admitted, expected,
                "{cadence:?} {with_calendar} {with_listing}"
            );
        }
    }

    /// `no_item_is_finer_than_the_ceiling` reads this order: a minute under a day ceiling is a
    /// breach, a minute under an instant ceiling is not.
    #[test]
    fn a_minute_is_coarser_than_an_instant_and_finer_than_a_day() {
        assert!(ValidPrecision::Instant < ValidPrecision::Minute);
        assert!(ValidPrecision::Minute < ValidPrecision::Day);
    }

    #[test]
    fn a_minute_publication_value_reaches_canon_and_the_label_as_a_minute() {
        let Ok(minute) = Minute::new(DateTime::<Utc>::UNIX_EPOCH) else {
            assert_eq!("a whole minute", "refused");
            return;
        };
        let published = Published::AtMinute(minute);
        assert_eq!(
            (stamp_of(&published), precision_of(&published).label()),
            (Some(canon::Stamp::Minute(minute.start())), "minute")
        );
    }

    #[test]
    fn a_calendar_naming_no_session_is_refused_rather_than_closing_every_day() {
        assert_eq!(Calendar::read(NO_SESSIONS), Err(CalendarError::NoSessions));
    }

    #[test]
    fn a_calendar_whose_schema_is_unknown_is_refused_rather_than_read_optimistically() {
        assert_eq!(
            Calendar::read(WRONG_SCHEMA),
            Err(CalendarError::UnknownSchema("calendar/2".to_owned()))
        );
    }

    #[test]
    fn a_truncated_calendar_is_named_unparseable_and_not_read_as_an_empty_one() {
        assert!(
            matches!(
                Calendar::read(TRUNCATED),
                Err(CalendarError::Unparseable(_))
            ),
            "{:?}",
            Calendar::read(TRUNCATED)
        );
    }

    #[test]
    fn a_calendar_answers_for_the_days_it_names_and_against_the_ones_it_does_not() {
        let Ok(calendar) = Calendar::read(GOOD_CALENDAR) else {
            assert_eq!(GOOD_CALENDAR, b"a readable calendar");
            return;
        };
        let (session, weekend) = (A_SESSION.parse(), A_WEEKEND.parse());
        match (session, weekend) {
            (Ok(session), Ok(weekend)) => {
                assert!(calendar.is_session(session));
                assert!(!calendar.is_session(weekend));
            }
            _ => assert_eq!(A_SESSION, "a readable date"),
        }
    }

    /// The exact value section 8 row R11 records as accepted by the type: every field at its zero.
    #[test]
    fn a_lag_with_no_probe_behind_it_is_refused_in_each_of_the_four_ways_it_can_be_empty() {
        let probed = MeasuredLag {
            probe_id: "pplx-2026-11-11".to_owned(),
            probes: 69,
            leaks: 0,
            seconds: 7_200,
        };
        let cases = [
            (None, refusal("no lag entry")),
            (
                Some(MeasuredLag {
                    probe_id: String::new(),
                    probes: 0,
                    leaks: 0,
                    seconds: 0,
                }),
                refusal("no probe id"),
            ),
            (
                Some(MeasuredLag {
                    probes: 0,
                    ..probed.clone()
                }),
                refusal("zero probes, so no leak rate"),
            ),
            (
                Some(MeasuredLag {
                    seconds: 0,
                    ..probed.clone()
                }),
                refusal("zero seconds"),
            ),
            (Some(probed.clone()), None),
        ];
        for (lag, expected) in cases {
            assert_eq!(
                probed_lag(&source(), &window(), lag.as_ref()).err(),
                expected,
                "{lag:?}"
            );
        }
    }

    /// Section 8 row R11: either lag passes every emptiness check above and still cannot date an
    /// item, so it is refused before the first request rather than at commit.
    #[test]
    fn a_lag_that_cannot_date_an_item_is_refused_before_any_request() {
        let probed = MeasuredLag {
            probe_id: "pplx-2026-11-11".to_owned(),
            probes: 69,
            leaks: 0,
            seconds: 7_200,
        };
        let cases = [
            (-7_200, refusal("negative seconds")),
            (i64::MAX / 2, refusal("more seconds than a duration holds")),
        ];
        for (seconds, expected) in cases {
            let lag = MeasuredLag {
                seconds,
                ..probed.clone()
            };
            assert_eq!(
                probed_lag(&source(), &window(), Some(&lag)).err(),
                expected,
                "{lag:?}"
            );
        }
    }
}
