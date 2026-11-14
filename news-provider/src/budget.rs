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
/// dot and no empty label. The field is private, so no caller can name a file outside `pace/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKey(String);

impl HostKey {
    /// Upper case is normalised, not refused: a url's host is case-insensitive (RFC 3986 §3.2.2).
    pub fn new(host: &str) -> Result<Self, BudgetError> {
        let key = host.to_ascii_lowercase();
        let allowed =
            |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".-".contains(&byte);
        if key.is_empty() || key.starts_with('.') || key.contains("..") || !key.bytes().all(allowed)
        {
            return Err(BudgetError::NotAHostKey {
                host: host.to_owned(),
            });
        }
        Ok(Self(key))
    }
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
    /// The smallest gap between two calls leaving our address for this vendor.
    pub min_interval: Duration,
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
        let interval = TimeDelta::from_std(self.min_interval)
            .map_err(|why| self.unusable("read as an interval", &why))?;
        let file = self.open()?;
        file.lock().map_err(|why| self.unusable("locked", &why))?;
        let reserved = self.read_reserve_write(&file, now, interval);
        let unlocked = file.unlock().map_err(|why| self.unusable("unlocked", &why));
        let go_at = reserved?;
        unlocked?;
        // Non-negative by the `max` in `read_reserve_write`; a negative delta has no `std` form.
        Ok((go_at - now).to_std().unwrap_or(Duration::ZERO))
    }

    fn open(&self) -> Result<File, BudgetError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|why| self.unusable("created with its directory", &why))?;
        }
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)
            .map_err(|why| self.unusable("opened", &why))
    }

    /// The critical section, and the only thing the lock is held for. No `fsync`: the predecessor
    /// decided against one for the reason that still holds, that a reservation left in the past by
    /// a restart collapses to `now` below, and the cost is iowait on the store's own disk.
    fn read_reserve_write(
        &self,
        mut file: &File,
        now: DateTime<Utc>,
        interval: TimeDelta,
    ) -> Result<DateTime<Utc>, BudgetError> {
        let mut held = String::new();
        file.read_to_string(&mut held)
            .map_err(|why| self.unusable("read", &why))?;
        let next_allowed = match (self.prior(&held)?, self.held) {
            (Some(prior), Held::NextAllowed) => Some(prior),
            (Some(granted), Held::LastGranted) => Some(granted + interval),
            (None, _) => None,
        };
        // A reservation already in the past is owed nothing: nobody used that slot.
        let go_at = next_allowed.map_or(now, |next| next.max(now));
        let written = match self.held {
            Held::NextAllowed => go_at + interval,
            Held::LastGranted => go_at,
        };
        file.set_len(0)
            .map_err(|why| self.unusable("truncated", &why))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|why| self.unusable("rewound", &why))?;
        file.write_all(stamp(written).as_bytes())
            .map_err(|why| self.unusable("written", &why))?;
        Ok(go_at)
    }

    /// `None` only for an empty file, which is the first call through a freshly created one.
    fn prior(&self, held: &str) -> Result<Option<DateTime<Utc>>, BudgetError> {
        let held = held.trim();
        if held.is_empty() {
            return Ok(None);
        }
        DateTime::parse_from_rfc3339(held)
            .map(|at| Some(at.with_timezone(&Utc)))
            .map_err(|_| BudgetError::Unreadable {
                path: self.path.clone(),
                held: held.to_owned(),
            })
    }

    fn unusable(&self, act: &'static str, why: &impl std::fmt::Display) -> BudgetError {
        BudgetError::Unusable {
            path: self.path.clone(),
            act,
            why: why.to_string(),
        }
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
    use super::{BudgetError, HostKey};

    const SHOUTED: &str = "WWW.SEC.GOV";
    const SEC: &str = "www.sec.gov";
    const TRAVERSAL: &str = "../../x";
    /// Each breaks exactly one rule, so deleting any one rule lets its row through.
    const ONE_RULE_BROKEN_EACH: [&str; 4] = ["", ".sec.gov", "www..sec.gov", "[::1]"];

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
