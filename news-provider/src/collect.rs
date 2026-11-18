//! The driver. `fetch` is not on [`Provider`], so this is what opens a slot, spends its permit,
//! classifies the answer, stores the payload, extracts it and closes the attempt - one HTTP call
//! per written ledger row.
//!
//! Two seams, so a run is drivable over stored payloads with no network and over a real archive
//! with no provider: [`Exchange`] is the socket, whose only implementation that opens one is
//! [`Wire`], and [`Archive`] is the store door.

use std::collections::{BTreeSet, VecDeque};

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
use crate::wire::{Headers, RawResponse, SendFailure, TransportError, Wire};
use crate::{Calendar, Listing, PlanRefusal};

/// One vendor conversation. Behind a trait because the driver is the thing under test and a test
/// may not touch a network; [`Wire`] is the only implementation in the workspace that opens a
/// socket, and `send` consumes the permit either way.
pub trait Exchange {
    fn send(
        &self,
        permit: SendPermit,
        request: &Request,
        clock: &dyn Clock,
    ) -> Result<RawResponse, SendFailure>;
}

impl Exchange for Wire {
    fn send(
        &self,
        permit: SendPermit,
        request: &Request,
        clock: &dyn Clock,
    ) -> Result<RawResponse, SendFailure> {
        // The inherent method by path, so this impl cannot recurse into itself.
        Wire::send(self, permit, request, clock)
    }
}

/// Bound on retry work per request — a MECHANISM cap, never derived from an observed loss rate
/// (§8 row R21, §10). A permanent failure (4xx, `vendor_refused`) is never re-queued.
pub const MAX_TRANSIENT_RETRIES: u32 = 2;

/// Why a request was recorded as not looked at instead of asked: a `deferred` row (architecture.md
/// section 3.2, S7-D4). `run_deadline` and `host_held` join with their producer, step 7 PR 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferReason {
    FollowUpCap,
}

impl DeferReason {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::FollowUpCap => "follow_up_cap",
        }
    }
}

/// One request this run did not ask, and why. It has no attempt row, because no call was made
/// (section 8 row R16); `story_id` by the rule `attempt_once` opens a `Document` attempt under.
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
}

impl Deferred {
    fn of(
        provider: &dyn Provider,
        request: Request,
        reason: DeferReason,
        at: ObservationTime,
    ) -> Self {
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
        }
    }
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
    /// failure, a permanent failure answered with a 3xx, a usable attempt with an unusable row the
    /// index row decided, a swept attempt and one in flight hold nothing (section 11.1).
    fn held_story_ids(
        &self,
        source: &SourceId,
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
    let mut queue = HostQueue::of(plan.requests, provider.follow_up_cap());
    let mut ordinal = 0;
    let mut issued = BTreeSet::new();
    while let Some(popped) = queue.pop() {
        let Queued {
            request, retries, ..
        } = match popped {
            Popped::Ask(queued) => queued,
            Popped::Defer(request) => {
                let at = ObservationTime::new(clock.now());
                let deferred = Deferred::of(provider, request, DeferReason::FollowUpCap, at);
                archive.record_deferred(&deferred)?;
                collected.tally.deferred += 1;
                collected.deferred.push(deferred);
                continue;
            }
        };
        let attempted = attempt_once(&driving, archive, &request, ordinal, &mut issued)?;
        ordinal += 1;
        let retrying = Queued::retried(attempted.is_transient(), retries);
        collected.absorb(&attempted, retrying);
        let asked = Queued {
            request,
            retries,
            counted: false,
        };
        queue.requeue(attempted.follow_ups, attempted.walked, asked, retrying);
    }
    collected.per_source = vec![tally_of(&source, &collected)];
    Ok(collected)
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
    /// listed. A planned request, an `Index` follow-up, a walk id and a retry are not.
    pub(crate) counted: bool,
}

impl Queued {
    /// A planned request or a walk id, not yet attempted.
    pub(crate) const fn first_attempt(request: Request) -> Self {
        Self {
            request,
            retries: 0,
            counted: false,
        }
    }

    /// A follow-up an index listed, not yet attempted.
    pub(crate) fn follow_up(request: Request) -> Self {
        Self {
            counted: request.phase == RequestPhase::Document,
            ..Self::first_attempt(request)
        }
    }

    /// A transient failure is asked again until `MAX_TRANSIENT_RETRIES` re-attempts. §8 R21.
    pub(crate) const fn retried(transient: bool, retries: u32) -> bool {
        transient && retries < MAX_TRANSIENT_RETRIES
    }
}

/// The driver's queue: one FIFO lane per url host (`host_of`, the key `Wire` paces by), served
/// round-robin in the order each host first entered, so a wait owed to one host does not stand in
/// front of another host's due request. One host is one lane, which is a plain FIFO (§10).
pub(crate) struct HostQueue {
    /// `None` keys every url `host_of` cannot key; `Wire` refuses or fails those itself.
    lanes: Vec<(Option<HostKey>, VecDeque<Queued>)>,
    next: usize,
    /// [`Provider::follow_up_cap`], and how many counted follow-ups `pop` has handed out to ask.
    cap: Option<u32>,
    issued: u32,
}

/// What [`HostQueue::pop`] hands the driver: a request to ask, or one past the cap to defer.
pub(crate) enum Popped {
    Ask(Queued),
    Defer(Request),
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

    /// To the back of its host's lane; a host not seen before opens a lane after every other.
    fn push(&mut self, queued: Queued) {
        let host = host_of(&queued.request.url).ok().flatten();
        match self.lanes.iter_mut().find(|(key, _)| *key == host) {
            Some((_, lane)) => lane.push_back(queued),
            None => self.lanes.push((host, VecDeque::from([queued]))),
        }
    }

    /// After `asked` is attempted: its follow-ups, its walk ids, then itself again when
    /// `retrying`, each to the back of its host's lane, so the pace limiter (R19) spaces the
    /// retry. §8 R21. A retry is never counted again.
    pub(crate) fn requeue(
        &mut self,
        follow_ups: Vec<Request>,
        walked: Vec<Request>,
        asked: Queued,
        retrying: bool,
    ) {
        for follow_up in follow_ups {
            self.push(Queued::follow_up(follow_up));
        }
        for id in walked {
            self.push(Queued::first_attempt(id));
        }
        if retrying {
            self.push(Queued {
                request: asked.request,
                retries: asked.retries + 1,
                counted: false,
            });
        }
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
            Some(cap) if self.issued >= cap => Some(Popped::Defer(queued.request)),
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

/// One attempt, from its `Opened` row to its committed rows.
struct Attempted {
    row: AttemptRow,
    follow_ups: Vec<Request>,
    /// [`Extracted::walk`]'s ids left to issue, outside the follow-up cap (S7-D6's exception).
    walked: Vec<Request>,
    kind: OutcomeKind,
    permanent: bool,
    bytes_written: u64,
    blob_written: bool,
    committed: Committed,
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

/// `issued` holds the story of every follow-up this run issued: a second index listing one of them
/// in the same run, PRN's sitemap beside its RSS (section 11.2), counts it as held.
fn attempt_once<A: Archive>(
    driving: &Driving,
    archive: &mut A,
    request: &Request,
    ordinal: u32,
    issued: &mut BTreeSet<String>,
) -> Result<Attempted, CollectError> {
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
    let mut slot = AttemptSlot::open(
        archive,
        request,
        story.as_ref(),
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
    let (follow_ups, walked) = match extracted {
        Some(extracted) if request.phase == RequestPhase::Index => {
            let matched = count(extracted.follow_ups.len());
            let archived = request.provenance == Provenance::VendorArchive;
            let mut stories = if archived {
                archive.record_planned_coverage(&attempt_id, matched)?;
                archive.covered_story_ids(&request.source, &request.target_window)?
            } else {
                archive.held_story_ids(&request.source)?
            };
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
            issued.extend(stories_of(&to_issue));
            stories.extend(stories_of(&to_issue));
            let walked = provider.follow_ups_to_issue(extracted.walk, &stories);
            issued.extend(stories_of(&walked));
            if !archived && refusal.is_none() {
                let held = matched.saturating_sub(count(to_issue.len()));
                archive.record_held(&attempt_id, held)?;
                archive.record_relisted(&attempt_id, &relisted_of(provider, listed, &to_issue))?;
            }
            (to_issue, walked)
        }
        _ => (Vec::new(), Vec::new()),
    };
    Ok(Attempted {
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
        },
        follow_ups,
        walked,
        kind,
        permanent: matches!(verdict, Verdict::PermanentFailure),
        bytes_written: match &answer {
            Ok(response) => u64::try_from(response.body().len()).unwrap_or(u64::MAX),
            Err(failed) => failed
                .cut_body()
                .map_or(0, |cut| u64::try_from(cut.len()).unwrap_or(u64::MAX)),
        },
        blob_written: payload.is_some(),
        committed,
    })
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
        let (body, headers, truncated) = match answer {
            Ok(response) => (response.body(), response.headers(), false),
            Err(failed) => match failed.cut_body() {
                Some(cut) => (cut, failed.headers(), true),
                None => return Ok(None),
            },
        };
        Ok(Some(Self {
            raw_blob: put(body)?,
            url: request.url.clone(),
            phase: request.phase,
            listed_date: request.listed_date.clone(),
            headers: headers.clone(),
            truncated,
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
                // The sweep's attempts are counted where they are closed, not here.
                OutcomeKind::Interrupted => {}
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
        let indexes = plan.iter().map(|(base, _)| asking(base, "index")).collect();
        let mut queue = HostQueue::of(indexes, cap);
        let (mut asked, mut deferred) = (Vec::new(), Vec::new());
        while let Some(popped) = queue.pop() {
            let Queued {
                request, retries, ..
            } = match popped {
                Popped::Ask(queued) => queued,
                Popped::Defer(request) => {
                    deferred.push(request.url);
                    continue;
                }
            };
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
            };
            queue.requeue(follow_ups, named("w", lane.walked), again, retrying);
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

    /// S7-D6 at its boundary: the 150th follow-up is asked and the 151st deferred; with no cap
    /// (edgar's `None`) all 151 are asked.
    #[test]
    fn the_cap_asks_the_150th_follow_up_and_defers_the_151st() {
        let at_cap = capped(&[(CNW, documents(150))], Some(150), "");
        let over = capped(&[(CNW, documents(151))], Some(150), "");
        let uncapped = capped(&[(CNW, documents(151))], None, "");
        assert_eq!(
            (
                at_cap.0.len(),
                at_cap.1.len(),
                over.0.len(),
                uncapped.0.len()
            ),
            (151, 0, 151, 152)
        );
        assert_eq!(
            (over.0.last(), over.1, uncapped.1.len()),
            (Some(&format!("{CNW}p149")), urls(&[(CNW, "p150")]), 0)
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
    /// GNW 20, PRN 20, CNW 100, Fed 20 and Yahoo 141, lose 55 of CNW's and 96 of Yahoo's.
    #[test]
    fn the_global_cap_cuts_55_of_cnws_and_96_of_yahoos_first_run_follow_ups() {
        const FED: &str = "https://www.federalreserve.gov/";
        const YAHOO: &str = "https://finance.yahoo.com/";
        let plan = [
            (GNW, documents(20)),
            (PRN, documents(20)),
            (CNW, documents(100)),
            (FED, documents(20)),
            (YAHOO, documents(141)),
        ];
        let (_, deferred) = capped(&plan, Some(150), "");
        let lost: Vec<usize> = plan
            .iter()
            .map(|(base, _)| deferred.iter().filter(|url| url.starts_with(base)).count())
            .collect();
        assert_eq!(lost, vec![0, 0, 55, 0, 96]);
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

    /// An `Index` follow-up, PRN's next sitemap page, is outside the 150: section 11.2's worst
    /// case is (150 + index requests) × 3 × 10 s, and a page cut would leave its poll unjoined.
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
}
