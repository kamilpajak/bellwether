//! How fast a vendor may be called, and the cross-process file that holds it to that. One locked
//! file per bucket carries one wall instant (`BudgetScope::held` says which); the lock is held
//! for the read-reserve-write and never across the wait, so callers stack reservations instead
//! of serialising on each other's wait.
//!
//! The shape is the predecessor's `sec_rate_coordinator.py`, written after the failure it
//! prevents: three processes on one residential IP, each with its own in-process throttle, whose
//! aggregate drew a traffic-threshold 403 (its epic #379). An in-process limiter cannot see the
//! other two, which is why this is a file and not a field.
//!
//! What it does and does not close: `docs/design/architecture.md` section 8 row R19.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};

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
/// keyable-host check share. `Ok(None)` when `ureq::http::Uri` cannot parse the url.
pub(crate) fn host_of(url: &str) -> Result<Option<HostKey>, BudgetError> {
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
/// owing both paces waits out the host's and then our address's, so two calls to one host leave at
/// least `per_host` less the per-IP wait owed after the host wait apart (row R19, hole 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestBudget {
    /// The smallest gap between two calls leaving our address for this vendor, when the vendor
    /// counts one; `None` paces each host alone (owner decision 1, 2026-11-17, §10).
    pub min_interval: Option<Duration>,
    /// The gap between two host reservations to one remote host, when the vendor states one.
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
}

/// The directory the pace file sits in, under the store root the run writes to.
const PACE_DIR: &str = "pace";

/// One bucket's next-allowed-call instant, on disk.
#[derive(Debug)]
pub struct Pace {
    path: PathBuf,
    min_interval: Duration,
    held: Held,
}

impl Pace {
    /// `<store>/pace/<source>.per-ip` or `<store>/pace/<host>.per-host` — beside the archive the
    /// run writes, never a system directory. The hole that leaves is named in row R19: one host
    /// running two stores holds two pace files and therefore two buckets.
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

/// `<store>/pace/<host>.hold`: the instant a server's `Retry-After` holds one host until, read by
/// every later run and process on the store (S7-D7). Raised by an answer, never lowered; the
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
    use std::time::Duration;

    use chrono::{DateTime, Utc};

    use super::{BudgetError, BudgetScope, HostKey, Pace};
    use crate::request::SourceId;

    const SHOUTED: &str = "WWW.SEC.GOV";
    const SEC: &str = "www.sec.gov";
    const TRAVERSAL: &str = "../../x";
    /// Each breaks exactly one rule, so deleting any one rule lets its row through.
    const ONE_RULE_BROKEN_EACH: [&str; 5] =
        ["", ".sec.gov", "www.sec.gov.", "www..sec.gov", "[::1]"];

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
}
