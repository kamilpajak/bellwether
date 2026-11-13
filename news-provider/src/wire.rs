//! The only module in the workspace that may hold an HTTP client. A send consumes a `SendPermit`,
//! which only an `AttemptSlot` that wrote its `Opened` row can hand out, so one HTTP call needs
//! one written ledger row. What that does and does not close: architecture.md section 8 row R4.
//!
//! The two instants are read here, either side of the socket, from an injected `Clock`: nothing
//! else can know when an answer arrived. The pace wait is read from the same clock, before the
//! socket, because `send` is the one place every vendor call passes through.

use std::path::Path;
use std::time::Duration;

use pit::ObservationTime;

use crate::budget::{BudgetError, Pace, RequestBudget};
use crate::clock::{Clock, Sleeper, ThreadSleeper};
use crate::ledger::{AttemptId, SendPermit};
use crate::request::{CredentialRef, HeaderValue, Request, SourceId};

/// ureq 3.5.2's nine timeout budgets. No `Default`: a provider states every one, so none is
/// inherited from a library's idea of a reasonable wait and this crate writes no number.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub global: Duration,
    pub per_call: Duration,
    pub resolve: Duration,
    pub connect: Duration,
    pub send_request: Duration,
    pub await_100: Duration,
    pub send_body: Duration,
    pub recv_response: Duration,
    pub recv_body: Duration,
}

/// What one vendor's conversation looks like. The transport is this crate's; the profile is the
/// provider's.
#[derive(Debug, Clone)]
pub struct TransportProfile {
    /// SEC answers a User-Agent with no contact address with 403 and one with a contact with 200,
    /// re-measured 2026-11-12, so this is where the contact lives - by name, never by value.
    pub user_agent: HeaderValue,
    pub timeouts: Timeouts,
    /// Bytes of response body read before ureq refuses the rest.
    pub body_limit: u64,
    /// How fast this vendor may be called and what the quota counts against. `Wire::new` builds
    /// the pace file from it, so the interval has one producer: the provider.
    pub budget: RequestBudget,
}

/// The only place a credential VALUE lives. A `Request` and every ledger row carry the name.
#[derive(Debug, Clone, Default)]
pub struct Credentials(Vec<(String, String)>);

impl Credentials {
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.0.push((name.into(), value.into()));
        self
    }

    fn value(&self, credential: &CredentialRef) -> Option<&str> {
        self.0
            .iter()
            .find(|(name, _)| name == credential.as_str())
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SendFailure {
    /// Before the socket: a header names a credential this process does not hold. A startup
    /// refusal, never a vendor event in the coverage ledger (design.md section 3).
    #[error("no credential named {0} is held")]
    CredentialMissing(String),
    /// Also before the socket: the pace file could not be read or reserved. A local refusal, so
    /// like `CredentialMissing` it is not a vendor event in the coverage ledger.
    #[error(transparent)]
    Budget(#[from] BudgetError),
    /// The vendor conversation itself. This is the arm the ledger records.
    #[error("the vendor conversation failed: {0}")]
    Transport(#[from] TransportError),
}

/// A payload as received, with both instants as measured. `new` is the only constructor, so a
/// backwards clock step is flagged rather than repaired: storing a repaired instant would store a
/// time nothing measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawResponse {
    attempt_id: AttemptId,
    status: u16,
    body: Vec<u8>,
    requested_at: ObservationTime,
    received_at: ObservationTime,
    clock_nonmonotonic: bool,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("{detail}")]
pub struct TransportError {
    attempt_id: AttemptId,
    requested_at: ObservationTime,
    failed_at: ObservationTime,
    clock_nonmonotonic: bool,
    detail: String,
}

/// True when the second read of the clock precedes the first.
fn stepped_backwards(first: ObservationTime, second: ObservationTime) -> bool {
    second.inner() < first.inner()
}

impl RawResponse {
    #[must_use]
    pub fn new(
        attempt_id: AttemptId,
        status: u16,
        body: Vec<u8>,
        requested_at: ObservationTime,
        received_at: ObservationTime,
    ) -> Self {
        Self {
            attempt_id,
            status,
            body,
            requested_at,
            received_at,
            clock_nonmonotonic: stepped_backwards(requested_at, received_at),
        }
    }

    #[must_use]
    pub const fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }

    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    #[must_use]
    pub const fn requested_at(&self) -> ObservationTime {
        self.requested_at
    }

    #[must_use]
    pub const fn received_at(&self) -> ObservationTime {
        self.received_at
    }

    #[must_use]
    pub const fn clock_nonmonotonic(&self) -> bool {
        self.clock_nonmonotonic
    }
}

impl TransportError {
    #[must_use]
    pub fn new(
        attempt_id: AttemptId,
        requested_at: ObservationTime,
        failed_at: ObservationTime,
        detail: String,
    ) -> Self {
        Self {
            attempt_id,
            requested_at,
            failed_at,
            clock_nonmonotonic: stepped_backwards(requested_at, failed_at),
            detail,
        }
    }

    #[must_use]
    pub const fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }

    #[must_use]
    pub const fn requested_at(&self) -> ObservationTime {
        self.requested_at
    }

    #[must_use]
    pub const fn failed_at(&self) -> ObservationTime {
        self.failed_at
    }

    #[must_use]
    pub const fn clock_nonmonotonic(&self) -> bool {
        self.clock_nonmonotonic
    }
}

/// The one HTTP client.
#[derive(Debug)]
pub struct Wire {
    agent: ureq::Agent,
    profile: TransportProfile,
    credentials: Credentials,
    pace: Pace,
    sleeper: Box<dyn Sleeper>,
}

const USER_AGENT: &str = "User-Agent";

impl Wire {
    /// One `Wire` is one vendor's conversation, so the pace file is named by `source` and the
    /// interval comes from the profile rather than from a second argument that could disagree
    /// with it.
    /// The waiting is this crate's, not the caller's. Measured 2026-11-13 while the sleeper was a
    /// parameter: wiring production to one whose `sleep` is empty left `clippy` and the whole suite
    /// at exit 0, so the limiter was a capability the caller granted rather than a constraint.
    #[must_use]
    pub fn new(
        profile: TransportProfile,
        credentials: Credentials,
        store: &Path,
        source: &SourceId,
    ) -> Self {
        Self::with_sleeper(profile, credentials, store, source, Box::new(ThreadSleeper))
    }

    /// The seam the suite needs to observe a wait without spending one. Not public: outside this
    /// crate there is no constructor that does not sleep.
    #[must_use]
    pub(crate) fn with_sleeper(
        profile: TransportProfile,
        credentials: Credentials,
        store: &Path,
        source: &SourceId,
        sleeper: Box<dyn Sleeper>,
    ) -> Self {
        let pace = Pace::beside_the_store(store, source, profile.budget);
        let timeouts = profile.timeouts;
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeouts.global))
            .timeout_per_call(Some(timeouts.per_call))
            .timeout_resolve(Some(timeouts.resolve))
            .timeout_connect(Some(timeouts.connect))
            .timeout_send_request(Some(timeouts.send_request))
            .timeout_await_100(Some(timeouts.await_100))
            .timeout_send_body(Some(timeouts.send_body))
            .timeout_recv_response(Some(timeouts.recv_response))
            .timeout_recv_body(Some(timeouts.recv_body))
            // A 403 carrying the vendor's own body is an ANSWER the classifier reads, not an
            // error: EDGAR answers a weekend, a holiday and an unpublished day with 403 and the
            // same generic S3 body, measured on six dates 2026-11-12 (design.md section 2.4).
            .http_status_as_error(false)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            profile,
            credentials,
            pace,
            sleeper,
        }
    }

    /// The profile's User-Agent first, then the request's own headers, with every credential
    /// reference replaced by its value. The only assembly of outgoing headers in the crate.
    fn headers_for(&self, request: &Request) -> Result<Vec<(String, String)>, SendFailure> {
        let mut named = vec![(USER_AGENT.to_owned(), &self.profile.user_agent)];
        named.extend(
            request
                .headers
                .iter()
                .map(|(name, value)| (name.as_str().to_owned(), value)),
        );
        named
            .into_iter()
            .map(|(name, value)| match value {
                HeaderValue::Literal(literal) => Ok((name, literal.clone())),
                HeaderValue::FromCredential(credential) => self
                    .credentials
                    .value(credential)
                    .map(|held| (name, held.to_owned()))
                    .ok_or_else(|| SendFailure::CredentialMissing(credential.as_str().to_owned())),
            })
            .collect()
    }

    /// Consumes the permit, so one HTTP call needs one written ledger row, and waits out the
    /// vendor's pace first, so one process cannot be the whole bucket.
    pub fn send(
        &self,
        permit: SendPermit,
        request: &Request,
        clock: &dyn Clock,
    ) -> Result<RawResponse, SendFailure> {
        let attempt_id = permit.into_attempt_id();
        let headers = self.headers_for(request)?;
        // The pace wait is owed only by a call that is going to be made, so it comes after the
        // credential check. Unconditional, with no zero-wait branch for a test to leave uncovered.
        self.sleeper.sleep(self.pace.reserve(clock.now())?);
        let requested_at = ObservationTime::new(clock.now());
        let answered = self.exchange(&request.url, &headers);
        let received_at = ObservationTime::new(clock.now());
        match answered {
            Ok((status, body)) => Ok(RawResponse::new(
                attempt_id,
                status,
                body,
                requested_at,
                received_at,
            )),
            Err(detail) => Err(SendFailure::Transport(TransportError::new(
                attempt_id,
                requested_at,
                received_at,
                detail,
            ))),
        }
    }

    /// The socket, and the only call to the agent. Between the two clock reads in [`Wire::send`],
    /// so the body is inside the measured interval.
    fn exchange(&self, url: &str, headers: &[(String, String)]) -> Result<(u16, Vec<u8>), String> {
        let mut call = self.agent.get(url);
        for (name, value) in headers {
            call = call.header(name, value);
        }
        let mut response = call.call().map_err(|why| why.to_string())?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(self.profile.body_limit)
            .read_to_vec()
            .map_err(|why| why.to_string())?;
        Ok((status, body))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Credentials, RawResponse, SendFailure, Timeouts, TransportProfile, Wire, USER_AGENT,
    };
    use crate::budget::{BudgetError, BudgetScope, RequestBudget};
    use crate::clock::testing::{StepClock, WaitRecorder};
    use crate::ledger::tests::{at, Recorder};
    use crate::ledger::{AttemptSlot, SendPermit};
    use crate::provider::Provenance;
    use crate::request::{
        CredentialRef, HeaderName, HeaderValue, Request, RequestPhase, RunId, SourceId,
        TargetWindow,
    };
    use canon::FileDigest;
    use std::path::Path;
    use std::time::Duration;

    type Checked = Result<(), Box<dyn std::error::Error>>;

    const CONTACT: &str = "sec_contact";
    const CONTACT_ADDRESS: &str = "bellwether contact@example.test";
    const ACCEPT: &str = "Accept";
    const PLAIN_TEXT: &str = "text/plain";
    /// Port 1 on the loopback refuses at once, so no test here waits on a network.
    const REFUSED: &str = "http://127.0.0.1:1/never";
    const SOURCE: &str = "edgar_press";
    const WINDOW: &str = "2026-11-10";
    const RUN: &str = "run-2026-11-10T13:00:00Z";
    const FIRST_ATTEMPT: u32 = 0;
    const SECOND_ATTEMPT: u32 = 1;
    const OPENED_AT: &str = "2026-11-10T13:00:00Z";
    const RESERVED_AT: &str = "2026-11-10T13:00:00Z";
    /// `ELAPSED` after `RESERVED_AT`, which is where the second send's reservation lands.
    const SIXTY_MS_LATER: &str = "2026-11-10T13:00:00.060Z";
    const REQUESTED_AT: &str = "2026-11-10T13:00:01Z";
    const RECEIVED_AT: &str = "2026-11-10T13:00:02Z";
    const BEFORE_IT_WAS_REQUESTED: &str = "2026-11-10T12:59:59Z";
    const OK_STATUS: u16 = 200;
    /// The reservation and both instants, and nothing else, come out of the clock during one send.
    const READS_PER_SEND: usize = 3;
    const NO_READS: usize = 0;
    const OPENED_ROW: usize = 1;
    const TEST_TIMEOUT: Duration = Duration::from_secs(2);
    const TEST_BODY_LIMIT: u64 = 1_024;
    /// The pace under test, kept distinct from the vendor's so a test reads as its own arithmetic.
    const TEST_INTERVAL: Duration = Duration::from_millis(100);
    const ELAPSED: Duration = Duration::from_millis(60);
    const EXPECTED_WAIT: Duration = Duration::from_millis(40);
    const NO_WAIT: Duration = Duration::ZERO;
    /// `<store>/pace/<source>.<scope>`, which is the layout `Pace::beside_the_store` states.
    const PACE_FILE: &str = "pace/edgar_press.per-ip";
    /// `RESERVED_AT` plus `TEST_INTERVAL`, in the form the pace file is written in.
    const RESERVED_UNTIL: &str = "2026-11-10T13:00:00.100000Z";
    const NOT_AN_INSTANT: &str = "soon";
    /// Any roster bytes: these tests are about the socket.
    const ROSTER: &[u8] = b"{\"320193\":\"AAPL\"}";

    fn contact() -> HeaderValue {
        HeaderValue::FromCredential(CredentialRef::new(CONTACT))
    }

    fn profile() -> TransportProfile {
        TransportProfile {
            user_agent: contact(),
            timeouts: Timeouts {
                global: TEST_TIMEOUT,
                per_call: TEST_TIMEOUT,
                resolve: TEST_TIMEOUT,
                connect: TEST_TIMEOUT,
                send_request: TEST_TIMEOUT,
                await_100: TEST_TIMEOUT,
                send_body: TEST_TIMEOUT,
                recv_response: TEST_TIMEOUT,
                recv_body: TEST_TIMEOUT,
            },
            body_limit: TEST_BODY_LIMIT,
            budget: RequestBudget {
                min_interval: TEST_INTERVAL,
                scope: BudgetScope::PerIp,
            },
        }
    }

    fn request() -> Request {
        Request {
            source: SourceId::new(SOURCE),
            target_window: TargetWindow::new(WINDOW),
            phase: RequestPhase::Index,
            provenance: Provenance::LiveIngest,
            url: REFUSED.to_owned(),
            universe_version: FileDigest::of(ROSTER),
            headers: vec![(
                HeaderName::new(ACCEPT),
                HeaderValue::Literal(PLAIN_TEXT.to_owned()),
            )],
            params: Vec::new(),
        }
    }

    /// Every `Wire` a test builds pays its pace into `store` and records its waits instead of
    /// taking them, so the suite's own wall clock is not part of any assertion.
    fn wire(credentials: Credentials, store: &Path, waits: &WaitRecorder) -> Wire {
        Wire::with_sleeper(
            profile(),
            credentials,
            store,
            &SourceId::new(SOURCE),
            Box::new(waits.clone()),
        )
    }

    /// One clock read for the reservation and two for the instants, per send.
    fn clock_over(sends: &[&str]) -> StepClock {
        let reads: Vec<&str> = sends
            .iter()
            .flat_map(|reserved| [*reserved, REQUESTED_AT, RECEIVED_AT])
            .collect();
        StepClock::over(&reads)
    }

    /// The loopback refuses at once, so the answer is always the failure arm and never a network
    /// wait; what these tests read is the wait BEFORE it. One `Wire` per call site on purpose:
    /// the two tests below send twice through ONE of them, which an in-process limiter satisfies,
    /// so the cross-instance test is the only one that is about the file.
    fn send_through(wire: &Wire, clock: &StepClock, ordinal: u32) -> Checked {
        let mut sink = Recorder::default();
        let Some(permit) = permit(&mut sink, ordinal) else {
            return Err("no permit".into());
        };
        match wire.send(permit, &request(), clock) {
            Err(SendFailure::Transport(_)) => Ok(()),
            other => Err(format!("{other:?}").into()),
        }
    }

    fn held() -> Credentials {
        Credentials::default().with(CONTACT, CONTACT_ADDRESS)
    }

    fn slot(sink: &mut Recorder) -> Option<AttemptSlot> {
        slot_at(sink, FIRST_ATTEMPT)
    }

    fn slot_at(sink: &mut Recorder, ordinal: u32) -> Option<AttemptSlot> {
        let run = RunId::new(RUN);
        AttemptSlot::open(sink, &request(), &run, ordinal, at(OPENED_AT)).ok()
    }

    fn permit(sink: &mut Recorder, ordinal: u32) -> Option<SendPermit> {
        slot_at(sink, ordinal)?.take_permit().ok()
    }

    #[test]
    fn a_header_value_from_a_credential_carries_the_name_and_the_substitution_happens_in_send(
    ) -> Checked {
        let store = tempfile::tempdir()?;
        assert_eq!(
            wire(held(), store.path(), &WaitRecorder::default()).headers_for(&request()),
            Ok(vec![
                (USER_AGENT.to_owned(), CONTACT_ADDRESS.to_owned()),
                (ACCEPT.to_owned(), PLAIN_TEXT.to_owned()),
            ]),
            "the profile holds {:?}",
            contact()
        );
        Ok(())
    }

    #[test]
    fn a_send_whose_credential_is_absent_refuses_before_the_clock_is_read_at_all() -> Checked {
        let store = tempfile::tempdir()?;
        let clock = clock_over(&[RESERVED_AT]);
        let waits = WaitRecorder::default();
        let mut sink = Recorder::default();
        let Some(permit) = permit(&mut sink, FIRST_ATTEMPT) else {
            return Err("no permit".into());
        };
        let refused =
            wire(Credentials::default(), store.path(), &waits).send(permit, &request(), &clock);
        // No clock read and no pace file: the credential check runs before the reservation, so a
        // call that cannot be made does not spend a slot of the vendor's budget either.
        assert_eq!(
            (
                refused.err(),
                clock.reads(),
                store.path().join(PACE_FILE).exists()
            ),
            (
                Some(SendFailure::CredentialMissing(CONTACT.to_owned())),
                NO_READS,
                false
            )
        );
        Ok(())
    }

    #[test]
    fn requested_at_and_received_at_are_read_either_side_of_the_call_by_the_injected_clock(
    ) -> Checked {
        let store = tempfile::tempdir()?;
        let clock = clock_over(&[RESERVED_AT]);
        let waits = WaitRecorder::default();
        let mut sink = Recorder::default();
        let Some(permit) = permit(&mut sink, FIRST_ATTEMPT) else {
            return Err("no permit".into());
        };
        // The loopback refuses, so this is the failure arm - the one that must carry both
        // instants, because an attempt that never answered still owes the ledger a row.
        let Err(SendFailure::Transport(failed)) =
            wire(held(), store.path(), &waits).send(permit, &request(), &clock)
        else {
            return Err(REFUSED.into());
        };
        assert_eq!(
            (failed.requested_at(), failed.failed_at(), clock.reads()),
            (at(REQUESTED_AT), at(RECEIVED_AT), READS_PER_SEND)
        );
        Ok(())
    }

    #[test]
    fn a_response_received_before_it_was_requested_is_flagged_rather_than_repaired() {
        let mut sink = Recorder::default();
        let Some(slot) = slot(&mut sink) else {
            assert_eq!(sink.written.len(), OPENED_ROW);
            return;
        };
        let backwards = RawResponse::new(
            slot.attempt_id().clone(),
            OK_STATUS,
            Vec::new(),
            at(REQUESTED_AT),
            at(BEFORE_IT_WAS_REQUESTED),
        );
        assert_eq!(
            (
                backwards.clock_nonmonotonic(),
                backwards.requested_at(),
                backwards.received_at()
            ),
            (true, at(REQUESTED_AT), at(BEFORE_IT_WAS_REQUESTED))
        );
    }

    /// The whole point of a FILE: two `Wire` instances share no memory, so an in-process limiter
    /// passes every test above and this one is the only place it fails.
    #[test]
    fn two_wires_sharing_one_pace_file_serialise_against_each_other() -> Checked {
        let store = tempfile::tempdir()?;
        let (first, second) = (WaitRecorder::default(), WaitRecorder::default());
        send_through(
            &wire(held(), store.path(), &first),
            &clock_over(&[RESERVED_AT]),
            FIRST_ATTEMPT,
        )?;
        send_through(
            &wire(held(), store.path(), &second),
            &clock_over(&[SIXTY_MS_LATER]),
            FIRST_ATTEMPT,
        )?;
        assert_eq!(
            (first.waits(), second.waits()),
            (vec![NO_WAIT], vec![EXPECTED_WAIT])
        );
        Ok(())
    }

    #[test]
    fn the_second_send_waits_where_the_first_did_not() -> Checked {
        let store = tempfile::tempdir()?;
        let waits = WaitRecorder::default();
        let clock = clock_over(&[RESERVED_AT, SIXTY_MS_LATER]);
        let wire = wire(held(), store.path(), &waits);
        send_through(&wire, &clock, FIRST_ATTEMPT)?;
        send_through(&wire, &clock, SECOND_ATTEMPT)?;
        let taken = waits.waits();
        assert_eq!(
            (
                taken.first().copied(),
                taken.get(1).copied() > Some(NO_WAIT)
            ),
            (Some(NO_WAIT), true),
            "{taken:?}"
        );
        Ok(())
    }

    /// The test above passes a limiter that sleeps the whole interval every time, which would
    /// cost 60 ms of pace this caller has already spent waiting for its own previous answer.
    #[test]
    fn the_wait_is_the_interval_less_the_time_already_elapsed() -> Checked {
        let store = tempfile::tempdir()?;
        let waits = WaitRecorder::default();
        let clock = clock_over(&[RESERVED_AT, SIXTY_MS_LATER]);
        let wire = wire(held(), store.path(), &waits);
        send_through(&wire, &clock, FIRST_ATTEMPT)?;
        send_through(&wire, &clock, SECOND_ATTEMPT)?;
        assert_eq!(waits.waits(), vec![NO_WAIT, EXPECTED_WAIT]);
        assert_eq!(TEST_INTERVAL - ELAPSED, EXPECTED_WAIT);
        Ok(())
    }

    #[test]
    fn the_pace_file_sits_beside_the_store_under_the_source_and_its_scope() -> Checked {
        let store = tempfile::tempdir()?;
        let waits = WaitRecorder::default();
        send_through(
            &wire(held(), store.path(), &waits),
            &clock_over(&[RESERVED_AT]),
            FIRST_ATTEMPT,
        )?;
        assert_eq!(
            std::fs::read_to_string(store.path().join(PACE_FILE))?,
            RESERVED_UNTIL
        );
        Ok(())
    }

    /// Refused, not reset to `now`: a value this crate did not write is either a torn write or
    /// another writer, and both answers to "when may I call" would be invented.
    #[test]
    fn a_pace_file_holding_a_value_this_crate_did_not_write_refuses_before_the_socket() -> Checked {
        let store = tempfile::tempdir()?;
        let pace = store.path().join(PACE_FILE);
        if let Some(parent) = pace.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&pace, NOT_AN_INSTANT)?;
        let clock = clock_over(&[RESERVED_AT]);
        let waits = WaitRecorder::default();
        let mut sink = Recorder::default();
        let Some(permit) = permit(&mut sink, FIRST_ATTEMPT) else {
            return Err("no permit".into());
        };
        let refused = wire(held(), store.path(), &waits).send(permit, &request(), &clock);
        assert_eq!(
            (refused.err(), waits.waits()),
            (
                Some(SendFailure::Budget(BudgetError::Unreadable {
                    path: pace,
                    held: NOT_AN_INSTANT.to_owned(),
                })),
                Vec::new()
            )
        );
        Ok(())
    }
}
