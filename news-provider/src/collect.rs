//! The driver. `fetch` is not on [`Provider`], so this is what opens a slot, spends its permit,
//! classifies the answer, stores the payload, extracts it and closes the attempt - one HTTP call
//! per written ledger row.
//!
//! Two seams, so a run is drivable over stored payloads with no network and over a real archive
//! with no provider: [`Exchange`] is the socket, whose only implementation that opens one is
//! [`Wire`], and [`Archive`] is the store door.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use pit::datedness::Published;
use pit::ObservationTime;

use crate::budget::{host_of, HostKey};
use crate::clock::Clock;
use crate::ledger::{
    abandon, AttemptId, AttemptReceipt, AttemptSlot, Baseline, Counts, LedgerError, Outcome,
    OutcomeKind, SendPermit, Sink, Verdict,
};
use crate::provider::{CollectedItem, DecidedBy, Plan, Provenance, Provider, RawBlob};
use crate::request::{Request, RequestPhase, RunId, SourceId, TargetWindow};
use crate::wire::{
    CappedHold, Headers, RawResponse, Reservation, SendFailure, Slowed, TransportError, Unreserved,
    Wire,
};
use crate::{Calendar, Listing, PlanRefusal};

/// One vendor conversation. Behind a trait because the driver is the thing under test and a test
/// may not touch a network; [`Wire`] is the only implementation in the workspace that opens a
/// socket, and `send` consumes the permit either way.
pub trait Exchange {
    /// Asked before the attempt row opens: `Go`, or the slot opens after `deadline` and nothing
    /// was written. An exchange that paces nothing answers `Go`, as a stub does.
    fn reserve(
        &self,
        _request: &Request,
        _clock: &dyn Clock,
        _deadline: Option<DateTime<Utc>>,
    ) -> Result<Reservation, Unreserved> {
        Ok(Reservation::Go)
    }

    fn send(
        &self,
        permit: SendPermit,
        request: &Request,
        clock: &dyn Clock,
    ) -> Result<RawResponse, SendFailure>;

    /// The answers this exchange held to [`crate::wire::HOLD_CEILING`] (S7-D7), read once a run ends.
    fn capped_holds(&self) -> Vec<CappedHold> {
        Vec::new()
    }

    /// What servers asked of `host` this run (S7-D11), read once a run ends.
    fn slowed(&self, _host: &str) -> Slowed {
        Slowed::default()
    }
}

// Each method calls the inherent one by path, so this impl cannot recurse into itself.
impl Exchange for Wire {
    fn reserve(
        &self,
        request: &Request,
        clock: &dyn Clock,
        deadline: Option<DateTime<Utc>>,
    ) -> Result<Reservation, Unreserved> {
        Wire::reserve(self, request, clock, deadline)
    }

    fn send(
        &self,
        permit: SendPermit,
        request: &Request,
        clock: &dyn Clock,
    ) -> Result<RawResponse, SendFailure> {
        Wire::send(self, permit, request, clock)
    }

    fn capped_holds(&self) -> Vec<CappedHold> {
        Wire::capped_holds(self)
    }

    fn slowed(&self, host: &str) -> Slowed {
        Wire::slowed(self, host)
    }
}

/// One host's share of a run (S7-D11). `waited` is the clock inside [`Exchange::reserve`] and,
/// for a send the gate refused, from there to the refusal; `requesting` each answer's own
/// `received_at - requested_at`; the host is `None` for a url `host_of` cannot key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostBusy {
    pub host: Option<String>,
    pub waited: TimeDelta,
    pub requesting: TimeDelta,
    pub requests: u32,
    /// The deadline or a refusal at the send gate stopped its lane.
    pub stopped: bool,
    /// Its `deferred` rows, every reason.
    pub not_looked: u32,
    pub held_until: Option<DateTime<Utc>>,
    pub doublings: u32,
}

impl HostBusy {
    /// Section 11.1 Pace: a host's busy time is its paced waits plus its request times.
    #[must_use]
    pub fn busy(&self) -> TimeDelta {
        self.waited + self.requesting
    }
}

/// Bound on retry work per request — a MECHANISM cap, never derived from an observed loss rate
/// (§8 row R21, §10). A permanent failure (4xx, `vendor_refused`) is never re-queued.
pub const MAX_TRANSIENT_RETRIES: u32 = 2;

/// Why a request was recorded as not looked at instead of asked: a `deferred` row (architecture.md
/// section 3.2, S7-D4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferReason {
    /// Its slot opens after the run's deadline under the profile's own pace, or the deadline
    /// has passed.
    RunDeadline,
    /// Its slot opens after the deadline only because a server held or slowed its host.
    HostHeld,
    FollowUpCap,
    /// Another sender kept the send gate from its send, or from the send of a request ahead of
    /// it in its host's lane: a local condition, never a vendor's answer. A send the gate
    /// refused for the run's deadline alone is `RunDeadline`.
    GateRefused,
}

impl DeferReason {
    /// Every arm, in the order the envelope's and the ledger's human lines print them.
    pub const ALL: [Self; 4] = [
        Self::RunDeadline,
        Self::HostHeld,
        Self::FollowUpCap,
        Self::GateRefused,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::RunDeadline => "run_deadline",
            Self::HostHeld => "host_held",
            Self::FollowUpCap => "follow_up_cap",
            Self::GateRefused => "gate_refused",
        }
    }
}

/// One request this run did not ask, and why. It has no attempt row, because no call was made
/// (section 8 row R16), but for the one request a gate refused, whose row was open by then and
/// is closed `not_sent`; `story_id` by the rule `attempt_once` opens a `Document` attempt under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deferred {
    pub source: String,
    pub target_window: String,
    /// `None` for a url `host_of` cannot key, which shares the queue's `None` lane.
    pub host: Option<String>,
    pub url: String,
    pub phase: RequestPhase,
    pub story_id: Option<String>,
    pub listed_date: Option<String>,
    pub reason: DeferReason,
    pub at: ObservationTime,
    /// The queued request's index attempt (`attempts.listed_by`), so a carry keeps its first listing.
    pub listed_by: Option<String>,
    /// When the pace would have sent it: set on the one request whose reservation came back
    /// past the deadline, `None` for the rest of its lane and for a follow-up past the cap.
    pub go_at: Option<DateTime<Utc>>,
}

impl Deferred {
    fn of(
        provider: &dyn Provider,
        queued: Queued,
        reason: DeferReason,
        at: ObservationTime,
    ) -> Self {
        let Queued {
            request, listed_by, ..
        } = queued;
        let story_id = (request.phase == RequestPhase::Document)
            .then(|| provider.story_of(&request))
            .flatten()
            .map(|story| story.hex().to_owned());
        Self {
            source: request.source.as_str().to_owned(),
            target_window: request.target_window.as_str().to_owned(),
            host: host_of(&request.url)
                .ok()
                .flatten()
                .map(|host| host.as_str().to_owned()),
            phase: request.phase,
            story_id,
            listed_date: request.listed_date,
            url: request.url,
            reason,
            at,
            listed_by,
            go_at: None,
        }
    }
}

/// S7-D5's age limit: an item whose oldest open row is this old at the run's start, or older,
/// is not carried, and is closed `expired` at the run's end unless the run asked its story.
pub const CARRY_LIMIT: TimeDelta = TimeDelta::hours(24);

/// One open `deferred` row of a `Document` request, one no `deferred_closed` row names (S7-D5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenDeferred {
    pub deferred_id: i64,
    pub target_window: String,
    pub url: String,
    pub story_id: Option<String>,
    pub listed_date: Option<String>,
    pub deferred_at: DateTime<Utc>,
    pub listed_by: Option<String>,
}

/// A story whose latest attempt is a permanent failure stored cut shorter than the limit in force
/// (section 11.1): what [`carry`] needs to ask it once more, read from that attempt and its payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasedCut {
    pub story_id: String,
    pub target_window: String,
    pub url: String,
    pub listed_date: Option<String>,
    pub listed_by: Option<String>,
    pub closed_at: DateTime<Utc>,
}

/// How an open `deferred` row is closed (S7-D5), stored by its [`Closing::name`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Closing {
    /// Asked by this run, whatever the answer: carried, or listed again by its own index.
    Answered { attempt_id: String },
    /// The archive held its story at the run's start, so it was not asked.
    Held,
    /// Its oldest open row was [`CARRY_LIMIT`] old or older at the run's start, and the run did
    /// not ask its url or its story.
    Expired,
}

impl Closing {
    /// Every name, in the order the envelope's and the ledger's human lines print them.
    pub const NAMES: [&'static str; 3] = ["answered", "held", "expired"];

    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Answered { .. } => "answered",
            Self::Held => "held",
            Self::Expired => "expired",
        }
    }

    #[must_use]
    pub fn attempt_id(&self) -> Option<&str> {
        match self {
            Self::Answered { attempt_id } => Some(attempt_id),
            Self::Held | Self::Expired => None,
        }
    }
}

/// One carried item: every open row of one url, oldest first (S7-D5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Carried {
    pub source: String,
    /// The oldest row's, which the rebuilt request is asked under.
    pub target_window: String,
    pub host: Option<String>,
    pub url: String,
    pub story_id: Option<String>,
    pub deferred_ids: Vec<i64>,
    pub first_deferred_at: DateTime<Utc>,
    /// Put at the front of its host's lane; false when closed `held`, over 24 h, or not rebuilt.
    pub queued: bool,
    /// `None`: still open, asked by no attempt of this run or never rebuilt.
    pub closed: Option<Closing>,
}

/// What the archive refused, and which act asked. The act is what the exit table reads: a refused
/// commit is `commit_failed` and a refused read is not.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("the archive refused to {act}: {detail}")]
pub struct ArchiveError {
    pub act: &'static str,
    pub detail: String,
}

/// What one [`Archive::commit`] put in the archive. `changed` is false for a replay.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Committed {
    pub items_committed: u32,
    pub blobs_indexed: u32,
    pub bytes_indexed: u64,
    pub seq_hi: Option<i64>,
    pub changed: bool,
}

/// P10's link row (architecture.md section 3.9): one per attempt that got an HTTP answer, an `Ok`
/// response or a body cut at `body_limit`, naming the payload as received and what asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload {
    /// The digest [`Archive::put_blob`] returned for the body, cut or whole.
    pub raw_blob: String,
    pub url: String,
    pub phase: RequestPhase,
    pub listed_date: Option<String>,
    pub headers: Headers,
    /// True when the body was over `body_limit` and `raw_blob` holds its first `body_limit` bytes.
    pub truncated: bool,
}

/// A follow-up a live index attempt did not issue because the archive held its story, with the
/// publication value the index listed for it: the arriving observation section 8 row R15 compares
/// with the stored item, without fetching the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relisted {
    pub story_id: String,
    /// [`Provider::listed_publication`] of the follow-up.
    pub listed: Published,
    /// The follow-up's `listed_date`, the value as the index wrote it.
    pub listed_raw: Option<String>,
}

/// The store side of the driver, in the terms this crate can name: a `CollectedItem` is a
/// provider's row and the archive's own row type is the archive's business.
pub trait Archive: Sink {
    /// Stores a payload under its own digest, BEFORE anything parses it (architecture.md section 3.1, `raw_blob_ref`).
    fn put_blob(&self, payload: &[u8]) -> Result<String, ArchiveError>;

    /// The one door. `payload` is absent exactly when the attempt brought no body back, which
    /// is also when `items` is empty; its `raw_blob` is every item's raw blob.
    fn commit(
        &mut self,
        closed: AttemptReceipt,
        items: &[CollectedItem],
        payload: Option<&Payload>,
        observed: ObservationTime,
    ) -> Result<Committed, ArchiveError>;

    /// This source's own `ok` attempts, oldest first, which is what makes a trailing slice
    /// trailing.
    fn ok_vendor_rows(&self, source: &SourceId) -> Result<Vec<u32>, ArchiveError>;

    /// Attempts opened and never finalised, which the startup sweep closes.
    fn attempts_in_flight(&self) -> Result<Vec<String>, ArchiveError>;

    /// The story ids this archive already holds a COMMITTED item for under (source, target window).
    /// The driver hands this to the provider's follow-up planning so a `vendor_archive` re-run
    /// fetches only the uncovered accessions (§8 row R20). "Covered" is a committed item, never an
    /// attempt that returned ok and committed nothing: that is a gap, not coverage.
    fn covered_story_ids(
        &self,
        source: &SourceId,
        target_window: &TargetWindow,
    ) -> Result<std::collections::BTreeSet<String>, ArchiveError>;

    /// The story ids this archive holds under `source` across EVERY target window: a committed
    /// item, or a closed Document attempt whose verdict was usable, empty or permanent. A transient
    /// failure, a permanent failure answered with a 3xx, a permanent failure whose body was stored
    /// cut shorter than `body_limit` (the limit in force now), a usable attempt with an unusable
    /// row the index row decided, a swept attempt and one in flight hold nothing (section 11.1).
    fn held_story_ids(
        &self,
        source: &SourceId,
        body_limit: u64,
    ) -> Result<std::collections::BTreeSet<String>, ArchiveError>;

    /// Records how many in-universe follow-ups an index attempt named: the denominator the coverage
    /// reader compares the committed rows against. Written whatever the count, so a window that
    /// matched nothing is distinguishable from one no index ever read.
    fn record_planned_coverage(
        &mut self,
        attempt_id: &str,
        matched: u32,
    ) -> Result<(), ArchiveError>;

    /// Records how many of a live index attempt's follow-ups were not issued because
    /// [`Archive::held_story_ids`] held their story: section 11.1's `held`, which `ledger --check`
    /// subtracts from `vendor_rows`. Written whatever the count, 0 included.
    fn record_held(&mut self, attempt_id: &str, held: u32) -> Result<(), ArchiveError>;

    /// Records, beside a live index attempt, each held story whose listed publication value
    /// differs from its stored item's, and how many were compared (section 8 row R15). No stored
    /// row is updated.
    fn record_relisted(
        &mut self,
        attempt_id: &str,
        relisted: &[Relisted],
    ) -> Result<(), ArchiveError>;

    /// Records a request this run did not ask, before the run moves on: "we did not look" is a
    /// row, apart from every attempt row (S7-D4).
    fn record_deferred(&mut self, deferred: &Deferred) -> Result<(), ArchiveError>;

    /// S7-D5's reader: the source's open `Document` rows, oldest `deferred_at` first.
    fn open_deferred(&self, source: &SourceId) -> Result<Vec<OpenDeferred>, ArchiveError>;

    /// Section 11.1: each story of `source` whose LATEST attempt is a permanent failure stored cut
    /// shorter than `body_limit`, oldest first; [`carry`] applies [`CARRY_LIMIT`].
    fn released_cuts(
        &self,
        source: &SourceId,
        body_limit: u64,
    ) -> Result<Vec<ReleasedCut>, ArchiveError>;

    /// Closes each of `deferred_ids` as `closing` at `at`; a row is closed once, never updated.
    fn close_deferred(
        &mut self,
        deferred_ids: &[i64],
        closing: &Closing,
        at: ObservationTime,
    ) -> Result<(), ArchiveError>;
}

/// Why a run could not be driven at all. A vendor's own answer is never one of these: it is a
/// ledger row and an outcome count.
#[derive(Debug, thiserror::Error)]
pub enum CollectError {
    #[error(transparent)]
    Planning(#[from] PlanRefusal),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    /// A startup refusal and never a vendor event in the coverage ledger (architecture.md
    /// section 5.1, `credential_contact_missing`).
    #[error("no credential named {0} is held")]
    Credential(String),
    /// The same kind of refusal, raised before the socket: the vendor's pace could not be read.
    #[error(transparent)]
    Budget(#[from] crate::budget::BudgetError),
}

/// What a run was asked to collect.
#[derive(Debug)]
pub struct Run<'a> {
    pub run: &'a RunId,
    pub target: &'a TargetWindow,
    /// Present exactly when the provider's [`crate::Cadence`] takes them; [`collect`] refuses
    /// any other combination before it touches the archive.
    pub calendar: Option<&'a Calendar>,
    pub listing: Option<&'a Listing>,
    /// The run's own observation instant, which decides each request's provenance. Injected,
    /// because a provider may not read a clock.
    pub at: ObservationTime,
    /// The last instant a request may be sent at (S7-D2, wall time); `None` sends every request.
    pub deadline: Option<DateTime<Utc>>,
}

/// Every state the exit table reads, counted. The table is a total function over these, so a new
/// state has to appear here and in [`Tally::exit`]'s match before it compiles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub committed: u32,
    pub ok: u32,
    pub empty: u32,
    pub transient: u32,
    pub permanent: u32,
    pub interrupted: u32,
    /// Targets not asked for, for a reason that is not clean work: [`NotPlanned::clean`].
    pub unclean_not_planned: u32,
    pub commit_refused: u32,
    /// Requests recorded as [`Deferred`] and never asked, which no exit may read as finished.
    pub deferred: u32,
}

/// What a run exits as. The registry codes are architecture.md section 5.1's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Ok,
    PartialCollect,
    VendorUnreachable,
    VendorRefusedAll,
    CommitFailed,
}

/// Every arm, for the table test: a sixth exit has to be added here before that test compiles.
pub const EXITS: [Exit; 5] = [
    Exit::Ok,
    Exit::PartialCollect,
    Exit::VendorUnreachable,
    Exit::VendorRefusedAll,
    Exit::CommitFailed,
];

impl Exit {
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Ok => 0,
            // 8 and not 7 for a total refusal: 7 is the retryable class and a permanent refusal
            // must not invite a timer to hammer it.
            Self::PartialCollect | Self::VendorRefusedAll | Self::CommitFailed => 8,
            Self::VendorUnreachable => 7,
        }
    }

    #[must_use]
    pub const fn error_code(self) -> Option<&'static str> {
        match self {
            Self::Ok => None,
            Self::PartialCollect => Some("partial_collect"),
            Self::VendorUnreachable => Some("vendor_unreachable"),
            Self::VendorRefusedAll => Some("vendor_refused_all"),
            Self::CommitFailed => Some("commit_failed"),
        }
    }

    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(self, Self::VendorUnreachable)
    }

    /// Decision D1's companion rule: stdout is non-empty if and only if the exit is 0, 1 or 8.
    /// One function, so the binary and the test cannot hold two versions of the rule.
    #[must_use]
    pub const fn carries_the_result_on_stdout(self) -> bool {
        matches!(self.code(), 0 | 1 | 8)
    }
}

impl Tally {
    /// Everything that is not finished work.
    #[must_use]
    pub const fn incomplete(&self) -> u32 {
        self.transient
            + self.permanent
            + self.interrupted
            + self.unclean_not_planned
            + self.commit_refused
            + self.deferred
    }

    /// The exit table as a TOTAL function over the counts, not a chain of ifs that falls through:
    /// architecture.md section 5.1's table, one arm per bucket.
    #[must_use]
    pub fn exit(&self) -> Exit {
        let incomplete = self.incomplete();
        match (
            self.commit_refused > 0,
            self.committed > 0,
            incomplete > 0,
            self.permanent > 0,
            self.transient == incomplete,
        ) {
            (true, _, _, _, _) => Exit::CommitFailed,
            (false, _, false, _, _) => Exit::Ok,
            (false, true, true, _, _) => Exit::PartialCollect,
            (false, false, true, true, _) => Exit::VendorRefusedAll,
            (false, false, true, false, true) => Exit::VendorUnreachable,
            // Nothing committed, nothing failed at the vendor: targets we did not ask for, or an
            // attempt a dead run left behind.
            (false, false, true, false, false) => Exit::PartialCollect,
        }
    }
}

/// A target the run did not ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotPlannedRow {
    pub target_window: String,
    pub reason: &'static str,
    pub clean: bool,
}

/// One attempt as the run envelope reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRow {
    pub attempt_id: String,
    pub source: String,
    pub target_window: String,
    pub phase: &'static str,
    pub provenance: &'static str,
    pub url: String,
    pub transport_status: Option<u16>,
    pub outcome: &'static str,
    pub vendor_rows: u32,
    pub filtered_out: u32,
    pub robots_disallowed: u32,
    pub unusable_rows: u32,
    pub items_committed: u32,
    /// Present when the store refused this attempt's rows, so exit 8 names the attempt rather
    /// than reporting a bare status.
    pub commit_refused: Option<String>,
    /// Present on a `not_sent` attempt: the gate's refusal as it read, which names the gate
    /// file and the wait.
    pub not_sent: Option<String>,
}

/// One planned source's own counts, written whatever they are: a source asked and answered empty
/// and a source never asked must not be byte-identical here (accepted attack A2.10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceTally {
    pub source: String,
    pub planned_requests: u32,
    pub not_planned: u32,
    pub attempts: u32,
    pub ok: u32,
    pub empty: u32,
    pub failed: u32,
    pub vendor_rows: u32,
    pub filtered_out: u32,
    pub robots_disallowed: u32,
    pub unusable_rows: u32,
    pub items_committed: u32,
    pub deferred: u32,
    pub carried: CarryTally,
}

/// S7-D5's carried items of one source, one per url: queued, and closed by each [`Closing`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CarryTally {
    pub queued: u32,
    pub answered: u32,
    pub held: u32,
    pub expired: u32,
}

/// What one run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collected {
    pub planned_requests: u32,
    pub not_planned: Vec<NotPlannedRow>,
    pub attempts: Vec<AttemptRow>,
    pub per_source: Vec<SourceTally>,
    /// Attempts a dead run left open, closed `interrupted` by this one.
    pub swept: Vec<String>,
    /// Requests this run did not ask, in the order the queue reached them.
    pub deferred: Vec<Deferred>,
    /// Open `deferred` rows read at the start, one item per url, oldest first (S7-D5).
    pub carried: Vec<Carried>,
    /// [`Exchange::capped_holds`] once the run ends.
    pub capped_holds: Vec<CappedHold>,
    /// Per host, in host order: S7-D11's busy fields.
    pub busy: Vec<HostBusy>,
    pub tally: Tally,
    pub blobs_written: u32,
    pub bytes_written: u64,
    pub blobs_indexed: u32,
    pub bytes_indexed: u64,
    pub seq_high_water: Option<i64>,
}

impl Collected {
    /// The plan half of a run: what it will ask for and what it will not. [`collect`] starts from
    /// this and a dry run returns it alone, so the mapping from a [`Plan`] has one home.
    #[must_use]
    pub fn of_plan(plan: &Plan, source: &SourceId) -> Self {
        let mut collected = Self {
            planned_requests: count(plan.requests.len()),
            not_planned: plan
                .not_planned
                .iter()
                .map(|(window, reason)| NotPlannedRow {
                    target_window: window.as_str().to_owned(),
                    reason: reason.name(),
                    clean: reason.clean(),
                })
                .collect(),
            attempts: Vec::new(),
            per_source: Vec::new(),
            swept: Vec::new(),
            deferred: Vec::new(),
            carried: Vec::new(),
            capped_holds: Vec::new(),
            busy: Vec::new(),
            tally: Tally {
                unclean_not_planned: count(
                    plan.not_planned
                        .iter()
                        .filter(|(_, reason)| !reason.clean())
                        .count(),
                ),
                ..Tally::default()
            },
            blobs_written: 0,
            bytes_written: 0,
            blobs_indexed: 0,
            bytes_indexed: 0,
            seq_high_water: None,
        };
        collected.per_source = vec![tally_of(source, &collected)];
        collected
    }

    #[must_use]
    pub fn exit(&self) -> Exit {
        self.tally.exit()
    }
}

/// Plans one target and drives every request it names, plus every follow-up those produce.
pub fn collect<A: Archive>(
    provider: &dyn Provider,
    archive: &mut A,
    exchange: &dyn Exchange,
    clock: &dyn Clock,
    run: &Run,
) -> Result<Collected, CollectError> {
    let source = provider.source();
    provider
        .cadence()
        .admits(&source, run.calendar, run.listing)?;
    // Before this run opens anything of its own, so the sweep never sees its own attempts.
    let swept = sweep(archive, run.run, ObservationTime::new(clock.now()))?;
    let plan = provider.plan(run.target, run.calendar, run.listing, run.at)?;
    let mut collected = Collected::of_plan(&plan, &source);
    collected.tally.interrupted = count(swept.len());
    collected.swept = swept;
    // Read ONCE per run, before this run's own attempts land: per attempt it would be both a
    // scan of every `ok` row this source ever wrote and a baseline the index attempt's own rows
    // had already moved, so a follow-up would be measured against its own parent.
    let baseline = Baseline::from_trailing_ok(&archive.ok_vendor_rows(&source)?);
    let driving = Driving {
        provider,
        exchange,
        clock,
        run: run.run,
        baseline: &baseline,
    };
    let started = clock.now();
    let CarryPlan {
        carried,
        queued: carrying,
        reasked,
    } = carry(provider, archive, started)?;
    let mut issued: BTreeSet<String> = carried
        .iter()
        .filter(|item| item.queued)
        .filter_map(|item| item.story_id.clone())
        .chain(reasked)
        .collect();
    collected.carried = carried;
    let mut queue = HostQueue::of(plan.requests, provider.follow_up_cap());
    queue.carry(carrying);
    let mut ordinal = 0;
    let mut asked = BTreeSet::new();
    let mut asked_by: BTreeMap<String, String> = BTreeMap::new();
    let mut pending_held = Vec::new();
    let mut busy: BTreeMap<Option<String>, HostBusy> = BTreeMap::new();
    while let Some(popped) = queue.pop() {
        let queued = match popped {
            Popped::Ask(queued) => queued,
            Popped::Defer(queued) => {
                let at = ObservationTime::new(clock.now());
                let deferred = Deferred::of(provider, queued, DeferReason::FollowUpCap, at);
                collected.defer(archive, deferred)?;
                continue;
            }
        };
        let host = busy_key(&queued.request.url);
        let asked_at = clock.now();
        // Before the attempt row: a request past the deadline gets none (condition A).
        let reserved = exchange.reserve(&queued.request, clock, run.deadline);
        let reserved_at = clock.now();
        busy.entry(host.clone()).or_default().waited += elapsed(asked_at, reserved_at);
        let (go_at, by_server) = match reserved {
            Ok(Reservation::Go) => (None, false),
            Ok(Reservation::PastDeadline { go_at, by_server }) => (Some(go_at), by_server),
            Err(Unreserved::CredentialMissing(name)) => return Err(CollectError::Credential(name)),
            Err(Unreserved::Budget(why)) => return Err(CollectError::Budget(why)),
        };
        if let Some(go_at) = go_at {
            let reason = if by_server {
                DeferReason::HostHeld
            } else {
                DeferReason::RunDeadline
            };
            let at = ObservationTime::new(clock.now());
            busy.entry(host).or_default().stopped = true;
            let lane = queue.stop(&queued);
            let mut first = Deferred::of(provider, queued, reason, at);
            first.go_at = Some(go_at);
            collected.defer(archive, first)?;
            for rest in lane {
                collected.defer(archive, Deferred::of(provider, rest, reason, at))?;
            }
            continue;
        }
        let listing = queued.listed_by.as_deref();
        let sent = attempt_once(
            &driving,
            archive,
            &queued.request,
            ordinal,
            listing,
            &mut issued,
        )?;
        ordinal += 1;
        let mut attempted = match sent {
            Sent::Attempted(attempted) => *attempted,
            // The lane is deferred whole, as at the deadline, and the other hosts go on.
            Sent::Refused { row, owed } => {
                let now = clock.now();
                let at = ObservationTime::new(now);
                let share = busy.entry(host).or_default();
                share.stopped = true;
                share.waited += elapsed(reserved_at, now);
                collected.attempts.push(*row);
                let reason = match owed {
                    Some(_) => DeferReason::RunDeadline,
                    None => DeferReason::GateRefused,
                };
                let lane = queue.stop(&queued);
                let mut first = Deferred::of(provider, queued, reason, at);
                first.go_at = owed.and_then(|owed| {
                    TimeDelta::from_std(owed)
                        .ok()
                        .and_then(|owed| now.checked_add_signed(owed))
                });
                collected.defer(archive, first)?;
                for rest in lane {
                    collected.defer(archive, Deferred::of(provider, rest, reason, at))?;
                }
                continue;
            }
        };
        let Queued {
            request,
            retries,
            carried,
            listed_by,
            ..
        } = queued;
        asked.extend(attempted.story.clone());
        for key in attempted.story.iter().chain([&request.url]) {
            asked_by
                .entry(key.clone())
                .or_insert_with(|| attempted.row.attempt_id.clone());
        }
        pending_held.extend(attempted.held.take());
        if !carried.is_empty() {
            let closing = Closing::Answered {
                attempt_id: attempted.row.attempt_id.clone(),
            };
            archive.close_deferred(&carried, &closing, ObservationTime::new(clock.now()))?;
            collected.close_carried(&carried, closing);
        }
        let lane = busy.entry(host).or_default();
        lane.requesting += attempted.requesting;
        lane.requests += 1;
        let retrying = Queued::retried(attempted.is_transient(), retries);
        collected.absorb(&attempted, retrying);
        let again = Queued {
            request,
            retries,
            counted: false,
            carried: Vec::new(),
            listed_by,
        };
        let listing = Some(attempted.row.attempt_id.as_str());
        queue.promote(&attempted.promoted, |request| {
            provider
                .story_of(request)
                .map(|story| story.hex().to_owned())
        });
        queue.requeue(
            attempted.follow_ups,
            attempted.walked,
            again,
            retrying,
            listing,
        );
    }
    // Once the queue is empty: a story this run had queued counts as held only if it was asked.
    for pending in pending_held {
        let queued_and_asked = pending
            .queued
            .iter()
            .filter(|story| asked.contains(*story))
            .count();
        if queued_and_asked > 0 {
            let held = pending.held.saturating_add(count(queued_and_asked));
            archive.record_held(&pending.attempt_id, held)?;
        }
    }
    close_uncarried(archive, &mut collected.carried, &asked_by, started, clock)?;
    collected.capped_holds = exchange.capped_holds();
    for row in &collected.deferred {
        busy.entry(row.host.clone()).or_default().not_looked += 1;
    }
    collected.busy = busy
        .into_iter()
        .map(|(host, mut lane)| {
            let slowed = host
                .as_deref()
                .map_or_else(Slowed::default, |host| exchange.slowed(host));
            lane.host = host;
            lane.doublings = slowed.doublings;
            lane.held_until = slowed.held_until;
            lane
        })
        .collect();
    collected.per_source = vec![tally_of(&source, &collected)];
    Ok(collected)
}

/// What [`carry`] hands the run: every carried item, the requests it queued, and the stories of
/// the released cuts among them.
struct CarryPlan {
    carried: Vec<Carried>,
    queued: Vec<Queued>,
    reasked: BTreeSet<String>,
}

/// S7-D5: the source's open `Document` rows, one item per url in the order of its oldest row. An
/// item whose story the archive holds is closed `held`; the provider rebuilds each other item
/// younger than [`CARRY_LIMIT`], and [`close_uncarried`] decides the rest at the run's end. Then
/// each [`ReleasedCut`] younger than [`CARRY_LIMIT`] is rebuilt and queued once, with no
/// `deferred` row, and its story returned so no listing of this run issues it again (§11.1).
fn carry<A: Archive>(
    provider: &dyn Provider,
    archive: &mut A,
    now: DateTime<Utc>,
) -> Result<CarryPlan, CollectError> {
    let source = provider.source();
    let body_limit = provider.transport().body_limit;
    let open = archive.open_deferred(&source)?;
    let released = archive.released_cuts(&source, body_limit)?;
    let (carried, mut queued) = carry_deferred(provider, archive, open, now)?;
    let carrying: BTreeSet<&str> = carried
        .iter()
        .filter(|item| item.queued)
        .filter_map(|item| item.story_id.as_deref())
        .collect();
    let mut reasked = BTreeSet::new();
    for cut in released {
        if now - cut.closed_at >= CARRY_LIMIT || carrying.contains(cut.story_id.as_str()) {
            continue;
        }
        let target = TargetWindow::new(&cut.target_window);
        let Some(request) = provider.carried(&target, &cut.url, cut.listed_date.as_deref()) else {
            continue;
        };
        reasked.insert(cut.story_id);
        queued.push(Queued {
            counted: !provider.is_walk_id(&request),
            request,
            retries: 0,
            carried: Vec::new(),
            listed_by: cut.listed_by,
        });
    }
    Ok(CarryPlan {
        carried,
        queued,
        reasked,
    })
}

fn carry_deferred<A: Archive>(
    provider: &dyn Provider,
    archive: &mut A,
    open: Vec<OpenDeferred>,
    now: DateTime<Utc>,
) -> Result<(Vec<Carried>, Vec<Queued>), CollectError> {
    if open.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let source = provider.source();
    let held = archive.held_story_ids(&source, provider.transport().body_limit)?;
    let mut items: Vec<(String, Vec<OpenDeferred>)> = Vec::new();
    for row in open {
        match items.iter_mut().find(|(url, _)| *url == row.url) {
            Some((_, rows)) => rows.push(row),
            None => items.push((row.url.clone(), vec![row])),
        }
    }
    let (mut carried, mut queued) = (Vec::new(), Vec::new());
    for (url, rows) in items {
        let Some(oldest) = rows.first() else {
            continue;
        };
        let ids: Vec<i64> = rows.iter().map(|row| row.deferred_id).collect();
        let story_id = rows.iter().find_map(|row| row.story_id.clone());
        let over_age = now - oldest.deferred_at >= CARRY_LIMIT;
        let closing = story_id
            .as_ref()
            .is_some_and(|story| held.contains(story))
            .then_some(Closing::Held);
        let mut item = Carried {
            source: source.as_str().to_owned(),
            target_window: oldest.target_window.clone(),
            host: busy_key(&url),
            url: url.clone(),
            story_id,
            deferred_ids: ids.clone(),
            first_deferred_at: oldest.deferred_at,
            queued: false,
            closed: None,
        };
        if let Some(closing) = closing {
            archive.close_deferred(&ids, &closing, ObservationTime::new(now))?;
            item.closed = Some(closing);
        } else if let Some(request) = (!over_age)
            .then(|| {
                provider.carried(
                    &TargetWindow::new(&oldest.target_window),
                    &url,
                    oldest.listed_date.as_deref(),
                )
            })
            .flatten()
        {
            item.queued = true;
            queued.push(Queued {
                counted: !provider.is_walk_id(&request),
                request,
                retries: 0,
                carried: ids,
                listed_by: oldest.listed_by.clone(),
            });
        }
        carried.push(item);
    }
    Ok((carried, queued))
}

/// Once the queue is empty, each open item the run did not carry: closed `answered` by the first
/// attempt that asked its url or its story (PR #53 review F1), else `expired` when it was over
/// [`CARRY_LIMIT`] at `started`, else left open.
fn close_uncarried<A: Archive>(
    archive: &mut A,
    carried: &mut [Carried],
    asked_by: &BTreeMap<String, String>,
    started: DateTime<Utc>,
    clock: &dyn Clock,
) -> Result<(), CollectError> {
    for item in carried
        .iter_mut()
        .filter(|item| !item.queued && item.closed.is_none())
    {
        let asked = item
            .story_id
            .iter()
            .chain([&item.url])
            .find_map(|key| asked_by.get(key));
        let closing = match asked {
            Some(attempt_id) => Closing::Answered {
                attempt_id: attempt_id.clone(),
            },
            None if started - item.first_deferred_at >= CARRY_LIMIT => Closing::Expired,
            None => continue,
        };
        archive.close_deferred(
            &item.deferred_ids,
            &closing,
            ObservationTime::new(clock.now()),
        )?;
        item.closed = Some(closing);
    }
    Ok(())
}

fn busy_key(url: &str) -> Option<String> {
    host_of(url)
        .ok()
        .flatten()
        .map(|host| host.as_str().to_owned())
}

/// Clock time from `from` to `to`, none when the clock stepped back.
fn elapsed(from: DateTime<Utc>, to: DateTime<Utc>) -> TimeDelta {
    (to - from).max(TimeDelta::zero())
}

/// A request waiting in the driver's queue and how many times it has already been re-attempted
/// after a transient failure. The count rides the queue slot rather than [`Request`], because a
/// `Request` is hashed into the ledger (`filter_params`) and a retry asks the identical question —
/// the row must keep saying what was asked, and the fresh ordinal alone makes the retry's
/// `attempt_id` distinct.
pub(crate) struct Queued {
    pub(crate) request: Request,
    pub(crate) retries: u32,
    /// Counted against the follow-up cap when first popped: a `Document` follow-up an index
    /// listed, or a carried one that is no walk id. A planned request, an `Index` follow-up, a
    /// walk id and a retry are not.
    pub(crate) counted: bool,
    /// The open `deferred` rows a carried request closes once asked (S7-D5); empty otherwise.
    pub(crate) carried: Vec<i64>,
    /// A `Document` follow-up's or walk id's index attempt; a carried one's oldest row's; else
    /// None (R22).
    pub(crate) listed_by: Option<String>,
}

impl Queued {
    /// A planned request, not yet attempted.
    pub(crate) const fn first_attempt(request: Request) -> Self {
        Self {
            request,
            retries: 0,
            counted: false,
            carried: Vec::new(),
            listed_by: None,
        }
    }

    /// A follow-up the index attempt `listing` listed, not yet attempted.
    pub(crate) fn follow_up(request: Request, listing: Option<&str>) -> Self {
        let document = request.phase == RequestPhase::Document;
        Self {
            counted: document,
            listed_by: listing.filter(|_| document).map(str::to_owned),
            ..Self::first_attempt(request)
        }
    }

    /// A walk id the index attempt `listing` walked to: linked like a follow-up, never counted.
    pub(crate) fn walked(request: Request, listing: Option<&str>) -> Self {
        Self {
            counted: false,
            ..Self::follow_up(request, listing)
        }
    }

    /// A transient failure is asked again until `MAX_TRANSIENT_RETRIES` re-attempts. §8 R21.
    pub(crate) const fn retried(transient: bool, retries: u32) -> bool {
        transient && retries < MAX_TRANSIENT_RETRIES
    }
}

/// The driver's queue: one lane per url host (`host_of`, the key `Wire` paces by), served
/// round-robin in the order each host first entered, so a wait owed to one host does not stand in
/// front of another host's due request. One host is one lane (§10).
pub(crate) struct HostQueue {
    /// `None` keys every url `host_of` cannot key; `Wire` refuses or fails those itself.
    lanes: Vec<(Option<HostKey>, Lane)>,
    next: usize,
    /// [`Provider::follow_up_cap`], and how many counted follow-ups `pop` has handed out to ask.
    cap: Option<u32>,
    issued: u32,
}

/// One host's requests: this run's own (planned, follow-ups, walk ids, retries) in `fresh`, an
/// earlier run's carried ones in `carried`, asked only once `fresh` is empty (S7-D5, 2026-11-19).
/// A fresh `Document` follow-up holds a [`Slot::Document`] in `fresh`, filled when popped from
/// `named` (its row names a ticker) first, then `unnamed`, each in listing order (2026-11-19).
#[derive(Default)]
struct Lane {
    fresh: VecDeque<Slot>,
    named: VecDeque<Queued>,
    unnamed: VecDeque<Queued>,
    carried: VecDeque<Queued>,
}

/// A place in a lane's `fresh` order: a request that keeps it, or one a document follow-up fills.
enum Slot {
    Kept(Box<Queued>),
    Document,
}

impl Lane {
    fn pop_front(&mut self) -> Option<Queued> {
        match self.fresh.pop_front() {
            Some(Slot::Kept(queued)) => Some(*queued),
            Some(Slot::Document) => self.named.pop_front().or_else(|| self.unnamed.pop_front()),
            None => self.carried.pop_front(),
        }
    }

    /// Every request in the order `pop_front` would hand them out, the lane left empty.
    fn drain(&mut self) -> Vec<Queued> {
        std::iter::from_fn(|| self.pop_front()).collect()
    }
}

/// What [`HostQueue::pop`] hands the driver: a request to ask, or one past the cap to defer.
pub(crate) enum Popped {
    Ask(Queued),
    Defer(Queued),
}

impl HostQueue {
    pub(crate) fn of(requests: Vec<Request>, cap: Option<u32>) -> Self {
        let mut queue = Self {
            lanes: Vec::new(),
            next: 0,
            cap,
            issued: 0,
        };
        for request in requests {
            queue.push(Queued::first_attempt(request));
        }
        queue
    }

    /// Its host's lane; a host not seen before opens a lane after every other.
    fn lane_of(&mut self, request: &Request) -> &mut Lane {
        let host = host_of(&request.url).ok().flatten();
        let at = match self.lanes.iter().position(|(key, _)| *key == host) {
            Some(at) => at,
            None => {
                self.lanes.push((host, Lane::default()));
                self.lanes.len() - 1
            }
        };
        &mut self.lanes[at].1
    }

    /// To the back of its host's fresh requests.
    fn push(&mut self, queued: Queued) {
        self.lane_of(&queued.request)
            .fresh
            .push_back(Slot::Kept(Box::new(queued)));
    }

    /// A fresh `Document` follow-up: a slot at the back of its host's fresh requests, filled by
    /// the lane's first named follow-up, else its first unnamed one.
    fn push_document(&mut self, queued: Queued) {
        let lane = self.lane_of(&queued.request);
        lane.fresh.push_back(Slot::Document);
        if queued.request.names_ticker {
            lane.named.push_back(queued);
        } else {
            lane.unnamed.push_back(queued);
        }
    }

    /// Each fresh unnamed follow-up whose story (`story_of`) is in `stories` moves to the back of
    /// its lane's named ones: a later index named a ticker for a story an earlier one listed
    /// without. Its request, listing and cap count are unchanged; anything else is left in place.
    pub(crate) fn promote(
        &mut self,
        stories: &BTreeSet<String>,
        story_of: impl Fn(&Request) -> Option<String>,
    ) {
        if stories.is_empty() {
            return;
        }
        for (_, lane) in &mut self.lanes {
            let (promoted, kept) = lane.unnamed.drain(..).partition(|queued: &Queued| {
                story_of(&queued.request).is_some_and(|story| stories.contains(&story))
            });
            lane.unnamed = kept;
            lane.named.extend(promoted);
        }
    }

    /// After `asked` is attempted as `listing`: its follow-ups, its walk ids, then itself again
    /// when `retrying`, each to the back of its host's fresh requests, so the pace limiter (R19)
    /// spaces the retry. §8 R21. A retry is never counted again.
    pub(crate) fn requeue(
        &mut self,
        follow_ups: Vec<Request>,
        walked: Vec<Request>,
        asked: Queued,
        retrying: bool,
        listing: Option<&str>,
    ) {
        for follow_up in follow_ups {
            let queued = Queued::follow_up(follow_up, listing);
            if queued.request.phase == RequestPhase::Document {
                self.push_document(queued);
            } else {
                self.push(queued);
            }
        }
        for id in walked {
            self.push(Queued::walked(id, listing));
        }
        if retrying {
            self.push(Queued {
                request: asked.request,
                retries: asked.retries + 1,
                counted: false,
                carried: Vec::new(),
                listed_by: asked.listed_by,
            });
        }
    }

    /// S7-D5 as amended 2026-11-19: each carried request, in the order given, behind every fresh
    /// request its host's lane holds or will hold this run.
    pub(crate) fn carry(&mut self, carried: Vec<Queued>) {
        for queued in carried {
            self.lane_of(&queued.request).carried.push_back(queued);
        }
    }

    /// `stopped` at the deadline: its cap slot back when counted, since it is never asked, and
    /// its host's lane emptied in the order it would have been asked.
    pub(crate) fn stop(&mut self, stopped: &Queued) -> Vec<Queued> {
        if stopped.counted {
            self.issued = self.issued.saturating_sub(1);
        }
        let host = host_of(&stopped.request.url).ok().flatten();
        self.lanes
            .iter_mut()
            .find(|(key, _)| *key == host)
            .map(|(_, lane)| lane.drain())
            .unwrap_or_default()
    }

    /// The front of the first non-empty lane at or after the one after the last served; a counted
    /// follow-up past the cap comes back as `Defer`, so the cap binds in round-robin order.
    pub(crate) fn pop(&mut self) -> Option<Popped> {
        let lanes = self.lanes.len();
        let queued = (0..lanes).find_map(|step| {
            let at = (self.next + step) % lanes;
            let queued = self.lanes.get_mut(at)?.1.pop_front()?;
            self.next = at + 1;
            Some(queued)
        })?;
        if !queued.counted {
            return Some(Popped::Ask(queued));
        }
        match self.cap {
            Some(cap) if self.issued >= cap => Some(Popped::Defer(queued)),
            _ => {
                self.issued += 1;
                Some(Popped::Ask(queued))
            }
        }
    }
}

/// What every attempt of one run is driven with. Grouped because all five are fixed for the whole
/// run and only the request and its ordinal change.
struct Driving<'a> {
    provider: &'a dyn Provider,
    exchange: &'a dyn Exchange,
    clock: &'a dyn Clock,
    run: &'a RunId,
    baseline: &'a Baseline,
}

/// What [`attempt_once`] made of one request.
enum Sent {
    Attempted(Box<Attempted>),
    /// The send gate refused it: its attempt is closed `not_sent` and `row` is its envelope row.
    /// `owed` is the gate's wait when the run's deadline, and no other sender, was the cause.
    Refused {
        row: Box<AttemptRow>,
        owed: Option<Duration>,
    },
}

/// One attempt, from its `Opened` row to its committed rows.
struct Attempted {
    row: AttemptRow,
    follow_ups: Vec<Request>,
    /// [`Extracted::walk`]'s ids left to issue, outside the follow-up cap (S7-D6's exception).
    walked: Vec<Request>,
    kind: OutcomeKind,
    permanent: bool,
    /// The answer's own `received_at` (or `failed_at`) less its `requested_at`.
    requesting: TimeDelta,
    bytes_written: u64,
    blob_written: bool,
    committed: Committed,
    /// The story a `Document` attempt asked for.
    story: Option<String>,
    /// A live index's `held`: its archive-held part is written at the attempt, the rest once the
    /// run's queue is empty.
    held: Option<PendingHeld>,
    /// Stories this index names a ticker for that an earlier index of this run queued without.
    promoted: BTreeSet<String>,
}

/// A live index's `held` (section 11.1) before the run ends: `held` follow-ups the archive held,
/// and the stories of `queued` ones this run had queued, which count only if asked.
struct PendingHeld {
    attempt_id: String,
    held: u32,
    queued: Vec<String>,
}

impl Attempted {
    /// A failed outcome the classifier did NOT call permanent: an HTTP 503 or a transport error,
    /// which §5.1 marks retryable (`vendor_unreachable`). The only input the driver's retry
    /// decision reads, derived from the fields already here rather than a new classifier: a
    /// permanent 4xx or `vendor_refused` is `Failed` with `permanent`, and `Ok`/`Empty` are not
    /// failures at all.
    const fn is_transient(&self) -> bool {
        matches!(self.kind, OutcomeKind::Failed) && !self.permanent
    }
}

/// `issued` holds the story of every request this run queued: a second index listing one of them
/// in the same run, PRN's sitemap beside its RSS (section 11.2), drops it, and counts it held once
/// the run has asked it (PR #50's question 1).
fn attempt_once<A: Archive>(
    driving: &Driving,
    archive: &mut A,
    request: &Request,
    ordinal: u32,
    listed_by: Option<&str>,
    issued: &mut BTreeSet<String>,
) -> Result<Sent, CollectError> {
    let Driving {
        provider,
        exchange,
        clock,
        run,
        baseline,
    } = *driving;
    // Document only: open-web's `item_key` can key an index url, which would hold a story.
    let story = (request.phase == RequestPhase::Document)
        .then(|| provider.story_of(request))
        .flatten();
    let mut slot = AttemptSlot::open_listed(
        archive,
        request,
        story.as_ref(),
        listed_by,
        run,
        ordinal,
        ObservationTime::new(clock.now()),
    )?;
    let attempt_id = slot.attempt_id().hex().to_owned();
    let permit = slot.take_permit()?;
    let answer = match exchange.send(permit, request, clock) {
        Ok(response) => Ok(response),
        Err(SendFailure::Transport(failed)) => Err(failed),
        Err(SendFailure::CredentialMissing(name)) => return Err(CollectError::Credential(name)),
        Err(SendFailure::Budget(why)) if why.is_a_gate_refusal() => {
            let at = ObservationTime::new(clock.now());
            let closed = slot.close_unsent(archive, at)?;
            archive.commit(closed, &[], None, at)?;
            let row = Box::new(AttemptRow {
                attempt_id,
                source: request.source.as_str().to_owned(),
                target_window: request.target_window.as_str().to_owned(),
                phase: request.phase.name(),
                provenance: request.provenance.name(),
                url: request.url.clone(),
                transport_status: None,
                outcome: OutcomeKind::NotSent.name(),
                vendor_rows: 0,
                filtered_out: 0,
                robots_disallowed: 0,
                unusable_rows: 0,
                items_committed: 0,
                commit_refused: None,
                not_sent: Some(why.to_string()),
            });
            return Ok(Sent::Refused {
                row,
                owed: why.owed_past_the_deadline(),
            });
        }
        Err(SendFailure::Budget(why)) => return Err(CollectError::Budget(why)),
    };
    let verdict = provider.classify(request, &answer);
    let payload = Payload::stored(request, &answer, |body| archive.put_blob(body))?;
    // Extraction only reads a payload the provider called usable, so a vendor's own error page and
    // a cut body are stored as evidence and never parsed into a row count.
    let usable = matches!(verdict, Verdict::Usable);
    let extracted = match (&answer, &payload) {
        (Ok(response), Some(stored)) if usable => Some(provider.extract(
            request,
            &RawBlob {
                digest: stored.raw_blob.clone(),
                bytes: response.body().to_vec(),
            },
        )),
        _ => None,
    };
    let (vendor_rows, filtered_out, robots_disallowed, unusable_rows, listing_unusable) =
        match &extracted {
            Some(extracted) => (
                extracted.vendor_rows,
                extracted.filtered_out,
                extracted.robots_disallowed,
                count(extracted.unusable.len()),
                count(
                    extracted
                        .unusable
                        .iter()
                        .filter(|row| row.decided_by == DecidedBy::IndexRow)
                        .count(),
                ),
            ),
            None => (0, 0, 0, 0, 0),
        };
    let counts = Counts {
        vendor_rows,
        listing_unusable,
        robots_disallowed,
    };
    let outcome = Outcome::decide(&verdict, counts, baseline);
    let kind = outcome.kind();
    let closed = slot.close(archive, outcome, &answer, ObservationTime::new(clock.now()))?;
    let (status, observed) = match &answer {
        Ok(response) => (Some(response.status()), response.received_at()),
        Err(failed) => (failed.status(), failed.failed_at()),
    };
    let no_items: Vec<CollectedItem> = Vec::new();
    let items = extracted.as_ref().map_or(&no_items, |one| &one.items);
    let filed = archive.commit(closed, items, payload.as_ref(), observed);
    let (committed, refusal) = match filed {
        Ok(committed) => (committed, None),
        Err(refused) => (Committed::default(), Some(refused.detail)),
    };
    // A vendor_archive index records `matched` and hands over one window's covered set (R20); a
    // live index records `held` and hands over the held set of every window (§11.1, R16).
    let mut held = None;
    let mut promoted = BTreeSet::new();
    let (follow_ups, walked) = match extracted {
        Some(extracted) if request.phase == RequestPhase::Index => {
            let matched = count(extracted.follow_ups.len());
            let archived = request.provenance == Provenance::VendorArchive;
            let mut stories = if archived {
                archive.record_planned_coverage(&attempt_id, matched)?;
                archive.covered_story_ids(&request.source, &request.target_window)?
            } else {
                archive.held_story_ids(&request.source, provider.transport().body_limit)?
            };
            let in_run: BTreeSet<String> = issued.difference(&stories).cloned().collect();
            stories.extend(issued.iter().cloned());
            let listed = extracted.follow_ups.clone();
            let to_issue = if matched == 0 {
                Vec::new()
            } else {
                provider.follow_ups_to_issue(extracted.follow_ups, &stories)
            };
            let stories_of = |issuing: &[Request]| -> Vec<String> {
                issuing
                    .iter()
                    .filter_map(|follow_up| provider.story_of(follow_up))
                    .map(|story| story.hex().to_owned())
                    .collect()
            };
            let named: Vec<Request> = listed
                .iter()
                .filter(|follow_up| follow_up.names_ticker)
                .cloned()
                .collect();
            promoted = stories_of(&named)
                .into_iter()
                .filter(|story| in_run.contains(story))
                .collect();
            issued.extend(stories_of(&to_issue));
            stories.extend(stories_of(&to_issue));
            let walked = provider.follow_ups_to_issue(extracted.walk, &stories);
            issued.extend(stories_of(&walked));
            if !archived && refusal.is_none() {
                let queued: Vec<String> = stories_of(&listed)
                    .into_iter()
                    .filter(|story| in_run.contains(story))
                    .collect();
                let dropped = matched.saturating_sub(count(to_issue.len()));
                let pending = PendingHeld {
                    attempt_id: attempt_id.clone(),
                    held: dropped.saturating_sub(count(queued.len())),
                    queued,
                };
                // The final part now, so a run that aborts leaves no NULL beside `vendor_rows`.
                archive.record_held(&attempt_id, pending.held)?;
                held = Some(pending);
                archive.record_relisted(&attempt_id, &relisted_of(provider, listed, &to_issue))?;
            }
            (to_issue, walked)
        }
        _ => (Vec::new(), Vec::new()),
    };
    Ok(Sent::Attempted(Box::new(Attempted {
        row: AttemptRow {
            attempt_id,
            source: request.source.as_str().to_owned(),
            target_window: request.target_window.as_str().to_owned(),
            phase: request.phase.name(),
            provenance: request.provenance.name(),
            url: request.url.clone(),
            transport_status: status,
            outcome: kind.name(),
            vendor_rows,
            filtered_out,
            robots_disallowed,
            unusable_rows,
            items_committed: committed.items_committed,
            commit_refused: refusal,
            not_sent: None,
        },
        follow_ups,
        walked,
        kind,
        permanent: matches!(verdict, Verdict::PermanentFailure),
        requesting: match &answer {
            Ok(response) => elapsed(response.requested_at().inner(), observed.inner()),
            Err(failed) => elapsed(failed.requested_at().inner(), observed.inner()),
        },
        bytes_written: match &answer {
            Ok(response) => u64::try_from(response.body().len()).unwrap_or(u64::MAX),
            Err(failed) => failed
                .cut_body()
                .map_or(0, |cut| u64::try_from(cut.len()).unwrap_or(u64::MAX)),
        },
        blob_written: payload.is_some(),
        committed,
        story: story.map(|story| story.hex().to_owned()),
        held,
        promoted,
    })))
}

/// The follow-ups `listed` holds and `issued` does not, each with its story and listed value.
fn relisted_of(provider: &dyn Provider, listed: Vec<Request>, issued: &[Request]) -> Vec<Relisted> {
    listed
        .into_iter()
        .filter(|follow_up| !issued.contains(follow_up))
        .filter_map(|follow_up| {
            Some(Relisted {
                story_id: provider.story_of(&follow_up)?.hex().to_owned(),
                listed: provider.listed_publication(&follow_up),
                listed_raw: follow_up.listed_date,
            })
        })
        .collect()
}

impl Payload {
    /// Hands `put` the body an answer carried, whole or cut at the limit, BEFORE anything parses
    /// it, and names the digest `put` returns. `None`, and no `put`, for an answer with no body.
    /// The one rule for `collect` and `newsvault robots refresh` alike.
    pub fn stored<E>(
        request: &Request,
        answer: &Result<RawResponse, TransportError>,
        put: impl FnOnce(&[u8]) -> Result<String, E>,
    ) -> Result<Option<Self>, E> {
        let Some(got) = crate::wire::received(answer) else {
            return Ok(None);
        };
        Ok(Some(Self {
            raw_blob: put(got.body)?,
            url: request.url.clone(),
            phase: request.phase,
            listed_date: request.listed_date.clone(),
            headers: got.headers.clone(),
            truncated: got.truncated,
        }))
    }
}

/// Closes every attempt no process can answer for any more, which is [`OutcomeKind::Interrupted`]'s
/// only producer. It runs under the same single-writer lease as the rest of the run.
fn sweep<A: Archive>(
    archive: &mut A,
    run: &RunId,
    at: ObservationTime,
) -> Result<Vec<String>, CollectError> {
    let mut swept = Vec::new();
    for attempt in archive.attempts_in_flight()? {
        let closed = abandon(archive, AttemptId::recorded(attempt.clone()), at, run)?;
        // The store refuses item rows under an abandoned attempt, so this cannot launder content.
        archive.commit(closed, &[], None, at)?;
        swept.push(attempt);
    }
    Ok(swept)
}

impl Collected {
    /// The carried item whose rows are `deferred_ids`, closed as `closing`.
    fn close_carried(&mut self, deferred_ids: &[i64], closing: Closing) {
        if let Some(item) = self
            .carried
            .iter_mut()
            .find(|item| item.deferred_ids == deferred_ids)
        {
            item.closed = Some(closing);
        }
    }

    /// Writes the `deferred` row before the run moves on, then counts it.
    fn defer<A: Archive>(
        &mut self,
        archive: &mut A,
        deferred: Deferred,
    ) -> Result<(), CollectError> {
        archive.record_deferred(&deferred)?;
        self.tally.deferred += 1;
        self.deferred.push(deferred);
        Ok(())
    }

    /// `retrying` true = the driver will re-queue this transient failure, so its outcome is held
    /// out of the exit tally (the request's final disposition is still pending); the attempt ROW
    /// is recorded either way — "we looked and failed" holds whatever the retry does (R4). §8 R21.
    fn absorb(&mut self, attempted: &Attempted, retrying: bool) {
        if !retrying {
            match attempted.kind {
                OutcomeKind::Ok => self.tally.ok += 1,
                OutcomeKind::Empty => self.tally.empty += 1,
                OutcomeKind::Failed if attempted.permanent => self.tally.permanent += 1,
                OutcomeKind::Failed => self.tally.transient += 1,
                // The sweep's attempts are counted where they are closed, and a refused send
                // where its `deferred` row is written, not here.
                OutcomeKind::Interrupted | OutcomeKind::NotSent => {}
            }
        }
        self.tally.committed += attempted.committed.items_committed;
        self.tally.commit_refused += u32::from(attempted.row.commit_refused.is_some());
        self.blobs_written += u32::from(attempted.blob_written);
        self.bytes_written += attempted.bytes_written;
        self.blobs_indexed += attempted.committed.blobs_indexed;
        self.bytes_indexed += attempted.committed.bytes_indexed;
        self.seq_high_water = self.seq_high_water.max(attempted.committed.seq_hi);
        self.attempts.push(attempted.row.clone());
    }
}

/// One entry per planned source, zeros included.
fn tally_of(source: &SourceId, collected: &Collected) -> SourceTally {
    let mine = |row: &&AttemptRow| row.source == source.as_str();
    let sum = |pick: fn(&AttemptRow) -> u32| -> u32 {
        collected.attempts.iter().filter(mine).map(pick).sum()
    };
    let outcomes = |label: &'static str| -> u32 {
        count(
            collected
                .attempts
                .iter()
                .filter(mine)
                .filter(|row| row.outcome == label)
                .count(),
        )
    };
    SourceTally {
        source: source.as_str().to_owned(),
        planned_requests: collected.planned_requests,
        not_planned: count(collected.not_planned.len()),
        attempts: count(collected.attempts.iter().filter(mine).count()),
        ok: outcomes(OutcomeKind::Ok.name()),
        empty: outcomes(OutcomeKind::Empty.name()),
        failed: outcomes(OutcomeKind::Failed.name()),
        vendor_rows: sum(|row| row.vendor_rows),
        filtered_out: sum(|row| row.filtered_out),
        robots_disallowed: sum(|row| row.robots_disallowed),
        unusable_rows: sum(|row| row.unusable_rows),
        items_committed: sum(|row| row.items_committed),
        deferred: count(
            collected
                .deferred
                .iter()
                .filter(|row| row.source == source.as_str())
                .count(),
        ),
        carried: carry_tally(source, &collected.carried),
    }
}

fn carry_tally(source: &SourceId, carried: &[Carried]) -> CarryTally {
    let mine = || carried.iter().filter(|item| item.source == source.as_str());
    let closed = |name: &str| -> u32 {
        count(
            mine()
                .filter(|item| item.closed.as_ref().map(Closing::name) == Some(name))
                .count(),
        )
    };
    CarryTally {
        queued: count(mine().filter(|item| item.queued).count()),
        answered: closed("answered"),
        held: closed("held"),
        expired: closed("expired"),
    }
}

fn count(of: usize) -> u32 {
    u32::try_from(of).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::{Exit, HostQueue, Popped, Queued, Tally, EXITS};
    use crate::provider::Provenance;
    use crate::request::{Request, RequestPhase, SourceId, TargetWindow};
    use canon::FileDigest;
    use std::collections::BTreeSet;

    const GNW: &str = "https://rss.globenewswire.com/";
    const PRN: &str = "https://www.prnewswire.com/";
    const CNW: &str = "https://www.newswire.ca/";
    const SEC: &str = "https://www.sec.gov/";

    fn asking(base: &str, path: &str) -> Request {
        Request {
            source: SourceId::new("open_web_wires"),
            target_window: TargetWindow::new("2026-11-16"),
            phase: RequestPhase::Document,
            provenance: Provenance::LiveIngest,
            url: format!("{base}{path}"),
            headers: Vec::new(),
            params: Vec::new(),
            universe_version: FileDigest::of(b"{}"),
            listed_date: None,
            names_ticker: false,
            hosts_version: None,
            extract_version: None,
        }
    }

    /// What one host's index answers in [`capped`]: `documents` follow-ups `p0..`, `walked` walk
    /// ids `w0..` and `pages` `Index` follow-ups `page0..`, each on the index's own host.
    #[derive(Clone, Copy)]
    struct Lane {
        documents: usize,
        walked: usize,
        pages: usize,
    }

    const fn documents(documents: usize) -> Lane {
        Lane {
            documents,
            walked: 0,
            pages: 0,
        }
    }

    /// `collect`'s loop without the socket, under `cap`: every url asked and every url deferred,
    /// in order; `failing` fails transiently on every attempt.
    fn capped(
        plan: &[(&str, Lane)],
        cap: Option<u32>,
        failing: &str,
    ) -> (Vec<String>, Vec<String>) {
        stopped(plan, cap, failing, "")
    }

    /// [`capped`], with `stop` reserved past the deadline as `collect` handles it: deferred with
    /// the rest of its host's lane.
    fn stopped(
        plan: &[(&str, Lane)],
        cap: Option<u32>,
        failing: &str,
        stop: &str,
    ) -> (Vec<String>, Vec<String>) {
        carrying(plan, Vec::new(), cap, failing, stop)
    }

    /// [`stopped`], with `carried` handed to `HostQueue::carry` before the first `pop`.
    fn carrying(
        plan: &[(&str, Lane)],
        carried: Vec<Queued>,
        cap: Option<u32>,
        failing: &str,
        stop: &str,
    ) -> (Vec<String>, Vec<String>) {
        let indexes = plan.iter().map(|(base, _)| asking(base, "index")).collect();
        let mut queue = HostQueue::of(indexes, cap);
        queue.carry(carried);
        let (mut asked, mut deferred) = (Vec::new(), Vec::new());
        while let Some(popped) = queue.pop() {
            let queued = match popped {
                Popped::Ask(queued) => queued,
                Popped::Defer(queued) => {
                    deferred.push(queued.request.url);
                    continue;
                }
            };
            if queued.request.url == stop {
                let lane = queue.stop(&queued);
                deferred.push(queued.request.url);
                deferred.extend(lane.into_iter().map(|rest| rest.request.url));
                continue;
            }
            let Queued {
                request, retries, ..
            } = queued;
            asked.push(request.url.clone());
            let base = request.url.trim_end_matches("index").to_owned();
            let base = base.as_str();
            let lane = plan
                .iter()
                .find(|(host, _)| request.url.ends_with("index") && *host == base)
                .map_or(documents(0), |(_, lane)| *lane);
            let named = |prefix: &str, many: usize| -> Vec<Request> {
                (0..many)
                    .map(|nth| asking(base, &format!("{prefix}{nth}")))
                    .collect()
            };
            let pages = named("page", lane.pages).into_iter().map(|page| Request {
                phase: RequestPhase::Index,
                ..page
            });
            let follow_ups = named("p", lane.documents)
                .into_iter()
                .chain(pages)
                .collect();
            let retrying = Queued::retried(request.url == failing, retries);
            let again = Queued {
                request,
                retries,
                counted: false,
                carried: Vec::new(),
                listed_by: None,
            };
            queue.requeue(follow_ups, named("w", lane.walked), again, retrying, None);
        }
        (asked, deferred)
    }

    /// [`capped`] with no cap, each `(host, pages)` index answering `pages` documents.
    fn driven(plan: &[(&str, usize)], failing: &str) -> Vec<String> {
        let lanes: Vec<(&str, Lane)> = plan
            .iter()
            .map(|(base, pages)| (*base, documents(*pages)))
            .collect();
        capped(&lanes, None, failing).0
    }

    /// S7-D6 at its boundary: the 300th follow-up is asked and the 301st deferred; with no cap
    /// (edgar's `None`) all 301 are asked.
    #[test]
    fn the_cap_asks_the_300th_follow_up_and_defers_the_301st() {
        let at_cap = capped(&[(CNW, documents(300))], Some(300), "");
        let over = capped(&[(CNW, documents(301))], Some(300), "");
        let uncapped = capped(&[(CNW, documents(301))], None, "");
        assert_eq!(
            (
                at_cap.0.len(),
                at_cap.1.len(),
                over.0.len(),
                uncapped.0.len()
            ),
            (301, 0, 301, 302)
        );
        assert_eq!(
            (over.0.last(), over.1, uncapped.1.len()),
            (Some(&format!("{CNW}p299")), urls(&[(CNW, "p300")]), 0)
        );
    }

    /// S7-D6: global and counted at issue in round-robin order, so the cap cuts the tails of the
    /// longest lanes, interleaved as round-robin reaches them, and never a host's whole lane.
    #[test]
    fn the_cap_is_counted_at_issue_across_hosts_in_round_robin_order() {
        let plan = [
            (GNW, documents(2)),
            (PRN, documents(4)),
            (CNW, documents(4)),
        ];
        let (asked, deferred) = capped(&plan, Some(5), "");
        assert_eq!(
            (asked, deferred),
            (
                urls(&[
                    (GNW, "index"),
                    (PRN, "index"),
                    (CNW, "index"),
                    (GNW, "p0"),
                    (PRN, "p0"),
                    (CNW, "p0"),
                    (GNW, "p1"),
                    (PRN, "p1"),
                ]),
                urls(&[
                    (CNW, "p1"),
                    (PRN, "p2"),
                    (CNW, "p2"),
                    (PRN, "p3"),
                    (CNW, "p3")
                ])
            )
        );
    }

    /// S7-D6's arithmetic (architecture.md section 11.5 step 7): a first run's 301 follow-ups,
    /// GNW 20, PRN 20, CNW 100, Fed 20 and Yahoo 141, lose 1 of Yahoo's, the longest lane's last,
    /// under the cap of 300 (2026-11-19); 55 of CNW's and 96 of Yahoo's under the 150 before it.
    #[test]
    fn the_global_cap_cuts_one_of_yahoos_first_run_follow_ups() {
        const FED: &str = "https://www.federalreserve.gov/";
        const YAHOO: &str = "https://finance.yahoo.com/";
        let plan = [
            (GNW, documents(20)),
            (PRN, documents(20)),
            (CNW, documents(100)),
            (FED, documents(20)),
            (YAHOO, documents(141)),
        ];
        let (_, deferred) = capped(&plan, Some(300), "");
        let lost: Vec<usize> = plan
            .iter()
            .map(|(base, _)| deferred.iter().filter(|url| url.starts_with(base)).count())
            .collect();
        assert_eq!(lost, vec![0, 0, 0, 0, 1]);
    }

    /// S7-D6's exception (owner, 2026-11-17 ~21:30Z): walk ids are never counted, so a cap the
    /// listed follow-ups exhaust still asks every walk id, and none is deferred.
    #[test]
    fn walk_ids_are_never_counted_against_the_cap_or_deferred() {
        let lane = Lane {
            documents: 2,
            walked: 3,
            pages: 0,
        };
        let (asked, deferred) = capped(&[(GNW, lane)], Some(1), "");
        assert_eq!(
            (asked, deferred),
            (
                urls(&[
                    (GNW, "index"),
                    (GNW, "p0"),
                    (GNW, "w0"),
                    (GNW, "w1"),
                    (GNW, "w2")
                ]),
                urls(&[(GNW, "p1")])
            )
        );
    }

    /// §8 R21 beside S7-D6: a retry is the same follow-up asked again, never a new one, so it is
    /// asked after the cap is reached and takes no other follow-up's place.
    #[test]
    fn a_retry_is_not_counted_against_the_cap() {
        let failing = format!("{PRN}p0");
        let (asked, deferred) = capped(&[(PRN, documents(3))], Some(2), &failing);
        assert_eq!(
            (asked, deferred),
            (
                urls(&[
                    (PRN, "index"),
                    (PRN, "p0"),
                    (PRN, "p1"),
                    (PRN, "p0"),
                    (PRN, "p0")
                ]),
                urls(&[(PRN, "p2")])
            )
        );
    }

    /// An `Index` follow-up, PRN's next sitemap page, is outside the cap: section 11.2's worst
    /// case counts index requests apart from it, and a page cut would leave its poll unjoined.
    #[test]
    fn an_index_follow_up_is_not_counted_against_the_cap() {
        let lane = Lane {
            documents: 2,
            walked: 0,
            pages: 2,
        };
        let (asked, deferred) = capped(&[(PRN, lane)], Some(1), "");
        assert_eq!(
            (asked, deferred),
            (
                urls(&[(PRN, "index"), (PRN, "p0"), (PRN, "page0"), (PRN, "page1")]),
                urls(&[(PRN, "p1")])
            )
        );
    }

    /// Condition A: a host stopped at the deadline leaves its whole lane deferred in queue order,
    /// walk ids and an `Index` page included, while the other hosts keep their turns.
    #[test]
    fn a_stopped_host_defers_its_whole_lane_and_the_other_hosts_keep_their_turns() {
        let lane = Lane {
            documents: 2,
            walked: 2,
            pages: 1,
        };
        let plan = [(GNW, lane), (PRN, documents(3))];
        let (asked, deferred) = stopped(&plan, None, "", &format!("{GNW}p0"));
        assert_eq!(
            (asked, deferred),
            (
                urls(&[
                    (GNW, "index"),
                    (PRN, "index"),
                    (PRN, "p0"),
                    (PRN, "p1"),
                    (PRN, "p2")
                ]),
                urls(&[
                    (GNW, "p0"),
                    (GNW, "p1"),
                    (GNW, "page0"),
                    (GNW, "w0"),
                    (GNW, "w1")
                ])
            )
        );
    }

    /// A counted follow-up stopped at the deadline is never asked, so it gives its cap slot back:
    /// PRN's second follow-up is asked under a cap of 2, not deferred as past the cap.
    #[test]
    fn a_follow_up_stopped_at_the_deadline_spends_no_cap_slot() {
        let plan = [(GNW, documents(2)), (PRN, documents(2))];
        let (asked, deferred) = stopped(&plan, Some(2), "", &format!("{GNW}p0"));
        assert_eq!(
            (asked, deferred),
            (
                urls(&[(GNW, "index"), (PRN, "index"), (PRN, "p0"), (PRN, "p1")]),
                urls(&[(GNW, "p0"), (GNW, "p1")])
            )
        );
    }

    fn urls(of: &[(&str, &str)]) -> Vec<String> {
        of.iter()
            .map(|(base, path)| format!("{base}{path}"))
            .collect()
    }

    /// Owner decision 2026-11-17 (§10): hosts in first-appearance order, one request each per turn.
    #[test]
    fn the_queue_serves_three_hosts_round_robin_in_the_order_each_host_first_appeared() {
        assert_eq!(
            driven(&[(GNW, 2), (PRN, 2), (CNW, 2)], ""),
            urls(&[
                (GNW, "index"),
                (PRN, "index"),
                (CNW, "index"),
                (GNW, "p0"),
                (PRN, "p0"),
                (CNW, "p0"),
                (GNW, "p1"),
                (PRN, "p1"),
                (CNW, "p1"),
            ])
        );
    }

    /// §8 R21: a retry goes to the back of its own host's lane, and stops at the cap.
    #[test]
    fn a_retry_goes_to_the_back_of_its_host_lane_and_stops_at_the_cap() {
        let failing = format!("{PRN}p0");
        assert_eq!(
            driven(&[(GNW, 2), (PRN, 2)], &failing),
            urls(&[
                (GNW, "index"),
                (PRN, "index"),
                (GNW, "p0"),
                (PRN, "p0"),
                (GNW, "p1"),
                (PRN, "p1"),
                (PRN, "p0"),
                (PRN, "p0"),
            ])
        );
    }

    /// §8 R21: a retry on a host with a shorter lane keeps that host's turn and does not wait for
    /// the longer lane to drain, which the former back-of-the-whole-queue order would.
    #[test]
    fn a_retry_on_a_short_lane_is_served_in_its_host_turn_while_a_longer_lane_drains() {
        let failing = format!("{GNW}p0");
        assert_eq!(
            driven(&[(GNW, 1), (PRN, 3)], &failing),
            urls(&[
                (GNW, "index"),
                (PRN, "index"),
                (GNW, "p0"),
                (PRN, "p0"),
                (GNW, "p0"),
                (PRN, "p1"),
                (GNW, "p0"),
                (PRN, "p2"),
            ])
        );
    }

    /// EDGAR asks one host, so its order is the FIFO it was before round-robin.
    #[test]
    fn one_host_is_one_lane_and_keeps_the_fifo_order() {
        let failing = format!("{SEC}p0");
        assert_eq!(
            driven(&[(SEC, 2)], &failing),
            urls(&[
                (SEC, "index"),
                (SEC, "p0"),
                (SEC, "p1"),
                (SEC, "p0"),
                (SEC, "p0"),
            ])
        );
    }

    /// One row per bucket of architecture.md section 5.1's table, each stating the counts that reach
    /// it.
    const TABLE: [(&str, Tally, Exit); 9] = [
        (
            "every target committed and a follow-up deferred (S7-D8)",
            Tally {
                committed: 2,
                ok: 1,
                deferred: 1,
                ..ZERO
            },
            Exit::PartialCollect,
        ),
        (
            "nothing committed, every failure transient and a follow-up deferred",
            Tally {
                transient: 1,
                deferred: 1,
                ..ZERO
            },
            Exit::PartialCollect,
        ),
        (
            "every target ok or empty",
            Tally {
                committed: 2,
                ok: 1,
                empty: 1,
                ..ZERO
            },
            Exit::Ok,
        ),
        (
            "nothing asked for, every reason clean",
            Tally { ..ZERO },
            Exit::Ok,
        ),
        (
            "something committed and something failed",
            Tally {
                committed: 2,
                ok: 1,
                transient: 1,
                ..ZERO
            },
            Exit::PartialCollect,
        ),
        (
            "nothing committed, every failure transient",
            Tally {
                transient: 2,
                ..ZERO
            },
            Exit::VendorUnreachable,
        ),
        (
            "nothing committed, one failure permanent",
            Tally {
                transient: 1,
                permanent: 1,
                ..ZERO
            },
            Exit::VendorRefusedAll,
        ),
        (
            "nothing committed, targets not asked for",
            Tally {
                unclean_not_planned: 1,
                ..ZERO
            },
            Exit::PartialCollect,
        ),
        (
            "a commit the store refused",
            Tally {
                committed: 0,
                commit_refused: 1,
                ..ZERO
            },
            Exit::CommitFailed,
        ),
    ];

    const ZERO: Tally = Tally {
        committed: 0,
        ok: 0,
        empty: 0,
        transient: 0,
        permanent: 0,
        interrupted: 0,
        unclean_not_planned: 0,
        commit_refused: 0,
        deferred: 0,
    };

    /// Two arms share one exit code, so the set comparison below is keyed on both.
    fn named(exit: Exit) -> String {
        exit.code().to_string() + exit.error_code().unwrap_or("ok")
    }

    #[test]
    fn every_bucket_of_the_exit_table_has_exactly_one_arm_and_this_table_test_covers_all_of_them() {
        let mut reached = BTreeSet::new();
        for (state, tally, expected) in TABLE {
            assert_eq!(tally.exit(), expected, "{state}");
            reached.insert(named(expected));
        }
        let every: BTreeSet<String> = EXITS.iter().copied().map(named).collect();
        assert_eq!(reached, every);
    }

    /// A run that committed nothing and was not asked to commit anything exits 0, and a sweep of
    /// a dead run's attempt is incomplete work rather than a quiet success.
    #[test]
    fn an_attempt_a_dead_run_left_open_makes_this_run_incomplete_rather_than_clean() {
        let swept = Tally {
            interrupted: 1,
            ..ZERO
        };
        assert_eq!(
            (Tally { ..ZERO }.exit(), swept.exit()),
            (Exit::Ok, Exit::PartialCollect)
        );
    }

    /// Decision D1's companion rule, over every arm rather than over the two a run usually takes.
    #[test]
    fn stdout_carries_the_result_for_exactly_the_exits_zero_one_and_eight() {
        let carrying: Vec<(u8, bool)> = EXITS
            .iter()
            .map(|exit| (exit.code(), exit.carries_the_result_on_stdout()))
            .collect();
        assert_eq!(
            carrying,
            vec![(0, true), (8, true), (7, false), (8, true), (8, true)]
        );
    }

    #[test]
    fn only_the_unreachable_vendor_is_retryable_and_a_total_refusal_is_not() {
        let retryable: Vec<(Option<&str>, bool)> = EXITS
            .iter()
            .map(|exit| (exit.error_code(), exit.retryable()))
            .collect();
        assert_eq!(
            retryable,
            vec![
                (None, false),
                (Some("partial_collect"), false),
                (Some("vendor_unreachable"), true),
                (Some("vendor_refused_all"), false),
                (Some("commit_failed"), false),
            ]
        );
    }

    /// A carried request of `base`'s host, closing open row `id`; `counted` unless a walk id.
    fn carried(base: &str, path: &str, counted: bool, id: i64) -> Queued {
        Queued {
            counted,
            carried: vec![id],
            ..Queued::first_attempt(asking(base, path))
        }
    }

    /// S7-D5 (amended 2026-11-19): a host's planned index first, then the follow-ups it lists,
    /// then its carried requests in the order given.
    #[test]
    fn a_lane_asks_its_planned_index_then_new_follow_ups_then_carried_requests() {
        let mut queue = HostQueue::of(vec![asking(GNW, "index")], None);
        queue.carry(vec![
            carried(GNW, "c1", true, 1),
            carried(GNW, "c2", true, 2),
        ]);
        let mut asked = Vec::new();
        if let Some(Popped::Ask(index)) = queue.pop() {
            asked.push(index.request.url.clone());
            queue.requeue(vec![asking(GNW, "f1")], Vec::new(), index, false, None);
        }
        while let Some(popped) = queue.pop() {
            if let Popped::Ask(queued) = popped {
                asked.push(queued.request.url);
            }
        }
        assert_eq!(
            asked,
            ["index", "f1", "c1", "c2"].map(|path| format!("{GNW}{path}"))
        );
    }

    /// S7-D5: carried requests go behind their lane's index and the follow-ups it lists; a counted
    /// one spends a cap slot and a walk id does not (S7-D6). Under a cap of 2, GNW's new follow-up
    /// and its first carried one are asked, the carried walk id too, the second carried deferred.
    #[test]
    fn carried_requests_follow_their_lanes_index_and_only_counted_ones_spend_the_cap() {
        let plan = [(GNW, documents(1)), (PRN, documents(0))];
        let carried = vec![
            carried(GNW, "c1", true, 1),
            carried(GNW, "w1", false, 2),
            carried(GNW, "c2", true, 3),
        ];
        let (asked, deferred) = carrying(&plan, carried, Some(2), "", "");
        assert_eq!(
            (asked, deferred),
            (
                urls(&[
                    (GNW, "index"),
                    (PRN, "index"),
                    (GNW, "p0"),
                    (GNW, "c1"),
                    (GNW, "w1")
                ]),
                urls(&[(GNW, "c2")])
            )
        );
    }

    /// S7-D5: fresh-first holds per lane, not across lanes. Under a cap of 3, GNW's lane is out of
    /// fresh requests after `p0`, so its carried `c1` is asked on GNW's next turn and takes the
    /// last slot; PRN's `p1` and `p2` are deferred.
    #[test]
    fn a_lane_out_of_fresh_requests_asks_its_carried_one_in_its_own_turn() {
        let plan = [(GNW, documents(1)), (PRN, documents(3))];
        let carried = vec![carried(GNW, "c1", true, 1)];
        let (asked, deferred) = carrying(&plan, carried, Some(3), "", "");
        assert_eq!(
            (asked, deferred),
            (
                urls(&[
                    (GNW, "index"),
                    (PRN, "index"),
                    (GNW, "p0"),
                    (PRN, "p0"),
                    (GNW, "c1")
                ]),
                urls(&[(PRN, "p1"), (PRN, "p2")])
            )
        );
    }

    /// The 14:30 run of 2026-11-19 under fresh-first: GNW 20 follow-ups and 19 walk ids, PRN 155
    /// new follow-ups and 75 carried, Yahoo 61, cap 300 (150 then). Every new one is asked, then
    /// 64 carried; 11 carried are deferred. The deadline is not modelled here.
    #[test]
    fn the_cap_is_spent_on_new_follow_ups_before_carried_ones() {
        const YAHOO: &str = "https://finance.yahoo.com/";
        let gnw = Lane {
            documents: 20,
            walked: 19,
            pages: 0,
        };
        let plan = [(GNW, gnw), (PRN, documents(155)), (YAHOO, documents(61))];
        let carried = (0..75)
            .map(|nth| carried(PRN, &format!("c{nth}"), true, nth))
            .collect();
        let (asked, deferred) = carrying(&plan, carried, Some(300), "", "");
        let count =
            |of: &[String], prefix: &str| of.iter().filter(|url| url.starts_with(prefix)).count();
        let prn_new = format!("{PRN}p");
        let prn_carried = format!("{PRN}c");
        assert_eq!(
            (
                count(&asked, &format!("{GNW}p")),
                count(&asked, &format!("{YAHOO}p")),
                count(&asked, &prn_new),
                count(&asked, &prn_carried),
                count(&deferred, &prn_new),
                count(&deferred, &prn_carried),
            ),
            (20, 61, 155, 64, 0, 11)
        );
    }

    /// Condition A with S7-D5: a lane stopped at the deadline defers its new requests first, then
    /// its carried ones, and nothing of it is left to ask.
    #[test]
    fn a_stopped_lane_defers_its_carried_requests_too() {
        let mut queue = HostQueue::of(vec![asking(GNW, "index")], None);
        queue.carry(vec![
            carried(GNW, "c1", true, 1),
            carried(GNW, "c2", true, 2),
        ]);
        if let Some(Popped::Ask(index)) = queue.pop() {
            let follow_ups = vec![asking(GNW, "f1"), asking(GNW, "f2")];
            queue.requeue(follow_ups, Vec::new(), index, false, None);
        }
        let lane = match queue.pop() {
            Some(Popped::Ask(stopped)) => queue.stop(&stopped),
            _ => Vec::new(),
        };
        assert_eq!(
            (
                lane.into_iter()
                    .map(|rest| rest.request.url)
                    .collect::<Vec<_>>(),
                queue.pop().is_none()
            ),
            (urls(&[(GNW, "f2"), (GNW, "c1"), (GNW, "c2")]), true)
        );
    }

    /// §8 row R22: a document follow-up and a walk id name the index attempt that produced them,
    /// the follow-up's retry and cap deferral keep the name, the walk id takes no cap slot, and a
    /// next page names none.
    #[test]
    fn a_document_follow_up_its_retry_and_its_deferral_name_their_listing_and_a_page_none() {
        let mut queue = HostQueue::of(vec![asking(GNW, "index")], Some(1));
        if let Some(Popped::Ask(index)) = queue.pop() {
            let page = Request {
                phase: RequestPhase::Index,
                ..asking(GNW, "page")
            };
            let follow_ups = vec![asking(GNW, "f1"), page, asking(GNW, "f2")];
            let walked = vec![asking(GNW, "w1")];
            queue.requeue(follow_ups, walked, index, false, Some("the index"));
        }
        let mut named = Vec::new();
        while let Some(popped) = queue.pop() {
            let (queued, deferred) = match popped {
                Popped::Ask(queued) => (queued, false),
                Popped::Defer(queued) => (queued, true),
            };
            named.push((
                queued.request.url.clone(),
                deferred,
                queued.listed_by.clone(),
            ));
            if queued.request.url.ends_with("f1") && queued.retries == 0 {
                let retry = Queued::retried(true, queued.retries);
                queue.requeue(Vec::new(), Vec::new(), queued, retry, Some("f1's own"));
            }
        }
        let listed = || Some("the index".to_owned());
        assert_eq!(
            named,
            vec![
                (format!("{GNW}f1"), false, listed()),
                (format!("{GNW}page"), false, None),
                (format!("{GNW}f2"), true, listed()),
                (format!("{GNW}w1"), false, listed()),
                (format!("{GNW}f1"), false, listed()),
            ]
        );
    }

    /// S7-D5: a carried request whose host has no planned lane opens one and is asked.
    #[test]
    fn a_carried_request_on_a_host_with_no_lane_opens_one_and_is_asked() {
        let mut queue = HostQueue::of(vec![asking(PRN, "index")], None);
        queue.carry(vec![Queued {
            carried: vec![1],
            ..Queued::first_attempt(asking(GNW, "c1"))
        }]);
        let mut asked = Vec::new();
        while let Some(popped) = queue.pop() {
            if let Popped::Ask(queued) = popped {
                asked.push((queued.request.url, queued.carried));
            }
        }
        assert_eq!(
            asked,
            vec![
                (format!("{PRN}index"), Vec::new()),
                (format!("{GNW}c1"), vec![1]),
            ]
        );
    }

    /// A follow-up whose index row names a ticker.
    fn ticker_named(base: &str, path: &str) -> Request {
        Request {
            names_ticker: true,
            ..asking(base, path)
        }
    }

    /// Every url `queue` hands out from now on, asked or deferred, with the listing each names.
    fn emptied(queue: &mut HostQueue) -> Vec<(String, Option<String>)> {
        std::iter::from_fn(|| queue.pop())
            .map(|popped| match popped {
                Popped::Ask(queued) | Popped::Defer(queued) => {
                    (queued.request.url, queued.listed_by)
                }
            })
            .collect()
    }

    /// Owner decision 2026-11-19: a lane's fresh document follow-ups whose row names a ticker are
    /// asked before those naming none, each group in listing order, while a next page, a walk id
    /// and a retry keep the places they were queued at. Mutations 2026-11-19: `Lane::pop_front`
    /// taking `unnamed` first: red; `push_document` ignoring `names_ticker`: red.
    #[test]
    fn named_follow_ups_go_first_in_listing_order_and_every_other_request_keeps_its_place() {
        let mut queue = HostQueue::of(vec![asking(PRN, "index")], None);
        if let Some(Popped::Ask(index)) = queue.pop() {
            let page = Request {
                phase: RequestPhase::Index,
                ..asking(PRN, "page")
            };
            let follow_ups = vec![
                asking(PRN, "u0"),
                ticker_named(PRN, "n0"),
                asking(PRN, "u1"),
                page,
                ticker_named(PRN, "n1"),
            ];
            let walked = vec![asking(PRN, "w0")];
            queue.requeue(follow_ups, walked, index, true, None);
        }
        let asked: Vec<String> = emptied(&mut queue)
            .into_iter()
            .map(|(url, _)| url)
            .collect();
        assert_eq!(
            asked,
            urls(&[
                (PRN, "n0"),
                (PRN, "n1"),
                (PRN, "u0"),
                (PRN, "page"),
                (PRN, "u1"),
                (PRN, "w0"),
                (PRN, "index"),
            ])
        );
    }

    /// Owner decision 2026-11-19: PRN's RSS lists `m` with a ticker and `x` without, its sitemap
    /// names one for `x`, which the run already queued, so the sitemap's own follow-up is dropped
    /// and the RSS's moves to the back of the named ones, behind `m` and ahead of the sitemap's new
    /// `n`, keeping the RSS as its listing; a carried request and a walk id of a promoted story stay
    /// where they are, and the cap still counts at `pop`. Mutations 2026-11-19: `promote` a no-op:
    /// red; `promote` putting the story at the front of the unnamed ones instead: red; at the front
    /// of the named ones (review of PR #60): red.
    #[test]
    fn a_later_listing_naming_a_ticker_promotes_a_queued_story_and_nothing_else() {
        let plan = vec![asking(PRN, "rss"), asking(PRN, "sitemap")];
        let mut queue = HostQueue::of(plan, Some(4));
        queue.carry(vec![carried(PRN, "c", true, 1)]);
        if let Some(Popped::Ask(rss)) = queue.pop() {
            let follow_ups = vec![
                asking(PRN, "a"),
                asking(PRN, "x"),
                ticker_named(PRN, "m"),
                asking(PRN, "b"),
            ];
            queue.requeue(follow_ups, vec![asking(PRN, "w")], rss, false, Some("rss"));
        }
        if let Some(Popped::Ask(sitemap)) = queue.pop() {
            let stories: BTreeSet<String> = ["x", "w", "c"]
                .iter()
                .map(|path| format!("{PRN}{path}"))
                .collect();
            queue.promote(&stories, |request| Some(request.url.clone()));
            let follow_ups = vec![ticker_named(PRN, "n")];
            queue.requeue(follow_ups, Vec::new(), sitemap, false, Some("sitemap"));
        }
        let mut handed = Vec::new();
        while let Some(popped) = queue.pop() {
            let (queued, asked) = match popped {
                Popped::Ask(queued) => (queued, true),
                Popped::Defer(queued) => (queued, false),
            };
            handed.push((queued.request.url, asked, queued.listed_by));
        }
        let by = |listing: &str| Some(listing.to_owned());
        assert_eq!(
            handed,
            vec![
                (format!("{PRN}m"), true, by("rss")),
                (format!("{PRN}x"), true, by("rss")),
                (format!("{PRN}n"), true, by("sitemap")),
                (format!("{PRN}a"), true, by("rss")),
                (format!("{PRN}w"), true, by("rss")),
                (format!("{PRN}b"), false, by("rss")),
                (format!("{PRN}c"), false, None),
            ]
        );
    }

    /// Condition A with ticker-first: a lane stopped at the deadline defers in the order it would
    /// have asked, named follow-ups first. Mutation 2026-11-19, `stop` draining `named` after
    /// `unnamed`: red.
    #[test]
    fn a_stopped_lane_defers_its_named_follow_ups_first() {
        let mut queue = HostQueue::of(vec![asking(GNW, "index")], None);
        if let Some(Popped::Ask(index)) = queue.pop() {
            let follow_ups = vec![
                asking(GNW, "u0"),
                ticker_named(GNW, "n0"),
                ticker_named(GNW, "n1"),
            ];
            queue.requeue(follow_ups, Vec::new(), index, false, None);
        }
        let lane = match queue.pop() {
            Some(Popped::Ask(stopped)) => queue.stop(&stopped),
            _ => Vec::new(),
        };
        let deferred: Vec<String> = lane.into_iter().map(|rest| rest.request.url).collect();
        assert_eq!(deferred, urls(&[(GNW, "n1"), (GNW, "u0")]));
    }
}
