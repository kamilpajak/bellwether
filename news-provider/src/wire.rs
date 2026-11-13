//! The only module in the workspace that may hold an HTTP client. A send consumes a `SendPermit`,
//! which only an `AttemptSlot` that wrote its `Opened` row can hand out, so one HTTP call needs
//! one written ledger row. What that does and does not close: architecture.md section 8 row R4.
//!
//! The two instants are read here, either side of the socket, from an injected `Clock`: nothing
//! else can know when an answer arrived.

use std::time::Duration;

use pit::ObservationTime;

use crate::clock::Clock;
use crate::ledger::{AttemptId, SendPermit};
use crate::request::{CredentialRef, HeaderValue, Request};

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
}

const USER_AGENT: &str = "User-Agent";

impl Wire {
    #[must_use]
    pub fn new(profile: TransportProfile, credentials: Credentials) -> Self {
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

    /// Consumes the permit, so one HTTP call needs one written ledger row.
    pub fn send(
        &self,
        permit: SendPermit,
        request: &Request,
        clock: &dyn Clock,
    ) -> Result<RawResponse, SendFailure> {
        let attempt_id = permit.into_attempt_id();
        let headers = self.headers_for(request)?;
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
    use crate::clock::StepClock;
    use crate::ledger::tests::{at, Recorder};
    use crate::ledger::{AttemptSlot, SendPermit};
    use crate::request::{
        CredentialRef, HeaderName, HeaderValue, Request, RunId, SourceId, TargetWindow,
    };
    use std::time::Duration;

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
    const OPENED_AT: &str = "2026-11-10T13:00:00Z";
    const REQUESTED_AT: &str = "2026-11-10T13:00:01Z";
    const RECEIVED_AT: &str = "2026-11-10T13:00:02Z";
    const BEFORE_IT_WAS_REQUESTED: &str = "2026-11-10T12:59:59Z";
    const OK_STATUS: u16 = 200;
    /// Both instants, and nothing else, come out of the clock during one send.
    const READS_PER_SEND: usize = 2;
    const NO_READS: usize = 0;
    const OPENED_ROW: usize = 1;
    const TEST_TIMEOUT: Duration = Duration::from_secs(2);
    const TEST_BODY_LIMIT: u64 = 1_024;

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
        }
    }

    fn request() -> Request {
        Request {
            source: SourceId::new(SOURCE),
            target_window: TargetWindow::new(WINDOW),
            url: REFUSED.to_owned(),
            headers: vec![(
                HeaderName::new(ACCEPT),
                HeaderValue::Literal(PLAIN_TEXT.to_owned()),
            )],
            params: Vec::new(),
        }
    }

    fn wire(credentials: Credentials) -> Wire {
        Wire::new(profile(), credentials)
    }

    fn held() -> Credentials {
        Credentials::default().with(CONTACT, CONTACT_ADDRESS)
    }

    fn slot(sink: &mut Recorder) -> Option<AttemptSlot> {
        let run = RunId::new(RUN);
        AttemptSlot::open(sink, &request(), &run, FIRST_ATTEMPT, at(OPENED_AT)).ok()
    }

    fn permit(sink: &mut Recorder) -> Option<SendPermit> {
        slot(sink)?.take_permit().ok()
    }

    #[test]
    fn a_header_value_from_a_credential_carries_the_name_and_the_substitution_happens_in_send() {
        assert_eq!(
            wire(held()).headers_for(&request()),
            Ok(vec![
                (USER_AGENT.to_owned(), CONTACT_ADDRESS.to_owned()),
                (ACCEPT.to_owned(), PLAIN_TEXT.to_owned()),
            ]),
            "the profile holds {:?}",
            contact()
        );
    }

    #[test]
    fn a_send_whose_credential_is_absent_refuses_before_the_clock_is_read_at_all() {
        let clock = StepClock::over(&[REQUESTED_AT, RECEIVED_AT]);
        let mut sink = Recorder::default();
        let Some(permit) = permit(&mut sink) else {
            assert_eq!(sink.written.len(), OPENED_ROW);
            return;
        };
        let refused = wire(Credentials::default()).send(permit, &request(), &clock);
        assert_eq!(
            (refused.err(), clock.reads()),
            (
                Some(SendFailure::CredentialMissing(CONTACT.to_owned())),
                NO_READS
            )
        );
    }

    #[test]
    fn requested_at_and_received_at_are_read_either_side_of_the_call_by_the_injected_clock() {
        let clock = StepClock::over(&[REQUESTED_AT, RECEIVED_AT]);
        let mut sink = Recorder::default();
        let Some(permit) = permit(&mut sink) else {
            assert_eq!(sink.written.len(), OPENED_ROW);
            return;
        };
        // The loopback refuses, so this is the failure arm - the one that must carry both
        // instants, because an attempt that never answered still owes the ledger a row.
        let Err(SendFailure::Transport(failed)) = wire(held()).send(permit, &request(), &clock)
        else {
            assert_eq!(REFUSED, "a refused connection");
            return;
        };
        assert_eq!(
            (failed.requested_at(), failed.failed_at(), clock.reads()),
            (at(REQUESTED_AT), at(RECEIVED_AT), READS_PER_SEND)
        );
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
}
