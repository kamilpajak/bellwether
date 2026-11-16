//! The only module in the workspace that may hold an HTTP client. A send consumes a `SendPermit`,
//! which only an `AttemptSlot` that wrote its `Opened` row can hand out, so one HTTP call needs
//! one written ledger row. What that does and does not close: architecture.md section 8 row R4.
//!
//! The two instants are read here, either side of the socket, from an injected `Clock`: nothing
//! else can know when an answer arrived. The pace wait is read from the same clock, before the
//! socket, because `send` is the one place every vendor call passes through.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, TimeDelta, Utc};
use pit::ObservationTime;

use crate::budget::{host_of, BudgetError, BudgetScope, HostKey, Pace, RequestBudget};
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
    /// How fast this vendor may be called, so both intervals have one producer: the provider.
    /// `Wire::new` builds the per-IP pace file; `Wire::send` opens a per-host one per url host.
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
    /// refusal, never a vendor event in the coverage ledger (architecture.md section 5.1).
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

/// The response headers P9 keeps beside the raw blob (architecture.md section 3.9), lowercase.
pub const KEPT_HEADERS: [&str; 8] = [
    "content-type",
    "last-modified",
    "etag",
    "date",
    "cache-control",
    "vary",
    "content-encoding",
    RETRY_AFTER,
];

const RETRY_AFTER: &str = "retry-after";
/// RFC 9110 §5.6.7's HTTP-date forms: IMF-fixdate, then the obsolete RFC 850 and asctime ones.
const HTTP_DATE_FORMATS: [&str; 3] = [
    "%a, %d %b %Y %H:%M:%S GMT",
    "%A, %d-%b-%y %H:%M:%S GMT",
    "%a %b %e %H:%M:%S %Y",
];
/// The answers by which a server asks a client to slow down: 429 (RFC 6585 §4) and 503 (RFC 9110
/// §15.6.4), as Google's crawler reads them (architecture.md section 10, server back-off).
const SLOW_DOWN: [u16; 2] = [429, 503];

/// What the slow-down answers of one host asked of this `Wire`, for the rest of its run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Backoff {
    /// Slow-down answers with no usable `Retry-After`; the host interval is `per_host` × 2^this.
    doublings: u32,
    /// The latest instant a `Retry-After` named; no host slot is granted before it.
    not_before: Option<DateTime<Utc>>,
}

/// The one `Content-Encoding` value ureq 3.5.2's `gzip` feature decodes (`ContentEncoding::from`
/// matches it exactly, case included), so the body stored here is the one ureq would have stored.
const GZIP: &str = "gzip";
/// No 3xx is followed: at 0 ureq 3.5.2 returns the 3xx as the answer and raises
/// `TooManyRedirects` only above 0 (`config.rs`); its default, 10, sends each hop unpaced and
/// unledgered (architecture.md section 8 row R19).
const MAX_REDIRECTS: u32 = 0;

/// The [`KEPT_HEADERS`] of one answer, as `(name, value)` pairs in the order `http` 1.x's
/// `HeaderMap` yields them: names by first appearance, each name's values in the order received.
/// A value is its bytes read as ISO-8859-1, so every byte round-trips.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    #[must_use]
    pub const fn new(pairs: Vec<(String, String)>) -> Self {
        Self(pairs)
    }

    fn kept_from(map: &ureq::http::HeaderMap) -> Self {
        Self(
            map.iter()
                .filter(|(name, _)| KEPT_HEADERS.contains(&name.as_str()))
                .map(|(name, value)| {
                    let latin1 = value.as_bytes().iter().copied().map(char::from).collect();
                    (name.as_str().to_owned(), latin1)
                })
                .collect(),
        )
    }

    #[must_use]
    pub fn pairs(&self) -> &[(String, String)] {
        &self.0
    }

    /// The stored form: a JSON array of `[name, value]` arrays in [`Headers`] order, no whitespace.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::Value::Array(
            self.0
                .iter()
                .map(|(name, value)| serde_json::json!([name, value]))
                .collect(),
        )
        .to_string()
    }
}

/// A payload as received, with both instants as measured. `new` is the only constructor, so a
/// backwards clock step is flagged rather than repaired: storing a repaired instant would store a
/// time nothing measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawResponse {
    attempt_id: AttemptId,
    status: u16,
    headers: Headers,
    body: Vec<u8>,
    requested_at: ObservationTime,
    received_at: ObservationTime,
    clock_nonmonotonic: bool,
}

/// What a provider's `classify` reads from a failed conversation. A body over the profile's
/// `body_limit` fails the same way on every refetch, so it is not a transient failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Conversation,
    BodyOverLimit,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("{detail}")]
pub struct TransportError {
    attempt_id: AttemptId,
    requested_at: ObservationTime,
    failed_at: ObservationTime,
    clock_nonmonotonic: bool,
    kind: TransportKind,
    /// The status of a head read before the body failed; `None` when no head arrived.
    status: Option<u16>,
    /// Boxed: inline, `SendFailure` fails clippy's `result_large_err` bound of 128 bytes.
    head: Box<HeadRead>,
    detail: String,
}

/// What a failed conversation read before it failed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct HeadRead {
    headers: Headers,
    /// The first `body_limit` bytes of a body over the limit, which `collect` stores truncated.
    cut_body: Option<Vec<u8>>,
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
            headers: Headers::default(),
            body,
            requested_at,
            received_at,
            clock_nonmonotonic: stepped_backwards(requested_at, received_at),
        }
    }

    #[must_use]
    pub fn with_headers(mut self, headers: Headers) -> Self {
        self.headers = headers;
        self
    }

    #[must_use]
    pub const fn headers(&self) -> &Headers {
        &self.headers
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
            kind: TransportKind::Conversation,
            status: None,
            head: Box::default(),
            detail,
        }
    }

    #[must_use]
    pub fn with_headers(mut self, headers: Headers) -> Self {
        self.head.headers = headers;
        self
    }

    #[must_use]
    pub fn headers(&self) -> &Headers {
        &self.head.headers
    }

    /// A body over `body_limit`, cut at the limit: P10's truncated payload (section 3.9).
    #[must_use]
    pub fn with_cut_body(mut self, body: Vec<u8>) -> Self {
        self.head.cut_body = Some(body);
        self
    }

    #[must_use]
    pub fn cut_body(&self) -> Option<&[u8]> {
        self.head.cut_body.as_deref()
    }

    /// The head's status when the body read failed after it (over `body_limit` or cut short), so a
    /// 3xx stays visible to the held read's 3xx exclusion (architecture.md section 11.1).
    #[must_use]
    pub const fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    #[must_use]
    pub const fn status(&self) -> Option<u16> {
        self.status
    }

    #[must_use]
    pub const fn of_kind(mut self, kind: TransportKind) -> Self {
        self.kind = kind;
        self
    }

    #[must_use]
    pub const fn kind(&self) -> TransportKind {
        self.kind
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
    /// Where a per-host pace file is opened, one per remote host a request names.
    store: PathBuf,
    sleeper: Box<dyn Sleeper>,
    /// Held in memory: one `Wire` is one run, and a second process sees only the host slot granted.
    backoff: RefCell<BTreeMap<HostKey, Backoff>>,
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
        let pace = Pace::beside_the_store(
            store,
            &BudgetScope::PerIp(source.clone()),
            profile.budget.min_interval,
        );
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
            // same generic S3 body (measured in `providers/edgar-press/src/lib.rs`'s header).
            .http_status_as_error(false)
            .max_redirects(MAX_REDIRECTS)
            // ureq 3.5.2 sends `accept-encoding: gzip` only with its `gzip` feature, which is off
            // (Cargo.toml); stated here so the request head stays byte for byte what it was.
            .accept_encoding(GZIP)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            profile,
            credentials,
            pace,
            store: store.to_path_buf(),
            sleeper,
            backoff: RefCell::default(),
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
        let host = self.paced_host(&request.url)?;
        self.wait_out_the_host(host.as_ref(), clock)?;
        self.sleeper.sleep(self.pace.reserve(clock.now())?);
        let requested_at = ObservationTime::new(clock.now());
        let answered = self.exchange(&request.url, &headers);
        let received_at = ObservationTime::new(clock.now());
        if let Some(host) = host {
            let (status, kept) = match &answered {
                Ok((status, kept, _)) => (Some(*status), kept),
                Err(failed) => (failed.status, &failed.headers),
            };
            self.heed(host, status, kept, received_at.inner());
        }
        match answered {
            Ok((status, headers, body)) => {
                Ok(
                    RawResponse::new(attempt_id, status, body, requested_at, received_at)
                        .with_headers(headers),
                )
            }
            Err(failed) => {
                let mut error =
                    TransportError::new(attempt_id, requested_at, received_at, failed.detail)
                        .of_kind(failed.kind)
                        .with_headers(failed.headers);
                if let Some(status) = failed.status {
                    error = error.with_status(status);
                }
                if let Some(cut) = failed.cut {
                    error = error.with_cut_body(cut);
                }
                Err(SendFailure::Transport(error))
            }
        }
    }

    /// Taken BEFORE the per-IP slot is reserved: an IP slot reserved and then held through a long
    /// host wait would push every other host's call behind it (row R19).
    fn wait_out_the_host(
        &self,
        host: Option<&HostKey>,
        clock: &dyn Clock,
    ) -> Result<(), BudgetError> {
        let (Some(interval), Some(host)) = (self.profile.budget.per_host, host) else {
            return Ok(());
        };
        let backoff = self.backoff.borrow().get(host).copied().unwrap_or_default();
        let interval = interval.saturating_mul(2_u32.saturating_pow(backoff.doublings));
        let scope = BudgetScope::PerHost(host.clone());
        let pace = Pace::beside_the_store(&self.store, &scope, interval);
        self.sleeper
            .sleep(pace.reserve_not_before(clock.now(), backoff.not_before)?);
        Ok(())
    }

    /// The url's host when the profile paces hosts. ureq 3.5.2 parses a url with this same
    /// `http::Uri` and fails before any socket, so an unparseable one goes on to fail as a
    /// `Transport` its attempt row records.
    fn paced_host(&self, url: &str) -> Result<Option<HostKey>, BudgetError> {
        match self.profile.budget.per_host {
            Some(_) => host_of(url),
            None => Ok(None),
        }
    }

    /// A [`SLOW_DOWN`] answer holds its host until its `Retry-After`, or without a usable one
    /// doubles the host interval; either lasts for the rest of this `Wire`'s run (row R19).
    fn heed(&self, host: HostKey, status: Option<u16>, kept: &Headers, received: DateTime<Utc>) {
        if !status.is_some_and(|status| SLOW_DOWN.contains(&status)) {
            return;
        }
        let mut held = self.backoff.borrow_mut();
        let backoff = held.entry(host).or_default();
        match retry_after(kept, received) {
            Some(until) => backoff.not_before = backoff.not_before.max(Some(until)),
            None => backoff.doublings = backoff.doublings.saturating_add(1),
        }
    }

    /// The socket, and the only call to the agent. Between the two clock reads in [`Wire::send`],
    /// so the body is inside the measured interval.
    fn exchange(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<(u16, Headers, Vec<u8>), Failed> {
        let mut call = self.agent.get(url);
        for (name, value) in headers {
            call = call.header(name, value);
        }
        let mut response = call.call().map_err(|why| Failed {
            kind: TransportKind::Conversation,
            status: None,
            headers: Headers::default(),
            cut: None,
            detail: why.to_string(),
        })?;
        let status = response.status().as_u16();
        let kept = Headers::kept_from(response.headers());
        let gzip = response
            .headers()
            .get(ureq::http::header::CONTENT_ENCODING)
            .and_then(|value| value.to_str().ok())
            == Some(GZIP);
        // Unbounded here because `body_limit` counts DECODED bytes, as ureq 3.5.2's limit did.
        let raw = response.body_mut().with_config().limit(u64::MAX).reader();
        let decoded: Box<dyn Read + '_> = if gzip {
            Box::new(flate2::read::MultiGzDecoder::new(raw))
        } else {
            Box::new(raw)
        };
        match read_up_to(decoded, self.profile.body_limit) {
            Ok(body) => Ok((status, kept, body)),
            Err(Unread::OverLimit(cut)) => Err(Failed {
                kind: TransportKind::BodyOverLimit,
                status: Some(status),
                headers: kept,
                cut: Some(cut),
                detail: format!(
                    "the body is over the limit of {} bytes",
                    self.profile.body_limit
                ),
            }),
            Err(Unread::Failed(why)) => Err(Failed {
                kind: TransportKind::Conversation,
                status: Some(status),
                headers: kept,
                cut: None,
                detail: why.to_string(),
            }),
        }
    }
}

/// Why a body read stopped before its end.
enum Unread {
    /// The first `limit` bytes; at least one more followed.
    OverLimit(Vec<u8>),
    Failed(std::io::Error),
}

/// ureq 3.5.2's `LimitReader` rule: `limit` bytes are read, then one more is probed, and only a
/// byte past the limit fails, so a body exactly `limit` long is read whole.
fn read_up_to(body: impl Read, limit: u64) -> Result<Vec<u8>, Unread> {
    let mut read = Vec::new();
    let mut taken = body.take(limit);
    taken.read_to_end(&mut read).map_err(Unread::Failed)?;
    let mut probe = Vec::new();
    taken
        .into_inner()
        .take(1)
        .read_to_end(&mut probe)
        .map_err(Unread::Failed)?;
    if probe.is_empty() {
        Ok(read)
    } else {
        Err(Unread::OverLimit(read))
    }
}

/// RFC 9110 §10.2.3: the instant a `Retry-After` names, never before `received`. `None` when absent,
/// repeated or unparseable, a negative value included, or past what `DateTime` holds.
fn retry_after(headers: &Headers, received: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let mut values = headers
        .pairs()
        .iter()
        .filter(|(name, _)| name == RETRY_AFTER)
        .map(|(_, value)| value.trim());
    let (Some(value), None) = (values.next(), values.next()) else {
        return None;
    };
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        let seconds = i64::try_from(value.parse::<u64>().ok()?).ok()?;
        return received.checked_add_signed(TimeDelta::try_seconds(seconds)?);
    }
    HTTP_DATE_FORMATS
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(value, format).ok())
        .map(|date| date.and_utc().max(received))
}

/// What [`Wire::exchange`] knew when the conversation failed.
struct Failed {
    kind: TransportKind,
    status: Option<u16>,
    headers: Headers,
    cut: Option<Vec<u8>>,
    detail: String,
}

#[cfg(test)]
mod tests {
    use super::{
        retry_after, Credentials, Headers, RawResponse, SendFailure, Timeouts, TransportError,
        TransportKind, TransportProfile, Wire, USER_AGENT,
    };
    use crate::budget::{BudgetError, RequestBudget};
    use crate::clock::testing::{StepClock, WaitRecorder, WaitedClock};
    use crate::clock::Clock;
    use crate::collect::Payload;
    use crate::ledger::tests::{at, Recorder};
    use crate::ledger::{AttemptSlot, SendPermit};
    use crate::provider::Provenance;
    use crate::request::{
        CredentialRef, HeaderName, HeaderValue, Request, RequestPhase, RunId, SourceId,
        TargetWindow,
    };
    use canon::FileDigest;
    use std::io::{ErrorKind, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

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
    /// `<store>/pace/<source>.per-ip`, the per-IP layout `BudgetScope::file_name` states.
    const PACE_FILE: &str = "pace/edgar_press.per-ip";
    /// `RESERVED_AT` plus `TEST_INTERVAL`, in the form the pace file is written in.
    const RESERVED_UNTIL: &str = "2026-11-10T13:00:00.100000Z";
    const NOT_AN_INSTANT: &str = "soon";
    const HOST_INTERVAL: Duration = Duration::from_secs(1);
    /// `HOST_INTERVAL` less `ELAPSED`: what a second send to one host owes it.
    const HOST_WAIT: Duration = Duration::from_millis(940);
    /// `RESERVED_AT` plus `HOST_INTERVAL`, which is where a host wait taken in full ends.
    const AFTER_THE_HOST_WAIT: &str = "2026-11-10T13:00:01Z";
    /// The per-IP slot reserved at `AFTER_THE_HOST_WAIT`, in the pace file's form.
    const IP_RESERVED_AFTER_THE_HOST_WAIT: &str = "2026-11-10T13:00:01.100000Z";
    /// A second loopback host that refuses at once, and the same host as a url may shout it.
    const LOCALHOST: &str = "http://localhost:1/never";
    const LOCALHOST_SHOUTED: &str = "http://LOCALHOST:1/never";
    const NO_HOST: &str = "/never";
    /// `http::Uri` keeps an IPv6 literal's brackets in its host, which no pace-file key admits.
    const IPV6_LOOPBACK: &str = "http://[::1]:1/never";
    const IPV6_HOST: &str = "[::1]";
    /// A space is no url `http::Uri` parses, so ureq refuses it as a transport error.
    const UNPARSEABLE: &str = "http://local host:1/never";
    const PACE_DIR: &str = "pace";
    const SOURCE_PACE_FILE: &str = "edgar_press.per-ip";
    const HOST_PACE_FILE: &str = "localhost.per-host";
    const ANOTHER_SOURCE: &str = "another_source";
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
                per_host: None,
            },
        }
    }

    /// The same profile with a per-host pace ten times the per-IP one, as a crawl delay is.
    fn polite_profile() -> TransportProfile {
        let mut polite = profile();
        polite.budget.per_host = Some(HOST_INTERVAL);
        polite
    }

    fn request() -> Request {
        Request {
            source: SourceId::new(SOURCE),
            target_window: TargetWindow::new(WINDOW),
            phase: RequestPhase::Index,
            provenance: Provenance::LiveIngest,
            url: REFUSED.to_owned(),
            universe_version: FileDigest::of(ROSTER),
            listed_date: None,
            hosts_version: None,
            extract_version: None,
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
        send_to(wire, clock, ordinal, REFUSED)
    }

    fn send_to(wire: &Wire, clock: &dyn Clock, ordinal: u32, url: &str) -> Checked {
        let mut sink = Recorder::default();
        let Some(permit) = permit(&mut sink, ordinal) else {
            return Err("no permit".into());
        };
        let asked = Request {
            url: url.to_owned(),
            ..request()
        };
        match wire.send(permit, &asked, clock) {
            Err(SendFailure::Transport(_)) => Ok(()),
            other => Err(format!("{other:?}").into()),
        }
    }

    fn polite_wire(store: &Path, source: &str, waits: &WaitRecorder) -> Wire {
        Wire::with_sleeper(
            polite_profile(),
            held(),
            store,
            &SourceId::new(source),
            Box::new(waits.clone()),
        )
    }

    /// Per send: the host reservation, the IP reservation after the host wait, both instants.
    fn polite_clock(sends: &[(&str, &str)]) -> StepClock {
        let reads: Vec<&str> = sends
            .iter()
            .flat_map(|(host, ip)| [*host, *ip, REQUESTED_AT, RECEIVED_AT])
            .collect();
        StepClock::over(&reads)
    }

    fn polite_refusal(
        store: &Path,
        waits: &WaitRecorder,
        url: &str,
    ) -> Result<Option<SendFailure>, Box<dyn std::error::Error>> {
        let mut sink = Recorder::default();
        let Some(permit) = permit(&mut sink, FIRST_ATTEMPT) else {
            return Err("no permit".into());
        };
        let asked = Request {
            url: url.to_owned(),
            ..request()
        };
        Ok(polite_wire(store, SOURCE, waits)
            .send(permit, &asked, &polite_clock(&[(RESERVED_AT, RESERVED_AT)]))
            .err())
    }

    /// Two sends through one polite `Wire`, 60 ms apart, the second paying its host wait in full.
    fn two_polite_sends(store: &Path, waits: &WaitRecorder, first: &str, second: &str) -> Checked {
        let wire = polite_wire(store, SOURCE, waits);
        let clock = polite_clock(&[
            (RESERVED_AT, RESERVED_AT),
            (SIXTY_MS_LATER, AFTER_THE_HOST_WAIT),
        ]);
        send_to(&wire, &clock, FIRST_ATTEMPT, first)?;
        send_to(&wire, &clock, SECOND_ATTEMPT, second)
    }

    fn held() -> Credentials {
        Credentials::default().with(CONTACT, CONTACT_ADDRESS)
    }

    fn slot(sink: &mut Recorder) -> Option<AttemptSlot> {
        slot_at(sink, FIRST_ATTEMPT)
    }

    fn slot_at(sink: &mut Recorder, ordinal: u32) -> Option<AttemptSlot> {
        let run = RunId::new(RUN);
        AttemptSlot::open(sink, &request(), None, &run, ordinal, at(OPENED_AT)).ok()
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

    /// Waits in order: host then IP for the first send, then for the second.
    #[test]
    fn a_second_send_to_one_host_waits_out_the_host_pace_and_not_only_the_ip_one() -> Checked {
        let store = tempfile::tempdir()?;
        let waits = WaitRecorder::default();
        two_polite_sends(store.path(), &waits, LOCALHOST, LOCALHOST)?;
        assert_eq!(waits.waits(), vec![NO_WAIT, NO_WAIT, HOST_WAIT, NO_WAIT]);
        Ok(())
    }

    #[test]
    fn a_send_to_another_host_owes_only_the_ip_pace() -> Checked {
        let store = tempfile::tempdir()?;
        let waits = WaitRecorder::default();
        let wire = polite_wire(store.path(), SOURCE, &waits);
        let clock = polite_clock(&[(RESERVED_AT, RESERVED_AT), (SIXTY_MS_LATER, SIXTY_MS_LATER)]);
        send_to(&wire, &clock, FIRST_ATTEMPT, REFUSED)?;
        send_to(&wire, &clock, SECOND_ATTEMPT, LOCALHOST)?;
        assert_eq!(
            waits.waits(),
            vec![NO_WAIT, NO_WAIT, NO_WAIT, EXPECTED_WAIT]
        );
        Ok(())
    }

    /// Measured 2026-11-14: on a `StepClock`, reserving both slots and then sleeping both waits
    /// kept the whole suite green, because a scripted read answers the same whatever came before.
    #[test]
    fn the_ip_slot_is_reserved_after_the_host_wait_and_not_before_it() -> Checked {
        let store = tempfile::tempdir()?;
        let clock = WaitedClock::at(RESERVED_AT)?;
        let wire = Wire::with_sleeper(
            polite_profile(),
            held(),
            store.path(),
            &SourceId::new(SOURCE),
            Box::new(clock.clone()),
        );
        send_to(&wire, &clock, FIRST_ATTEMPT, LOCALHOST)?;
        clock.elapse(ELAPSED);
        send_to(&wire, &clock, SECOND_ATTEMPT, LOCALHOST)?;
        assert_eq!(
            (
                clock.waits(),
                std::fs::read_to_string(store.path().join(PACE_FILE))?
            ),
            (
                vec![NO_WAIT, NO_WAIT, HOST_WAIT, NO_WAIT],
                IP_RESERVED_AFTER_THE_HOST_WAIT.to_owned()
            )
        );
        Ok(())
    }

    /// The file NAME is asserted, not only the waits: on a case-insensitive disk (APFS by default)
    /// an unlowercased key passed the waits alone, measured 2026-11-14.
    #[test]
    fn one_host_spelled_in_two_cases_is_one_bucket_under_its_lowercase_name() -> Checked {
        let store = tempfile::tempdir()?;
        let waits = WaitRecorder::default();
        two_polite_sends(store.path(), &waits, LOCALHOST_SHOUTED, LOCALHOST)?;
        let mut files: Vec<String> = std::fs::read_dir(store.path().join(PACE_DIR))?
            .map(|entry| entry.map(|found| found.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_, _>>()?;
        files.sort();
        assert_eq!(
            (waits.waits(), files),
            (
                vec![NO_WAIT, NO_WAIT, HOST_WAIT, NO_WAIT],
                vec![SOURCE_PACE_FILE.to_owned(), HOST_PACE_FILE.to_owned()]
            )
        );
        Ok(())
    }

    /// Two sources hold two per-IP files, so only a host file keyed on the host alone makes the
    /// second source wait.
    #[test]
    fn two_sources_asking_one_host_share_its_pace() -> Checked {
        let store = tempfile::tempdir()?;
        let (first, second) = (WaitRecorder::default(), WaitRecorder::default());
        send_to(
            &polite_wire(store.path(), SOURCE, &first),
            &polite_clock(&[(RESERVED_AT, RESERVED_AT)]),
            FIRST_ATTEMPT,
            LOCALHOST,
        )?;
        send_to(
            &polite_wire(store.path(), ANOTHER_SOURCE, &second),
            &polite_clock(&[(SIXTY_MS_LATER, AFTER_THE_HOST_WAIT)]),
            FIRST_ATTEMPT,
            LOCALHOST,
        )?;
        assert_eq!(second.waits(), vec![HOST_WAIT, NO_WAIT]);
        Ok(())
    }

    #[test]
    fn a_per_host_pace_on_a_url_with_no_host_refuses_before_any_wait() -> Checked {
        let store = tempfile::tempdir()?;
        let waits = WaitRecorder::default();
        let refused = polite_refusal(store.path(), &waits, NO_HOST)?;
        assert_eq!(
            (refused, waits.waits()),
            (
                Some(SendFailure::Budget(BudgetError::NoHost {
                    url: NO_HOST.to_owned()
                })),
                Vec::new()
            )
        );
        Ok(())
    }

    #[test]
    fn a_per_host_pace_on_a_host_no_pace_file_can_be_named_after_refuses_before_any_wait() -> Checked
    {
        let store = tempfile::tempdir()?;
        let waits = WaitRecorder::default();
        let refused = polite_refusal(store.path(), &waits, IPV6_LOOPBACK)?;
        assert_eq!(
            (refused, waits.waits()),
            (
                Some(SendFailure::Budget(BudgetError::NotAHostKey {
                    host: IPV6_HOST.to_owned()
                })),
                Vec::new()
            )
        );
        Ok(())
    }

    type Server = JoinHandle<std::io::Result<()>>;

    const OK_HEAD: &str = "200 OK";
    const MOVED_HEAD: &str = "301 Moved Permanently\r\nLocation: /elsewhere";

    /// One loopback connection answered with `head`, declaring `declared` bytes and writing
    /// `written`.
    fn serve_once(
        head: &'static str,
        declared: usize,
        written: usize,
    ) -> Result<(String, Server), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}/page", listener.local_addr()?);
        let served = thread::spawn(move || -> std::io::Result<()> {
            let (mut stream, _peer) = listener.accept()?;
            let mut asked = Vec::new();
            let mut byte = [0_u8; 1];
            while !asked.ends_with(b"\r\n\r\n") && stream.read(&mut byte)? == 1 {
                asked.extend_from_slice(&byte);
            }
            let head = format!(
                "HTTP/1.1 {head}\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(head.as_bytes())?;
            stream.write_all(&vec![b'x'; written])
        });
        Ok((url, served))
    }

    /// The body length read, or the kind of the transport failure and the status it kept.
    type Received = Result<usize, (TransportKind, Option<u16>)>;

    fn sent_to(url: &str) -> Result<Received, Box<dyn std::error::Error>> {
        let store = tempfile::tempdir()?;
        let mut sink = Recorder::default();
        let Some(permit) = permit(&mut sink, FIRST_ATTEMPT) else {
            return Err("no permit".into());
        };
        let asked = Request {
            url: url.to_owned(),
            ..request()
        };
        let clock = clock_over(&[RESERVED_AT]);
        match wire(held(), store.path(), &WaitRecorder::default()).send(permit, &asked, &clock) {
            Ok(answer) => Ok(Ok(answer.body().len())),
            Err(SendFailure::Transport(failed)) => Ok(Err((failed.kind(), failed.status()))),
            Err(other) => Err(format!("{other:?}").into()),
        }
    }

    /// ureq 3.5.2 reads `body_limit` bytes, then probes one more; only a byte past it fails.
    #[test]
    fn a_body_over_the_limit_fails_as_its_own_kind_and_one_at_the_limit_is_read() -> Checked {
        let at_limit = usize::try_from(TEST_BODY_LIMIT)?;
        for (declared, written, expected) in [
            (at_limit, at_limit, Ok(at_limit)),
            (
                at_limit + 1,
                at_limit + 1,
                Err((TransportKind::BodyOverLimit, Some(OK_STATUS))),
            ),
            (
                at_limit,
                1,
                Err((TransportKind::Conversation, Some(OK_STATUS))),
            ),
        ] {
            let (url, served) = serve_once(OK_HEAD, declared, written)?;
            let answer = sent_to(&url)?;
            assert_eq!(
                (answer, matches!(served.join(), Ok(Ok(())))),
                (expected, true),
                "{written} of {declared} bytes"
            );
        }
        assert_eq!(sent_to(REFUSED)?, Err((TransportKind::Conversation, None)));
        Ok(())
    }

    /// RFC 9110's 3xx codes but 305 and 306, each with a `Location` but 304, which carries none.
    const EVERY_REDIRECT: [(&str, u16); 7] = [
        ("300 Multiple Choices\r\nLocation: /elsewhere", 300),
        ("301 Moved Permanently\r\nLocation: /elsewhere", 301),
        ("302 Found\r\nLocation: /elsewhere", 302),
        ("303 See Other\r\nLocation: /elsewhere", 303),
        ("304 Not Modified", 304),
        ("307 Temporary Redirect\r\nLocation: /elsewhere", 307),
        ("308 Permanent Redirect\r\nLocation: /elsewhere", 308),
    ];

    /// What both providers' `a_redirect_is_permanent_never_transient` assume: every 3xx above
    /// reaches `classify` as an answer, over one connection.
    #[test]
    fn every_redirect_is_answered_as_itself() -> Checked {
        for (head, status) in EVERY_REDIRECT {
            let (url, served) = serve_once(head, 1, 1)?;
            let store = tempfile::tempdir()?;
            let mut sink = Recorder::default();
            let Some(permit) = permit(&mut sink, FIRST_ATTEMPT) else {
                return Err("no permit".into());
            };
            let asked = Request { url, ..request() };
            let clock = clock_over(&[RESERVED_AT]);
            let answer = wire(held(), store.path(), &WaitRecorder::default())
                .send(permit, &asked, &clock)
                .map(|answer| answer.status());
            assert_eq!(
                (answer, matches!(served.join(), Ok(Ok(())))),
                (Ok(status), true)
            );
        }
        Ok(())
    }

    /// A 3xx is the answer and its body is read under the same limit; the failure
    /// keeps the 301, which the held read's 3xx exclusion keys on (architecture.md section 11.1).
    #[test]
    fn a_redirect_whose_body_is_over_the_limit_fails_with_the_status_its_head_carried() -> Checked {
        let over = usize::try_from(TEST_BODY_LIMIT)? + 1;
        let (url, served) = serve_once(MOVED_HEAD, over, over)?;
        let answer = sent_to(&url)?;
        assert_eq!(
            (answer, matches!(served.join(), Ok(Ok(())))),
            (Err((TransportKind::BodyOverLimit, Some(MOVED))), true)
        );
        Ok(())
    }

    /// The run goes on: `collect` records a `Transport` failure on its attempt row and aborts on
    /// a `Budget` one.
    #[test]
    fn a_url_that_does_not_parse_skips_the_host_wait_and_fails_as_a_transport_error() -> Checked {
        let store = tempfile::tempdir()?;
        let waits = WaitRecorder::default();
        let refused = polite_refusal(store.path(), &waits, UNPARSEABLE)?;
        assert_eq!(
            (
                matches!(refused, Some(SendFailure::Transport(_))),
                waits.waits()
            ),
            (true, vec![NO_WAIT]),
            "{refused:?}"
        );
        Ok(())
    }

    /// The host file holds the instant last granted, so the 1 s source counts its own interval
    /// from the 100 ms source's send; holding `go_at + interval` let the last writer decide.
    #[test]
    fn a_longer_host_interval_is_counted_from_a_shorter_one_s_send() -> Checked {
        let store = tempfile::tempdir()?;
        let (brisk, polite) = (WaitRecorder::default(), WaitRecorder::default());
        let mut brisk_profile = profile();
        brisk_profile.budget.per_host = Some(TEST_INTERVAL);
        let brisk_wire = Wire::with_sleeper(
            brisk_profile,
            held(),
            store.path(),
            &SourceId::new(ANOTHER_SOURCE),
            Box::new(brisk.clone()),
        );
        send_to(
            &brisk_wire,
            &polite_clock(&[(RESERVED_AT, RESERVED_AT)]),
            FIRST_ATTEMPT,
            LOCALHOST,
        )?;
        send_to(
            &polite_wire(store.path(), SOURCE, &polite),
            &polite_clock(&[(SIXTY_MS_LATER, AFTER_THE_HOST_WAIT)]),
            FIRST_ATTEMPT,
            LOCALHOST,
        )?;
        assert_eq!(polite.waits(), vec![HOST_WAIT, NO_WAIT]);
        Ok(())
    }

    #[test]
    fn a_host_pace_file_holding_a_value_this_crate_did_not_write_refuses_before_any_wait() -> Checked
    {
        let store = tempfile::tempdir()?;
        let pace_dir = store.path().join(PACE_DIR);
        std::fs::create_dir_all(&pace_dir)?;
        let pace = pace_dir.join(HOST_PACE_FILE);
        std::fs::write(&pace, NOT_AN_INSTANT)?;
        let waits = WaitRecorder::default();
        let refused = polite_refusal(store.path(), &waits, LOCALHOST)?;
        assert_eq!(
            (refused, waits.waits()),
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

    const REDIRECTED: &str = "/page";
    const REDIRECT_TARGET: &str = "/elsewhere";
    const MOVED: u16 = 301;
    const MOVED_BODY: &[u8] = b"moved to /elsewhere";
    const ARRIVED_BODY: &[u8] = b"arrived";
    /// How often the non-blocking accept loop looks again, and how long it waits in all.
    const ACCEPT_POLL: Duration = Duration::from_millis(5);
    const SERVER_DEADLINE: Duration = Duration::from_secs(5);

    /// The request targets a loopback server saw, in order.
    type Targets = Vec<String>;
    type RedirectServer = JoinHandle<std::io::Result<Targets>>;
    /// Status, body, and the targets the server saw.
    type Redirected = (u16, Vec<u8>, Targets);

    /// Answers `/page` with a 301 to `/elsewhere` and anything else with 200, one connection at a
    /// time, until `stop` or `SERVER_DEADLINE`, so a mutation that sends nothing cannot hang it.
    fn serve_redirects(
        stop: Arc<AtomicBool>,
    ) -> Result<(String, RedirectServer), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("http://{}{REDIRECTED}", listener.local_addr()?);
        let served = thread::spawn(move || -> std::io::Result<Targets> {
            let deadline = Instant::now() + SERVER_DEADLINE;
            let mut targets = Vec::new();
            while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _peer)) => targets.push(answer_one(stream)?),
                    Err(why) if why.kind() == ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
                    Err(why) => return Err(why),
                }
            }
            Ok(targets)
        });
        Ok((url, served))
    }

    /// Reads one request head and answers it; returns the request line's target.
    fn answer_one(mut stream: TcpStream) -> std::io::Result<String> {
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(TEST_TIMEOUT))?;
        let mut asked = Vec::new();
        let mut byte = [0_u8; 1];
        while !asked.ends_with(b"\r\n\r\n") && stream.read(&mut byte)? == 1 {
            asked.extend_from_slice(&byte);
        }
        let target = String::from_utf8_lossy(&asked)
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        let (head, body) = if target == REDIRECTED {
            (
                format!("301 Moved Permanently\r\nLocation: {REDIRECT_TARGET}"),
                MOVED_BODY,
            )
        } else {
            ("200 OK".to_owned(), ARRIVED_BODY)
        };
        let len = body.len();
        stream.write_all(
            format!("HTTP/1.1 {head}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )?;
        stream.write_all(body)?;
        Ok(target)
    }

    /// One send through a `Wire`: the answer, and every request target the loopback server saw.
    /// The pace waits go to `waits`.
    fn send_redirected(waits: &WaitRecorder) -> Result<Redirected, Box<dyn std::error::Error>> {
        let stop = Arc::new(AtomicBool::new(false));
        let (url, served) = serve_redirects(Arc::clone(&stop))?;
        let store = tempfile::tempdir()?;
        let mut sink = Recorder::default();
        let Some(permit) = permit(&mut sink, FIRST_ATTEMPT) else {
            return Err("no permit".into());
        };
        let asked = Request { url, ..request() };
        let wire = Wire::with_sleeper(
            profile(),
            held(),
            store.path(),
            &SourceId::new(SOURCE),
            Box::new(waits.clone()),
        );
        let answer = wire.send(permit, &asked, &clock_over(&[RESERVED_AT]));
        stop.store(true, Ordering::SeqCst);
        let Ok(targets) = served.join() else {
            return Err("the loopback server panicked".into());
        };
        let answer = answer?;
        Ok((answer.status(), answer.body().to_vec(), targets?))
    }

    /// §8 row R19 (redirects): the 3xx is the answer, the hop it names is never sent and the pace
    /// is reserved once. Mutation 2026-11-16, `MAX_REDIRECTS` 1: red, the server saw two targets.
    #[test]
    fn a_redirect_is_answered_as_its_3xx_and_its_target_never_requested() -> Checked {
        let waits = WaitRecorder::default();
        assert_eq!(
            (send_redirected(&waits)?, waits.waits()),
            (
                (MOVED, MOVED_BODY.to_vec(), vec![REDIRECTED.to_owned()]),
                vec![NO_WAIT]
            )
        );
        Ok(())
    }

    type HeadServer = JoinHandle<std::io::Result<String>>;

    /// One loopback answer: status 200, `head`'s lines, then `body`. Hands back the request head.
    fn serve_bytes(
        head: Vec<u8>,
        body: Vec<u8>,
    ) -> Result<(String, HeadServer), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}{REDIRECTED}", listener.local_addr()?);
        let served = thread::spawn(move || -> std::io::Result<String> {
            let (mut stream, _peer) = listener.accept()?;
            let mut asked = Vec::new();
            let mut byte = [0_u8; 1];
            while !asked.ends_with(b"\r\n\r\n") && stream.read(&mut byte)? == 1 {
                asked.extend_from_slice(&byte);
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\n")?;
            stream.write_all(&head)?;
            let len = body.len();
            stream.write_all(
                format!("Content-Length: {len}\r\nConnection: close\r\n\r\n").as_bytes(),
            )?;
            stream.write_all(&body)?;
            Ok(String::from_utf8_lossy(&asked).into_owned())
        });
        Ok((url, served))
    }

    type Answered = Result<RawResponse, TransportError>;

    /// The answer to one send to `url`, and the request head the server read.
    fn exchanged(
        url: String,
        served: HeadServer,
    ) -> Result<(Answered, String), Box<dyn std::error::Error>> {
        let store = tempfile::tempdir()?;
        let mut sink = Recorder::default();
        let Some(permit) = permit(&mut sink, FIRST_ATTEMPT) else {
            return Err("no permit".into());
        };
        let asked = Request { url, ..request() };
        let clock = clock_over(&[RESERVED_AT]);
        let answer = match wire(held(), store.path(), &WaitRecorder::default())
            .send(permit, &asked, &clock)
        {
            Ok(answer) => Ok(answer),
            Err(SendFailure::Transport(failed)) => Err(failed),
            Err(other) => return Err(format!("{other:?}").into()),
        };
        let Ok(head) = served.join() else {
            return Err("the loopback server panicked".into());
        };
        Ok((answer, head?))
    }

    fn pairs(of: &[(&str, &str)]) -> Vec<(String, String)> {
        of.iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// `PLAIN_PAGE` compressed by Python 3.14.7 `gzip.compress(page, compresslevel=9, mtime=0)`.
    const GZIPPED_PAGE: [u8; 104] = [
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0x2d, 0xcc, 0xd1, 0x09, 0x80,
        0x20, 0x14, 0x05, 0xd0, 0xff, 0xa6, 0x78, 0x03, 0x54, 0x2e, 0xf0, 0x10, 0x5a, 0x20, 0xa2,
        0x0d, 0x2c, 0x2f, 0x26, 0x58, 0x8a, 0x4f, 0x92, 0x9a, 0x3e, 0xa8, 0xfe, 0x0f, 0x87, 0xb7,
        0xb2, 0x07, 0xcd, 0x4b, 0xb4, 0x97, 0xe6, 0xa4, 0x07, 0xca, 0x08, 0x30, 0x02, 0x12, 0xe4,
        0x13, 0x96, 0xdc, 0xed, 0x53, 0x87, 0x63, 0x8d, 0x16, 0xb6, 0x25, 0x23, 0x34, 0xcd, 0x34,
        0xa2, 0x4a, 0xf5, 0xf9, 0x37, 0x42, 0xbe, 0x08, 0x25, 0xe3, 0x20, 0x3d, 0xab, 0xa4, 0x59,
        0x7d, 0x99, 0x7a, 0xe7, 0xe6, 0x01, 0x80, 0x77, 0x54, 0x23, 0x61, 0x00, 0x00, 0x00,
    ];
    const PLAIN_PAGE: &[u8] =
        b"<html><body><p>A release served gzip-encoded, as PR Newswire serves its pages.</p></body></html>\n";
    /// Measured 2026-11-16 at e2ed6d4 (ureq 3.5.2, `gzip` feature on, before this change) by a
    /// temporary test sending `GZIPPED_PAGE` through this server: the body's sha256 and request head.
    const MAIN_BODY_SHA256: &str =
        "a2c552e149680b1c8865bd10a97b38c37541513f0bf8d4287361c996fbf0f096";
    const MAIN_REQUEST_HEAD: &str = "GET /page HTTP/1.1\r\naccept-encoding: gzip\r\nhost: HOST\r\n\
        user-agent: bellwether contact@example.test\r\naccept: text/plain\r\n\r\n";
    const GZIP_HEAD: &[u8] = b"Content-Type: text/html\r\nContent-Encoding: gzip\r\n";

    /// P9 keeps `Content-Encoding`, which ureq's `gzip` feature deletes, and the blob stays the body
    /// ureq decoded before this change, asked for with the same request head.
    #[test]
    fn a_gzip_body_is_decoded_to_the_digest_ureq_gave_it_and_keeps_its_content_encoding() -> Checked
    {
        let (url, served) = serve_bytes(GZIP_HEAD.to_vec(), GZIPPED_PAGE.to_vec())?;
        let host = url
            .trim_start_matches("http://")
            .trim_end_matches(REDIRECTED)
            .to_owned();
        let (answer, head) = exchanged(url, served)?;
        let answer = answer.map_err(|failed| format!("{failed:?}"))?;
        assert_eq!(
            (
                answer.body(),
                FileDigest::of(answer.body()).hex(),
                answer.headers().pairs(),
                head
            ),
            (
                PLAIN_PAGE,
                MAIN_BODY_SHA256,
                pairs(&[("content-type", "text/html"), ("content-encoding", "gzip")]).as_slice(),
                MAIN_REQUEST_HEAD.replace("HOST", &host)
            )
        );
        Ok(())
    }

    /// ureq 3.5.2 decodes with `MultiGzDecoder` (`body/gzip.rs`), which reads every member of the
    /// stream; `GzDecoder` would stop after the first.
    #[test]
    fn a_gzip_body_of_two_members_is_decoded_to_both() -> Checked {
        let (url, served) = serve_bytes(GZIP_HEAD.to_vec(), [GZIPPED_PAGE; 2].concat())?;
        let (answer, _head) = exchanged(url, served)?;
        let answer = answer.map_err(|failed| format!("{failed:?}"))?;
        assert_eq!(answer.body(), [PLAIN_PAGE; 2].concat().as_slice());
        Ok(())
    }

    /// ureq 3.5.2 decodes `gzip` alone, matched exactly; any other coding is stored as received.
    #[test]
    fn a_coding_ureq_did_not_decode_is_stored_as_received() -> Checked {
        let (url, served) = serve_bytes(
            b"Content-Encoding: GZIP\r\n".to_vec(),
            GZIPPED_PAGE.to_vec(),
        )?;
        let (answer, _head) = exchanged(url, served)?;
        let answer = answer.map_err(|failed| format!("{failed:?}"))?;
        assert_eq!(
            (answer.body(), answer.headers().pairs()),
            (
                GZIPPED_PAGE.as_slice(),
                pairs(&[("content-encoding", "GZIP")]).as_slice()
            )
        );
        Ok(())
    }

    /// `Server`, `Set-Cookie`, `Location` and `X-Other` are not kept; a repeated `Vary` is.
    const MIXED_HEAD: &[u8] = b"Date: Mon, 16 Nov 2026 09:42:00 GMT\r\nServer: nginx\r\n\
        Content-Type: text/html; charset=utf-8\r\nSet-Cookie: a=b\r\nVary: Accept-Encoding\r\n\
        ETag: \"caf\xe9\"\r\nVARY: User-Agent\r\nLast-Modified: Sun, 15 Nov 2026 10:00:00 GMT\r\n\
        Cache-Control: max-age=60\r\nLocation: /elsewhere\r\nX-Other: 1\r\nRetry-After: 120\r\n";

    #[test]
    fn only_the_allowlisted_headers_are_kept_lowercase_with_every_repeat() -> Checked {
        let (url, served) = serve_bytes(MIXED_HEAD.to_vec(), b"page".to_vec())?;
        let (answer, _head) = exchanged(url, served)?;
        let answer = answer.map_err(|failed| format!("{failed:?}"))?;
        assert_eq!(
            answer.headers().pairs(),
            pairs(&[
                ("date", "Mon, 16 Nov 2026 09:42:00 GMT"),
                ("content-type", "text/html; charset=utf-8"),
                ("vary", "Accept-Encoding"),
                ("vary", "User-Agent"),
                ("etag", "\"caf\u{e9}\""),
                ("last-modified", "Sun, 15 Nov 2026 10:00:00 GMT"),
                ("cache-control", "max-age=60"),
                ("retry-after", "120"),
            ])
            .as_slice()
        );
        Ok(())
    }

    #[test]
    fn the_stored_headers_are_one_json_array_of_name_value_pairs() {
        let headers = Headers::new(pairs(&[("etag", "\"caf\u{e9}\""), ("vary", "a")]));
        assert_eq!(
            headers.to_json(),
            "[[\"etag\",\"\\\"caf\u{e9}\\\"\"],[\"vary\",\"a\"]]"
        );
    }

    /// P10's over-limit rule: the first `body_limit` bytes and the head stay with the failure.
    #[test]
    fn a_body_over_the_limit_keeps_its_first_limit_bytes_and_its_headers() -> Checked {
        let limit = usize::try_from(TEST_BODY_LIMIT)?;
        let mut body = vec![b'a'; limit];
        body.push(b'z');
        let (url, served) = serve_bytes(b"Content-Type: text/html\r\n".to_vec(), body)?;
        let (answer, _head) = exchanged(url, served)?;
        let Err(failed) = answer else {
            return Err("a body over the limit was read whole".into());
        };
        assert_eq!(
            (
                failed.kind(),
                failed.status(),
                failed.cut_body(),
                failed.headers().pairs()
            ),
            (
                TransportKind::BodyOverLimit,
                Some(OK_STATUS),
                Some(vec![b'a'; limit].as_slice()),
                pairs(&[("content-type", "text/html")]).as_slice()
            )
        );
        Ok(())
    }

    /// ureq 3.5.2's `BodyReader::new` puts its `LimitReader` after the decoder, so the limit
    /// counts decoded bytes; a small gzip stream over the limit once decoded is cut decoded.
    #[test]
    fn a_gzip_body_over_the_limit_once_decoded_is_cut_at_the_limit_decoded() -> Checked {
        let limit = usize::try_from(TEST_BODY_LIMIT)?;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&vec![b'x'; limit * 2])?;
        let gzipped = encoder.finish()?;
        let (url, served) = serve_bytes(GZIP_HEAD.to_vec(), gzipped.clone())?;
        let (answer, _head) = exchanged(url, served)?;
        let Err(failed) = answer else {
            return Err("a body over the limit was read whole".into());
        };
        assert_eq!(
            (gzipped.len() < limit, failed.kind(), failed.cut_body()),
            (
                true,
                TransportKind::BodyOverLimit,
                Some(vec![b'x'; limit].as_slice())
            )
        );
        Ok(())
    }

    type AnswerServer = JoinHandle<std::io::Result<usize>>;

    /// Answers one connection per entry of `heads`, in order, each with an empty body, until all
    /// are answered or `SERVER_DEADLINE` passes, so a send that never comes cannot hang the suite.
    fn serve_answers(
        heads: Vec<String>,
    ) -> Result<(String, AnswerServer), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("http://{}{REDIRECTED}", listener.local_addr()?);
        let served = thread::spawn(move || -> std::io::Result<usize> {
            let deadline = Instant::now() + SERVER_DEADLINE;
            let mut answered = 0;
            while answered < heads.len() && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _peer)) => {
                        stream.set_nonblocking(false)?;
                        stream.set_read_timeout(Some(TEST_TIMEOUT))?;
                        let mut asked = Vec::new();
                        let mut byte = [0_u8; 1];
                        while !asked.ends_with(b"\r\n\r\n") && stream.read(&mut byte)? == 1 {
                            asked.extend_from_slice(&byte);
                        }
                        let head = heads.get(answered).cloned().unwrap_or_default();
                        let answer = format!(
                            "HTTP/1.1 {head}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                        stream.write_all(answer.as_bytes())?;
                        answered += 1;
                    }
                    Err(why) if why.kind() == ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
                    Err(why) => return Err(why),
                }
            }
            Ok(answered)
        });
        Ok((url, served))
    }

    /// One send to `url`: the status of an answer or of a failure's head, `None` for neither.
    fn status_of(
        wire: &Wire,
        clock: &dyn Clock,
        ordinal: u32,
        url: &str,
    ) -> Result<Option<u16>, Box<dyn std::error::Error>> {
        let mut sink = Recorder::default();
        let Some(permit) = permit(&mut sink, ordinal) else {
            return Err("no permit".into());
        };
        let asked = Request {
            url: url.to_owned(),
            ..request()
        };
        match wire.send(permit, &asked, clock) {
            Ok(answer) => Ok(Some(answer.status())),
            Err(SendFailure::Transport(failed)) => Ok(failed.status()),
            Err(other) => Err(format!("{other:?}").into()),
        }
    }

    fn answered_by(served: AnswerServer) -> Result<usize, Box<dyn std::error::Error>> {
        let Ok(answered) = served.join() else {
            return Err("the loopback server panicked".into());
        };
        Ok(answered?)
    }

    /// The loopback answers `heads` in order, 60 ms apart; then one send to `REFUSED`, which is the
    /// same host, `127.0.0.1`, and one to `LOCALHOST`, another host. Every wait, in order.
    fn waits_after(heads: &[&str]) -> Result<Vec<Duration>, Box<dyn std::error::Error>> {
        let store = tempfile::tempdir()?;
        let clock = WaitedClock::at(RESERVED_AT)?;
        let wire = Wire::with_sleeper(
            polite_profile(),
            held(),
            store.path(),
            &SourceId::new(SOURCE),
            Box::new(clock.clone()),
        );
        let (url, served) = serve_answers(heads.iter().map(|head| (*head).to_owned()).collect())?;
        let mut ordinal = FIRST_ATTEMPT;
        for _ in heads {
            status_of(&wire, &clock, ordinal, &url)?;
            clock.elapse(ELAPSED);
            ordinal += 1;
        }
        status_of(&wire, &clock, ordinal, REFUSED)?;
        status_of(&wire, &clock, ordinal + 1, LOCALHOST)?;
        if answered_by(served)? != heads.len() {
            return Err("the loopback server did not answer every send".into());
        }
        Ok(clock.waits())
    }

    const RETRY_AFTER_SECONDS: &str = "429 Too Many Requests\r\nRetry-After: 120";
    /// `RESERVED_AT` plus 5 minutes, as an IMF-fixdate.
    const RETRY_AFTER_DATE: &str =
        "503 Service Unavailable\r\nRetry-After: Tue, 10 Nov 2026 13:05:00 GMT";
    /// An hour before `RESERVED_AT`.
    const RETRY_AFTER_PAST: &str =
        "503 Service Unavailable\r\nRetry-After: Tue, 10 Nov 2026 12:00:00 GMT";
    const NO_RETRY_AFTER: &str = "429 Too Many Requests";
    /// Negative, a word, a fraction, and delta-seconds past `u64`.
    const UNUSABLE_RETRY_AFTERS: [&str; 4] = [
        "429 Too Many Requests\r\nRetry-After: -5",
        "429 Too Many Requests\r\nRetry-After: soon",
        "429 Too Many Requests\r\nRetry-After: 1.5",
        "429 Too Many Requests\r\nRetry-After: 99999999999999999999999",
    ];
    const SERVER_ERROR_WITH_RETRY_AFTER: &str = "500 Internal Server Error\r\nRetry-After: 120";
    /// 120 s after the answer at `RESERVED_AT`, less the 60 ms elapsed before the next send.
    const WAIT_OUT_120_S: Duration = Duration::from_millis(119_940);
    /// 13:05:00 less 13:00:00.060.
    const WAIT_OUT_THE_DATE: Duration = Duration::from_millis(299_940);
    /// The 1 s host interval doubled once (2 s) and twice (4 s), less the 60 ms elapsed each time.
    const DOUBLED_ONCE_WAIT: Duration = Duration::from_millis(1_940);
    const DOUBLED_TWICE_WAIT: Duration = Duration::from_millis(3_940);
    /// The per-IP interval, owed by a send to another host straight after the one before.
    const IP_WAIT: Duration = TEST_INTERVAL;
    const RETRY_AFTER_JSON: &str = "[[\"retry-after\",\"120\"]]";

    /// RFC 9110 §10.2.3 delta-seconds: the next send to the host waits until 120 s after the
    /// answer, and another host keeps its own pace.
    #[test]
    fn a_429_with_retry_after_seconds_holds_that_host_until_then_and_no_other() -> Checked {
        assert_eq!(
            waits_after(&[RETRY_AFTER_SECONDS])?,
            vec![NO_WAIT, NO_WAIT, WAIT_OUT_120_S, NO_WAIT, NO_WAIT, IP_WAIT]
        );
        Ok(())
    }

    #[test]
    fn a_503_with_an_http_date_holds_that_host_until_that_date() -> Checked {
        assert_eq!(
            waits_after(&[RETRY_AFTER_DATE])?,
            vec![
                NO_WAIT,
                NO_WAIT,
                WAIT_OUT_THE_DATE,
                NO_WAIT,
                NO_WAIT,
                IP_WAIT
            ]
        );
        Ok(())
    }

    /// A date already past is a wait of 0, and a usable value, so the interval is not doubled.
    #[test]
    fn a_retry_after_date_in_the_past_holds_nothing_and_doubles_nothing() -> Checked {
        assert_eq!(
            waits_after(&[RETRY_AFTER_PAST])?,
            vec![NO_WAIT, NO_WAIT, HOST_WAIT, NO_WAIT, NO_WAIT, IP_WAIT]
        );
        Ok(())
    }

    #[test]
    fn a_429_without_retry_after_doubles_the_host_interval_at_each_answer() -> Checked {
        assert_eq!(
            waits_after(&[NO_RETRY_AFTER, NO_RETRY_AFTER])?,
            vec![
                NO_WAIT,
                NO_WAIT,
                DOUBLED_ONCE_WAIT,
                NO_WAIT,
                DOUBLED_TWICE_WAIT,
                NO_WAIT,
                NO_WAIT,
                IP_WAIT
            ]
        );
        Ok(())
    }

    #[test]
    fn an_unparseable_or_negative_retry_after_is_absent_so_the_interval_doubles() -> Checked {
        for head in UNUSABLE_RETRY_AFTERS {
            assert_eq!(
                waits_after(&[head])?,
                vec![
                    NO_WAIT,
                    NO_WAIT,
                    DOUBLED_ONCE_WAIT,
                    NO_WAIT,
                    NO_WAIT,
                    IP_WAIT
                ],
                "{head}"
            );
        }
        Ok(())
    }

    /// Only 429 and 503 ask for less: a 500 carrying a `Retry-After` leaves the pace alone.
    #[test]
    fn a_500_is_no_slow_down_answer_even_with_a_retry_after() -> Checked {
        assert_eq!(
            waits_after(&[SERVER_ERROR_WITH_RETRY_AFTER])?,
            vec![NO_WAIT, NO_WAIT, HOST_WAIT, NO_WAIT, NO_WAIT, IP_WAIT]
        );
        Ok(())
    }

    /// P9: a 429's `Retry-After` reaches the `payloads` row's headers, `Payload::stored`'s output.
    #[test]
    fn a_429_s_retry_after_is_kept_in_the_payload_headers() -> Checked {
        let (url, served) = serve_answers(vec![RETRY_AFTER_SECONDS.to_owned()])?;
        let store = tempfile::tempdir()?;
        let mut sink = Recorder::default();
        let Some(permit) = permit(&mut sink, FIRST_ATTEMPT) else {
            return Err("no permit".into());
        };
        let asked = Request { url, ..request() };
        let clock = clock_over(&[RESERVED_AT]);
        let answer = match wire(held(), store.path(), &WaitRecorder::default())
            .send(permit, &asked, &clock)
        {
            Ok(answer) => Ok(answer),
            Err(SendFailure::Transport(failed)) => Err(failed),
            Err(other) => return Err(format!("{other:?}").into()),
        };
        let stored = Payload::stored(&asked, &answer, |_| Ok::<_, String>(String::new()))?;
        assert_eq!(
            (
                answered_by(served)?,
                stored.map(|payload| payload.headers.to_json())
            ),
            (1, Some(RETRY_AFTER_JSON.to_owned()))
        );
        Ok(())
    }

    /// RFC 9110 §5.6.7: a recipient reads all three HTTP-date forms; a past one is the answer's
    /// own instant, a wait of 0.
    #[test]
    fn a_retry_after_in_each_http_date_form_is_read_as_one_instant() -> Checked {
        let received: chrono::DateTime<chrono::Utc> = RESERVED_AT.parse()?;
        let expected: chrono::DateTime<chrono::Utc> = "1994-11-06T08:49:37Z".parse()?;
        let of = |value: &str, at| retry_after(&Headers::new(pairs(&[("retry-after", value)])), at);
        let read: Vec<_> = [
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
        ]
        .into_iter()
        .map(|value| of(value, expected))
        .collect();
        assert_eq!(
            (read, of("Sun, 06 Nov 1994 08:49:37 GMT", received)),
            (vec![Some(expected); 3], Some(received))
        );
        Ok(())
    }
}
