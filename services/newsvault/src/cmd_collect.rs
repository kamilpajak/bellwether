//! `newsvault collect` - the one command that writes to the archive. What it reports and which
//! exit each state takes are architecture.md section 5.1; the exit table itself is
//! `news_provider::Tally::exit` and is not restated here.
//!
//! The run envelope is on disk with `finished_at` null BEFORE the lease is attempted, so a run
//! that dies holding nothing still left a record that it started (accepted attack A3.13).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use canon::digest::{doc_id, item_id, story_id, ItemFields};
use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use edgar_press::EdgarPress;
use news_provider::lag_policy::{LagPolicy, LagPolicyError};
use news_provider::provider::{Cadence, Calendar, CalendarError, Listing, PlanRefusal, Provenance};
use news_provider::request::RequestPhase;
use news_provider::{
    stamp_of, Archive, ArchiveError, AttemptReceipt, AttemptRow, Clock, Closing, CollectError,
    Collected, CollectedItem, Committed, Credentials, DeferReason, Deferred, Exchange, Exit,
    Headers, OpenDeferred, Payload, Provider, ReleasedCut, Relisted, Run, RunId, SourceId,
    TargetWindow, Wire,
};
use open_web_feed::{
    Basis, Continuity, CrossChecker, DirectAnswer, Generation, HostRobots, OpenWebFeed, Paging,
    RobotsPin, Walk, WalkBasis, WalkPlan, WalkRange, GENERATION_HEADER,
};
use pit::{MeasuredLag, ObservationTime};
use serde_json::{json, Value};

use crate::cmd_ledger::{Envelope, RunBusy, RunHold, RunKind, LEASE_HELD, RUN_ENVELOPE_SCHEMA};
use crate::robots::{Robots, RobotsError};
use crate::schedule::{self, ScheduleRefusal, SlotAt};
use crate::store::{
    Committer, IndexPayload, ItemRow, StoreError, WalkRecord, POLICY_DIR, RUNS_DIR,
};

/// `/2` since 2026-11-15: `calendar_version` became nullable, a type change, and `window`,
/// `source` and `hosts_version` were added.
pub const SCHEMA: &str = "newsvault.collect/2";
/// What a run with no schedule slot is keyed by, so an ad-hoc run still has a run id and a
/// partition key (accepted attack A1.7).
const ADHOC: &str = "adhoc";
pub(crate) const RUN_ID_FORMAT: &str = "%Y%m%dT%H%M%SZ";
const STAGING_PREFIX: &str = ".tmp-";
/// The one window spelling that is not a day: the UTC date of the run's own `at`.
pub const TODAY: &str = "today";
/// Section 11.1's 7 s, held apart from `open-web-feed`'s own interval so the floor check does not
/// move when the profile it checks does.
const PACE_FLOOR: Duration = Duration::from_secs(7);
/// Section 11.2's option C match: `open_web_feed::normalised` titles, equal.
const CROSS_CHECK_MATCH: &str = "normalised_title";
/// A walked id GNW's CDN answers 410 for 30 days (owner decision 2026-11-17, section 11.2).
const GONE: u16 = 410;
const REDIRECTED: std::ops::RangeInclusive<u16> = 300..=399;

/// What a caller asked `collect` for. `calendar` and `listing` are files for the same reason the
/// rules file is one: a hashed input a second reader can reproduce. Which of the four optional
/// files a source takes is `Registered`'s table and its provider's `Cadence`.
#[derive(Debug, Clone)]
pub struct Ask {
    pub sources: Vec<String>,
    pub window: String,
    pub schedule: Option<String>,
    pub expected_runs: u32,
    pub store: PathBuf,
    pub calendar: Option<PathBuf>,
    pub listing: Option<PathBuf>,
    pub roster: Option<PathBuf>,
    pub hosts: Option<PathBuf>,
    /// The chosen provider's own rules file.
    pub rules: PathBuf,
    /// The measured observation lag per (source, provenance), hashed like the files above.
    /// Absent is not "no lag of zero": a `vendor_archive` window then has nothing behind it and
    /// `probed_lag` refuses it before the first request (decision D5).
    pub lag_policy: Option<PathBuf>,
    pub contact: String,
    pub at: DateTime<Utc>,
    /// The last instant a request may be sent at (condition A), overriding the schedule's (S7-D3).
    /// `None`: the schedule's for a source on it, and no deadline for one that is not.
    pub deadline: Option<DateTime<Utc>>,
    pub dry_run: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum CollectRunError {
    #[error("no provider is registered under {0}")]
    UnknownSource(String),
    /// A second SourceId needs a run envelope listing one `hosts_version` per source
    /// (architecture.md section 11.2), which is not built.
    #[error("one source per invocation; asked for {0:?}")]
    MoreThanOneSource(Vec<String>),
    #[error("{source_id} needs --{flag}")]
    FlagRequired {
        flag: &'static str,
        source_id: &'static str,
    },
    /// Refused, never ignored: a file a run does not read would still read as an input of it.
    #[error("{source_id} takes no --{flag}")]
    FlagRefused {
        flag: &'static str,
        source_id: &'static str,
    },
    /// An every-day provider's window is the run's UTC day, so an `at` on another day than the
    /// clock's would label today's fetch with that day.
    #[error("--at names {at}, and {source_id} collects only the clock's own UTC day, {today}")]
    AtIsNotTheClockDay {
        source_id: &'static str,
        at: NaiveDate,
        today: NaiveDate,
    },
    /// `today` is a UTC date; a session-bound source plans New York session days, and a run at
    /// 02:00Z planned the next UTC day (review 2026-11-15).
    #[error("{source_id} plans session days and takes no --window today; name the day")]
    TodayRefused { source_id: &'static str },
    /// A deadline at or before `--at` would defer every request before the first is asked.
    #[error("--deadline {deadline} is not after --at {at}")]
    DeadlineNotAfterAt {
        deadline: DateTime<Utc>,
        at: DateTime<Utc>,
    },
    /// S7-D2: the deadline bounds the run's wall time, so one the clock has reached would defer
    /// every request too; `--at` may lie in the past, so the check above does not catch it.
    #[error("--deadline {deadline} is not after the clock's {now}")]
    DeadlineNotAfterNow {
        deadline: DateTime<Utc>,
        now: DateTime<Utc>,
    },
    /// The GNW walk's frontier must be received before the start of `--at`'s slot, so a live
    /// every-day run may not plan after the clock (section 11.2).
    #[error("--at {at} is after the clock's {now}")]
    AtAfterTheClock {
        at: DateTime<Utc>,
        now: DateTime<Utc>,
    },
    /// The committed schedule (section 11.2) is not one; `cargo test` reaches this first.
    #[error("the poll schedule is unusable: {0}")]
    ScheduleUnusable(#[from] ScheduleRefusal),
    #[error("no {what} at {path}")]
    InputAbsent { what: &'static str, path: PathBuf },
    #[error("the calendar at {path} is unusable: {why}")]
    UnusableCalendar { path: PathBuf, why: CalendarError },
    /// Every refusal `plan` can give, carried by its own message. One blanket `map_err` used to
    /// report "neither a day nor a FROM..TO range" for a well-formed day whose observation lag had
    /// no probe behind it - found by review 2026-11-13 on the `--dry-run` path, which is the one an
    /// agent reads.
    #[error(transparent)]
    PlanRefused(#[from] PlanRefusal),
    /// The caller's `--window`: unreadable, or not the run's UTC day for a live-only source.
    #[error(transparent)]
    WindowRefused(PlanRefusal),
    #[error(transparent)]
    Refused(#[from] edgar_press::Refusal),
    #[error(transparent)]
    OpenWebRefused(#[from] open_web_feed::Refusal),
    /// Registry code `input_unparseable`, exit 2. A policy nothing can read is not a run with no
    /// lag: that would refuse the window with R11's message and hide the unreadable file.
    #[error("the observation-lag policy is unusable: {0}")]
    UnusableLagPolicy(#[from] LagPolicyError),
    /// Code `robots_unverified`, exit 4: `fetched` is `None` when no answered robots fetch of the
    /// host is ledgered, and names the digest whose blob is absent from disk otherwise.
    #[error(
        "admitted host {host} pins robots {pinned}, and {}; run `newsvault robots refresh`",
        match .fetched {
            Some(digest) => format!("the newest ledgered fetch's blob {digest} is not on disk"),
            None => "the store ledgers no answered robots.txt fetch of it".to_owned(),
        }
    )]
    RobotsUnverified {
        host: String,
        pinned: String,
        fetched: Option<String>,
    },
    /// Code `robots_refused`, exit 2: the newest answered fetch is not the pin.
    #[error(
        "admitted host {host} pins robots {pinned}, and its newest ledgered fetch is {fetched}; \
         edit the hosts file once the change is read"
    )]
    RobotsRefused {
        host: String,
        pinned: String,
        fetched: String,
    },
    /// Code `robots_refused`, exit 2: the pinned robots body is no robots.txt this reader takes.
    #[error("admitted host {host} pins robots {pinned}, which cannot be read: {why}")]
    RobotsUnreadable {
        host: String,
        pinned: String,
        why: RobotsError,
    },
    /// Code `robots_refused`, exit 2: the pinned blob's bytes on disk hash to `found`, not the pin.
    #[error(
        "admitted host {host} pins robots {pinned}, and its stored blob now hashes to {found}"
    )]
    RobotsBlobAltered {
        host: String,
        pinned: String,
        found: String,
    },
    /// Code `robots_refused`, exit 2: the pinned robots.txt disallows a path the row asks.
    #[error("the pinned robots.txt of admitted host {host} disallows {path}, which its row asks")]
    RobotsDisallowsPath { host: String, path: String },
    /// Code `robots_refused`, exit 2: the provider's per-host interval is below section 11.1's
    /// 7 s floor (`None`: no per-host pace at all).
    #[error("admitted host {host} would be paced every {per_host:?}, below the floor {floor:?}")]
    PaceBelowFloor {
        host: String,
        per_host: Option<Duration>,
        floor: Duration,
    },
    /// Code `robots_refused`, exit 2: the pinned robots Crawl-delay is above the one per-host
    /// interval every host shares (owner decision 1, 2026-11-16; §8 row R19 hole 7).
    #[error(
        "admitted host {host} asks a Crawl-delay of {crawl_delay:?}, above the {interval:?} \
         interval every host shares; such a host waits for per-host intervals"
    )]
    CrawlDelayOverInterval {
        host: String,
        crawl_delay: Duration,
        interval: Duration,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Driver(#[from] CollectError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
}

impl CollectRunError {
    /// `invalid_argument` (section 5.1), which `main` writes as `usage error:`.
    #[must_use]
    pub const fn is_usage(&self) -> bool {
        match self {
            Self::UnknownSource(_)
            | Self::MoreThanOneSource(_)
            | Self::FlagRequired { .. }
            | Self::FlagRefused { .. }
            | Self::AtIsNotTheClockDay { .. }
            | Self::TodayRefused { .. }
            | Self::DeadlineNotAfterAt { .. }
            | Self::DeadlineNotAfterNow { .. }
            | Self::AtAfterTheClock { .. }
            | Self::WindowRefused(_) => true,
            Self::InputAbsent { .. }
            | Self::UnusableCalendar { .. }
            | Self::PlanRefused(_)
            | Self::Refused(_)
            | Self::OpenWebRefused(_)
            | Self::ScheduleUnusable(_)
            | Self::UnusableLagPolicy(_)
            | Self::RobotsUnverified { .. }
            | Self::RobotsRefused { .. }
            | Self::RobotsUnreadable { .. }
            | Self::RobotsBlobAltered { .. }
            | Self::RobotsDisallowsPath { .. }
            | Self::PaceBelowFloor { .. }
            | Self::CrawlDelayOverInterval { .. }
            | Self::Store(_)
            | Self::Driver(_)
            | Self::Io(_)
            | Self::Serde(_) => false,
        }
    }

    fn of_plan(refusal: PlanRefusal) -> Self {
        match refusal {
            PlanRefusal::TargetWindowUnreadable(_) | PlanRefusal::NotTheRunDay { .. } => {
                Self::WindowRefused(refusal)
            }
            other => Self::PlanRefused(other),
        }
    }

    fn of_driver(failure: CollectError) -> Self {
        match failure {
            CollectError::Planning(refusal) => Self::of_plan(refusal),
            other => Self::Driver(other),
        }
    }
}

/// The envelope a caller gets and the exit it takes. One object rendered two ways, so the human
/// and the machine output cannot carry different facts.
#[derive(Debug)]
pub struct Answer {
    pub envelope: Value,
    pub exit: Exit,
}

/// What one run collected, with the input digests the envelope names it by.
#[derive(Debug)]
pub struct Driven {
    pub collected: Collected,
    /// `None` when the provider's cadence takes no calendar and none was read.
    pub calendar_version: Option<String>,
    /// Section 11.2's option C for this run, or null: no cross-checker, or a dry run.
    pub cross_check: Value,
    /// Each paged index's poll and how it joins its basis (section 11.2), or null: no paged
    /// index, or a dry run.
    pub continuity: Value,
    /// Each walked index's poll, its range and what its ids answered (section 11.2), or null: no
    /// walked index, or a dry run.
    pub walk: Value,
}

/// The table of registered providers: per source, its credential and the files it takes beside
/// the calendar and listing its `Cadence` decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registered {
    EdgarPress,
    OpenWebWires,
}

impl Registered {
    pub const ALL: [Self; 2] = [Self::EdgarPress, Self::OpenWebWires];

    /// The one source an invocation collects; the same id given twice is one source.
    pub fn chosen(sources: &[String]) -> Result<Self, CollectRunError> {
        let mut distinct: Vec<String> = sources.to_vec();
        distinct.sort();
        distinct.dedup();
        let [only] = distinct.as_slice() else {
            return Err(CollectRunError::MoreThanOneSource(distinct));
        };
        Self::ALL
            .into_iter()
            .find(|entry| entry.source() == only)
            .ok_or_else(|| CollectRunError::UnknownSource(only.clone()))
    }

    #[must_use]
    pub const fn source(self) -> &'static str {
        match self {
            Self::EdgarPress => edgar_press::SOURCE,
            Self::OpenWebWires => open_web_feed::SOURCE,
        }
    }

    /// The name the User-Agent's contact travels under inside `Wire::send`.
    #[must_use]
    pub const fn credential(self) -> &'static str {
        match self {
            Self::EdgarPress => edgar_press::CONTACT_CREDENTIAL,
            Self::OpenWebWires => open_web_feed::CONTACT_CREDENTIAL,
        }
    }

    /// The provider's `transport().body_limit`, readable without opening it: `ledger` compares
    /// a cut body with it (section 11.1).
    #[must_use]
    pub const fn body_limit(self) -> u64 {
        match self {
            Self::EdgarPress => edgar_press::BODY_LIMIT,
            Self::OpenWebWires => open_web_feed::BODY_LIMIT,
        }
    }

    /// Each provider's contact from a variable of its own, never another vendor's.
    #[must_use]
    pub const fn contact_variable(self) -> &'static str {
        match self {
            Self::EdgarPress => "BELLWETHER_SEC_CONTACT",
            Self::OpenWebWires => "BELLWETHER_FEED_CONTACT",
        }
    }

    /// The `_FILE` twin of [`Self::contact_variable`], naming a file that holds the contact.
    #[must_use]
    pub const fn contact_file_variable(self) -> &'static str {
        match self {
            Self::EdgarPress => "BELLWETHER_SEC_CONTACT_FILE",
            Self::OpenWebWires => "BELLWETHER_FEED_CONTACT_FILE",
        }
    }

    /// What the provider's `cadence()` returns once opened, known before any file is read so a
    /// usage error is never hidden behind an absent input: review 2026-11-15 measured exit 4 for
    /// a run that was missing `--calendar` and also named an absent roster.
    #[must_use]
    pub const fn cadence(self) -> Cadence {
        match self {
            Self::EdgarPress => Cadence::SessionBound,
            Self::OpenWebWires => Cadence::EveryUtcDay,
        }
    }

    /// Whether section 11.2's schedule sets the run's deadline: EDGAR's session-bound runs are
    /// not on it.
    #[must_use]
    pub const fn on_schedule(self) -> bool {
        matches!(self, Self::OpenWebWires)
    }

    /// (flag, taken): `roster` is `edgar-press`'s universe, `hosts` is `open-web-feed`'s.
    const fn own_files(self) -> [(&'static str, bool); 2] {
        let edgar = matches!(self, Self::EdgarPress);
        [("roster", edgar), ("hosts", !edgar)]
    }
}

/// The provider and the policy that dated it. One struct, because the digest the envelope reports
/// and the lag `plan` was handed have to come from ONE read of one file: two reads of a path a
/// caller names are two different files when the second read lands after an edit.
pub struct Opened {
    pub registered: Registered,
    pub provider: Box<dyn Provider>,
    /// sha256 of the hosts file the provider was opened on; `None` for a provider with none.
    pub hosts_version: Option<String>,
    /// The provider's `extract_version` (architecture.md section 2); `None` for a provider that does
    /// not version its extraction code.
    pub extract_version: Option<String>,
    /// Every admitted hosts-file row as `collect` checks it; empty for a provider with none.
    pub admitted_robots: Vec<AdmittedRobots>,
    pub lag: Option<AppliedLag>,
    /// The keyed index's cross-check against the direct indexes (section 11.2, option C).
    pub cross_checker: Option<CrossChecker>,
    /// The paged indexes and the basis each was handed, read before the lease.
    pub paged: Option<PagedPolls>,
    /// The walked indexes and the range each was handed, read before the lease.
    pub walked: Option<WalkedPolls>,
}

/// Section 11.2's paged indexes and, per page-1 url, the basis poll's keys handed to the feed.
#[derive(Debug, Clone)]
pub struct PagedPolls {
    pub paging: Paging,
    pub basis: BTreeMap<String, Basis>,
}

/// Section 11.2's walked indexes and, per index url, what each poll walks; only a `WalkPlan::Range`
/// is handed to the feed.
#[derive(Debug, Clone)]
pub struct WalkedPolls {
    pub walk: Walk,
    pub ranges: BTreeMap<String, WalkPlan>,
}

/// One admitted hosts row (section 11.2): its pin, its pinned robots body as read when the
/// provider was opened, and the paths the row asks of its host.
#[derive(Debug, Clone)]
pub struct AdmittedRobots {
    pub host: String,
    pub pin: Option<RobotsPin>,
    pub robots: PinnedRobots,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinnedRobots {
    /// Pinned `absent`, a 404: no Crawl-delay and no path disallowed.
    NoRobots,
    Read(Robots),
    /// No blob of the pinned digest was in the store when the provider was opened.
    BlobMissing,
    /// The pinned blob's bytes hash to this digest instead of the pin.
    Altered(String),
    Unreadable(RobotsError),
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened")
            .field("registered", &self.registered)
            .field("hosts_version", &self.hosts_version)
            .field("extract_version", &self.extract_version)
            .field("admitted_robots", &self.admitted_robots)
            .field("lag", &self.lag)
            .finish_non_exhaustive()
    }
}

/// The lag a `vendor_archive` window of this run is dated through, as read.
#[derive(Debug, Clone)]
pub struct AppliedLag {
    /// sha256 of the policy file, which is also the name it is copied into the vault under.
    pub version: String,
    pub probe_id: String,
    pub seconds: i64,
    bytes: Vec<u8>,
}

fn given(
    path: Option<&PathBuf>,
    flag: &'static str,
    entry: Registered,
    taken: bool,
) -> Result<(), CollectRunError> {
    match (path.is_some(), taken) {
        (false, true) => Err(CollectRunError::FlagRequired {
            flag,
            source_id: entry.source(),
        }),
        (true, false) => Err(CollectRunError::FlagRefused {
            flag,
            source_id: entry.source(),
        }),
        _ => Ok(()),
    }
}

fn required<'a>(
    path: Option<&'a PathBuf>,
    flag: &'static str,
    entry: Registered,
) -> Result<&'a Path, CollectRunError> {
    path.map(PathBuf::as_path)
        .ok_or(CollectRunError::FlagRequired {
            flag,
            source_id: entry.source(),
        })
}

/// The one place a collect builds its `Wire`, for `main.rs` and the loopback tests alike: the
/// provider's profile unchanged, the contact under the provider's credential name, the pace files
/// beside the run's own store.
#[must_use]
pub fn wire_for(ask: &Ask, opened: &Opened) -> Wire {
    Wire::new(
        opened.provider.transport(),
        Credentials::default().with(opened.registered.credential(), ask.contact.clone()),
        &ask.store,
        &opened.provider.source(),
    )
}

/// Opens the one provider the sources name, after refusing every input flag its table entry and
/// its cadence do not take, so a refused run has read no store and written nothing.
pub fn open_provider(ask: &Ask) -> Result<Opened, CollectRunError> {
    let registered = Registered::chosen(&ask.sources)?;
    let [roster, hosts] = registered.own_files();
    given(ask.roster.as_ref(), roster.0, registered, roster.1)?;
    given(ask.hosts.as_ref(), hosts.0, registered, hosts.1)?;
    let session_bound = registered.cadence() == Cadence::SessionBound;
    given(ask.calendar.as_ref(), "calendar", registered, session_bound)?;
    given(ask.listing.as_ref(), "listing", registered, session_bound)?;
    if session_bound && ask.window == TODAY {
        return Err(CollectRunError::TodayRefused {
            source_id: registered.source(),
        });
    }
    let mut cross_checker = None;
    let mut paged = None;
    let mut walked = None;
    let (provider, file_versions, admitted_robots, lag): (Box<dyn Provider>, _, _, _) =
        match registered {
            Registered::EdgarPress => {
                let roster = required(ask.roster.as_ref(), roster.0, registered)?;
                let provider = EdgarPress::open(roster, &ask.rules, &ask.contact)?;
                match lag_of(ask, registered)? {
                    Some((applied, measured)) => (
                        Box::new(provider.with_observation_lag(measured)),
                        (None, None),
                        Vec::new(),
                        Some(applied),
                    ),
                    None => (Box::new(provider), (None, None), Vec::new(), None),
                }
            }
            Registered::OpenWebWires => {
                // Live only (section 11.1): no window of it is dated by a lag, so a policy is refused.
                if ask.lag_policy.is_some() {
                    return Err(CollectRunError::FlagRefused {
                        flag: "lag-policy",
                        source_id: registered.source(),
                    });
                }
                let hosts = required(ask.hosts.as_ref(), hosts.0, registered)?;
                let provider = OpenWebFeed::open(hosts, &ask.rules, &ask.contact)?;
                let version = provider.hosts_version().hex().to_owned();
                let extraction = provider.extract_version().hex().to_owned();
                let admitted = admitted_robots(&ask.store, &provider)?;
                let paging = provider.paging();
                let basis = basis_of(&ask.store, &paging)?;
                let walk = provider.walk();
                let slot = schedule::committed(patience_of(&provider))?
                    .slot_at(ask.at)
                    .ok_or(ScheduleRefusal::NoSlotAt { at: ask.at })?;
                let ranges = ranges_of(&ask.store, &walk, slot.start)?;
                let handed = ranges
                    .iter()
                    .filter_map(|(url, plan)| Some((url.clone(), plan.range()?.clone())))
                    .collect();
                let provider = provider
                    .with_robots(handed_robots(&admitted))
                    .with_basis(basis.clone())
                    .with_walks(handed);
                cross_checker = provider.cross_checker();
                paged = Some(PagedPolls { paging, basis });
                walked = Some(WalkedPolls { walk, ranges });
                (
                    Box::new(provider),
                    (Some(version), Some(extraction)),
                    admitted,
                    None,
                )
            }
        };
    let (hosts_version, extract_version) = file_versions;
    Ok(Opened {
        registered,
        provider,
        hosts_version,
        extract_version,
        admitted_robots,
        lag,
        cross_checker,
        paged,
        walked,
    })
}

/// The provider's patience for one request, S7-D3's `VENDOR_PATIENCE`: its global timeout.
fn patience_of(provider: &dyn Provider) -> TimeDelta {
    TimeDelta::from_std(provider.transport().timeouts.global).unwrap_or(TimeDelta::MAX)
}

/// Each walked index's range (section 11.2): through the highest id the newest usable answer
/// received before `frontier_by`, the start of `--at`'s slot, lists, after the id the newest basis
/// poll walked through. Only younger answers and a basis: `range_unread`, no basis.
fn ranges_of(
    store: &Path,
    walk: &Walk,
    frontier_by: DateTime<Utc>,
) -> Result<BTreeMap<String, WalkPlan>, CollectRunError> {
    let kept = basis_names();
    let source = SourceId::new(open_web_feed::SOURCE);
    let held = |digest: &str| -> Result<Vec<u8>, CollectRunError> {
        crate::store::blob_bytes(store, digest)?.ok_or_else(|| {
            StoreError::BlobAbsent {
                digest: digest.to_owned(),
            }
            .into()
        })
    };
    let mut ranges = BTreeMap::new();
    for index_url in walk.index_urls() {
        let read = crate::store::walk_read(store, &source, index_url, &kept, frontier_by)?;
        if read.previous.is_none() && read.younger && read.basis.is_some() {
            ranges.insert(index_url.to_owned(), WalkPlan::RangeUnread);
            continue;
        }
        let previous = read.previous.as_deref().map(held).transpose()?;
        let basis = match &read.basis {
            Some((through, digest)) => Some((*through, held(digest)?)),
            None => None,
        };
        let answered = read
            .answered
            .iter()
            .filter_map(|url| walk.release_id(index_url, url))
            .collect();
        let range = walk.range(
            index_url,
            previous.as_deref(),
            basis.as_ref().map(|(walked_through, index)| WalkBasis {
                walked_through: *walked_through,
                index,
            }),
            answered,
        );
        ranges.insert(index_url.to_owned(), range);
    }
    Ok(ranges)
}

/// Every `Continuity` a next poll anchors to, by name.
fn basis_names() -> Vec<&'static str> {
    Continuity::ALL
        .into_iter()
        .filter(|arm| arm.is_basis())
        .map(Continuity::name)
        .collect()
}

/// Each paged index's basis: the pages of the newest poll whose continuity `is_basis`, in page
/// order from page 1 (section 11.2). An index with none is read at page 1 only (`first_poll`).
fn basis_of(store: &Path, paging: &Paging) -> Result<BTreeMap<String, Basis>, CollectRunError> {
    let kept = basis_names();
    let source = SourceId::new(open_web_feed::SOURCE);
    let mut basis = BTreeMap::new();
    for index_url in paging.index_urls() {
        let pages = crate::store::basis_pages(store, &source, index_url, &kept)?;
        let mut bytes = Vec::new();
        for (_, page) in poll_pages(paging, index_url, &pages) {
            match crate::store::blob_bytes(store, &page.raw_blob)? {
                Some(read) => bytes.push(read),
                None => {
                    return Err(StoreError::BlobAbsent {
                        digest: page.raw_blob.clone(),
                    }
                    .into())
                }
            }
        }
        if let Some(read) = (!bytes.is_empty())
            .then(|| paging.basis_of(index_url, &bytes))
            .flatten()
        {
            basis.insert(index_url.to_owned(), read);
        }
    }
    Ok(basis)
}

/// One poll's pages of the paged index at `index_url`: pages 1, 2, ... while each is there, the
/// first payload of a page kept.
fn poll_pages<'a>(
    paging: &Paging,
    index_url: &str,
    payloads: &'a [IndexPayload],
) -> Vec<(u32, &'a IndexPayload)> {
    let mut by_page: BTreeMap<u32, &IndexPayload> = BTreeMap::new();
    for payload in payloads {
        if let Some(page) = paging.page_of(index_url, &payload.url) {
            by_page.entry(page).or_insert(payload);
        }
    }
    by_page
        .into_iter()
        .zip(1..)
        .take_while(|((page, _), expected)| page == expected)
        .map(|(read, _)| read)
        .collect()
}

/// Each admitted row's pinned robots body, read from the store's blobs by digest when the
/// provider is opened, so the body `collect` checks is the one read before any `Wire` exists.
fn admitted_robots(
    store: &Path,
    feed: &OpenWebFeed,
) -> Result<Vec<AdmittedRobots>, CollectRunError> {
    let paths = feed.admitted_paths();
    let mut admitted = Vec::new();
    for row in feed.host_pins().into_iter().filter(|row| row.admitted) {
        let robots = match &row.robots {
            Some(RobotsPin::Sha256(hex)) => match crate::store::blob_bytes(store, hex) {
                Ok(Some(body)) => {
                    Robots::parse(&body).map_or_else(PinnedRobots::Unreadable, PinnedRobots::Read)
                }
                Ok(None) => PinnedRobots::BlobMissing,
                Err(StoreError::BlobAltered { found, .. }) => PinnedRobots::Altered(found),
                Err(other) => return Err(other.into()),
            },
            Some(RobotsPin::Absent) | None => PinnedRobots::NoRobots,
        };
        admitted.push(AdmittedRobots {
            paths: paths
                .iter()
                .filter(|used| used.host == row.host)
                .map(|used| used.path.clone())
                .collect(),
            host: row.host,
            pin: row.robots,
            robots,
        });
    }
    Ok(admitted)
}

/// The pinned robots the feed checks each follow-up against (owner decision 2026-11-17). A row
/// whose pin cannot be read is handed nothing, so the feed follows none of its rows; the run is
/// refused before any request anyway (`refuse_what_the_pinned_robots_forbids`).
fn handed_robots(admitted: &[AdmittedRobots]) -> BTreeMap<String, HostRobots> {
    admitted
        .iter()
        .filter_map(|row| {
            let handed = match &row.robots {
                PinnedRobots::NoRobots => HostRobots::Absent,
                PinnedRobots::Read(robots) => HostRobots::Rules(Arc::new(robots.clone())),
                PinnedRobots::BlobMissing
                | PinnedRobots::Altered(_)
                | PinnedRobots::Unreadable(_) => return None,
            };
            Some((row.host.clone(), handed))
        })
        .collect()
}

/// The policy's entry for this source's archive arm, read once per call from the caller's file.
fn lag_of(
    ask: &Ask,
    registered: Registered,
) -> Result<Option<(AppliedLag, MeasuredLag)>, CollectRunError> {
    let Some(path) = ask.lag_policy.as_deref() else {
        return Ok(None);
    };
    let bytes = read_input("observation-lag policy", path)?;
    let policy = LagPolicy::read(&bytes)?;
    // Per (source, provenance), because the same adapter reading today's index and an earlier one
    // are two populations (R10) and one measured lag does not describe both.
    let Some(lag) = policy.lag(
        &SourceId::new(registered.source()),
        Provenance::VendorArchive,
    ) else {
        return Ok(None);
    };
    Ok(Some((
        AppliedLag {
            version: policy.version().hex().to_owned(),
            probe_id: lag.probe_id.clone(),
            seconds: lag.seconds,
            bytes,
        },
        lag.clone(),
    )))
}

/// The policy as a file in the archive, named by its own digest, on first use. A run that dated
/// items by a lag keeps the bytes that stated it, so the number survives an edit to the caller's
/// copy; a digest already there is the same bytes and nothing is written.
pub fn copy_policy_into_vault(
    root: &Path,
    applied: &AppliedLag,
) -> Result<PathBuf, CollectRunError> {
    let dir = root.join(POLICY_DIR);
    let stored = dir.join(format!("{}.json", applied.version));
    if stored.exists() {
        return Ok(stored);
    }
    std::fs::create_dir_all(&dir)?;
    let staging = dir.join(format!("{STAGING_PREFIX}{}.json", applied.version));
    // `create_new` and not `create`: the content files of this archive are never truncated, and
    // a staged file a crash left behind is a refusal rather than a silent overwrite (row R1).
    let mut staged = std::fs::File::create_new(&staging)?;
    std::io::Write::write_all(&mut staged, &applied.bytes)?;
    staged.sync_all()?;
    drop(staged);
    std::fs::rename(&staging, &stored)?;
    std::fs::File::open(&dir)?.sync_all()?;
    Ok(stored)
}

fn read_input(what: &'static str, path: &Path) -> Result<Vec<u8>, CollectRunError> {
    std::fs::read(path).map_err(|_| CollectRunError::InputAbsent {
        what,
        path: path.to_path_buf(),
    })
}

/// The calendar and listing the caller named, each read only when given; `open_provider` already
/// refused a flag the provider's cadence does not take and a missing one it needs.
fn inputs(ask: &Ask) -> Result<(Option<Calendar>, Option<Listing>), CollectRunError> {
    let calendar = match ask.calendar.as_deref() {
        Some(path) => Some(
            Calendar::read(&read_input("calendar", path)?).map_err(|why| {
                CollectRunError::UnusableCalendar {
                    path: path.to_path_buf(),
                    why,
                }
            })?,
        ),
        None => None,
    };
    let listing = match ask.listing.as_deref() {
        Some(path) => Some(edgar_press::read_listing(&read_input(
            "quarter listing",
            path,
        )?)?),
        None => None,
    };
    Ok((calendar, listing))
}

/// The window as planned: `today` is the UTC date of the run's own `at`, never the local one;
/// `open_provider` refuses it for a session-bound source.
#[must_use]
pub fn target_of(ask: &Ask) -> TargetWindow {
    if ask.window == TODAY {
        TargetWindow::new(ask.at.date_naive().to_string())
    } else {
        TargetWindow::new(&ask.window)
    }
}

/// Drives one run against an archive the caller holds. `run` takes the lease and calls this; a
/// test passes an archive that refuses a commit, which is the only honest way to reach the
/// `commit_failed` bucket of the exit table.
pub fn drive<A: Archive>(
    ask: &Ask,
    provider: &dyn Provider,
    archive: &mut A,
    exchange: &dyn Exchange,
    clock: &dyn Clock,
    run: &RunId,
) -> Result<Driven, CollectRunError> {
    let (calendar, listing) = inputs(ask)?;
    let target = target_of(ask);
    let collected = news_provider::collect(
        provider,
        archive,
        exchange,
        clock,
        &Run {
            run,
            target: &target,
            calendar: calendar.as_ref(),
            listing: listing.as_ref(),
            at: ObservationTime::new(ask.at),
            deadline: ask.deadline,
        },
    )
    .map_err(CollectRunError::of_driver)?;
    Ok(Driven {
        collected,
        calendar_version: version_of(calendar.as_ref()),
        cross_check: Value::Null,
        continuity: Value::Null,
        walk: Value::Null,
    })
}

fn version_of(calendar: Option<&Calendar>) -> Option<String> {
    calendar.map(|calendar| calendar.version().hex().to_owned())
}

/// Plans and collects one window. A dry run stops after the plan: it takes no lease, writes no
/// envelope and sends nothing.
pub fn run(
    ask: &Ask,
    opened: &Opened,
    exchange: &dyn Exchange,
    clock: &dyn Clock,
) -> Result<Answer, CollectRunError> {
    let provider = opened.provider.as_ref();
    let run_id = run_id(ask);
    let run_date = ask.at.date_naive();
    if let Some(deadline) = ask.deadline.filter(|deadline| *deadline <= ask.at) {
        return Err(CollectRunError::DeadlineNotAfterAt {
            deadline,
            at: ask.at,
        });
    }
    let now = clock.now();
    let stop = stop_of(ask, opened, now)?;
    let ask = &Ask {
        deadline: stop.deadline,
        ..ask.clone()
    };
    if ask.dry_run {
        return planned(ask, opened, &run_id, run_date, &stop);
    }
    if let Some(deadline) = stop
        .given
        .then_some(ask.deadline)
        .flatten()
        .filter(|deadline| *deadline <= now)
    {
        return Err(CollectRunError::DeadlineNotAfterNow { deadline, now });
    }
    // A dry run may name another day, which tests and plans need; a fetch may not.
    let today = now.date_naive();
    if provider.cadence() == Cadence::EveryUtcDay && run_date != today {
        return Err(CollectRunError::AtIsNotTheClockDay {
            source_id: opened.registered.source(),
            at: run_date,
            today,
        });
    }
    if provider.cadence() == Cadence::EveryUtcDay && ask.at > now {
        return Err(CollectRunError::AtAfterTheClock { at: ask.at, now });
    }
    refuse_the_window(ask, provider)?;
    refuse_an_unverified_robots(ask, opened)?;
    let run = RunId::new(run_id.clone());
    // Before the policy copy, the envelope and the lease: a store at a newer user_version or of
    // another v0 shape is left as found. A v0 step SQLite refuses still leaves a run record.
    crate::store::refuse_an_unreadable_version(&ask.store)?;
    // Before the plan, because the plan is where a lag dates a window: a run that refuses after
    // dating nothing still leaves the policy it would have dated by.
    if let Some(applied) = opened.lag.as_ref() {
        copy_policy_into_vault(&ask.store, applied)?;
    }
    let envelope = Envelope {
        schema: RUN_ENVELOPE_SCHEMA.to_owned(),
        run_id: run_id.clone(),
        run_date,
        source: Some(opened.registered.source().to_owned()),
        expected_runs: ask.expected_runs,
        started_at: ask.at,
        finished_at: None,
        kind: RunKind::Collect,
        refused: None,
        wall_s: None,
        busy: Vec::new(),
        holds_capped: Vec::new(),
    };
    let prior = prior_run_file(&ask.store, &run_id)?;
    // Before the lease, so a run that never gets one is a record rather than a hole.
    write_envelope(&ask.store, &envelope)?;
    let mut store = match Committer::open(&ask.store, &run) {
        Ok(store) => store,
        // S7-D9: finished and marked, so the ledger counts it apart from the runs it observes.
        // A file of the same run id that was there first is put back, never marked.
        Err(StoreError::LeaseHeld(lock)) => {
            match prior {
                Some(bytes) => write_run_file(&ask.store, &run_id, &bytes)?,
                None => write_envelope(
                    &ask.store,
                    &Envelope {
                        finished_at: Some(clock.now()),
                        refused: Some(LEASE_HELD.to_owned()),
                        ..envelope
                    },
                )?,
            }
            return Err(StoreError::LeaseHeld(lock).into());
        }
        Err(other) => return Err(other.into()),
    };
    let mut driven = drive(ask, provider, &mut store, exchange, clock, &run)?;
    if let Some(checker) = opened.cross_checker.as_ref() {
        driven.cross_check = cross_check_of(&store, checker, &driven.collected);
    }
    if let Some(paged) = opened.paged.as_ref() {
        driven.continuity = continuity_of(&mut store, paged);
    }
    if let Some(walked) = opened.walked.as_ref() {
        driven.walk = walk_of(&mut store, walked, &driven.collected);
    }
    let finished_at = clock.now();
    let wall = finished_at - now;
    let finished = Envelope {
        finished_at: Some(finished_at),
        wall_s: Some(seconds(wall)),
        busy: driven
            .collected
            .busy
            .iter()
            .map(|lane| RunBusy {
                host: lane.host.clone(),
                busy_s: seconds(lane.busy()),
            })
            .collect(),
        holds_capped: driven
            .collected
            .capped_holds
            .iter()
            .map(|hold| RunHold {
                host: hold.host.clone(),
                retry_after: hold.retry_after.clone(),
                held_until: hold.held_until,
            })
            .collect(),
        ..envelope
    };
    write_envelope(&ask.store, &finished)?;
    let timing = Timing {
        stop,
        wall: Some(wall),
    };
    Ok(Answer {
        exit: driven.collected.exit(),
        envelope: envelope_of(ask, opened, &run_id, run_date, &driven, &timing),
    })
}

/// Where a run stops asking (S7-D3): the caller's `--deadline`, else the schedule's for a source
/// on it, else none. `slot` is the schedule slot the clock's `now` belongs to when it decided.
#[derive(Debug, Clone, Copy)]
pub struct Stop {
    pub deadline: Option<DateTime<Utc>>,
    pub given: bool,
    pub slot: Option<SlotAt>,
}

/// A run started more than half a period late gets a deadline already passed and is not
/// refused: every request is then a `run_deadline` row, so "we did not look" is in the store.
fn stop_of(ask: &Ask, opened: &Opened, now: DateTime<Utc>) -> Result<Stop, CollectRunError> {
    if ask.deadline.is_some() || !opened.registered.on_schedule() {
        return Ok(Stop {
            deadline: ask.deadline,
            given: ask.deadline.is_some(),
            slot: None,
        });
    }
    let patience = patience_of(opened.provider.as_ref());
    let slot = schedule::committed(patience)?.slot_at(now);
    Ok(Stop {
        deadline: slot.map(|slot| slot.deadline(patience)),
        given: false,
        slot,
    })
}

/// What the envelope reports of a run's clock: its stop and, once it ran, its wall time.
struct Timing {
    stop: Stop,
    wall: Option<TimeDelta>,
}

fn seconds(span: TimeDelta) -> f64 {
    span.num_microseconds()
        .map_or(f64::MAX, |micros| micros as f64 / 1e6)
}

/// Section 11.2's option C: this run's keyed index answer against every direct index answer the
/// store holds from `CrossChecker::read_from` on. `result` is null when this run filed no readable
/// keyed payload, so "not looked" stays apart from "nothing unlisted". It runs after the commit,
/// so a store it cannot read is `error` here and never fails the run.
fn cross_check_of(store: &Committer, checker: &CrossChecker, collected: &Collected) -> Value {
    // The last attempt: a retried index's earlier answers are the failures it was retried for.
    let keyed = collected
        .attempts
        .iter()
        .rev()
        .find(|row| row.phase == RequestPhase::Index.name() && row.url == checker.index_url());
    let read = keyed.map_or_else(
        || Ok(Read::default()),
        |row| read_answers(store, checker, &row.attempt_id, &row.source),
    );
    let mut checked = match read {
        Ok(read) => checked_json(checker, &read),
        Err(failure) => json!({
            "watched_from": Value::Null,
            "direct_payloads_read": Value::Null,
            "direct_payloads_parsed": Value::Null,
            "result": Value::Null,
            "error": failure,
        }),
    };
    if let Some(fields) = checked.as_object_mut() {
        fields.insert("index_url".to_owned(), json!(checker.index_url()));
        fields.insert(
            "attempt_id".to_owned(),
            json!(keyed.map(|row| row.attempt_id.clone())),
        );
        fields.insert("matching_rule".to_owned(), json!(CROSS_CHECK_MATCH));
    }
    checked
}

/// Section 11.2: each paged index's poll in this run, its continuity against the basis the feed
/// was handed, recorded on its page-1 attempt. After the commit, so a store it cannot read or
/// write is `error` here, the poll's column stays NULL ("not looked") and the run finishes.
fn continuity_of(store: &mut Committer, paged: &PagedPolls) -> Value {
    let source = SourceId::new(open_web_feed::SOURCE);
    let read = store
        .usable_index_pages(&source)
        .and_then(|usable| Ok((usable, store.empty_index_pages(&source)?)));
    let (payloads, empty) = match read {
        Ok(read) => read,
        Err(failure) => {
            return json!([{"error": read_failure("store_unreadable", failure.to_string())}])
        }
    };
    let polls: Vec<Value> = paged
        .paging
        .index_urls()
        .map(|index_url| {
            let pages = poll_pages(&paged.paging, index_url, &payloads);
            let basis = paged.basis.get(index_url);
            let mut poll = json!({
                "index_url": index_url,
                "attempt_id": pages.first().map(|(_, page)| page.attempt_id.clone()),
                "pages_read": pages.iter().map(|(page, _)| *page).collect::<Vec<u32>>(),
                "basis_keys": basis.map(|basis| basis.keys().len()),
                "continuity": Value::Null,
                "boundaries": Value::Null,
                "error": Value::Null,
            });
            let next = pages.last().and_then(|(page, _)| page.checked_add(1));
            let ran_out = empty
                .iter()
                .any(|payload| paged.paging.page_of(index_url, &payload.url) == next);
            match recorded(store, paged, index_url, &pages, ran_out, basis) {
                Ok(Some((continuity, boundaries))) => {
                    poll["continuity"] = json!(continuity.name());
                    poll["boundaries"] = Generation::ALL
                        .iter()
                        .zip(boundaries)
                        .map(|(label, count)| (label.name().to_owned(), json!(count)))
                        .collect::<serde_json::Map<String, Value>>()
                        .into();
                }
                Ok(None) => {}
                Err(failure) => poll["error"] = failure,
            }
            poll
        })
        .collect();
    json!(polls)
}

/// One poll's continuity and its boundaries sharing no key counted per `Generation::ALL`, written
/// on its page-1 attempt; `None` when it read no usable page. `ran_out`: the page after the last
/// one read answered with no row.
fn recorded(
    store: &mut Committer,
    paged: &PagedPolls,
    index_url: &str,
    pages: &[(u32, &IndexPayload)],
    ran_out: bool,
    basis: Option<&Basis>,
) -> Result<Option<(Continuity, [u32; 4])>, Value> {
    let Some((_, first)) = pages.first() else {
        return Ok(None);
    };
    let bytes = pages
        .iter()
        .map(|(_, page)| read_blob(store, &page.raw_blob))
        .collect::<Result<Vec<Vec<u8>>, Value>>()?;
    let headers = pages
        .iter()
        .map(|(_, page)| Headers::from_json(&page.headers))
        .collect::<Result<Vec<Headers>, _>>()
        .map_err(|failure| read_failure("store_unreadable", failure.to_string()))?;
    let stamps: Vec<Option<&str>> = headers
        .iter()
        .map(|kept| kept.first(GENERATION_HEADER))
        .collect();
    let continuity = paged.paging.continuity(index_url, &bytes, basis, ran_out);
    let labels = paged.paging.generations(index_url, &bytes, &stamps);
    let (Some(continuity), Some(labels)) = (continuity, labels) else {
        return Ok(None);
    };
    let boundaries =
        Generation::ALL.map(|label| labels.iter().filter(|one| **one == label).count());
    let boundaries = boundaries.map(|count| u32::try_from(count).unwrap_or(u32::MAX));
    store
        .record_continuity(&first.attempt_id, continuity.name(), boundaries)
        .map_err(|failure| read_failure("store_unwritable", failure.to_string()))?;
    Ok(Some((continuity, boundaries)))
}

/// Section 11.2: each walked index's poll in this run, written on its usable attempt. After the
/// commit, so a store it cannot read or write is `error` here, the walk columns stay NULL ("not
/// looked") and the run finishes. An index with no usable answer this run walked nothing.
fn walk_of(store: &mut Committer, walked: &WalkedPolls, collected: &Collected) -> Value {
    let source = SourceId::new(open_web_feed::SOURCE);
    let read = store
        .usable_index_pages(&source)
        .and_then(|usable| Ok((usable, store.answered_document_urls(&source)?)));
    let (usable, answered) = match read {
        Ok(read) => read,
        Err(failure) => {
            return json!([{"error": read_failure("store_unreadable", failure.to_string())}])
        }
    };
    let polls: Vec<Value> = walked
        .walk
        .index_urls()
        .map(|index_url| {
            let plan = walked.ranges.get(index_url);
            let range = plan.and_then(WalkPlan::range);
            let attempt = usable.iter().rev().find(|payload| payload.url == index_url);
            let mut poll = json!({
                "index_url": index_url,
                "attempt_id": attempt.map(|payload| payload.attempt_id.clone()),
                "after": range.map(WalkRange::after),
                "through": range.map(WalkRange::through),
                "date": range.map(WalkRange::date),
                "asked": Value::Null,
                "continuity": Value::Null,
                "unread": Value::Null,
                "gone": Value::Null,
                "other_language": Value::Null,
                "error": Value::Null,
            });
            let Some(attempt) = attempt else {
                return poll;
            };
            let answered_ids: BTreeSet<u64> = answered
                .iter()
                .filter_map(|url| walked.walk.release_id(index_url, url))
                .collect();
            let (continuity, unread) = plan.map_or((Continuity::FirstPoll, Vec::new()), |plan| {
                Walk::continuity(plan, &answered_ids)
            });
            let answers = range.map_or_else(BTreeMap::new, |range| {
                walked_answers(&walked.walk, index_url, range, collected)
            });
            let mut gone = Vec::new();
            let mut moved = Vec::new();
            for (id, row) in &answers {
                match row.transport_status {
                    Some(GONE) => gone.push(*id),
                    Some(status) if REDIRECTED.contains(&status) => {
                        match moved_language(store, &walked.walk, index_url, row) {
                            Ok(Some(language)) => {
                                moved.push(json!({"id": id, "language": language}))
                            }
                            Ok(None) => {}
                            Err(failure) => {
                                poll["error"] = failure;
                                return poll;
                            }
                        }
                    }
                    _ => {}
                }
            }
            let record = WalkRecord {
                continuity: continuity.name(),
                through: range.map(|range| range.through().max(range.after())),
                gone: count(gone.len()),
                other_language: count(moved.len()),
                unread: count(unread.len()),
            };
            match store.record_walk(&attempt.attempt_id, record) {
                Ok(()) => {
                    poll["asked"] = json!(answers.len());
                    poll["continuity"] = json!(continuity.name());
                    poll["unread"] = json!(unread);
                    poll["gone"] = json!(gone);
                    poll["other_language"] = json!(moved);
                }
                Err(failure) => {
                    poll["error"] = read_failure("store_unwritable", failure.to_string());
                }
            }
            poll
        })
        .collect();
    json!(polls)
}

/// Each id this run walked under `range`, with its last attempt: a retried id keeps its final one.
fn walked_answers<'a>(
    walk: &Walk,
    index_url: &str,
    range: &WalkRange,
    collected: &'a Collected,
) -> BTreeMap<u64, &'a AttemptRow> {
    collected
        .attempts
        .iter()
        .filter(|row| row.phase == RequestPhase::Document.name())
        .filter_map(|row| Some((walk.walked_id(index_url, range, &row.url)?, row)))
        .collect()
}

/// The language a walked id's 3xx moved it to, read from its stored `location` (P9).
fn moved_language(
    store: &Committer,
    walk: &Walk,
    index_url: &str,
    row: &AttemptRow,
) -> Result<Option<String>, Value> {
    let unreadable = |failure: String| read_failure("store_unreadable", failure);
    let Some(payload) = store
        .payload_of(&row.attempt_id)
        .map_err(|failure| unreadable(failure.to_string()))?
    else {
        return Ok(None);
    };
    let headers =
        Headers::from_json(&payload.headers).map_err(|failure| unreadable(failure.to_string()))?;
    Ok(headers
        .first(news_provider::wire::LOCATION)
        .and_then(|location| walk.moved_to(index_url, &row.url, location)))
}

fn count(of: usize) -> u32 {
    u32::try_from(of).unwrap_or(u32::MAX)
}

/// What `cross_check_of` read from the store: the keyed bytes and the direct answers it compares.
#[derive(Default)]
struct Read {
    keyed: Option<Vec<u8>>,
    direct: Vec<DirectAnswer>,
    watched_from: Option<DateTime<Utc>>,
}

fn checked_json(checker: &CrossChecker, read: &Read) -> Value {
    let checked = read
        .keyed
        .as_deref()
        .and_then(|bytes| checker.check(bytes, &read.direct, read.watched_from));
    let result = checked.as_ref().map(|checked| {
        json!({
            "rows": {
                "followed": checked.followed,
                "cross_checked": checked.cross_checked,
                "other_key": checked.other_key,
                "no_key": checked.no_key,
            },
            "not_compared": checked.not_compared,
            "not_looked": checked.not_looked,
            "no_direct_index": checked.no_direct_index,
            "listed_directly": checked.listed_directly,
            "unlisted_count": checked.unlisted.len(),
            "unlisted": checked.unlisted.iter().map(|row| json!({
                "key": row.key,
                "url": row.url,
                "title": row.title,
                "listed_raw": row.listed_raw,
            })).collect::<Vec<Value>>(),
            "direct_titles": checked.direct_titles,
        })
    });
    json!({
        "watched_from": read.watched_from.map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)),
        "direct_payloads_read": read.direct.len(),
        "direct_payloads_parsed": checked.map(|checked| checked.direct_parsed),
        "result": result,
        "error": Value::Null,
    })
}

/// A store failure as the envelope's `cross_check.error`: a stable code and a free message.
fn read_failure(code: &str, message: String) -> Value {
    json!({"code": code, "message": message})
}

/// Through the writer's own `BlobStore`, so each dictionary is loaded once per run.
fn read_blob(store: &Committer, hex: &str) -> Result<Vec<u8>, Value> {
    let Some(digest) = store.blobs().named(hex) else {
        return Err(read_failure(
            "blob_absent",
            format!("no blob is stored under {hex}"),
        ));
    };
    match store
        .blobs()
        .read_verified(&digest)
        .map_err(StoreError::from)
    {
        Ok(bytes) => Ok(bytes),
        Err(failure @ StoreError::BlobAltered { .. }) => {
            Err(read_failure("blob_altered", failure.to_string()))
        }
        Err(failure) => Err(read_failure("store_unreadable", failure.to_string())),
    }
}

fn read_answers(
    store: &Committer,
    checker: &CrossChecker,
    attempt_id: &str,
    source: &str,
) -> Result<Read, Value> {
    let unreadable = |failure: StoreError| read_failure("store_unreadable", failure.to_string());
    let keyed = match store.payload_of(attempt_id).map_err(unreadable)? {
        Some(payload) => Some(read_blob(store, &payload.raw_blob)?),
        None => None,
    };
    let direct: Vec<IndexPayload> = store
        .index_payloads(&SourceId::new(source.to_owned()))
        .map_err(unreadable)?
        .into_iter()
        .filter(|payload| checker.is_direct_index(&payload.url))
        .collect();
    let watched_from = direct.first().and_then(|first| instant(&first.received_at));
    let since = keyed.as_deref().and_then(|bytes| checker.read_from(bytes));
    let mut answers = Vec::new();
    for payload in &direct {
        let Some(at) = instant(&payload.received_at) else {
            continue;
        };
        if since.is_some_and(|since| at >= since) {
            answers.push(DirectAnswer {
                url: payload.url.clone(),
                received_at: at,
                bytes: read_blob(store, &payload.raw_blob)?,
            });
        }
    }
    Ok(Read {
        keyed,
        direct: answers,
        watched_from,
    })
}

fn instant(stamp: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(stamp)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// A driven run's window refusal before the store, the lock or an envelope exists (section 5.1).
/// Only `WindowRefused`: edgar's no-lag `PlanRefused` keeps its policy copy and envelope (D5),
/// and an unreadable input is still reported by `drive`, after the envelope, as before.
fn refuse_the_window(ask: &Ask, provider: &dyn Provider) -> Result<(), CollectRunError> {
    let Ok((calendar, listing)) = inputs(ask) else {
        return Ok(());
    };
    let at = ObservationTime::new(ask.at);
    match provider.plan(&target_of(ask), calendar.as_ref(), listing.as_ref(), at) {
        Err(refusal) => match CollectRunError::of_plan(refusal) {
            refused @ CollectRunError::WindowRefused(_) => Err(refused),
            _ => Ok(()),
        },
        Ok(_) => Ok(()),
    }
}

/// Section 8 rows R4(b) and R19(b): every admitted row's newest answered robots fetch in the
/// store is its pin, and the pinned robots allows the row's paths and its pace. Read before the
/// lease; a refresh running alongside can only add a newer answer.
fn refuse_an_unverified_robots(ask: &Ask, opened: &Opened) -> Result<(), CollectRunError> {
    if opened.admitted_robots.is_empty() {
        return Ok(());
    }
    let ledger = crate::store::RobotsLedger::open(&ask.store)?;
    let per_host = opened.provider.transport().budget.per_host;
    for row in &opened.admitted_robots {
        let pinned = row
            .pin
            .as_ref()
            .map_or_else(String::new, |pin| pin.text().to_owned());
        let newest = match ledger.newest_robots(&row.host) {
            Ok(newest) => newest,
            Err(StoreError::BlobAbsent { digest }) => {
                return Err(CollectRunError::RobotsUnverified {
                    host: row.host.clone(),
                    pinned,
                    fetched: Some(digest),
                })
            }
            Err(other) => return Err(other.into()),
        };
        let Some(answer) = newest else {
            return Err(CollectRunError::RobotsUnverified {
                host: row.host.clone(),
                pinned,
                fetched: None,
            });
        };
        let fetched = crate::cmd_robots::fetched_pin(&answer);
        if fetched != pinned {
            return Err(CollectRunError::RobotsRefused {
                host: row.host.clone(),
                pinned,
                fetched,
            });
        }
        refuse_what_the_pinned_robots_forbids(row, pinned, per_host)?;
    }
    Ok(())
}

/// Section 11.2's path and floor checks on one row whose newest fetch is its pin.
fn refuse_what_the_pinned_robots_forbids(
    row: &AdmittedRobots,
    pinned: String,
    per_host: Option<Duration>,
) -> Result<(), CollectRunError> {
    let robots = match &row.robots {
        PinnedRobots::NoRobots => None,
        PinnedRobots::Read(robots) => Some(robots),
        // A refresh that wrote the blob after `open_provider` read the store.
        PinnedRobots::BlobMissing => {
            return Err(CollectRunError::RobotsUnverified {
                host: row.host.clone(),
                fetched: Some(pinned.clone()),
                pinned,
            })
        }
        PinnedRobots::Altered(found) => {
            return Err(CollectRunError::RobotsBlobAltered {
                host: row.host.clone(),
                pinned,
                found: found.clone(),
            })
        }
        PinnedRobots::Unreadable(why) => {
            return Err(CollectRunError::RobotsUnreadable {
                host: row.host.clone(),
                pinned,
                why: why.clone(),
            })
        }
    };
    let disallowed = row
        .paths
        .iter()
        .find(|path| robots.is_some_and(|robots| !robots.allows(path)));
    if let Some(path) = disallowed {
        return Err(CollectRunError::RobotsDisallowsPath {
            host: row.host.clone(),
            path: path.clone(),
        });
    }
    refuse_a_pace_below_the_floor(&row.host, per_host, robots.and_then(Robots::crawl_delay))
}

/// Section 8 row R19(b): `per_host` must be at least 7 s (`None` is refused), and the Crawl-delay
/// at most `per_host`, the one interval every host shares (owner decision 1, 2026-11-16).
pub fn refuse_a_pace_below_the_floor(
    host: &str,
    per_host: Option<Duration>,
    crawl_delay: Option<Duration>,
) -> Result<(), CollectRunError> {
    let Some(interval) = per_host.filter(|interval| *interval >= PACE_FLOOR) else {
        return Err(CollectRunError::PaceBelowFloor {
            host: host.to_owned(),
            per_host,
            floor: PACE_FLOOR,
        });
    };
    match crawl_delay {
        Some(delay) if crawl_delay_over_interval(delay, interval) => {
            Err(CollectRunError::CrawlDelayOverInterval {
                host: host.to_owned(),
                crawl_delay: delay,
                interval,
            })
        }
        _ => Ok(()),
    }
}

/// The one rule `collect` refuses by and `robots refresh` prints as `crawl_delay_over_interval`.
#[must_use]
pub fn crawl_delay_over_interval(crawl_delay: Duration, interval: Duration) -> bool {
    crawl_delay > interval
}

/// The plan alone, which is what `--dry-run` returns: everything except the mutation.
fn planned(
    ask: &Ask,
    opened: &Opened,
    run_id: &str,
    run_date: NaiveDate,
    stop: &Stop,
) -> Result<Answer, CollectRunError> {
    let provider = opened.provider.as_ref();
    let (calendar, listing) = inputs(ask)?;
    let target = target_of(ask);
    let (calendar_in, listing_in) = (calendar.as_ref(), listing.as_ref());
    // The same refusal `news_provider::collect` gives first, so a dry run cannot plan what a run
    // would refuse.
    provider
        .cadence()
        .admits(&provider.source(), calendar_in, listing_in)?;
    let plan = provider
        .plan(
            &target,
            calendar_in,
            listing_in,
            ObservationTime::new(ask.at),
        )
        .map_err(CollectRunError::of_plan)?;
    refuse_an_unverified_robots(ask, opened)?;
    let driven = Driven {
        collected: Collected::of_plan(&plan, &provider.source()),
        calendar_version: version_of(calendar_in),
        cross_check: Value::Null,
        continuity: Value::Null,
        walk: Value::Null,
    };
    Ok(Answer {
        // A dry run answers the plan question and nothing else, so its exit is the plan's.
        exit: driven.collected.exit(),
        envelope: envelope_of(
            ask,
            opened,
            run_id,
            run_date,
            &driven,
            &Timing {
                stop: *stop,
                wall: None,
            },
        ),
    })
}

fn run_id(ask: &Ask) -> String {
    format!(
        "{}-{}",
        ask.schedule.as_deref().unwrap_or(ADHOC),
        ask.at.format(RUN_ID_FORMAT)
    )
}

/// tmp + rename, like every other file in the layout: a run killed while its envelope is rewritten
/// otherwise leaves unparseable JSON and disables the command whose job is to report the kill.
pub(crate) fn write_envelope(root: &Path, envelope: &Envelope) -> Result<(), CollectRunError> {
    write_run_file(
        root,
        &envelope.run_id,
        &serde_json::to_vec_pretty(envelope)?,
    )
}

/// The bytes of `runs/<run_id>.json` as found, `None` when there is no such file.
fn prior_run_file(root: &Path, run_id: &str) -> Result<Option<Vec<u8>>, CollectRunError> {
    match std::fs::read(root.join(RUNS_DIR).join(format!("{run_id}.json"))) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(absent) if absent.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(other) => Err(other.into()),
    }
}

fn write_run_file(root: &Path, run_id: &str, bytes: &[u8]) -> Result<(), CollectRunError> {
    let runs = root.join(RUNS_DIR);
    std::fs::create_dir_all(&runs)?;
    let staging = runs.join(format!("{STAGING_PREFIX}{run_id}.json"));
    // `File::create` TRUNCATES, which is right HERE and wrong for a blob or a segment: one run
    // writes its envelope twice by design, and refusing a staged file a crash left behind would
    // wedge that run id for good. The archive's content files keep `create_new`.
    let mut staged = std::fs::File::create(&staging)?;
    std::io::Write::write_all(&mut staged, bytes)?;
    staged.sync_all()?;
    drop(staged);
    std::fs::rename(&staging, runs.join(format!("{run_id}.json")))?;
    // A rename is durable only once the directory holding the new name is fsynced, which is the
    // pair `vault_core::blobs` and `publish.rs` already keep.
    std::fs::File::open(&runs)?.sync_all()?;
    Ok(())
}

fn envelope_of(
    ask: &Ask,
    opened: &Opened,
    run_id: &str,
    run_date: NaiveDate,
    driven: &Driven,
    timing: &Timing,
) -> Value {
    let changed = timing.wall.is_some();
    let provider = opened.provider.as_ref();
    let collected = &driven.collected;
    let exit = collected.exit();
    let mut envelope = json!({
        "schema": SCHEMA,
        "run_id": run_id,
        "run_date": run_date.to_string(),
        "source": opened.registered.source(),
        // As planned: `today` is reported as the day it resolved to.
        "window": target_of(ask).as_str(),
        "schedule_id": ask.schedule,
        "collector_version": env!("CARGO_PKG_VERSION"),
        "calendar_version": driven.calendar_version,
        "universe_version": provider.universe_version().hex(),
        "extract_rules_version": provider.extract_rules_version().hex(),
        // Which hosts and paces this run was allowed to use (section 11.1); null for a provider
        // that reads no hosts file.
        "hosts_version": opened.hosts_version,
        // Rules plus extraction code (section 2); null for a provider that does not version its code.
        "extract_version": opened.extract_version,
        // What dated this run's archive windows, or null. Three fields and not one: the digest
        // names the file, the probe id names the measurement inside it, and the seconds are the
        // quantity every backfilled observation in this run is publication plus.
        "observation_lag_policy_version": opened.lag.as_ref().map(|lag| lag.version.clone()),
        "observation_lag_probe_id": opened.lag.as_ref().map(|lag| lag.probe_id.clone()),
        "observation_lag_seconds": opened.lag.as_ref().map(|lag| lag.seconds),
        "at": ask.at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        "deadline": ask.deadline.map(micros),
        // S7-D3: the schedule slot the deadline was computed from; null when given or none.
        "slot": timing.stop.slot.map(|slot| json!({
            "start": micros(slot.start),
            "period_s": slot.period.num_seconds(),
            "next": micros(slot.next),
        })),
        "dry_run": ask.dry_run,
        "plan": {
            "requests": collected.planned_requests,
            "not_planned": collected.not_planned.iter().map(|row| json!({
                "target_window": row.target_window,
                "reason": row.reason,
                "clean": row.clean,
            })).collect::<Vec<Value>>(),
        },
        "swept": collected.swept,
        // S7-D4: requests this run did not ask, in the order the queue reached them.
        "deferred": deferred_json(collected),
        // S7-D7: answers whose Retry-After was past the 24 h ceiling, each host held to it.
        "holds_capped": collected.capped_holds.iter().map(|hold| json!({
            "host": hold.host,
            "retry_after": hold.retry_after,
            "held_until": micros(hold.held_until),
        })).collect::<Vec<Value>>(),
        // S7-D11, null for a dry run: the run's wall time and each host's share of it.
        "busy": timing.wall.map(|wall| json!({
            "wall_s": seconds(wall),
            "hosts": collected.busy.iter().map(|lane| json!({
                "host": lane.host,
                "waited_s": seconds(lane.waited),
                "requesting_s": seconds(lane.requesting),
                "busy_s": seconds(lane.busy()),
                "requests": lane.requests,
                "stopped": lane.stopped,
                "not_looked": lane.not_looked,
                "held_until": lane.held_until.map(micros),
                "doublings": lane.doublings,
            })).collect::<Vec<Value>>(),
        })),
        "attempts": collected.attempts.iter().map(|row| json!({
            "attempt_id": row.attempt_id,
            "source": row.source,
            "target_window": row.target_window,
            "phase": row.phase,
            "provenance": row.provenance,
            "url": row.url,
            "transport_status": row.transport_status,
            "outcome": row.outcome,
            "vendor_rows": row.vendor_rows,
            "filtered_out": row.filtered_out,
            "robots_disallowed": row.robots_disallowed,
            "unusable_rows": row.unusable_rows,
            "items_committed": row.items_committed,
            "commit_refused": row.commit_refused,
        })).collect::<Vec<Value>>(),
        "per_source": per_source_json(collected),
        "provenance_counts": provenance_counts(collected),
        "items_committed": collected.tally.committed,
        "blobs_written": collected.blobs_written,
        "bytes_written": collected.bytes_written,
        "blobs_indexed": collected.blobs_indexed,
        "bytes_indexed": collected.bytes_indexed,
        "seq_high_water": collected.seq_high_water,
        "cross_check": driven.cross_check,
        "continuity": driven.continuity,
        "walk": driven.walk,
        "changed": changed && collected.tally.committed > 0,
        "exit": {
            "code": exit.code(),
            "error_code": exit.error_code(),
            "retryable": exit.retryable(),
        },
    });
    // S7-D5, outside the literal, which is at `json!`'s recursion limit: the open `deferred` rows
    // this run read, one item per url, oldest first.
    envelope["carried"] = carried_json(collected);
    envelope
}

fn deferred_json(collected: &Collected) -> Value {
    collected
        .deferred
        .iter()
        .map(|row| {
            json!({
                "source": row.source,
                "target_window": row.target_window,
                "host": row.host,
                "url": row.url,
                "phase": row.phase.name(),
                "story_id": row.story_id,
                "listed_date": row.listed_date,
                "reason": row.reason.name(),
                "go_at": row.go_at.map(micros),
            })
        })
        .collect()
}

fn carried_json(collected: &Collected) -> Value {
    collected
        .carried
        .iter()
        .map(|item| {
            json!({
                "source": item.source,
                "target_window": item.target_window,
                "host": item.host,
                "url": item.url,
                "story_id": item.story_id,
                "deferred_ids": item.deferred_ids,
                "first_deferred_at": micros(item.first_deferred_at),
                "queued": item.queued,
                "closed_as": item.closed.as_ref().map(Closing::name),
                "attempt_id": item.closed.as_ref().and_then(Closing::attempt_id),
            })
        })
        .collect()
}

fn per_source_json(collected: &Collected) -> Value {
    collected
        .per_source
        .iter()
        .map(|row| {
            json!({
                "source": row.source,
                "planned_requests": row.planned_requests,
                "not_planned": row.not_planned,
                "attempts": row.attempts,
                "ok": row.ok,
                "empty": row.empty,
                "failed": row.failed,
                "vendor_rows": row.vendor_rows,
                "filtered_out": row.filtered_out,
                "robots_disallowed": row.robots_disallowed,
                "unusable_rows": row.unusable_rows,
                "items_committed": row.items_committed,
                "deferred": row.deferred,
                "carried": {
                    "queued": row.carried.queued,
                    "answered": row.carried.answered,
                    "held": row.carried.held,
                    "expired": row.carried.expired,
                },
            })
        })
        .collect()
}

fn micros(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// The mix of retrieval models this run read under, because two of them are two populations
/// (architecture.md section 8 row R10's disclosure clause).
fn provenance_counts(collected: &Collected) -> Value {
    let mut counts: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
    for attempt in &collected.attempts {
        *counts.entry(attempt.provenance).or_default() += 1;
    }
    json!(counts)
}

/// The human rendering of the same object the JSON carries.
pub fn render_human(envelope: &Value, out: &mut impl std::io::Write) -> std::io::Result<()> {
    writeln!(out, "run              {}", text(envelope, "run_id"))?;
    writeln!(out, "run date         {}", text(envelope, "run_date"))?;
    writeln!(
        out,
        "plan             {} request(s), {} not planned",
        envelope["plan"]["requests"],
        envelope["plan"]["not_planned"]
            .as_array()
            .map_or(0, Vec::len)
    )?;
    let none = Vec::new();
    for attempt in envelope["attempts"].as_array().unwrap_or(&none) {
        writeln!(
            out,
            "  {:<9} {:<8} rows {} filtered {} robots {} items {}",
            text(attempt, "phase"),
            text(attempt, "outcome"),
            attempt["vendor_rows"],
            attempt["filtered_out"],
            attempt["robots_disallowed"],
            attempt["items_committed"],
        )?;
    }
    writeln!(out, "items committed  {}", envelope["items_committed"])?;
    let cross_check = &envelope["cross_check"];
    let checked = &cross_check["result"];
    if !cross_check["error"].is_null() {
        writeln!(
            out,
            "cross-check      error {}",
            text(&cross_check["error"], "code")
        )?;
    } else if !cross_check.is_null() {
        writeln!(
            out,
            "cross-check      {} unlisted directly, {} listed, {} not compared, {} not looked, \
             {} with no direct index",
            checked["unlisted_count"],
            checked["listed_directly"],
            checked["not_compared"],
            checked["not_looked"],
            checked["no_direct_index"]
        )?;
    }
    for poll in envelope["continuity"].as_array().unwrap_or(&none) {
        let pages = poll["pages_read"].as_array().map_or(0, Vec::len);
        let state = poll["continuity"]
            .as_str()
            .or_else(|| poll["error"]["code"].as_str())
            .unwrap_or("no usable page");
        let boundaries: String = Generation::ALL
            .iter()
            .filter_map(|label| {
                let count = poll["boundaries"].get(label.name())?;
                Some(format!(" {} {count}", label.name()))
            })
            .collect();
        writeln!(
            out,
            "continuity       {} pages {pages} {state}{boundaries}",
            text(poll, "index_url")
        )?;
    }
    for poll in envelope["walk"].as_array().unwrap_or(&none) {
        let state = poll["continuity"]
            .as_str()
            .or_else(|| poll["error"]["code"].as_str())
            .unwrap_or("no usable answer");
        let listed = |field: &str| poll[field].as_array().map_or(0, Vec::len);
        writeln!(
            out,
            "walk             {} ids {}..{} asked {} {state} unread {} gone {} other_language {}",
            text(poll, "index_url"),
            poll["after"],
            poll["through"],
            poll["asked"],
            listed("unread"),
            listed("gone"),
            listed("other_language"),
        )?;
    }
    writeln!(
        out,
        "blobs            {} written, {} indexed",
        envelope["blobs_written"], envelope["blobs_indexed"]
    )?;
    writeln!(
        out,
        "swept            {}",
        envelope["swept"].as_array().map_or(0, Vec::len)
    )?;
    let deferred = envelope["deferred"].as_array().unwrap_or(&none);
    let by_reason: String = DeferReason::ALL
        .iter()
        .map(|reason| {
            let rows = deferred
                .iter()
                .filter(|row| text(row, "reason") == reason.name())
                .count();
            format!(" {} {rows}", reason.name())
        })
        .collect();
    writeln!(out, "deferred         {}{by_reason}", deferred.len())?;
    let carried = envelope["carried"].as_array().unwrap_or(&none);
    let by_closing: String = Closing::NAMES
        .iter()
        .map(|name| {
            let items = carried
                .iter()
                .filter(|item| item["closed_as"].as_str() == Some(name))
                .count();
            format!(" {name} {items}")
        })
        .collect();
    let queued = carried
        .iter()
        .filter(|item| item["queued"].as_bool() == Some(true))
        .count();
    writeln!(
        out,
        "carried          {} queued {queued}{by_closing}",
        carried.len()
    )?;
    writeln!(
        out,
        "holds capped     {}",
        envelope["holds_capped"].as_array().map_or(0, Vec::len)
    )?;
    writeln!(
        out,
        "deadline         {} slot {}",
        envelope["deadline"].as_str().unwrap_or("-"),
        envelope["slot"]["start"].as_str().unwrap_or("-")
    )?;
    let busy = &envelope["busy"];
    if !busy.is_null() {
        writeln!(out, "busy             wall_s {}", busy["wall_s"])?;
        for lane in busy["hosts"].as_array().unwrap_or(&none) {
            writeln!(
                out,
                "  {} waited_s {} requesting_s {} busy_s {} requests {} stopped {} \
                 not_looked {} held_until {} doublings {}",
                lane["host"].as_str().unwrap_or("-"),
                lane["waited_s"],
                lane["requesting_s"],
                lane["busy_s"],
                lane["requests"],
                lane["stopped"],
                lane["not_looked"],
                lane["held_until"].as_str().unwrap_or("-"),
                lane["doublings"],
            )?;
        }
    }
    writeln!(out, "exit             {}", envelope["exit"]["code"])?;
    Ok(())
}

fn text<'a>(of: &'a Value, key: &str) -> &'a str {
    of[key].as_str().unwrap_or_default()
}

/// The store side of the driver. This is where a provider's row becomes an archive row, which is
/// the first production caller of `canon`'s three item digests (architecture.md section 8 row R7).
impl Archive for Committer {
    fn put_blob(&self, payload: &[u8]) -> Result<String, ArchiveError> {
        Committer::put_blob(self, payload)
            .map(|digest| digest.hex().to_owned())
            .map_err(|why| refused("store a payload", &why))
    }

    fn commit(
        &mut self,
        closed: AttemptReceipt,
        items: &[CollectedItem],
        payload: Option<&Payload>,
        observed: ObservationTime,
    ) -> Result<Committed, ArchiveError> {
        let raw_blob = payload.map(|payload| payload.raw_blob.as_str());
        self.labelled_as_attempt(closed.attempt_id().hex(), items)
            .map_err(|why| refused("build an item row", &why))?;
        let rows = self
            .rows_of(items, raw_blob, observed)
            .map_err(|why| refused("build an item row", &why))?;
        // The attempt's own close instant, so the row's `committed_at` is not a third clock.
        let at = closed.closed_at();
        let receipt = Committer::record(self, closed, payload, &rows, at)
            .map_err(|why| refused("commit", &why))?;
        Ok(Committed {
            items_committed: receipt.items_committed,
            blobs_indexed: receipt.blobs_indexed,
            bytes_indexed: receipt.bytes,
            seq_hi: receipt.seq_hi,
            changed: receipt.changed,
        })
    }

    fn ok_vendor_rows(&self, source: &SourceId) -> Result<Vec<u32>, ArchiveError> {
        Committer::ok_vendor_rows(self, source).map_err(|why| refused("read the baseline", &why))
    }

    fn attempts_in_flight(&self) -> Result<Vec<String>, ArchiveError> {
        Committer::attempts_in_flight(self)
            .map_err(|why| refused("read the attempts in flight", &why))
    }

    fn covered_story_ids(
        &self,
        source: &SourceId,
        target_window: &TargetWindow,
    ) -> Result<std::collections::BTreeSet<String>, ArchiveError> {
        Committer::covered_story_ids(self, source, target_window)
            .map_err(|why| refused("read the covered set", &why))
    }

    fn held_story_ids(
        &self,
        source: &SourceId,
        body_limit: u64,
    ) -> Result<std::collections::BTreeSet<String>, ArchiveError> {
        Committer::held_story_ids(self, source, body_limit)
            .map_err(|why| refused("read the held set", &why))
    }

    fn record_planned_coverage(
        &mut self,
        attempt_id: &str,
        matched: u32,
    ) -> Result<(), ArchiveError> {
        Committer::record_planned_coverage(self, attempt_id, matched)
            .map_err(|why| refused("record planned coverage", &why))
    }

    fn record_held(&mut self, attempt_id: &str, held: u32) -> Result<(), ArchiveError> {
        Committer::record_held(self, attempt_id, held)
            .map_err(|why| refused("record the held count", &why))
    }

    fn record_relisted(
        &mut self,
        attempt_id: &str,
        relisted: &[Relisted],
    ) -> Result<(), ArchiveError> {
        Committer::record_relisted(self, attempt_id, relisted)
            .map_err(|why| refused("record the re-listed stories", &why))
    }

    fn record_deferred(&mut self, deferred: &Deferred) -> Result<(), ArchiveError> {
        Committer::record_deferred(self, deferred)
            .map_err(|why| refused("record a deferred request", &why))
    }

    fn open_deferred(&self, source: &SourceId) -> Result<Vec<OpenDeferred>, ArchiveError> {
        Committer::open_deferred(self, source)
            .map_err(|why| refused("read the deferred rows", &why))
    }

    fn released_cuts(
        &self,
        source: &SourceId,
        body_limit: u64,
    ) -> Result<Vec<ReleasedCut>, ArchiveError> {
        Committer::released_cuts(self, source, body_limit)
            .map_err(|why| refused("read the released cuts", &why))
    }

    fn close_deferred(
        &mut self,
        deferred_ids: &[i64],
        closing: &Closing,
        at: ObservationTime,
    ) -> Result<(), ArchiveError> {
        Committer::close_deferred(self, deferred_ids, closing, at)
            .map_err(|why| refused("close a deferred request", &why))
    }
}

fn refused(act: &'static str, why: &StoreError) -> ArchiveError {
    ArchiveError {
        act,
        detail: why.to_string(),
    }
}

impl Committer {
    /// Refuses the first item whose provenance is not its attempt's stored one. Reads no row
    /// when there is no item, so an unknown attempt stays `record`'s refusal.
    fn labelled_as_attempt(
        &self,
        attempt: &str,
        items: &[CollectedItem],
    ) -> Result<(), StoreError> {
        if items.is_empty() {
            return Ok(());
        }
        let recorded = self.provenance_of(attempt)?;
        match items
            .iter()
            .find(|item| recorded.as_deref() != Some(item.provenance.name()))
        {
            Some(item) => Err(StoreError::ProvenanceDiffersFromAttempt {
                attempt: attempt.to_owned(),
                item: item.source_item_id.clone(),
                offered: item.provenance.name(),
                recorded,
            }),
            None => Ok(()),
        }
    }

    /// One provider row as an archive row: the body is a blob of its own, because `item_id`'s
    /// `body_ref` is that blob's digest and an edit to the body has to move the identity.
    fn rows_of(
        &self,
        items: &[CollectedItem],
        raw_blob: Option<&str>,
        observed: ObservationTime,
    ) -> Result<Vec<ItemRow>, StoreError> {
        let mut rows = Vec::with_capacity(items.len());
        for item in items {
            let Some(raw) = raw_blob else {
                return Err(StoreError::BlobAbsent {
                    digest: item.source_item_id.clone(),
                });
            };
            let body = match &item.body {
                Some(body) => Some(Committer::put_blob(self, body.text.as_bytes())?),
                None => None,
            };
            let Some(valid) = stamp_of(&item.valid) else {
                return Err(StoreError::ItemHasNoPublicationValue {
                    item: item.source_item_id.clone(),
                });
            };
            // A live ingest observes when the fetch returned; a vendor archive observes where its
            // own publication plus the probed lag put it, which only the provider can compute
            // because only the provider holds the lag. Never a fallback between the two.
            let observed = match (item.provenance, item.observed) {
                (Provenance::LiveIngest, Some(_)) => {
                    return Err(StoreError::LiveItemPlacedItsOwnObservation {
                        item: item.source_item_id.clone(),
                    })
                }
                (_, Some(placed)) => placed,
                (Provenance::VendorArchive, None) => {
                    return Err(StoreError::ArchiveItemHasNoObservation {
                        item: item.source_item_id.clone(),
                    })
                }
                (_, None) => observed,
            };
            rows.push(ItemRow {
                item_id: named(
                    item,
                    item_id(&ItemFields {
                        source: item.source.as_str(),
                        source_item_id: &item.source_item_id,
                        url: &item.url,
                        valid,
                        title: &item.title,
                        body_ref: body.as_ref().map(crate::blobs::BlobDigest::hex),
                    }),
                )?,
                story_id: named(item, story_id(item.source.as_str(), &item.source_item_id))?,
                doc_id: Some(named(item, doc_id(&item.canonical_url))?),
                valid: item.valid,
                valid_raw: item.valid_raw.clone(),
                valid_field: item.valid_field.clone(),
                valid_source: item.valid_source,
                observed,
                raw_blob: self.blob_named(raw)?,
                body_blob: body,
            });
        }
        Ok(rows)
    }
}

/// A refused canonical form names the row it refused, so a reader of the error knows which item
/// of a payload has no identity.
fn named(
    item: &CollectedItem,
    digest: Result<canon::Digest, canon::CanonError>,
) -> Result<canon::Digest, StoreError> {
    digest.map_err(|why| StoreError::ItemIdentityRefused {
        item: item.source_item_id.clone(),
        why: why.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::{Archive, ArchiveError, Committer, Provenance, StoreError};
    use crate::store::COMMIT_LOG;
    use canon::FileDigest;
    use news_provider::provider::CollectedItem;
    use news_provider::{
        AttemptReceipt, AttemptSlot, Baseline, Counts, Headers, Outcome, Payload, RawResponse,
        Request, RequestPhase, RunId, SourceId, TargetWindow, ValidSource, Verdict,
    };
    use pit::datedness::Published;
    use pit::{ObservationTime, ValidTime};
    use rusqlite::Connection;
    use tempfile::TempDir;

    const RUN: &str = "a-run";
    const SOURCE: &str = "edgar_press";
    const WINDOW: &str = "2025-11-10";
    const ITEM: &str = "0000320193-26-000071";
    const SECOND_ITEM: &str = "0000320193-26-000072";
    const VALID_RAW: &str = "20251110213215";
    const VALID_FIELD: &str = "ACCEPTANCE-DATETIME";
    const PUBLISHED_AT: i64 = 1_762_810_335;
    const FETCHED_AT: i64 = 1_763_000_000;

    fn instant(seconds: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(seconds, 0).unwrap_or(chrono::DateTime::UNIX_EPOCH)
    }

    fn archived(observed: Option<ObservationTime>) -> CollectedItem {
        CollectedItem {
            source: SourceId::new(SOURCE),
            source_item_id: ITEM.to_owned(),
            url: "https://example.test/a".to_owned(),
            canonical_url: "/a".to_owned(),
            title: "A filing".to_owned(),
            valid: Published::At(ValidTime::new(instant(PUBLISHED_AT))),
            valid_raw: String::new(),
            valid_field: "none".to_owned(),
            valid_source: ValidSource::VendorPublished,
            provenance: Provenance::VendorArchive,
            observed,
            body: None,
        }
    }

    /// The store never fills a missing archive observation in from the fetch clock. Unreachable
    /// through `edgar-press`, whose `plan` refuses an archive window with no probed lag before any
    /// request exists - so this is the gate for the NEXT provider, exercised here through the one
    /// path that can reach it.
    #[test]
    fn an_archive_row_with_no_placed_observation_is_refused_rather_than_dated_by_the_fetch() {
        let temp = match TempDir::new() {
            Ok(temp) => temp,
            Err(why) => {
                assert_eq!(why.to_string(), "a temporary directory");
                return;
            }
        };
        let Ok(store) = Committer::open(temp.path(), &RunId::new(RUN)) else {
            assert_eq!(temp.path().display().to_string(), "an opened store");
            return;
        };
        let Ok(raw) = Committer::put_blob(&store, b"payload") else {
            assert_eq!(temp.path().display().to_string(), "a stored blob");
            return;
        };
        let refused = store.rows_of(
            &[archived(None)],
            Some(raw.hex()),
            ObservationTime::new(instant(FETCHED_AT)),
        );
        assert!(
            matches!(refused, Err(StoreError::ArchiveItemHasNoObservation { .. })),
            "{refused:?}"
        );
    }

    /// A live item that placed an instant of its own would replace the fetch clock, and an earlier
    /// one dates it before we read it.
    #[test]
    fn a_live_row_that_placed_its_own_observation_is_refused() {
        let Ok(temp) = TempDir::new() else { return };
        let Ok(store) = Committer::open(temp.path(), &RunId::new(RUN)) else {
            return;
        };
        let Ok(raw) = Committer::put_blob(&store, b"payload") else {
            return;
        };
        let live = CollectedItem {
            provenance: Provenance::LiveIngest,
            ..archived(Some(ObservationTime::new(instant(PUBLISHED_AT + 1))))
        };
        let refused = store.rows_of(
            &[live],
            Some(raw.hex()),
            ObservationTime::new(instant(FETCHED_AT)),
        );
        assert!(
            matches!(
                refused,
                Err(StoreError::LiveItemPlacedItsOwnObservation { .. })
            ),
            "{refused:?}"
        );
    }

    /// The other direction: a placed observation is the one stored, and the fetch instant handed
    /// alongside it is ignored.
    #[test]
    fn a_placed_archive_observation_is_the_one_stored_and_not_the_fetch_instant() {
        let Ok(temp) = TempDir::new() else { return };
        let Ok(store) = Committer::open(temp.path(), &RunId::new(RUN)) else {
            return;
        };
        let Ok(raw) = Committer::put_blob(&store, b"payload") else {
            return;
        };
        let placed = ObservationTime::new(instant(PUBLISHED_AT + 7_200));
        let rows = store.rows_of(
            &[archived(Some(placed))],
            Some(raw.hex()),
            ObservationTime::new(instant(FETCHED_AT)),
        );
        match rows {
            Ok(rows) => assert_eq!(rows.first().map(|row| row.observed), Some(placed)),
            Err(why) => assert_eq!(why.to_string(), "rows"),
        }
    }

    /// Through `rows_of` and the store's one door into the columns, so a writer dropping any of
    /// the three on either side of the hand-over is red.
    #[test]
    fn an_items_row_keeps_the_raw_publication_value_its_field_and_its_source(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let mut store = Committer::open(temp.path(), &RunId::new(RUN))?;
        let raw = Committer::put_blob(&store, b"payload")?;
        let live = CollectedItem {
            provenance: Provenance::LiveIngest,
            valid_raw: VALID_RAW.to_owned(),
            valid_field: VALID_FIELD.to_owned(),
            valid_source: ValidSource::OurExtraction,
            ..archived(None)
        };
        let rows = store.rows_of(
            &[live],
            Some(raw.hex()),
            ObservationTime::new(instant(FETCHED_AT)),
        )?;
        let request = Request {
            source: SourceId::new(SOURCE),
            target_window: TargetWindow::new(WINDOW),
            phase: RequestPhase::Document,
            provenance: Provenance::LiveIngest,
            url: "https://example.test/a".to_owned(),
            headers: Vec::new(),
            params: Vec::new(),
            universe_version: FileDigest::of(b"{}"),
            listed_date: None,
            names_ticker: false,
            hosts_version: None,
            extract_version: None,
        };
        let at = ObservationTime::new(instant(FETCHED_AT));
        let slot = AttemptSlot::open(&mut store, &request, None, &RunId::new(RUN), 0, at)?;
        let answer = RawResponse::new(slot.attempt_id().clone(), 200, b"payload".to_vec(), at, at);
        let outcome = Outcome::decide(
            &Verdict::Usable,
            Counts {
                vendor_rows: 1,
                listing_unusable: 0,
                robots_disallowed: 0,
            },
            &Baseline::from_trailing_ok(&[1]),
        );
        let closed = slot.close(&mut store, outcome, &Ok(answer), at)?;
        store.record(closed, None, &rows, at)?;
        drop(store);
        let db = Connection::open(temp.path().join(COMMIT_LOG))?;
        let stored: (String, String, String) = db.query_row(
            "SELECT valid_raw, valid_field, valid_source FROM items",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(
            stored,
            (
                VALID_RAW.to_owned(),
                VALID_FIELD.to_owned(),
                ValidSource::OurExtraction.name().to_owned()
            )
        );
        Ok(())
    }
    fn live_request() -> Request {
        Request {
            source: SourceId::new(SOURCE),
            target_window: TargetWindow::new(WINDOW),
            phase: RequestPhase::Index,
            provenance: Provenance::LiveIngest,
            url: "https://example.test/a".to_owned(),
            headers: Vec::new(),
            params: Vec::new(),
            universe_version: FileDigest::of(b"{}"),
            listed_date: None,
            names_ticker: false,
            hosts_version: None,
            extract_version: None,
        }
    }

    /// Opens attempt 0 of [`live_request`], answers it usable with one row and closes it.
    fn closed_live_attempt(
        store: &mut Committer,
    ) -> Result<AttemptReceipt, Box<dyn std::error::Error>> {
        let at = ObservationTime::new(instant(FETCHED_AT));
        let slot = AttemptSlot::open(store, &live_request(), None, &RunId::new(RUN), 0, at)?;
        let answer = RawResponse::new(slot.attempt_id().clone(), 200, b"payload".to_vec(), at, at);
        let outcome = Outcome::decide(
            &Verdict::Usable,
            Counts {
                vendor_rows: 1,
                listing_unusable: 0,
                robots_disallowed: 0,
            },
            &Baseline::from_trailing_ok(&[1]),
        );
        Ok(slot.close(store, outcome, &Ok(answer), at)?)
    }

    fn a_payload(store: &Committer) -> Result<Payload, Box<dyn std::error::Error>> {
        Ok(Payload {
            raw_blob: Committer::put_blob(store, b"payload")?.hex().to_owned(),
            url: "https://example.test/a".to_owned(),
            phase: RequestPhase::Index,
            listed_date: None,
            headers: Headers::default(),
            truncated: false,
        })
    }

    /// Commits `items` under `closed` into `store` through the driver's door, `Archive::commit`.
    fn commit_into(
        store: &mut Committer,
        closed: AttemptReceipt,
        items: &[CollectedItem],
    ) -> Result<Result<u32, ArchiveError>, Box<dyn std::error::Error>> {
        let payload = a_payload(store)?;
        let fetched = ObservationTime::new(instant(FETCHED_AT));
        Ok(
            Archive::commit(store, closed, items, Some(&payload), fetched)
                .map(|committed| committed.items_committed),
        )
    }

    /// Commits `items` under a closed live attempt of the same store.
    fn commit_under_a_live_attempt(
        store: &mut Committer,
        items: &[CollectedItem],
    ) -> Result<Result<u32, ArchiveError>, Box<dyn std::error::Error>> {
        let closed = closed_live_attempt(store)?;
        commit_into(store, closed, items)
    }

    fn attempt_hex() -> Result<String, Box<dyn std::error::Error>> {
        let attempt =
            canon::digest::attempt_id(RUN, SOURCE, WINDOW, 0).map_err(|why| why.to_string())?;
        Ok(attempt.hex().to_owned())
    }

    fn refusal_of(
        item: &str,
        offered: Provenance,
        recorded: Option<&str>,
    ) -> Result<Result<u32, ArchiveError>, Box<dyn std::error::Error>> {
        Ok(Err(ArchiveError {
            act: "build an item row",
            detail: StoreError::ProvenanceDiffersFromAttempt {
                attempt: attempt_hex()?,
                item: item.to_owned(),
                offered: offered.name(),
                recorded: recorded.map(str::to_owned),
            }
            .to_string(),
        }))
    }

    fn refusal(
        offered: Provenance,
        recorded: Option<&str>,
    ) -> Result<Result<u32, ArchiveError>, Box<dyn std::error::Error>> {
        refusal_of(ITEM, offered, recorded)
    }

    fn not_opened(
        act: &'static str,
    ) -> Result<Result<u32, ArchiveError>, Box<dyn std::error::Error>> {
        Ok(Err(ArchiveError {
            act,
            detail: StoreError::AttemptNotOpened {
                attempt: attempt_hex()?,
            }
            .to_string(),
        }))
    }

    /// §8 row R10's record-time check: the item's label, which picked its `observed_at`, is the
    /// attempt's stored one, and a matching item still commits.
    #[test]
    fn an_item_whose_provenance_differs_from_its_attempt_is_refused(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (differing, matching) = (TempDir::new()?, TempDir::new()?);
        let placed = ObservationTime::new(instant(PUBLISHED_AT + 7_200));
        let mut store = Committer::open(differing.path(), &RunId::new(RUN))?;
        let refused = commit_under_a_live_attempt(&mut store, &[archived(Some(placed))])?;
        let mut store = Committer::open(matching.path(), &RunId::new(RUN))?;
        let live = CollectedItem {
            provenance: Provenance::LiveIngest,
            ..archived(None)
        };
        let committed = commit_under_a_live_attempt(&mut store, &[live])?;
        assert_eq!(
            (refused, committed),
            (
                refusal(Provenance::VendorArchive, Some("live_ingest"))?,
                Ok(1)
            )
        );
        Ok(())
    }

    /// An attempt whose label is not recorded (an `edgar_press` row from before user_version 17)
    /// takes no item, and is never read as live.
    #[test]
    fn an_item_under_an_attempt_with_no_recorded_provenance_is_refused(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let mut store = Committer::open(temp.path(), &RunId::new(RUN))?;
        let attempt =
            canon::digest::attempt_id(RUN, SOURCE, WINDOW, 0).map_err(|why| why.to_string())?;
        Connection::open(temp.path().join(COMMIT_LOG))?.execute(
            "INSERT INTO attempts (attempt_id, run_id, source, target_window, filter_params,
                 universe_version, opened_at)
             VALUES (?1, ?2, ?3, ?4, 'f', 'u', '2025-11-13T02:13:20.000000Z')",
            [attempt.hex(), RUN, SOURCE, WINDOW],
        )?;
        let live = CollectedItem {
            provenance: Provenance::LiveIngest,
            ..archived(None)
        };
        let refused = commit_under_a_live_attempt(&mut store, &[live])?;
        assert_eq!(refused, refusal(Provenance::LiveIngest, None)?);
        Ok(())
    }
    /// Each committed item is checked, not only the first: a differing item after a matching
    /// one is the one the refusal names.
    #[test]
    fn a_differing_item_after_a_matching_one_is_the_one_refused(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let mut store = Committer::open(temp.path(), &RunId::new(RUN))?;
        let matching = CollectedItem {
            provenance: Provenance::LiveIngest,
            ..archived(None)
        };
        let differing = CollectedItem {
            source_item_id: SECOND_ITEM.to_owned(),
            url: "https://example.test/b".to_owned(),
            canonical_url: "/b".to_owned(),
            ..archived(Some(ObservationTime::new(instant(PUBLISHED_AT + 7_200))))
        };
        let refused = commit_under_a_live_attempt(&mut store, &[matching, differing])?;
        assert_eq!(
            refused,
            refusal_of(SECOND_ITEM, Provenance::VendorArchive, Some("live_ingest"))?
        );
        Ok(())
    }

    /// An attempt this store has no row for is `AttemptNotOpened`, never a "not recorded"
    /// label: the receipt comes from another store.
    #[test]
    fn an_item_under_an_attempt_this_store_never_opened_is_refused_as_not_opened(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (elsewhere, here) = (TempDir::new()?, TempDir::new()?);
        let closed =
            closed_live_attempt(&mut Committer::open(elsewhere.path(), &RunId::new(RUN))?)?;
        let mut store = Committer::open(here.path(), &RunId::new(RUN))?;
        let live = CollectedItem {
            provenance: Provenance::LiveIngest,
            ..archived(None)
        };
        let refused = commit_into(&mut store, closed, &[live])?;
        assert_eq!(refused, not_opened("build an item row")?);
        Ok(())
    }

    /// With no item the provenance check reads nothing, so an unknown attempt is refused by
    /// `record`'s own door, under its own act.
    #[test]
    fn an_empty_commit_under_an_attempt_this_store_never_opened_is_records_refusal(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (elsewhere, here) = (TempDir::new()?, TempDir::new()?);
        let closed =
            closed_live_attempt(&mut Committer::open(elsewhere.path(), &RunId::new(RUN))?)?;
        let mut store = Committer::open(here.path(), &RunId::new(RUN))?;
        let refused = commit_into(&mut store, closed, &[])?;
        assert_eq!(refused, not_opened("commit")?);
        Ok(())
    }
}
