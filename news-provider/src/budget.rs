//! How fast a vendor may be called, and the cross-process file that holds it to that. One locked
//! file per bucket carries one wall instant (`BudgetScope::held` says which); a pace file's lock is
//! held for the read-reserve-write and never across the wait, so callers stack reservations
//! instead of serialising on each other's wait. A `.sent` file's lock is held across the gate's
//! wait (`SendGate::lock`) and waited for within a limit (`GATE_TURNS`).
//!
//! The shape is the predecessor's `sec_rate_coordinator.py`, written after the failure it
//! prevents: three processes on one residential IP, each with its own in-process throttle, whose
//! aggregate drew a traffic-threshold 403 (its epic #379). An in-process limiter cannot see the
//! other two, which is why this is a file and not a field.
//!
//! What it does and does not close: `docs/design/architecture.md` section 8 row R19.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, SubsecRound, TimeDelta, Utc};

use crate::clock::Sleeper;
use crate::request::SourceId;

/// Which bucket one pace file counts, and so which callers share it. A third arm, per-CREDENTIAL,
/// would follow the key across hosts with a second key as a second bucket; no vendor here has one,
/// so it is named and not written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetScope {
    /// Our outbound address, keyed per SOURCE (`<source>.per-ip`): every process collecting one
    /// source shares the bucket, and two sources on one address hold two (row R19).
    PerIp(SourceId),
    /// One remote host, and NOT the source: two sources asking one host share it.
    PerHost(HostKey),
}

/// A remote host as a pace-file name: lowercased, then only `[a-z0-9.-]`, not empty, no leading
/// or trailing dot, no empty label (architecture.md section 10, per-host pace). The field is
/// private, so no caller can name a file outside `pace/`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct HostKey(String);

impl HostKey {
    /// Upper case is normalised, not refused: a url's host is case-insensitive (RFC 3986 §3.2.2).
    pub fn new(host: &str) -> Result<Self, BudgetError> {
        let key = host.to_ascii_lowercase();
        let allowed =
            |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".-".contains(&byte);
        if key.is_empty()
            || key.starts_with('.')
            || key.ends_with('.')
            || key.contains("..")
            || !key.bytes().all(allowed)
        {
            return Err(BudgetError::NotAHostKey {
                host: host.to_owned(),
            });
        }
        Ok(Self(key))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A url's host as a [`HostKey`], by the one rule `Wire`'s host pace and the conformance harness's
/// keyable-host check share, and `ledger`'s latency rows. `Ok(None)` when `ureq::http::Uri` cannot
/// parse the url.
pub fn host_of(url: &str) -> Result<Option<HostKey>, BudgetError> {
    let Ok(uri) = url.parse::<ureq::http::Uri>() else {
        return Ok(None);
    };
    let host = uri.host().ok_or_else(|| BudgetError::NoHost {
        url: url.to_owned(),
    })?;
    HostKey::new(host).map(Some)
}

impl BudgetScope {
    /// The pace file's name, whose suffix keeps the two kinds of bucket off one file.
    fn file_name(&self) -> String {
        match self {
            Self::PerIp(source) => format!("{}.per-ip", source.as_str()),
            Self::PerHost(host) => format!("{}.per-host", host.0),
        }
    }

    /// The per-IP file holds the next allowed instant; the per-host file holds the instant last
    /// granted, so each caller adds its OWN `per_host` and a shorter one cannot shorten a longer.
    const fn held(&self) -> Held {
        match self {
            Self::PerIp(_) => Held::NextAllowed,
            Self::PerHost(_) => Held::LastGranted,
        }
    }
}

/// What the instant in one pace file means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    NextAllowed,
    LastGranted,
}

/// How fast one vendor may be called. Stated by the provider, like the timeouts beside it. A call
/// owing both paces waits out the host's and then our address's, then [`SendGate`] holds each
/// bucket's consecutive `requested_at` at least its interval apart (row R19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestBudget {
    /// The smallest gap between two calls leaving our address for this vendor, when the vendor
    /// counts one; `None` paces each host alone (owner decision 1, 2026-11-17, §10).
    pub min_interval: Option<Duration>,
    /// The gap between two calls to one remote host, when the vendor states one.
    pub per_host: Option<Duration>,
}

/// Why no call could be paced. Raised before the socket, so it is never a vendor event in the
/// coverage ledger.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum BudgetError {
    #[error("the pace file at {path} could not be {act}: {why}")]
    Unusable {
        path: PathBuf,
        act: &'static str,
        why: String,
    },
    /// Refused rather than reset: a value this crate did not write is either a torn write or
    /// another writer, and both answers to "when may I call" would be invented.
    #[error("the pace file at {path} holds {held:?} and not an instant this crate wrote")]
    Unreadable { path: PathBuf, held: String },
    /// A per-host pace and a url with no host to key it on: refused rather than left unpaced.
    #[error("{url} names no host to key a per-host pace on")]
    NoHost { url: String },
    /// A host that is not a safe pace-file name, such as an IPv6 literal: refused, not unpaced.
    #[error("{host} is not a host a per-host pace file can be named after")]
    NotAHostKey { host: String },
    /// A budget with neither a per-IP nor a per-host pace: refused, not sent unpaced.
    #[error("the profile states neither a per-IP nor a per-host pace")]
    NoPace,
    /// The send gate still owed a wait after this many tries (`SendGate`, row R19).
    #[error("the send gate still owed {owed:?} after {tries} tries")]
    NeverCleared { tries: u32, owed: Duration },
    /// Another sender kept a gate's lock past `GATE_TURNS` of the caller's interval, or past
    /// what was left of the caller's run: stopped and not dead, or waiting out a longer interval
    /// (row R19, hole 4).
    #[error("the send gate at {path} was still locked by another sender after {waited:?}")]
    GateHeld { path: PathBuf, waited: Duration },
    /// The run's deadline, and no other sender: the gate owed a wait longer than the run had
    /// left, or its lock was free and the deadline had passed before the stamp.
    #[error("the send gate owed {owed:?} and the run had {left:?} left before its deadline")]
    PastDeadline { owed: Duration, left: Duration },
}

impl BudgetError {
    /// The gate refused the send and no file is at fault: nothing was stamped and no socket was
    /// opened, so the caller records the request as not asked and may ask it again.
    #[must_use]
    pub const fn is_a_gate_refusal(&self) -> bool {
        matches!(
            self,
            Self::NeverCleared { .. } | Self::GateHeld { .. } | Self::PastDeadline { .. }
        )
    }

    /// The wait the gate owed when the run's deadline, and no other sender, refused the send.
    #[must_use]
    pub const fn owed_past_the_deadline(&self) -> Option<Duration> {
        match self {
            Self::PastDeadline { owed, .. } => Some(*owed),
            _ => None,
        }
    }
}

/// How many of its own interval a caller waits for one gate's lock. Not fitted: a live line of
/// `k` senders at one interval is `k` intervals long, so 32 leaves room for 31 ahead, or for one
/// holder doubled 4 times (architecture.md section 12.6, rule B1, written before any run).
const GATE_TURNS: u32 = 32;

/// The share of what is left of a run that one wait for a gate's lock may take: half, so a gate
/// another sender keeps leaves the hosts whose gates are free the other half. Not fitted
/// (architecture.md section 12.8, step 9b, decision D5, written before any run).
const LOCK_SHARE: u32 = 2;

/// The directory the pace files sit in, under the pace root the `Wire` is given.
pub(crate) const PACE_DIR: &str = "pace";

/// One bucket's next-allowed-call instant, on disk.
#[derive(Debug)]
pub struct Pace {
    path: PathBuf,
    min_interval: Duration,
    held: Held,
}

impl Pace {
    /// `<pace root>/pace/<source>.per-ip` or `<pace root>/pace/<host>.per-host`; `store` is the
    /// pace root, which is the store only when no other was named (architecture.md section 12.2).
    #[must_use]
    pub fn beside_the_store(store: &Path, scope: &BudgetScope, min_interval: Duration) -> Self {
        Self {
            path: store.join(PACE_DIR).join(scope.file_name()),
            min_interval,
            held: scope.held(),
        }
    }

    /// Reserves this caller's slot and answers how long it must wait before the socket. The lock
    /// is released before the answer comes back, so the wait is outside the critical section.
    pub fn reserve(&self, now: DateTime<Utc>) -> Result<Duration, BudgetError> {
        match self.reserve_within(now, None, None, self.min_interval)? {
            Reserved::Wait(wait) | Reserved::PastDeadline { wait, .. } => Ok(wait),
        }
    }

    /// As [`Pace::reserve`], with no slot granted before `floor` (a server's hold, row R19), and
    /// nothing written when the slot opens after `deadline` (condition A). `ordinary` is the
    /// profile's own interval, for when the slot opens with no floor and no doubling.
    pub fn reserve_within(
        &self,
        now: DateTime<Utc>,
        floor: Option<DateTime<Utc>>,
        deadline: Option<DateTime<Utc>>,
        ordinary: Duration,
    ) -> Result<Reserved, BudgetError> {
        let interval = self.delta(self.min_interval)?;
        let ordinary = self.delta(ordinary)?;
        let file = self.open()?;
        file.lock().map_err(|why| self.unusable("locked", &why))?;
        let reserved = self.read_reserve_write(&file, now, floor, interval, ordinary, deadline);
        let unlocked = file.unlock().map_err(|why| self.unusable("unlocked", &why));
        let reserved = reserved?;
        unlocked?;
        // Non-negative by the `max` in `read_reserve_write`; a negative delta has no `std` form.
        let wait = |at: DateTime<Utc>| (at - now).to_std().unwrap_or(Duration::ZERO);
        Ok(match reserved {
            Slot::Granted(go_at) => Reserved::Wait(wait(go_at)),
            Slot::Late { go_at, ordinary_at } => Reserved::PastDeadline {
                go_at,
                ordinary_at,
                wait: wait(go_at),
            },
        })
    }

    fn delta(&self, interval: Duration) -> Result<TimeDelta, BudgetError> {
        TimeDelta::from_std(interval).map_err(|why| self.unusable("read as an interval", &why))
    }

    fn open(&self) -> Result<File, BudgetError> {
        open_for_update(&self.path)
    }

    /// The critical section, and the only thing the lock is held for. No `fsync`: the predecessor
    /// decided against one for the reason that still holds, that a reservation left in the past by
    /// a restart collapses to `now` below, and the cost is iowait on the store's own disk.
    fn read_reserve_write(
        &self,
        mut file: &File,
        now: DateTime<Utc>,
        floor: Option<DateTime<Utc>>,
        interval: TimeDelta,
        ordinary: TimeDelta,
        deadline: Option<DateTime<Utc>>,
    ) -> Result<Slot, BudgetError> {
        let mut held = String::new();
        file.read_to_string(&mut held)
            .map_err(|why| self.unusable("read", &why))?;
        let prior = self.prior(&held)?;
        // A reservation already in the past is owed nothing: nobody used that slot.
        let go_at = self
            .next_allowed(prior, interval)?
            .map_or(now, |next| next.max(now))
            .max(floor.unwrap_or(now));
        if deadline.is_some_and(|deadline| go_at > deadline) {
            let ordinary_at = self
                .next_allowed(prior, ordinary)?
                .map_or(now, |next| next.max(now));
            return Ok(Slot::Late { go_at, ordinary_at });
        }
        let written = match self.held {
            Held::NextAllowed => self.advanced(go_at, interval)?,
            Held::LastGranted => go_at,
        };
        file.set_len(0)
            .map_err(|why| self.unusable("truncated", &why))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|why| self.unusable("rewound", &why))?;
        file.write_all(stamp(written).as_bytes())
            .map_err(|why| self.unusable("written", &why))?;
        Ok(Slot::Granted(go_at))
    }

    fn next_allowed(
        &self,
        prior: Option<DateTime<Utc>>,
        interval: TimeDelta,
    ) -> Result<Option<DateTime<Utc>>, BudgetError> {
        Ok(match (prior, self.held) {
            (Some(prior), Held::NextAllowed) => Some(prior),
            (Some(granted), Held::LastGranted) => Some(self.advanced(granted, interval)?),
            (None, _) => None,
        })
    }

    /// `None` only for an empty file, which is the first call through a freshly created one.
    fn prior(&self, held: &str) -> Result<Option<DateTime<Utc>>, BudgetError> {
        read_stamp(&self.path, held)
    }

    /// A robots Crawl-delay of 9,000,000,000,000 s panicked here before 2026-11-15 (PR #23 review).
    fn advanced(&self, at: DateTime<Utc>, by: TimeDelta) -> Result<DateTime<Utc>, BudgetError> {
        at.checked_add_signed(by)
            .ok_or_else(|| self.unusable("advanced", &"past the last representable instant"))
    }

    fn unusable(&self, act: &'static str, why: &impl std::fmt::Display) -> BudgetError {
        unusable(&self.path, act, why)
    }
}

/// What a bounded reservation answered. `wait` is how long the caller would owe either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reserved {
    /// Reserved and written: wait this long, then call.
    Wait(Duration),
    /// Nothing written: the slot opens at `go_at`, after the deadline; `ordinary_at` is when it
    /// would open under the profile's own interval with no server hold.
    PastDeadline {
        go_at: DateTime<Utc>,
        ordinary_at: DateTime<Utc>,
        wait: Duration,
    },
}

/// The critical section's answer, before it is turned into a wait.
enum Slot {
    Granted(DateTime<Utc>),
    Late {
        go_at: DateTime<Utc>,
        ordinary_at: DateTime<Utc>,
    },
}

/// `<pace root>/pace/<host>.hold`: the instant a server's `Retry-After` holds one host until, read
/// by every later run and process on the pace root (S7-D7). Raised by an answer, never lowered; the
/// caller caps what it writes, so the file holds no instant past that cap.
#[derive(Debug)]
pub struct Hold {
    path: PathBuf,
}

impl Hold {
    #[must_use]
    pub fn beside_the_store(store: &Path, host: &HostKey) -> Self {
        Self {
            path: store.join(PACE_DIR).join(format!("{}.hold", host.0)),
        }
    }

    /// `None` when no server ever held the host on this store.
    pub fn until(&self) -> Result<Option<DateTime<Utc>>, BudgetError> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(why) => return Err(unusable(&self.path, "opened", &why)),
        };
        file.lock_shared()
            .map_err(|why| unusable(&self.path, "locked", &why))?;
        let mut held = String::new();
        let read = file
            .read_to_string(&mut held)
            .map_err(|why| unusable(&self.path, "read", &why));
        let unlocked = file
            .unlock()
            .map_err(|why| unusable(&self.path, "unlocked", &why));
        read?;
        unlocked?;
        read_stamp(&self.path, &held)
    }

    /// Writes `until` unless the file already holds a later instant.
    pub fn raise(&self, until: DateTime<Utc>) -> Result<(), BudgetError> {
        let file = open_for_update(&self.path)?;
        file.lock()
            .map_err(|why| unusable(&self.path, "locked", &why))?;
        let raised = self.raise_locked(&file, until);
        let unlocked = file
            .unlock()
            .map_err(|why| unusable(&self.path, "unlocked", &why));
        raised?;
        unlocked
    }

    fn raise_locked(&self, mut file: &File, until: DateTime<Utc>) -> Result<(), BudgetError> {
        let mut held = String::new();
        file.read_to_string(&mut held)
            .map_err(|why| unusable(&self.path, "read", &why))?;
        let written = read_stamp(&self.path, &held)?.map_or(until, |prior| prior.max(until));
        file.set_len(0)
            .map_err(|why| unusable(&self.path, "truncated", &why))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|why| unusable(&self.path, "rewound", &why))?;
        file.write_all(stamp(written).as_bytes())
            .map_err(|why| unusable(&self.path, "written", &why))
    }
}

/// `<pace root>/pace/<bucket file>.sent`: the `requested_at` of the last call through one bucket. A
/// slot counts from its reservation, and a call leaves after its attempt row is written: two
/// consecutive `requested_at` of one 7 s host were 5.93 s apart on production, 2026-11-20/21.
#[derive(Debug)]
pub struct SendGate {
    path: PathBuf,
    interval: TimeDelta,
    /// How long this gate's lock is waited for: `GATE_TURNS` of the interval.
    patience: Duration,
}

/// What a stamp at the gate answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stamped {
    /// Written to every gate: the call's `requested_at`.
    Sent(DateTime<Utc>),
    /// Nothing written: the last call through some gate was this long too recent.
    Owed(Duration),
}

impl SendGate {
    pub fn beside_the_store(
        store: &Path,
        scope: &BudgetScope,
        interval: Duration,
    ) -> Result<Self, BudgetError> {
        let path = store
            .join(PACE_DIR)
            .join(format!("{}.sent", scope.file_name()));
        let patience = interval.saturating_mul(GATE_TURNS);
        let interval = TimeDelta::from_std(interval)
            .map_err(|why| unusable(&path, "read as an interval", &why))?;
        Ok(Self {
            path,
            interval,
            patience,
        })
    }

    /// Locks every gate in the order given. The caller keeps the locks through the wait a stamp
    /// answers `Owed` with: released for it, the opening went to whichever process read its clock
    /// first, and a sender lost 8 times running was refused (13 of 600 sends, two processes at
    /// 100 ms, 2026-11-26; architecture.md section 12.6). `left` is what is left of the caller's
    /// run, read before each wait for a lock and never for a lock that is free.
    fn lock<'gates>(
        gates: &'gates [Self],
        wait: &'gates dyn Sleeper,
        left: &'gates dyn Fn() -> Option<Duration>,
    ) -> Result<Locked<'gates>, BudgetError> {
        let mut locked = Locked {
            gates,
            held: Vec::with_capacity(gates.len()),
            wait,
            left,
            not_after: None,
        };
        locked.lock_the_rest()?;
        Ok(locked)
    }
}

/// The gates of one call in the order their locks are taken: the host's, then our address's.
/// Per-IP first, its lock was kept through a host's wait (`gates.reverse()` in `Wire` passed
/// every test, review 2026-11-26), so the order is stated here alone and the field is private.
#[derive(Debug)]
pub(crate) struct Gates(Vec<SendGate>);

impl Gates {
    pub(crate) fn of_a_call(
        store: &Path,
        host: Option<(&HostKey, Duration)>,
        per_ip: Option<(&SourceId, Duration)>,
    ) -> Result<Self, BudgetError> {
        let host = host.map(|(host, interval)| (BudgetScope::PerHost(host.clone()), interval));
        let per_ip =
            per_ip.map(|(source, interval)| (BudgetScope::PerIp(source.clone()), interval));
        [host, per_ip]
            .into_iter()
            .flatten()
            .map(|(scope, interval)| SendGate::beside_the_store(store, &scope, interval))
            .collect::<Result<_, _>>()
            .map(Self)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The gates among `refused` are not waited for: each kept its lock through a whole wait of
    /// this run already, so a second wait would spend the run on the same holder.
    #[must_use]
    pub(crate) fn impatient_at(mut self, refused: &BTreeSet<PathBuf>) -> Self {
        for gate in &mut self.0 {
            if refused.contains(&gate.path) {
                gate.patience = Duration::ZERO;
            }
        }
        self
    }

    pub(crate) fn paths(&self) -> impl Iterator<Item = &Path> {
        self.0.iter().map(|gate| gate.path.as_path())
    }

    pub(crate) fn lock<'gates>(
        &'gates self,
        wait: &'gates dyn Sleeper,
        left: &'gates dyn Fn() -> Option<Duration>,
    ) -> Result<Locked<'gates>, BudgetError> {
        SendGate::lock(&self.0, wait, left)
    }
}

/// `file` locked, waited for in the kernel's queue for at most `patience` or `LOCK_SHARE` of what
/// `left` says is left of the run, whichever is less; not at all when that is nothing. A thread
/// waits there in the caller's place, since `flock` takes no limit and polling `try_lock` keeps no
/// place in the queue; granted the lock after the caller left, the thread drops the file and so
/// the lock.
fn lock_within(
    path: &Path,
    file: File,
    patience: Duration,
    wait: &dyn Sleeper,
    left: &dyn Fn() -> Option<Duration>,
) -> Result<File, BudgetError> {
    match file.try_lock() {
        Ok(()) => return Ok(file),
        Err(TryLockError::WouldBlock) => {}
        Err(TryLockError::Error(why)) => return Err(unusable(path, "locked", &why)),
    }
    let patience = left().map_or(patience, |left| (left / LOCK_SHARE).min(patience));
    let held = || BudgetError::GateHeld {
        path: path.to_path_buf(),
        waited: patience,
    };
    if patience.is_zero() {
        return Err(held());
    }
    let (granted, waiting) = mpsc::channel();
    thread::Builder::new()
        .spawn(move || drop(granted.send(file.lock().map(|()| file))))
        .map_err(|why| unusable(path, "waited for on a thread", &why))?;
    match wait.granted(&waiting, patience) {
        Some(Ok(file)) => Ok(file),
        Some(Err(why)) => Err(unusable(path, "locked", &why)),
        None => Err(held()),
    }
}

/// The gates of one call and the locks held on the first of them, until dropped: closing a file
/// releases its lock, as the kernel does for a process that died holding it.
pub(crate) struct Locked<'gates> {
    gates: &'gates [SendGate],
    held: Vec<File>,
    wait: &'gates dyn Sleeper,
    left: &'gates dyn Fn() -> Option<Duration>,
    /// The run's deadline: no stamp is written for a clock read after it.
    not_after: Option<DateTime<Utc>>,
}

impl Locked<'_> {
    #[must_use]
    pub(crate) const fn not_after(mut self, deadline: Option<DateTime<Utc>>) -> Self {
        self.not_after = deadline;
        self
    }

    fn lock_the_rest(&mut self) -> Result<(), BudgetError> {
        for gate in self.gates.iter().skip(self.held.len()) {
            let file = open_for_update(&gate.path)?;
            let locked = lock_within(&gate.path, file, gate.patience, self.wait, self.left)?;
            self.held.push(locked);
        }
        Ok(())
    }

    /// Under every gate's lock, reads `clock` and writes that instant, cut to the file's
    /// microseconds, to every gate, or to none when any one's last call is less than its interval
    /// ago. `Owed` keeps the locks up to the gate owed the most and releases the later ones.
    pub(crate) fn stamp(
        &mut self,
        clock: &dyn Fn() -> DateTime<Utc>,
    ) -> Result<Stamped, BudgetError> {
        self.lock_the_rest()?;
        let now = clock().trunc_subsecs(STAMP_DIGITS);
        let (mut owed, mut kept) = (TimeDelta::zero(), 0);
        for (at, (gate, file)) in self.gates.iter().zip(&self.held).enumerate() {
            let mut file: &File = file;
            let mut held = String::new();
            // A second try reads the same open file, which the first left at its end.
            file.seek(SeekFrom::Start(0))
                .map_err(|why| unusable(&gate.path, "rewound", &why))?;
            file.read_to_string(&mut held)
                .map_err(|why| unusable(&gate.path, "read", &why))?;
            if let Some(last) = read_stamp(&gate.path, &held)? {
                let opens = last.checked_add_signed(gate.interval).ok_or_else(|| {
                    unusable(
                        &gate.path,
                        "advanced",
                        &"past the last representable instant",
                    )
                })?;
                if opens > now && opens - now >= owed {
                    (owed, kept) = (opens - now, at + 1);
                }
            }
        }
        if owed > TimeDelta::zero() {
            // A per-IP lock held through a host's wait stalled another host's sender 1,807.7 ms at
            // a 100 ms per-IP pace (review 2026-11-26, architecture.md section 12.6, rule B3).
            self.held.truncate(kept);
            return Ok(Stamped::Owed(owed.to_std().unwrap_or(Duration::ZERO)));
        }
        if self.not_after.is_some_and(|deadline| now > deadline) {
            return Err(BudgetError::PastDeadline {
                owed: Duration::ZERO,
                left: Duration::ZERO,
            });
        }
        for (gate, file) in self.gates.iter().zip(&self.held) {
            let mut file: &File = file;
            file.set_len(0)
                .map_err(|why| unusable(&gate.path, "truncated", &why))?;
            file.seek(SeekFrom::Start(0))
                .map_err(|why| unusable(&gate.path, "rewound", &why))?;
            file.write_all(stamp(now).as_bytes())
                .map_err(|why| unusable(&gate.path, "written", &why))?;
        }
        Ok(Stamped::Sent(now))
    }
}

/// The digits of a second [`stamp`] writes, so an instant read back is the instant compared.
const STAMP_DIGITS: u16 = 6;

/// Opened read-write, created with its directory when absent, never truncated on open.
fn open_for_update(path: &Path) -> Result<File, BudgetError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|why| unusable(path, "created with its directory", &why))?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|why| unusable(path, "opened", &why))
}

/// `None` only for an empty file. Anything else this crate did not write is refused, not reset.
fn read_stamp(path: &Path, held: &str) -> Result<Option<DateTime<Utc>>, BudgetError> {
    let held = held.trim();
    if held.is_empty() {
        return Ok(None);
    }
    DateTime::parse_from_rfc3339(held)
        .map(|at| Some(at.with_timezone(&Utc)))
        .map_err(|_| BudgetError::Unreadable {
            path: path.to_path_buf(),
            held: held.to_owned(),
        })
}

fn unusable(path: &Path, act: &'static str, why: &impl std::fmt::Display) -> BudgetError {
    BudgetError::Unusable {
        path: path.to_path_buf(),
        act,
        why: why.to_string(),
    }
}

/// The written form: RFC 3339 with microseconds and a `Z`, which is the same width for every
/// instant, so a torn write is a parse failure and never an earlier instant two callers would
/// both be cleared by.
fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

#[cfg(test)]
mod tests {
    use std::fs::TryLockError;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use chrono::{DateTime, Utc};

    use super::{
        open_for_update, BudgetError, BudgetScope, Gates, Hold, HostKey, Pace, SendGate, Stamped,
    };
    use crate::clock::ThreadSleeper;
    use crate::request::SourceId;

    const SHOUTED: &str = "WWW.SEC.GOV";
    const SEC: &str = "www.sec.gov";
    const TRAVERSAL: &str = "../../x";
    /// Each breaks exactly one rule, so deleting any one rule lets its row through.
    const ONE_RULE_BROKEN_EACH: [&str; 5] =
        ["", ".sec.gov", "www.sec.gov.", "www..sec.gov", "[::1]"];

    const TWO_MINUTES_ON: &str = "2026-11-10T13:02:00Z";
    const FIVE_SECONDS_ON: &str = "2026-11-10T13:00:05Z";

    /// A parseable robots Crawl-delay that `DateTime` cannot add (review of PR #23, 2026-11-15).
    const AN_INTERVAL_PAST_THE_CALENDAR: Duration = Duration::from_secs(9_000_000_000_000);

    #[test]
    fn a_pace_too_long_to_add_is_refused_rather_than_panicking() -> Result<(), String> {
        let store = tempfile::tempdir().map_err(|why| why.to_string())?;
        let host = HostKey::new(SEC).map_err(|why| why.to_string())?;
        let pace = Pace::beside_the_store(
            store.path(),
            &BudgetScope::PerHost(host),
            AN_INTERVAL_PAST_THE_CALENDAR,
        );
        let now: DateTime<Utc> = DateTime::UNIX_EPOCH;
        let first = pace.reserve(now);
        let second = pace.reserve(now);
        assert!(
            matches!(
                (&first, &second),
                (
                    Ok(_),
                    Err(BudgetError::Unusable {
                        act: "advanced",
                        ..
                    })
                )
            ),
            "{first:?} {second:?}"
        );
        Ok(())
    }

    /// The per-IP arm adds the interval on its first call, so one call reaches it.
    #[test]
    fn a_per_ip_pace_too_long_to_add_is_refused_on_its_first_call() -> Result<(), String> {
        let store = tempfile::tempdir().map_err(|why| why.to_string())?;
        let pace = Pace::beside_the_store(
            store.path(),
            &BudgetScope::PerIp(SourceId::new("open_web_wires")),
            AN_INTERVAL_PAST_THE_CALENDAR,
        );
        let first = pace.reserve(DateTime::UNIX_EPOCH);
        assert!(
            matches!(
                first,
                Err(BudgetError::Unusable {
                    act: "advanced",
                    ..
                })
            ),
            "{first:?}"
        );
        Ok(())
    }

    /// A run with no deadline.
    const NO_DEADLINE: fn() -> Option<Duration> = || None;

    /// One try at the gate, its locks released after it.
    fn stamp_once(gates: &[SendGate], now: DateTime<Utc>) -> Result<Stamped, BudgetError> {
        SendGate::lock(gates, &ThreadSleeper, &NO_DEADLINE)?.stamp(&|| now)
    }

    const A_TURN: Duration = Duration::from_millis(2);
    /// 32 of `A_TURN`, written out so that another `GATE_TURNS` is red.
    const ITS_TURNS: Duration = Duration::from_millis(64);
    /// Ends only a caller that never gives up, so that it is red and not hung.
    const HUNG: Duration = Duration::from_secs(120);

    /// The holder is this test: a second open file of one process is another `flock` owner. It
    /// keeps the lock until the refused caller is back, which no clock decides. The wait itself
    /// is not timed here: this crate reads no clock outside `wire` and `clock` (row R4).
    #[test]
    fn a_gate_lock_another_sender_keeps_is_refused_by_name_after_its_turns() -> Result<(), String> {
        let store = tempfile::tempdir().map_err(|why| why.to_string())?;
        let host = HostKey::new(SEC).map_err(|why| why.to_string())?;
        let gate = [
            SendGate::beside_the_store(store.path(), &BudgetScope::PerHost(host), A_TURN)
                .map_err(|why| why.to_string())?,
        ];
        let [SendGate { path, .. }] = &gate;
        let held = open_for_update(path).map_err(|why| why.to_string())?;
        held.lock().map_err(|why| why.to_string())?;
        let (release, released) = mpsc::channel::<()>();
        let holder = thread::spawn(move || {
            let told = released.recv_timeout(HUNG).is_ok();
            drop(held);
            told
        });
        let refused = SendGate::lock(&gate, &ThreadSleeper, &NO_DEADLINE).err();
        release.send(()).map_err(|why| why.to_string())?;
        let told = holder.join().map_err(|_| "the holder panicked")?;
        let refused_by_name = Some(BudgetError::GateHeld {
            path: path.clone(),
            waited: ITS_TURNS,
        });
        assert_eq!(
            (
                refused,
                told,
                SendGate::lock(&gate, &ThreadSleeper, &NO_DEADLINE).is_ok()
            ),
            (refused_by_name, true, true)
        );
        Ok(())
    }

    /// Two clock reads a nanosecond-precise clock gives, a hair over one interval apart.
    const FIRST_SEND: &str = "2026-11-10T13:00:00.000000500Z";
    const A_HAIR_PAST_ONE_INTERVAL: &str = "2026-11-10T13:00:01.000000200Z";
    const GATE_INTERVAL: Duration = Duration::from_secs(1);

    /// The gate compares the instant it reads back, cut to the file's microseconds, so the
    /// instant it hands out must be that one: an uncut first send leaves 999,999,700 ns here.
    #[test]
    fn two_stamps_through_one_gate_are_never_closer_than_its_interval() -> Result<(), String> {
        let store = tempfile::tempdir().map_err(|why| why.to_string())?;
        let host = HostKey::new(SEC).map_err(|why| why.to_string())?;
        let gate =
            SendGate::beside_the_store(store.path(), &BudgetScope::PerHost(host), GATE_INTERVAL)
                .map_err(|why| why.to_string())?;
        let gates = [gate];
        let mut sent = Vec::new();
        for read in [FIRST_SEND, A_HAIR_PAST_ONE_INTERVAL] {
            let now = read
                .parse::<DateTime<Utc>>()
                .map_err(|why| why.to_string())?;
            match stamp_once(&gates, now).map_err(|why| why.to_string())? {
                Stamped::Sent(at) => sent.push(at),
                Stamped::Owed(wait) => return Err(format!("owed {wait:?} at {read}")),
            }
        }
        let gap = match sent.as_slice() {
            [first, second] => (*second - *first).to_std().map_err(|why| why.to_string())?,
            other => return Err(format!("{other:?}")),
        };
        assert_eq!(gap, GATE_INTERVAL);
        Ok(())
    }

    const LAST_SEND: &str = "2026-11-10T13:00:00Z";
    const ONE_MICROSECOND_SHORT: &str = "2026-11-10T13:00:00.999999Z";
    const ONE_INTERVAL_ON: &str = "2026-11-10T13:00:01Z";
    const ONE_MICROSECOND: Duration = Duration::from_micros(1);

    /// A tolerance of 1 ms or 10 ms at the comparison passed every other test (2026-11-22).
    #[test]
    fn a_stamp_one_microsecond_short_of_the_interval_is_owed_it_and_one_on_it_is_sent(
    ) -> Result<(), String> {
        let store = tempfile::tempdir().map_err(|why| why.to_string())?;
        let host = HostKey::new(SEC).map_err(|why| why.to_string())?;
        let gates =
            [
                SendGate::beside_the_store(
                    store.path(),
                    &BudgetScope::PerHost(host),
                    GATE_INTERVAL,
                )
                .map_err(|why| why.to_string())?,
            ];
        let at = |read: &str| read.parse::<DateTime<Utc>>().map_err(|why| why.to_string());
        stamp_once(&gates, at(LAST_SEND)?).map_err(|why| why.to_string())?;
        let short = stamp_once(&gates, at(ONE_MICROSECOND_SHORT)?);
        let on_it = stamp_once(&gates, at(ONE_INTERVAL_ON)?);
        assert_eq!(
            (short, on_it),
            (
                Ok(Stamped::Owed(ONE_MICROSECOND)),
                Ok(Stamped::Sent(at(ONE_INTERVAL_ON)?))
            )
        );
        Ok(())
    }

    const PER_IP: Duration = Duration::from_millis(100);
    const HALF_A_HOST_INTERVAL_ON: &str = "2026-11-10T13:00:00.5Z";
    const HALF_A_HOST_INTERVAL: Duration = Duration::from_millis(500);
    const ANOTHER_HOST_S_SEND: &str = "2026-11-10T13:00:00.95Z";
    const LEFT_OF_THE_PER_IP_INTERVAL: Duration = Duration::from_millis(50);
    const PAST_BOTH: &str = "2026-11-10T13:00:01.05Z";
    const BOTH_SENT: &str = "2026-11-10T13:00:02Z";
    const BOTH_OWED_ALIKE: &str = "2026-11-10T13:00:02.05Z";

    /// A host owed more than the per-IP bucket is waited out without the per-IP lock, so another
    /// host's call is stamped meanwhile, and the next stamp takes that lock and reads that bucket
    /// again. Kept, the lock refuses the other call `GateHeld` after 3.2 s. A host owed exactly
    /// what the per-IP bucket is (SEC's shape, one interval for both) keeps the per-IP lock.
    #[test]
    fn a_call_another_host_made_during_a_host_s_wait_is_still_owed_the_per_ip_interval(
    ) -> Result<(), String> {
        let store = tempfile::tempdir().map_err(|why| why.to_string())?;
        let source = SourceId::new("edgar_press");
        let gates = |host: &str, interval| {
            let host = HostKey::new(host)?;
            Gates::of_a_call(
                store.path(),
                Some((&host, interval)),
                Some((&source, PER_IP)),
            )
        };
        let ours = gates(SEC, GATE_INTERVAL).map_err(|why| why.to_string())?;
        let theirs = gates("data.sec.gov", PER_IP).map_err(|why| why.to_string())?;
        let at = |read: &str| read.parse::<DateTime<Utc>>().map_err(|why| why.to_string());
        let (half_on, one_on, past_both) = (
            at(HALF_A_HOST_INTERVAL_ON)?,
            at(ONE_INTERVAL_ON)?,
            at(PAST_BOTH)?,
        );
        let (both_sent, alike) = (at(BOTH_SENT)?, at(BOTH_OWED_ALIKE)?);
        stamp_once(&ours.0, at(LAST_SEND)?).map_err(|why| why.to_string())?;
        let mut locked = ours
            .lock(&ThreadSleeper, &NO_DEADLINE)
            .map_err(|why| why.to_string())?;
        let stamps = [
            locked.stamp(&|| half_on),
            stamp_once(&theirs.0, at(ANOTHER_HOST_S_SEND)?),
            locked.stamp(&|| one_on),
            locked.stamp(&|| past_both),
        ];
        drop(locked);
        let mut tied = theirs
            .lock(&ThreadSleeper, &NO_DEADLINE)
            .map_err(|why| why.to_string())?;
        let alike = [tied.stamp(&|| both_sent), tied.stamp(&|| alike)];
        let [_, SendGate { path, .. }] = theirs.0.as_slice() else {
            return Err("no per-IP gate".to_owned());
        };
        let per_ip = open_for_update(path).map_err(|why| why.to_string())?;
        let per_ip_kept = matches!(per_ip.try_lock(), Err(TryLockError::WouldBlock));
        assert_eq!(
            (stamps, alike, per_ip_kept),
            (
                [
                    Ok(Stamped::Owed(HALF_A_HOST_INTERVAL)),
                    Ok(Stamped::Sent(at(ANOTHER_HOST_S_SEND)?)),
                    Ok(Stamped::Owed(LEFT_OF_THE_PER_IP_INTERVAL)),
                    Ok(Stamped::Sent(past_both))
                ],
                [
                    Ok(Stamped::Sent(both_sent)),
                    Ok(Stamped::Owed(LEFT_OF_THE_PER_IP_INTERVAL))
                ],
                true
            )
        );
        Ok(())
    }

    const A_READ_WITH_NANOSECONDS: &str = "2026-11-10T13:00:00.123456789Z";
    const THAT_READ_IN_MICROSECONDS: &str = "2026-11-10T13:00:00.123456Z";

    /// `requested_at` is this instant, and the ledger keeps microseconds.
    #[test]
    fn a_stamp_keeps_the_microseconds_of_its_clock_read() -> Result<(), String> {
        let store = tempfile::tempdir().map_err(|why| why.to_string())?;
        let host = HostKey::new(SEC).map_err(|why| why.to_string())?;
        let gates =
            [
                SendGate::beside_the_store(
                    store.path(),
                    &BudgetScope::PerHost(host),
                    GATE_INTERVAL,
                )
                .map_err(|why| why.to_string())?,
            ];
        let at = |read: &str| read.parse::<DateTime<Utc>>().map_err(|why| why.to_string());
        let stamped = stamp_once(&gates, at(A_READ_WITH_NANOSECONDS)?);
        assert_eq!(stamped, Ok(Stamped::Sent(at(THAT_READ_IN_MICROSECONDS)?)));
        Ok(())
    }

    #[test]
    fn a_host_key_in_upper_case_is_normalised_to_one_lowercase_bucket() {
        assert_eq!(HostKey::new(SHOUTED), Ok(HostKey(SEC.to_owned())));
    }

    #[test]
    fn a_host_key_climbing_out_of_the_pace_directory_is_refused() {
        assert_eq!(
            HostKey::new(TRAVERSAL),
            Err(BudgetError::NotAHostKey {
                host: TRAVERSAL.to_owned()
            })
        );
    }

    #[test]
    fn a_host_key_breaking_any_one_rule_is_refused() {
        let admitted: Vec<&str> = ONE_RULE_BROKEN_EACH
            .into_iter()
            .filter(|host| HostKey::new(host).is_ok())
            .collect();
        assert_eq!(admitted, Vec::<&str>::new());
    }

    /// S7-D7: a hold is raised, never lowered, so a later, shorter `Retry-After` (a robots
    /// refresh, or another run on the store) leaves the longer one in force.
    #[test]
    fn a_later_shorter_hold_does_not_lower_the_hold_on_file() -> Result<(), String> {
        let store = tempfile::tempdir().map_err(|why| why.to_string())?;
        let host = HostKey::new(SEC).map_err(|why| why.to_string())?;
        let hold = Hold::beside_the_store(store.path(), &host);
        let instant = |at: &str| at.parse::<DateTime<Utc>>().map_err(|why| why.to_string());
        let (longer, shorter) = (instant(TWO_MINUTES_ON)?, instant(FIVE_SECONDS_ON)?);
        hold.raise(longer).map_err(|why| why.to_string())?;
        hold.raise(shorter).map_err(|why| why.to_string())?;
        assert_eq!(hold.until(), Ok(Some(longer)));
        Ok(())
    }
}
