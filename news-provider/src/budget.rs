//! How fast a vendor may be called, and the cross-process file that holds it to that. One locked
//! file carries the wall instant of the next allowed call; the lock is held for the
//! read-reserve-write and never across the wait, so callers stack reservations instead of
//! serialising on each other's wait.
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

/// Which bucket one pace file counts, and so which callers share it. The other arm is
/// per-CREDENTIAL, where the bucket follows the key across hosts and a second key is a second
/// bucket; no vendor here has one, so it is named and not written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetScope {
    /// Our outbound address, per source. The caller names itself in the User-Agent and that does
    /// not scope the quota, so every process on one host shares one bucket.
    PerIp(SourceId),
    /// One remote host, lowercased, and NOT the source: two sources asking one host share it.
    PerHost(String),
}

impl BudgetScope {
    /// The pace file's name, whose suffix keeps the two kinds of bucket off one file.
    fn file_name(&self) -> String {
        match self {
            Self::PerIp(source) => format!("{}.per-ip", source.as_str()),
            Self::PerHost(host) => format!("{host}.per-host"),
        }
    }
}

/// How fast one vendor may be called. Stated by the provider, like the timeouts beside it. A call
/// owing both paces waits out each in turn, so it is held to the stricter of the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestBudget {
    /// The smallest gap between two calls leaving our address for this vendor.
    pub min_interval: Duration,
    /// The smallest gap between two calls to one remote host, when the vendor states one.
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
}

/// The directory the pace file sits in, under the store root the run writes to.
const PACE_DIR: &str = "pace";

/// One bucket's next-allowed-call instant, on disk.
#[derive(Debug)]
pub struct Pace {
    path: PathBuf,
    min_interval: Duration,
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
        let go_at = match self.prior(&held)? {
            // A reservation already in the past is owed nothing: nobody used that slot.
            Some(prior) => prior.max(now),
            None => now,
        };
        file.set_len(0)
            .map_err(|why| self.unusable("truncated", &why))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|why| self.unusable("rewound", &why))?;
        file.write_all(stamp(go_at + interval).as_bytes())
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
