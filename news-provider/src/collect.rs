//! The driver. `fetch` is not on [`Provider`], so this is what opens a slot, spends its permit,
//! classifies the answer, stores the payload, extracts it and closes the attempt - one HTTP call
//! per written ledger row. Why the provider does not do it: `docs/design/design.md` section 2.2.
//!
//! Two seams, so a run is drivable over stored payloads with no network and over a real archive
//! with no provider: [`Exchange`] is the socket, whose only implementation that opens one is
//! [`Wire`], and [`Archive`] is the store door.

use std::collections::VecDeque;

use pit::ObservationTime;

use crate::clock::Clock;
use crate::ledger::{
    abandon, AttemptId, AttemptReceipt, AttemptSlot, Baseline, LedgerError, Outcome, OutcomeKind,
    SendPermit, Sink, Verdict,
};
use crate::provider::{CollectedItem, Plan, Provider, RawBlob};
use crate::request::{Request, RunId, SourceId, TargetWindow};
use crate::wire::{RawResponse, SendFailure, Wire};
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

/// The store side of the driver, in the terms this crate can name: a `CollectedItem` is a
/// provider's row and the archive's own row type is the archive's business.
pub trait Archive: Sink {
    /// Stores a payload under its own digest, BEFORE anything parses it (design.md section 6).
    fn put_blob(&self, payload: &[u8]) -> Result<String, ArchiveError>;

    /// The one door. `raw_blob` is absent exactly when the attempt brought nothing back, which is
    /// also when `items` is empty.
    fn commit(
        &mut self,
        closed: AttemptReceipt,
        items: &[CollectedItem],
        raw_blob: Option<&str>,
        observed: ObservationTime,
    ) -> Result<Committed, ArchiveError>;

    /// This source's own `ok` attempts, oldest first, which is what makes a trailing slice
    /// trailing.
    fn ok_vendor_rows(&self, source: &SourceId) -> Result<Vec<u32>, ArchiveError>;

    /// Attempts opened and never finalised, which the startup sweep closes.
    fn attempts_in_flight(&self) -> Result<Vec<String>, ArchiveError>;
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
    /// A startup refusal and never a vendor event in the coverage ledger (design.md section 3).
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
    pub calendar: &'a Calendar,
    pub listing: &'a Listing,
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
            // must not invite a timer to hammer it (design.md section 4).
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
    }

    /// The exit table as a TOTAL function over the counts, not a chain of ifs that falls through:
    /// design.md section 4's table, one arm per bucket.
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
    pub unusable_rows: u32,
    pub items_committed: u32,
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
    let mut queue: VecDeque<Request> = plan.requests.into_iter().collect();
    let mut ordinal = 0;
    while let Some(request) = queue.pop_front() {
        let attempted = attempt_once(&driving, archive, &request, ordinal)?;
        ordinal += 1;
        collected.absorb(&attempted);
        queue.extend(attempted.follow_ups);
    }
    collected.per_source = vec![tally_of(&source, &collected)];
    Ok(collected)
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
    kind: OutcomeKind,
    permanent: bool,
    bytes_written: u64,
    blob_written: bool,
    committed: Committed,
}

fn attempt_once<A: Archive>(
    driving: &Driving,
    archive: &mut A,
    request: &Request,
    ordinal: u32,
) -> Result<Attempted, CollectError> {
    let Driving {
        provider,
        exchange,
        clock,
        run,
        baseline,
    } = *driving;
    let mut slot = AttemptSlot::open(
        archive,
        request,
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
    let stored = match &answer {
        Ok(response) => Some(archive.put_blob(response.body())?),
        Err(_) => None,
    };
    // Extraction only reads a payload the provider called usable, so a vendor's own error page is
    // stored as evidence and never parsed into a row count.
    let usable = matches!(verdict, Verdict::Usable);
    let extracted = match (&answer, &stored) {
        (Ok(response), Some(digest)) if usable => Some(provider.extract(
            request,
            &RawBlob {
                digest: digest.clone(),
                bytes: response.body().to_vec(),
            },
        )),
        _ => None,
    };
    let (vendor_rows, filtered_out, unusable_rows) = match &extracted {
        Some(extracted) => (
            extracted.vendor_rows,
            extracted.filtered_out,
            count(extracted.unusable.len()),
        ),
        None => (0, 0, 0),
    };
    let outcome = Outcome::decide(&verdict, vendor_rows, baseline);
    let kind = outcome.kind();
    let closed = slot.close(archive, outcome, &answer, ObservationTime::new(clock.now()))?;
    let (status, observed) = match &answer {
        Ok(response) => (Some(response.status()), response.received_at()),
        Err(failed) => (None, failed.failed_at()),
    };
    let no_items: Vec<CollectedItem> = Vec::new();
    let items = extracted.as_ref().map_or(&no_items, |one| &one.items);
    let filed = archive.commit(closed, items, stored.as_deref(), observed);
    let (committed, refusal) = match filed {
        Ok(committed) => (committed, None),
        Err(refused) => (Committed::default(), Some(refused.detail)),
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
            unusable_rows,
            items_committed: committed.items_committed,
            commit_refused: refusal,
        },
        follow_ups: extracted.map(|one| one.follow_ups).unwrap_or_default(),
        kind,
        permanent: matches!(verdict, Verdict::PermanentFailure),
        bytes_written: match &answer {
            Ok(response) => u64::try_from(response.body().len()).unwrap_or(u64::MAX),
            Err(_) => 0,
        },
        blob_written: stored.is_some(),
        committed,
    })
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
    fn absorb(&mut self, attempted: &Attempted) {
        match attempted.kind {
            OutcomeKind::Ok => self.tally.ok += 1,
            OutcomeKind::Empty => self.tally.empty += 1,
            OutcomeKind::Failed if attempted.permanent => self.tally.permanent += 1,
            OutcomeKind::Failed => self.tally.transient += 1,
            // The sweep's attempts are counted where they are closed, not here.
            OutcomeKind::Interrupted => {}
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
        unusable_rows: sum(|row| row.unusable_rows),
        items_committed: sum(|row| row.items_committed),
    }
}

fn count(of: usize) -> u32 {
    u32::try_from(of).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::{Exit, Tally, EXITS};
    use std::collections::BTreeSet;

    /// One row per bucket of design.md section 4's table, each stating the counts that reach it.
    const TABLE: [(&str, Tally, Exit); 7] = [
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
