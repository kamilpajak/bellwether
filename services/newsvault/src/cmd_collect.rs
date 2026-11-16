//! `newsvault collect` - the one command that writes to the archive. What it reports and which
//! exit each state takes are architecture.md section 5.1; the exit table itself is
//! `news_provider::Tally::exit` and is not restated here.
//!
//! The run envelope is on disk with `finished_at` null BEFORE the lease is attempted, so a run
//! that dies holding nothing still left a record that it started (accepted attack A3.13).

use std::path::{Path, PathBuf};
use std::time::Duration;

use canon::digest::{doc_id, item_id, story_id, ItemFields};
use chrono::{DateTime, NaiveDate, Utc};
use edgar_press::EdgarPress;
use news_provider::lag_policy::{LagPolicy, LagPolicyError};
use news_provider::provider::{Cadence, Calendar, CalendarError, Listing, PlanRefusal, Provenance};
use news_provider::{
    stamp_of, Archive, ArchiveError, AttemptReceipt, Clock, CollectError, Collected, CollectedItem,
    Committed, Credentials, Exchange, Exit, Payload, Provider, Relisted, Run, RunId, SourceId,
    TargetWindow, Wire,
};
use open_web_feed::{OpenWebFeed, RobotsPin};
use pit::{MeasuredLag, ObservationTime};
use serde_json::{json, Value};

use crate::cmd_ledger::{Envelope, RunKind, RUN_ENVELOPE_SCHEMA};
use crate::robots::{Robots, RobotsError};
use crate::store::{Committer, ItemRow, StoreError, POLICY_DIR, RUNS_DIR};

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
/// Section 11.1's 10 s, held apart from `open-web-feed`'s own interval so the floor check does not
/// move when the profile it checks does.
const PACE_FLOOR: Duration = Duration::from_secs(10);

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
    /// 10 s floor (`None`: no per-host pace at all).
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
            | Self::WindowRefused(_) => true,
            Self::InputAbsent { .. }
            | Self::UnusableCalendar { .. }
            | Self::PlanRefused(_)
            | Self::Refused(_)
            | Self::OpenWebRefused(_)
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

    /// Each provider's contact from a variable of its own, never another vendor's.
    #[must_use]
    pub const fn contact_variable(self) -> &'static str {
        match self {
            Self::EdgarPress => "BELLWETHER_SEC_CONTACT",
            Self::OpenWebWires => "BELLWETHER_FEED_CONTACT",
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
    })
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
        },
    )
    .map_err(CollectRunError::of_driver)?;
    Ok(Driven {
        collected,
        calendar_version: version_of(calendar.as_ref()),
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
    if ask.dry_run {
        return planned(ask, opened, &run_id, run_date);
    }
    // A dry run may name another day, which tests and plans need; a fetch may not.
    let today = clock.now().date_naive();
    if provider.cadence() == Cadence::EveryUtcDay && run_date != today {
        return Err(CollectRunError::AtIsNotTheClockDay {
            source_id: opened.registered.source(),
            at: run_date,
            today,
        });
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
        expected_runs: ask.expected_runs,
        started_at: ask.at,
        finished_at: None,
        kind: RunKind::Collect,
    };
    // Before the lease, so a run that never gets one is a record rather than a hole.
    write_envelope(&ask.store, &envelope)?;
    let mut store = Committer::open(&ask.store, &run)?;
    let driven = drive(ask, provider, &mut store, exchange, clock, &run)?;
    let finished = Envelope {
        finished_at: Some(clock.now()),
        ..envelope
    };
    write_envelope(&ask.store, &finished)?;
    Ok(Answer {
        exit: driven.collected.exit(),
        envelope: envelope_of(ask, opened, &run_id, run_date, &driven, true),
    })
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

/// Section 8 row R19(b): `per_host` must be at least 10 s (`None` is refused), and the Crawl-delay
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
    };
    Ok(Answer {
        // A dry run answers the plan question and nothing else, so its exit is the plan's.
        exit: driven.collected.exit(),
        envelope: envelope_of(ask, opened, run_id, run_date, &driven, false),
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
    let runs = root.join(RUNS_DIR);
    std::fs::create_dir_all(&runs)?;
    let staging = runs.join(format!("{STAGING_PREFIX}{}.json", envelope.run_id));
    // `File::create` TRUNCATES, which is right HERE and wrong for a blob or a segment: one run
    // writes its envelope twice by design, and refusing a staged file a crash left behind would
    // wedge that run id for good. The archive's content files keep `create_new`.
    let mut staged = std::fs::File::create(&staging)?;
    std::io::Write::write_all(&mut staged, &serde_json::to_vec_pretty(envelope)?)?;
    staged.sync_all()?;
    drop(staged);
    std::fs::rename(&staging, runs.join(format!("{}.json", envelope.run_id)))?;
    // A rename is durable only once the directory holding the new name is fsynced, which is the
    // pair `blobs.rs` and `publish.rs` already keep.
    std::fs::File::open(&runs)?.sync_all()?;
    Ok(())
}

fn envelope_of(
    ask: &Ask,
    opened: &Opened,
    run_id: &str,
    run_date: NaiveDate,
    driven: &Driven,
    changed: bool,
) -> Value {
    let provider = opened.provider.as_ref();
    let collected = &driven.collected;
    let exit = collected.exit();
    json!({
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
            "unusable_rows": row.unusable_rows,
            "items_committed": row.items_committed,
            "commit_refused": row.commit_refused,
        })).collect::<Vec<Value>>(),
        "per_source": collected.per_source.iter().map(|row| json!({
            "source": row.source,
            "planned_requests": row.planned_requests,
            "not_planned": row.not_planned,
            "attempts": row.attempts,
            "ok": row.ok,
            "empty": row.empty,
            "failed": row.failed,
            "vendor_rows": row.vendor_rows,
            "filtered_out": row.filtered_out,
            "unusable_rows": row.unusable_rows,
            "items_committed": row.items_committed,
        })).collect::<Vec<Value>>(),
        "provenance_counts": provenance_counts(collected),
        "items_committed": collected.tally.committed,
        "blobs_written": collected.blobs_written,
        "bytes_written": collected.bytes_written,
        "blobs_indexed": collected.blobs_indexed,
        "bytes_indexed": collected.bytes_indexed,
        "seq_high_water": collected.seq_high_water,
        "changed": changed && collected.tally.committed > 0,
        "exit": {
            "code": exit.code(),
            "error_code": exit.error_code(),
            "retryable": exit.retryable(),
        },
    })
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
            "  {:<9} {:<8} rows {} filtered {} items {}",
            text(attempt, "phase"),
            text(attempt, "outcome"),
            attempt["vendor_rows"],
            attempt["filtered_out"],
            attempt["items_committed"],
        )?;
    }
    writeln!(out, "items committed  {}", envelope["items_committed"])?;
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
    ) -> Result<std::collections::BTreeSet<String>, ArchiveError> {
        Committer::held_story_ids(self, source).map_err(|why| refused("read the held set", &why))
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
}

fn refused(act: &'static str, why: &StoreError) -> ArchiveError {
    ArchiveError {
        act,
        detail: why.to_string(),
    }
}

impl Committer {
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
    use super::{Committer, Provenance, StoreError};
    use crate::store::COMMIT_LOG;
    use canon::FileDigest;
    use news_provider::provider::CollectedItem;
    use news_provider::{
        AttemptSlot, Baseline, Counts, Outcome, RawResponse, Request, RequestPhase, RunId,
        SourceId, TargetWindow, ValidSource, Verdict,
    };
    use pit::datedness::Published;
    use pit::{ObservationTime, ValidTime};
    use rusqlite::Connection;
    use tempfile::TempDir;

    const RUN: &str = "a-run";
    const SOURCE: &str = "edgar_press";
    const WINDOW: &str = "2025-11-10";
    const ITEM: &str = "0000320193-26-000071";
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
}
