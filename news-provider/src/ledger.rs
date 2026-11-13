//! The coverage ledger's write half, and the capability for the socket. An `AttemptSlot` exists
//! only once its `Opened` row is in a `Sink`, and a `SendPermit` comes out of nothing but that
//! slot, so one HTTP call needs one written row. Which field each event carries and why:
//! `docs/design/architecture.md` section 3.2.
//!
//! A provider never holds a `Sink`: the driver opens and closes every slot. Why that is stronger
//! than letting `fetch` do it is `docs/design/design.md` section 2.2.

use canon::digest::attempt_id;
use canon::{CanonError, Digest};
use pit::ObservationTime;

use crate::request::{Request, RunId, SourceId, TargetWindow};
use crate::wire::{RawResponse, TransportError};

/// An attempt's identity, over (run, source, target window, ordinal).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AttemptId(Digest);

impl AttemptId {
    #[must_use]
    pub fn hex(&self) -> &str {
        self.0.hex()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LedgerError {
    #[error("a second permit was asked of one attempt slot")]
    PermitAlreadyTaken,
    #[error("the ledger refused the {event} row: {detail}")]
    WriteFailed { event: &'static str, detail: String },
    #[error("an answer for attempt {answered} cannot close attempt {slot}")]
    AnswerFromAnotherAttempt { slot: String, answered: String },
    #[error("the attempt has no canonical identity: {0}")]
    Identity(CanonError),
}

/// Written out rather than derived with `#[from]`: `CanonError` carries `Display` and not
/// `std::error::Error`, and thiserror's `#[from]` needs the latter for its `source()`.
impl From<CanonError> for LedgerError {
    fn from(error: CanonError) -> Self {
        Self::Identity(error)
    }
}

/// Where the four events go. The SQLite implementation is the store's.
pub trait Sink {
    /// Returns once the event is durable, because an `Opened` row is what records a hang.
    fn write(&mut self, event: &AttemptEvent) -> Result<(), LedgerError>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum AttemptEvent {
    Opened {
        attempt_id: AttemptId,
        run: RunId,
        source: SourceId,
        target_window: TargetWindow,
        filter_params: Digest,
        opened_at: ObservationTime,
    },
    Answered {
        attempt_id: AttemptId,
        requested_at: ObservationTime,
        received_at: ObservationTime,
        /// Absent when the conversation produced no status.
        transport_status: Option<u16>,
        clock_nonmonotonic: bool,
    },
    Finalised {
        attempt_id: AttemptId,
        outcome: Outcome,
        /// Absent for an abandoned attempt: nothing can ask a dead process whether its socket
        /// was used.
        cost: Option<Cost>,
        closed_at: ObservationTime,
    },
    /// A kill, an OOM or a reboot between `open` and `close`, noticed by the next run's startup
    /// sweep. Produced by [`abandon`].
    Abandoned {
        attempt_id: AttemptId,
        noticed_at: ObservationTime,
        by_run: RunId,
    },
}

impl AttemptEvent {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Opened { .. } => "opened",
            Self::Answered { .. } => "answered",
            Self::Finalised { .. } => "finalised",
            Self::Abandoned { .. } => "abandoned",
        }
    }
}

/// What the vendor's answer was, read from the body as well as the status. There is no `Premature`
/// arm and that is a resolution rather than an omission: `docs/design/design.md` section 2.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Usable,
    Empty,
    PermanentFailure,
    TransientFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeKind {
    Ok,
    Empty,
    Failed,
    Interrupted,
}

/// What the payload held and what entered the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    /// Rows the payload held, before any filtering of ours.
    pub vendor_rows: u32,
    pub items_new: u32,
}

/// What one attempt cost. There is no metered arm until a metered vendor arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cost {
    Slots(u32),
}

/// Never the single previous count. manna's rule is `prev >= 5 && new < max(1, 0.2 * prev)`, so
/// 0.2 x 181 = 36.2 misses the 59-item day it exists for, and a 4x-per-run decay (181, 45, 11, 3)
/// never fires before `prev` drops under 5 and disables it for good.
const TRAILING_MEDIAN: &str = concat!(
    "trailing median, lower of the two middles at even length, ",
    "of vendor_rows over this source's own ok attempts"
);

/// The baseline the collapse question will be asked against, written on every attempt whatever the
/// verdict. `ratio` is here so the floor becomes derivable from our own runs; no floor fires.
#[derive(Debug, Clone, PartialEq)]
pub struct Baseline {
    pub count: u32,
    pub window: u32,
    pub construction: &'static str,
    pub ratio: Option<f64>,
}

impl Baseline {
    #[must_use]
    pub fn from_trailing_ok(vendor_rows_per_ok_attempt: &[u32]) -> Self {
        let mut sorted = vendor_rows_per_ok_attempt.to_vec();
        sorted.sort_unstable();
        let middle = sorted.len().saturating_sub(1) / 2;
        Self {
            count: sorted.get(middle).copied().unwrap_or_default(),
            window: u32::try_from(sorted.len()).unwrap_or(u32::MAX),
            construction: TRAILING_MEDIAN,
            ratio: None,
        }
    }

    fn against(&self, vendor_rows: u32) -> Self {
        Self {
            ratio: (self.count > 0).then(|| f64::from(vendor_rows) / f64::from(self.count)),
            ..self.clone()
        }
    }
}

/// The ledger row's verdict about the attempt as a whole. Two constructors and nothing else.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    kind: OutcomeKind,
    counts: Option<Counts>,
    baseline: Option<Baseline>,
}

impl Outcome {
    /// A total match over `Verdict`. `vendor_rows` is the quantity the collapse question is about
    /// and `items_new` is not: a replay of a stored window has `items_new` zero and is `ok`.
    #[must_use]
    pub fn decide(
        verdict: &Verdict,
        vendor_rows: u32,
        items_new: u32,
        baseline: &Baseline,
    ) -> Self {
        let kind = match verdict {
            Verdict::Usable => OutcomeKind::Ok,
            Verdict::Empty => OutcomeKind::Empty,
            Verdict::PermanentFailure | Verdict::TransientFailure => OutcomeKind::Failed,
        };
        Self {
            kind,
            counts: Some(Counts {
                vendor_rows,
                items_new,
            }),
            baseline: Some(baseline.against(vendor_rows)),
        }
    }

    /// The only producer of [`OutcomeKind::Interrupted`], reached through [`abandon`]. An attempt
    /// the sweep found in flight has no counts and no baseline, because nobody measured either.
    #[must_use]
    pub fn abandoned() -> Self {
        Self {
            kind: OutcomeKind::Interrupted,
            counts: None,
            baseline: None,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> OutcomeKind {
        self.kind
    }

    #[must_use]
    pub const fn counts(&self) -> Option<Counts> {
        self.counts
    }

    #[must_use]
    pub const fn baseline(&self) -> Option<&Baseline> {
        self.baseline.as_ref()
    }
}

/// An attempt whose `Opened` row is written. Its permit is the capability for one HTTP call.
#[derive(Debug)]
#[must_use = "a dropped slot leaves an Opened row with no Answered for the next run's sweep"]
pub struct AttemptSlot {
    attempt_id: AttemptId,
    permit: Option<SendPermit>,
}

/// The capability for one send: its field is private and no constructor is public, so the only
/// route to one from outside this module is [`AttemptSlot::take_permit`]. What that does and does
/// not close is architecture.md section 8 row R4.
#[derive(Debug)]
#[must_use]
pub struct SendPermit {
    attempt_id: AttemptId,
}

impl SendPermit {
    /// Consumes the permit, so the attempt it belongs to travels with the answer.
    pub(crate) fn into_attempt_id(self) -> AttemptId {
        self.attempt_id
    }
}

/// A closed attempt. Private fields and no public constructor, so the store's one door cannot be
/// handed anything but a closed attempt.
#[derive(Debug, Clone, PartialEq)]
#[must_use]
pub struct AttemptReceipt {
    attempt_id: AttemptId,
    outcome: Outcome,
    closed_at: ObservationTime,
}

impl AttemptReceipt {
    #[must_use]
    pub const fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }

    #[must_use]
    pub const fn outcome(&self) -> &Outcome {
        &self.outcome
    }

    #[must_use]
    pub const fn closed_at(&self) -> ObservationTime {
        self.closed_at
    }
}

impl AttemptSlot {
    /// The only constructor, and it writes before it returns. `ordinal` is the attempt's position
    /// within its run, because a retry of one request inside one run is a second attempt.
    pub fn open(
        sink: &mut dyn Sink,
        request: &Request,
        run: &RunId,
        ordinal: u32,
        at: ObservationTime,
    ) -> Result<Self, LedgerError> {
        let attempt_id = AttemptId(attempt_id(
            run.as_str(),
            request.source.as_str(),
            request.target_window.as_str(),
            ordinal,
        )?);
        sink.write(&AttemptEvent::Opened {
            attempt_id: attempt_id.clone(),
            run: run.clone(),
            source: request.source.clone(),
            target_window: request.target_window.clone(),
            filter_params: request.filter_params()?,
            opened_at: at,
        })?;
        Ok(Self {
            permit: Some(SendPermit {
                attempt_id: attempt_id.clone(),
            }),
            attempt_id,
        })
    }

    #[must_use]
    pub const fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }

    pub fn take_permit(&mut self) -> Result<SendPermit, LedgerError> {
        self.permit.take().ok_or(LedgerError::PermitAlreadyTaken)
    }

    /// Writes `Answered` and then `Finalised`, and is the only route to an [`AttemptReceipt`].
    pub fn close(
        self,
        sink: &mut dyn Sink,
        outcome: Outcome,
        answer: &Result<RawResponse, TransportError>,
        at: ObservationTime,
    ) -> Result<AttemptReceipt, LedgerError> {
        let (answered_for, requested_at, received_at, transport_status, clock_nonmonotonic) =
            match answer {
                Ok(response) => (
                    response.attempt_id(),
                    response.requested_at(),
                    response.received_at(),
                    Some(response.status()),
                    response.clock_nonmonotonic(),
                ),
                Err(failure) => (
                    failure.attempt_id(),
                    failure.requested_at(),
                    failure.failed_at(),
                    None,
                    failure.clock_nonmonotonic(),
                ),
            };
        if answered_for != &self.attempt_id {
            return Err(LedgerError::AnswerFromAnotherAttempt {
                slot: self.attempt_id.hex().to_owned(),
                answered: answered_for.hex().to_owned(),
            });
        }
        sink.write(&AttemptEvent::Answered {
            attempt_id: self.attempt_id.clone(),
            requested_at,
            received_at,
            transport_status,
            clock_nonmonotonic,
        })?;
        // One permit per slot, so a taken permit is one slot spent and an untaken one is none.
        let cost = Cost::Slots(u32::from(self.permit.is_none()));
        sink.write(&AttemptEvent::Finalised {
            attempt_id: self.attempt_id.clone(),
            outcome: outcome.clone(),
            cost: Some(cost),
            closed_at: at,
        })?;
        Ok(AttemptReceipt {
            attempt_id: self.attempt_id,
            outcome,
            closed_at: at,
        })
    }
}

/// Closes an attempt no process can answer for any more. Called by the next run's startup sweep,
/// which already holds the single-writer lease; without it an attempt killed between `open` and
/// `close` is in flight forever with a growing age.
pub fn abandon(
    sink: &mut dyn Sink,
    attempt_id: AttemptId,
    noticed_at: ObservationTime,
    by_run: &RunId,
) -> Result<AttemptReceipt, LedgerError> {
    sink.write(&AttemptEvent::Abandoned {
        attempt_id: attempt_id.clone(),
        noticed_at,
        by_run: by_run.clone(),
    })?;
    let outcome = Outcome::abandoned();
    sink.write(&AttemptEvent::Finalised {
        attempt_id: attempt_id.clone(),
        outcome: outcome.clone(),
        cost: None,
        closed_at: noticed_at,
    })?;
    Ok(AttemptReceipt {
        attempt_id,
        outcome,
        closed_at: noticed_at,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{
        abandon, AttemptEvent, AttemptSlot, Baseline, Cost, LedgerError, Outcome, OutcomeKind,
        Sink, Verdict,
    };
    use crate::request::{HeaderName, HeaderValue, Param, Request, RunId, SourceId, TargetWindow};
    use crate::wire::RawResponse;
    use chrono::{DateTime, Utc};
    use pit::ObservationTime;

    const SOURCE: &str = "edgar_press";
    const WINDOW: &str = "2026-11-10";
    const RUN: &str = "run-2026-11-10T13:00:00Z";
    const URL: &str = "https://www.sec.gov/Archives/edgar/daily-index/2026/QTR4/form.20261110.idx";
    const FIRST_ATTEMPT: u32 = 0;
    const SECOND_ATTEMPT: u32 = 1;
    const OPENED_AT: &str = "2026-11-10T13:00:00Z";
    const REQUESTED_AT: &str = "2026-11-10T13:00:01Z";
    const RECEIVED_AT: &str = "2026-11-10T13:00:02Z";
    const CLOSED_AT: &str = "2026-11-10T13:00:03Z";
    const NOTICED_AT: &str = "2026-11-11T13:00:00Z";
    const OK_STATUS: u16 = 200;
    /// architecture.md section 3.9's attempt-count paragraph, which carries the caveat.
    const VENDOR_ROWS: u32 = 348;
    const ITEMS_NEW: u32 = 103;
    /// The incident the collapse outcome exists for: 59 items against a median of 181.
    const COLLAPSE_SHAPED_ROWS: u32 = 59;
    const PREVIOUS_OK_COUNTS: [u32; 4] = [181, 45, 11, 3];
    /// Lower of the two middles of `[3, 11, 45, 181]`.
    const LOWER_MEDIAN: u32 = 11;
    const OPENED_ONLY: [&str; 1] = ["opened"];
    const EVERY_VERDICT: [(Verdict, OutcomeKind); 4] = [
        (Verdict::Usable, OutcomeKind::Ok),
        (Verdict::Empty, OutcomeKind::Empty),
        (Verdict::PermanentFailure, OutcomeKind::Failed),
        (Verdict::TransientFailure, OutcomeKind::Failed),
    ];

    /// Every event it was handed, in order, plus an optional refusal at a stated write.
    #[derive(Debug, Default)]
    pub(crate) struct Recorder {
        pub(crate) written: Vec<AttemptEvent>,
        refuse_write: Option<usize>,
    }

    impl Recorder {
        fn refusing(write: usize) -> Self {
            Self {
                written: Vec::new(),
                refuse_write: Some(write),
            }
        }

        fn names(&self) -> Vec<&'static str> {
            self.written.iter().map(AttemptEvent::name).collect()
        }

        fn cost(&self) -> Option<Cost> {
            self.written.iter().find_map(|event| match event {
                AttemptEvent::Finalised { cost, .. } => *cost,
                _ => None,
            })
        }
    }

    impl Sink for Recorder {
        fn write(&mut self, event: &AttemptEvent) -> Result<(), LedgerError> {
            if self.refuse_write == Some(self.written.len()) {
                return Err(LedgerError::WriteFailed {
                    event: event.name(),
                    detail: "no space left on device".to_owned(),
                });
            }
            self.written.push(event.clone());
            Ok(())
        }
    }

    pub(crate) fn at(text: &str) -> ObservationTime {
        ObservationTime::new(text.parse().unwrap_or(DateTime::<Utc>::UNIX_EPOCH))
    }

    pub(crate) fn request() -> Request {
        Request {
            source: SourceId::new(SOURCE),
            target_window: TargetWindow::new(WINDOW),
            url: URL.to_owned(),
            headers: vec![(
                HeaderName::new("Accept"),
                HeaderValue::Literal("text/plain".to_owned()),
            )],
            params: vec![Param {
                name: "action".to_owned(),
                value: HeaderValue::Literal("getcompany".to_owned()),
            }],
        }
    }

    fn baseline() -> Baseline {
        Baseline::from_trailing_ok(&PREVIOUS_OK_COUNTS)
    }

    fn usable() -> Outcome {
        Outcome::decide(&Verdict::Usable, VENDOR_ROWS, ITEMS_NEW, &baseline())
    }

    fn slot(sink: &mut dyn Sink, ordinal: u32) -> Result<AttemptSlot, LedgerError> {
        AttemptSlot::open(sink, &request(), &RunId::new(RUN), ordinal, at(OPENED_AT))
    }

    fn answer(slot: &AttemptSlot) -> RawResponse {
        RawResponse::new(
            slot.attempt_id().clone(),
            OK_STATUS,
            Vec::new(),
            at(REQUESTED_AT),
            at(RECEIVED_AT),
        )
    }

    #[test]
    fn opening_a_slot_writes_its_opened_row_before_any_permit_exists() {
        let mut sink = Recorder::default();
        let opened = slot(&mut sink, FIRST_ATTEMPT);
        assert!(opened.is_ok(), "{opened:?}");
        assert_eq!(sink.names(), OPENED_ONLY.to_vec());
    }

    #[test]
    fn a_sink_that_refuses_the_opened_row_yields_no_slot_and_therefore_no_permit() {
        let mut sink = Recorder::refusing(0);
        assert_eq!(
            slot(&mut sink, FIRST_ATTEMPT).err(),
            Some(LedgerError::WriteFailed {
                event: "opened",
                detail: "no space left on device".to_owned(),
            })
        );
    }

    #[test]
    fn taking_a_second_permit_from_one_slot_is_a_named_error_and_not_silence() {
        let mut sink = Recorder::default();
        let Ok(mut opened) = slot(&mut sink, FIRST_ATTEMPT) else {
            assert_eq!(sink.names(), vec!["an opened slot"]);
            return;
        };
        assert!(opened.take_permit().is_ok());
        assert_eq!(
            opened.take_permit().err(),
            Some(LedgerError::PermitAlreadyTaken)
        );
    }

    #[test]
    fn closing_a_slot_writes_answered_before_finalised_and_never_one_without_the_other() {
        let mut sink = Recorder::default();
        let Ok(mut opened) = slot(&mut sink, FIRST_ATTEMPT) else {
            assert_eq!(sink.names(), vec!["an opened slot"]);
            return;
        };
        let answered = Ok(answer(&opened));
        assert!(opened.take_permit().is_ok());
        let receipt = opened.close(&mut sink, usable(), &answered, at(CLOSED_AT));
        assert!(receipt.is_ok(), "{receipt:?}");
        assert_eq!(sink.names(), vec!["opened", "answered", "finalised"]);
    }

    /// The ENOSPC case: an attempt with `Opened` and `Answered` and no `Finalised` appears in no
    /// outcome count, which is why the ledger reports it as a field of its own.
    #[test]
    fn a_sink_that_refuses_the_finalised_row_leaves_the_answered_row_standing() {
        let mut sink = Recorder::refusing(2);
        let Ok(opened) = slot(&mut sink, FIRST_ATTEMPT) else {
            assert_eq!(sink.names(), vec!["an opened slot"]);
            return;
        };
        let answered = Ok(answer(&opened));
        let closed = opened.close(&mut sink, usable(), &answered, at(CLOSED_AT));
        assert!(closed.is_err(), "{closed:?}");
        assert_eq!(sink.names(), vec!["opened", "answered"]);
    }

    #[test]
    fn an_answer_from_another_attempt_is_refused_rather_than_closed_into_this_slot() {
        let mut sink = Recorder::default();
        let (Ok(first), Ok(second)) = (
            slot(&mut sink, FIRST_ATTEMPT),
            slot(&mut sink, SECOND_ATTEMPT),
        ) else {
            assert_eq!(sink.names(), vec!["two opened slots"]);
            return;
        };
        let (mine, theirs) = (
            first.attempt_id().hex().to_owned(),
            second.attempt_id().hex().to_owned(),
        );
        let answered = Ok(answer(&second));
        assert_eq!(
            first
                .close(&mut sink, usable(), &answered, at(CLOSED_AT))
                .err(),
            Some(LedgerError::AnswerFromAnotherAttempt {
                slot: mine,
                answered: theirs,
            })
        );
    }

    #[test]
    fn the_cost_of_a_closed_attempt_counts_the_slot_only_when_its_permit_was_taken() {
        let mut taken = Recorder::default();
        let mut untaken = Recorder::default();
        let (Ok(mut used), Ok(unused)) = (
            slot(&mut taken, FIRST_ATTEMPT),
            slot(&mut untaken, FIRST_ATTEMPT),
        ) else {
            assert_eq!(taken.names(), vec!["two opened slots"]);
            return;
        };
        let (spent, unspent) = (Ok(answer(&used)), Ok(answer(&unused)));
        assert!(used.take_permit().is_ok());
        assert!(used
            .close(&mut taken, usable(), &spent, at(CLOSED_AT))
            .is_ok());
        assert!(unused
            .close(&mut untaken, usable(), &unspent, at(CLOSED_AT))
            .is_ok());
        assert_eq!(
            (taken.cost(), untaken.cost()),
            (Some(Cost::Slots(1)), Some(Cost::Slots(0)))
        );
    }

    #[test]
    fn an_interrupted_attempt_is_closed_by_the_abandoned_constructor_and_not_left_in_flight() {
        let mut sink = Recorder::default();
        let Ok(opened) = slot(&mut sink, FIRST_ATTEMPT) else {
            assert_eq!(sink.names(), vec!["an opened slot"]);
            return;
        };
        let id = opened.attempt_id().clone();
        drop(opened);
        let receipt = abandon(&mut sink, id, at(NOTICED_AT), &RunId::new(RUN));
        assert_eq!(
            receipt.map(|closed| closed.outcome().kind()),
            Ok(OutcomeKind::Interrupted)
        );
        assert_eq!(sink.names(), vec!["opened", "abandoned", "finalised"]);
    }

    #[test]
    fn a_verdict_match_is_total_so_a_new_arm_does_not_compile_until_it_is_handled() {
        for (verdict, expected) in EVERY_VERDICT {
            // `Outcome::decide`'s match carries no wildcard; this table is the other half, so a
            // fifth arm has to appear here too before the suite compiles.
            let named = match verdict {
                Verdict::Usable => OutcomeKind::Ok,
                Verdict::Empty => OutcomeKind::Empty,
                Verdict::PermanentFailure | Verdict::TransientFailure => OutcomeKind::Failed,
            };
            let decided = Outcome::decide(&verdict, VENDOR_ROWS, ITEMS_NEW, &baseline());
            assert_eq!((named, decided.kind()), (expected, expected));
        }
    }

    /// The floor this waits for needs our own construction: `Baseline.ratio` is written on every
    /// attempt so it can be picked from a quantile of real runs, with its date.
    #[test]
    fn decide_cannot_return_collapsed_until_a_floor_with_our_own_construction_exists() {
        let decided = Outcome::decide(
            &Verdict::Usable,
            COLLAPSE_SHAPED_ROWS,
            ITEMS_NEW,
            &baseline(),
        );
        assert_eq!(decided.kind(), OutcomeKind::Ok);
        assert_eq!(
            decided.baseline().and_then(|b| b.ratio),
            Some(f64::from(COLLAPSE_SHAPED_ROWS) / f64::from(LOWER_MEDIAN))
        );
    }

    #[test]
    fn a_baseline_is_the_trailing_median_and_not_the_single_previous_count() {
        let trailing = baseline();
        let attempts = u32::try_from(PREVIOUS_OK_COUNTS.len()).unwrap_or_default();
        assert_eq!((trailing.count, trailing.window), (LOWER_MEDIAN, attempts));
    }

    #[test]
    fn a_baseline_with_no_prior_attempts_has_no_ratio_rather_than_a_division_by_zero() {
        let first_ever = Outcome::decide(
            &Verdict::Usable,
            VENDOR_ROWS,
            ITEMS_NEW,
            &Baseline::from_trailing_ok(&[]),
        );
        assert_eq!(first_ever.baseline().and_then(|b| b.ratio), None);
    }
}
