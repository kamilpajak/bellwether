//! Our own forward news feed: dated indexes and article pages from admitted hosts, collected live
//! for the run's UTC day only. The design is `docs/design/architecture.md` section 11; GNW, PR
//! Newswire, Canada Newswire, the Federal Reserve and Yahoo Finance's wire keys are the admitted
//! hosts (section 11.5 steps 2 and 6). It declares no HTTP dependency and holds no clock and no
//! `Sink`.

mod crosscheck;
mod depth;
mod hosts;
mod paging;
mod parse;
mod rules;
mod strict;
mod version;
mod walk;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use canon::{Digest, FileDigest};
use chrono::{DateTime, NaiveDateTime, Utc};
use news_provider::provider::{
    Cadence, Calendar, CollectedItem, DecidedBy, Extracted, ExtractedBody, Listing, Plan,
    PlanRefusal, Provenance, Provider, RawBlob, UnusableReason, UnusableRow, ValidPrecision,
    ValidSource,
};
use news_provider::request::{days_of, HeaderValue, Request, RequestPhase};
use news_provider::wire::{RawResponse, Timeouts, TransportError, TransportKind, TransportProfile};
use news_provider::{CredentialRef, RequestBudget, SourceId, TargetWindow, Verdict};
use pit::datedness::{Minute, Published};
use pit::ObservationTime;
use serde_json::{Map, Value};

pub use crate::crosscheck::{
    normalised, CrossCheck, CrossChecker, DirectAnswer, Unlisted, DIRECT_LEAD, LONGEST_POLL_PERIOD,
};
pub use crate::hosts::RobotsPin;
use crate::hosts::{read_hosts, HostRow, Hosts, Status, HOSTS_SCHEMA};
pub use crate::paging::{Basis, Continuity, Generation, Paging, GENERATION_HEADER};
use crate::rules::{parse_rules, Rule, StoryKey, RULES_SCHEMA};
use crate::walk::{release_of, WalkedIndex};
pub use crate::walk::{Walk, WalkBasis, WalkPlan, WalkRange};

pub const SOURCE: &str = "open_web_wires";
/// The User-Agent, a product token plus a contact address, travels by NAME as SEC's does
/// (architecture.md section 3.2's `filter_params` row); its own name, not another vendor's.
pub const CONTACT_CREDENTIAL: &str = "open_web_contact";

const OK: u16 = 200;
const REQUEST_TIMEOUT: u16 = 408;
const TOO_MANY_REQUESTS: u16 = 429;
const SERVER_ERRORS: std::ops::RangeInclusive<u16> = 500..=599;
const REDIRECTS: std::ops::RangeInclusive<u16> = 300..=399;
const SCHEMES: [&str; 2] = ["https://", "http://"];
const NOT_PLAIN: [u8; 2] = *b"%#";
/// An ASSUMPTION taken by default, as edgar-press's is: nothing here has timed a GNW response.
const VENDOR_PATIENCE: Duration = Duration::from_secs(30);
/// The largest page of the 2026-11-14 probes was 979,950 B (cnbc.com), the GNW article 71,657 B
/// and the GNW RSS 36,687 B, all as decompressed by curl (§11.6's method).
const BODY_LIMIT: u64 = 2 * 1_024 * 1_024;
/// Section 11.1's floor, as `per_host` for every host (§8 row R19 hole 7): `newsvault collect`
/// refuses a host whose robots Crawl-delay is above it (owner decision 1, 2026-11-16). No per-IP
/// pace: each host is its own queue (owner decision 1, 2026-11-17, §10).
const INTERVAL: Duration = Duration::from_secs(10);
/// S7-D6 (owner, 2026-11-17): the owner's choice, not measured (architecture.md section 11.2).
pub const FOLLOW_UP_CAP: u32 = 150;

/// Why no request may be made at all, each raised by `open` before the first request.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Refusal {
    #[error("no hosts file at {0}")]
    HostsAbsent(PathBuf),
    #[error("the hosts file at {path} is present and is not {HOSTS_SCHEMA}: {why}")]
    HostsUnparseable { path: PathBuf, why: String },
    /// Either file: a field nothing reads is a setting nobody applies.
    #[error("{path} holds the field {field}, which this reader does not know")]
    UnknownField { path: PathBuf, field: String },
    #[error("the hosts file at {path} names {host} twice")]
    DuplicateHost { path: PathBuf, host: String },
    #[error("the hosts file at {path} names {host}, which no per-host pace file can be keyed on")]
    NotAHost { path: PathBuf, host: String },
    #[error(
        "the hosts file at {path} reads {host} under rule {rule}, which the rules do not hold"
    )]
    UnknownRule {
        path: PathBuf,
        host: String,
        rule: String,
    },
    /// The index would be asked of a host the row did not admit.
    #[error("the hosts file at {path} reads the index of {host} from {url}, which is elsewhere")]
    IndexOffHost {
        path: PathBuf,
        host: String,
        url: String,
    },
    /// Decision 4: robots.txt is matched against the path as written, which such a byte gives a
    /// second spelling.
    #[error(
        "the hosts file at {path} reads the index of {host} at {target:?}, which holds %, a non-ASCII or a control byte"
    )]
    IndexTargetNotPlain {
        path: PathBuf,
        host: String,
        target: String,
    },
    /// R6: zero admitted rows plans zero requests, which reads as a quiet day.
    #[error("the hosts file at {0} admits no host")]
    NoHostAdmitted(PathBuf),
    #[error("no extraction rules at {0}")]
    RulesAbsent(PathBuf),
    #[error("the rules at {path} are present and are not {RULES_SCHEMA}: {why}")]
    RulesUnparseable { path: PathBuf, why: String },
    #[error("the {CONTACT_CREDENTIAL} credential carries no contact address")]
    ContactMissing,
    /// Either file: `serde_json` keeps the last of two values, a reader sees the first.
    #[error("{path} names the key {key} twice in one object")]
    DuplicateKey { path: PathBuf, key: String },
    #[error(
        "the rules at {path} rewrite under rule {rule} from or to {host}, which is not a host"
    )]
    RewriteNotAHost {
        path: PathBuf,
        rule: String,
        host: String,
    },
    #[error("the rules at {path} rewrite {host} twice under rule {rule}")]
    RewriteNamedTwice {
        path: PathBuf,
        rule: String,
        host: String,
    },
    /// A url path starts with `/`, so any other prefix filters out every link.
    #[error("the rules at {path} filter rule {rule} on {prefix}, which does not start with /")]
    PathPrefixNotRooted {
        path: PathBuf,
        rule: String,
        prefix: String,
    },
    /// Decision 4, as `IndexTargetNotPlain`.
    #[error(
        "the rules at {path} filter rule {rule} on {prefix:?}, which holds %, a non-ASCII or a control byte"
    )]
    PathPrefixNotPlain {
        path: PathBuf,
        rule: String,
        prefix: String,
    },
    #[error(
        "the rules at {path} read rule {rule}'s valid time as {format}, which reads no minute"
    )]
    FormatReadsNoMinute {
        path: PathBuf,
        rule: String,
        format: String,
    },
    /// Section 11.2: a `cross_check` key's rows are "not looked" until its host's index answers,
    /// so a host no other admitted row holds would leave them unlooked forever.
    #[error(
        "the rules at {path} map rule {rule}'s cross_check key {key} to {host}, which no other admitted hosts row holds"
    )]
    CrossCheckHostNotAdmitted {
        path: PathBuf,
        rule: String,
        key: String,
        host: String,
    },
    /// Page N is page 1's url with N in place of its final `page=1` query pair (section 11.2).
    #[error(
        "the hosts file at {path} pages {host}'s index from {url}, which does not end in page=1"
    )]
    PagedIndexNotPageOne {
        path: PathBuf,
        host: String,
        url: String,
    },
    #[error(
        "the hosts file at {path} pages {host}'s index under rule {rule}, which is no news sitemap"
    )]
    PagedIndexNotASitemap {
        path: PathBuf,
        host: String,
        rule: String,
    },
    /// A document's rule is the one whose valid format reads its listed value, so only one may.
    #[error("the hosts file at {path} reads {host} under rules {rules}, whose formats read one value alike")]
    IndexFormatsOverlap {
        path: PathBuf,
        host: String,
        rules: String,
    },
    /// A walked id is one story with its listed release only when both key by release, and its
    /// page is undated unless the rule reads the page's own time (section 11.2).
    #[error(
        "the hosts file at {path} walks {host}'s ids under rule {rule}, which keys no story by release or reads no page time"
    )]
    IdWalkRuleNotReleaseKeyed {
        path: PathBuf,
        host: String,
        rule: String,
    },
    #[error(
        "the hosts file at {path} walks {host}'s ids at {url}, which does not name one release on that host"
    )]
    IdWalkNotARelease {
        path: PathBuf,
        host: String,
        url: String,
    },
}

/// What `item_key` reads: every rule's host rewrite, and each host whose rule keys stories by
/// release with that rule's path prefix.
#[derive(Debug, Clone, Default)]
pub(crate) struct Keys {
    rewrites: BTreeMap<String, String>,
    released: BTreeMap<String, String>,
}

impl Keys {
    /// The rewritten host and the path.
    pub(crate) fn rewritten(&self, url: &str) -> Option<(String, String)> {
        let (host, path) = host_and_path(url)?;
        Some((self.rewrites.get(&host).cloned().unwrap_or(host), path))
    }

    /// The rewritten host plus the path; on a host keyed by release, the host, the prefix, the
    /// id and the language, so any slug or date of one release is one key.
    pub(crate) fn key(&self, url: &str) -> Option<String> {
        let (host, path) = self.rewritten(url)?;
        match self.released.get(&host) {
            Some(prefix) => {
                let release = release_of(prefix, &path)?;
                Some(format!("{host}{prefix}{}/{}", release.id, release.language))
            }
            None => Some(format!("{host}{path}")),
        }
    }
}

/// One hosts row as `OpenWebFeed::host_pins` hands it out. `host` is lowercased.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPin {
    pub host: String,
    pub admitted: bool,
    pub robots: Option<RobotsPin>,
}

/// A path an admitted row asks of its host, which that host's robots.txt must allow: the index
/// url's path with its query, and the rule's `path_prefix`. `host` is lowercased.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedPath {
    pub host: String,
    pub path: String,
}

/// A host's pinned robots.txt as the driver parsed it (section 11.1), asked about a url's path
/// with its params and query, as robots.cc's `GetPathParamsQuery` cuts it.
pub trait RobotsRules: std::fmt::Debug + Send + Sync {
    fn allows(&self, path: &str) -> bool;
}

/// What the driver hands the feed for one admitted host.
#[derive(Debug, Clone)]
pub enum HostRobots {
    /// Pinned `absent`, a 404: every path allowed.
    Absent,
    Rules(Arc<dyn RobotsRules>),
}

#[derive(Debug)]
pub struct OpenWebFeed {
    hosts: Hosts,
    rules: BTreeMap<String, Rule>,
    rules_version: FileDigest,
    extract_version: Digest,
    /// What `item_key` applies whatever host a url names.
    keys: Keys,
    /// Keyed by lowercased host; a host absent here allows no follow-up.
    robots: BTreeMap<String, HostRobots>,
    /// Keyed by a paged index's page-1 url; an index absent here is read at page 1 only.
    basis: BTreeMap<String, Basis>,
    /// Keyed by a walked index's url; an index absent here walks no id (a first poll).
    walks: BTreeMap<String, WalkRange>,
}

impl OpenWebFeed {
    /// Reads the rules, then the hosts file against them, and refuses before any request exists.
    /// The contact is checked and not stored, as edgar-press's is.
    pub fn open(hosts: &Path, rules: &Path, contact: &str) -> Result<Self, Refusal> {
        usable_contact(contact)?;
        let rules_bytes =
            std::fs::read(rules).map_err(|_| Refusal::RulesAbsent(rules.to_path_buf()))?;
        let parsed = parse_rules(&rules_bytes, rules)?;
        let mut rewrites = BTreeMap::new();
        for rule in parsed.values() {
            for (from, to) in &rule.rewrite_hosts {
                if rewrites
                    .insert(from.clone(), to.clone())
                    .is_some_and(|was| was != *to)
                {
                    return Err(Refusal::RulesUnparseable {
                        path: rules.to_path_buf(),
                        why: format!("{from} is rewritten to two hosts"),
                    });
                }
            }
        }
        let rules_version = FileDigest::of(&rules_bytes);
        let extract_version = version::extract_version(&rules_version).map_err(|error| {
            Refusal::RulesUnparseable {
                path: rules.to_path_buf(),
                why: format!("the extraction version has no canonical form: {error}"),
            }
        })?;
        let hosts = read_hosts(hosts, &parsed)?;
        cross_checked_hosts_admitted(&hosts, &parsed, rules)?;
        let released = hosts
            .rows
            .iter()
            .filter_map(|row| {
                let rule = parsed.get(&row.rule)?;
                (rule.story_key == StoryKey::Release)
                    .then(|| (row.host.clone(), rule.path_prefix.clone()))
            })
            .collect();
        Ok(Self {
            hosts,
            rules: parsed,
            rules_version,
            extract_version,
            keys: Keys { rewrites, released },
            robots: BTreeMap::new(),
            basis: BTreeMap::new(),
            walks: BTreeMap::new(),
        })
    }

    /// Hands over each walked index's range, keyed by its index url, replacing any handed before.
    #[must_use]
    pub fn with_walks(self, walks: BTreeMap<String, WalkRange>) -> Self {
        Self { walks, ..self }
    }

    /// Every admitted index with an id walk, for the driver that reads its range from the store.
    #[must_use]
    pub fn walk(&self) -> Walk {
        Walk::new(
            self.hosts
                .admitted()
                .filter_map(|row| {
                    Some(WalkedIndex {
                        index_url: row.index_url.clone(),
                        host: row.host.clone(),
                        walk: row.walk.clone()?,
                        rule: self.rules.get(&row.rule)?.clone(),
                    })
                })
                .collect(),
            self.keys.clone(),
        )
    }

    /// Hands over each host's pinned robots, keyed by lowercased host, replacing any handed before.
    #[must_use]
    pub fn with_robots(self, robots: BTreeMap<String, HostRobots>) -> Self {
        Self { robots, ..self }
    }

    /// Hands over each paged index's basis, keyed by its page-1 url, replacing any handed before.
    #[must_use]
    pub fn with_basis(self, basis: BTreeMap<String, Basis>) -> Self {
        Self { basis, ..self }
    }

    /// Every admitted paged index, for the driver that finds its polls in the store.
    #[must_use]
    pub fn paging(&self) -> Paging {
        Paging::new(
            self.hosts
                .admitted()
                .filter_map(|row| {
                    let paged = row.paged.as_ref()?;
                    Some((paged.clone(), self.rules.get(&paged.rule)?.clone()))
                })
                .collect(),
            self.keys.clone(),
        )
    }

    /// Whether `url`'s host's handed robots allow its path with params and query, the same
    /// `host_and_target` the index target is checked through; a host never handed: no.
    fn robots_allow(&self, url: &str) -> bool {
        let Some((host, target)) = host_and_target(url) else {
            return false;
        };
        match self.robots.get(&host) {
            Some(HostRobots::Absent) => true,
            Some(HostRobots::Rules(rules)) => rules.allows(&target),
            None => false,
        }
    }

    /// Every path an admitted row asks of its host (section 11.2), in hosts-file order.
    #[must_use]
    pub fn admitted_paths(&self) -> Vec<AdmittedPath> {
        self.hosts
            .admitted()
            .flat_map(|row| {
                let prefix = self.rules.get(&row.rule).map(|rule| &rule.path_prefix);
                let paged = row.paged.as_ref().map(|paged| &paged.index_target);
                std::iter::once(&row.index_target)
                    .chain(paged)
                    .chain(prefix)
                    .map(|path| AdmittedPath {
                        host: row.host.clone(),
                        path: path.clone(),
                    })
            })
            .collect()
    }

    /// Every row of the hosts file, admitted and candidate, with its robots pin: what `newsvault
    /// robots refresh` asks and compares (section 11.2).
    #[must_use]
    pub fn host_pins(&self) -> Vec<HostPin> {
        self.hosts
            .rows
            .iter()
            .map(|row| HostPin {
                host: row.host.clone(),
                admitted: row.status == Status::Admitted,
                robots: row.robots.clone(),
            })
            .collect()
    }

    /// The first admitted row whose rule names `cross_check` keys, against every other admitted
    /// row's index (section 11.2, option C); `None` when no rule names one.
    #[must_use]
    pub fn cross_checker(&self) -> Option<CrossChecker> {
        let (keyed, rule, keys) = self.hosts.admitted().find_map(|row| {
            let rule = self.rules.get(&row.rule)?;
            let keys = rule.provider_keys.as_ref()?;
            (!keys.cross_check.is_empty()).then_some((row, rule, keys))
        })?;
        let direct = self
            .hosts
            .admitted()
            .filter(|row| row.host != keyed.host)
            .filter_map(|row| Some((row.index_url.clone(), self.rules.get(&row.rule)?.clone())))
            .collect();
        let index_of = keys
            .cross_check
            .iter()
            .filter_map(|(key, host)| {
                let row = self.hosts.admitted().find(|row| row.host == *host)?;
                Some((key.clone(), row.index_url.clone()))
            })
            .collect();
        Some(CrossChecker::new(
            keyed.index_url.clone(),
            rule.clone(),
            keys.clone(),
            direct,
            index_of,
        ))
    }

    /// `hosts_version`: the sha256 of the hosts file as read.
    #[must_use]
    pub const fn hosts_version(&self) -> &FileDigest {
        &self.hosts.version
    }

    /// `extract_version`: the rules file's digest and this crate's extraction source together
    /// (architecture.md section 2), carried by every request this feed builds.
    #[must_use]
    pub const fn extract_version(&self) -> &Digest {
        &self.extract_version
    }

    /// The one function behind `source_item_id` and `story_of` (section 11.1): the host rewrite,
    /// then the lowercased host plus the path, with no scheme, query or fragment; on a host whose
    /// rule keys by release, the host, the path prefix, the release id and the language
    /// (section 11.2). `None` for a url this crate cannot key. A page's canonical never reaches it.
    #[must_use]
    pub fn item_key(&self, url: &str) -> Option<String> {
        self.keys.key(url)
    }

    fn rewritten(&self, url: &str) -> Option<(String, String)> {
        self.keys.rewritten(url)
    }

    fn admitted_row(&self, url: &str) -> Option<&HostRow> {
        let (host, _) = host_and_path(url)?;
        self.hosts.admitted().find(|row| row.host == host)
    }

    /// An index page of a row's paged index is read under its rule; a document whose listed value
    /// that rule's format reads came from it; anything else is read under the row's own rule.
    fn rule_of(&self, request: &Request) -> Option<&Rule> {
        let row = self.admitted_row(&request.url)?;
        let paged = row.paged.as_ref().and_then(|paged| {
            let rule = self.rules.get(&paged.rule)?;
            let listed_by_it = match request.phase {
                RequestPhase::Index => paged.page_of(&request.url).is_some(),
                RequestPhase::Document => request
                    .listed_date
                    .as_deref()
                    .is_some_and(|raw| minute_of(raw, &rule.valid_format).is_some()),
            };
            listed_by_it.then_some(rule)
        });
        paged.or_else(|| self.rules.get(&row.rule))
    }

    fn index_request(&self, url: &str, window: &TargetWindow) -> Request {
        Request {
            source: SourceId::new(SOURCE),
            target_window: window.clone(),
            phase: RequestPhase::Index,
            provenance: Provenance::LiveIngest,
            url: url.to_owned(),
            headers: Vec::new(),
            params: Vec::new(),
            universe_version: empty_roster(),
            listed_date: None,
            hosts_version: Some(self.hosts.version.clone()),
            extract_version: Some(self.extract_version.clone()),
        }
    }

    /// Each index row whose rewritten link stays on the index's own host, passes the path filter
    /// and the provider key filter, and keys a story no earlier row of this index keyed is one
    /// follow-up unless the host's robots disallow its url (`robots_disallowed`); the raw
    /// publication value rides `listed_date`, never sent.
    fn index_rows_of(&self, request: &Request, raw: &RawBlob) -> Extracted {
        let (Some(rule), Some((host, _))) = (self.rule_of(request), host_and_path(&request.url))
        else {
            return nothing();
        };
        // An unreadable index is `classify`'s `PermanentFailure`, so nothing extracts it.
        let rows = parse::index_rows(&raw.bytes, rule).unwrap_or_default();
        let mut follow_ups = Vec::new();
        let mut filtered_out = 0;
        let mut robots_disallowed = 0;
        let mut keyed = BTreeSet::new();
        for row in &rows {
            let kept = row.link.as_deref().and_then(|link| {
                let (at, path) = self.rewritten(link)?;
                let url = format!("https://{at}{path}");
                let readable = followable_path(&path);
                let followed = rule.provider_keys.as_ref().is_none_or(|keys| {
                    row.image
                        .as_deref()
                        .and_then(|image| keys.key_of(image))
                        .is_some_and(|key| keys.follow.contains(key))
                });
                let key = self.item_key(&url)?;
                (at == host && path.starts_with(&rule.path_prefix) && readable && followed)
                    .then_some((url, key))
            });
            // A row robots disallows keys nothing, so another spelling of its release is followed.
            match kept {
                Some((url, _)) if !self.robots_allow(&url) => robots_disallowed += 1,
                Some((url, key)) if keyed.insert(key.clone()) => follow_ups.push(Request {
                    source: SourceId::new(SOURCE),
                    target_window: request.target_window.clone(),
                    phase: RequestPhase::Document,
                    provenance: request.provenance,
                    url,
                    headers: Vec::new(),
                    params: Vec::new(),
                    universe_version: empty_roster(),
                    listed_date: row.valid_raw.clone(),
                    hosts_version: request.hosts_version.clone(),
                    extract_version: request.extract_version.clone(),
                }),
                _ => filtered_out += 1,
            }
        }
        follow_ups.extend(self.next_page_of(request, raw, rule));
        Extracted {
            items: Vec::new(),
            follow_ups,
            unusable: Vec::new(),
            vendor_rows: u32::try_from(rows.len()).unwrap_or(u32::MAX),
            filtered_out,
            robots_disallowed,
            walk: self.walk_of(request, rule, &rows),
        }
    }

    /// The handed range's ids as `Document` requests under its date, leaving out each id the
    /// store answered and each this answer lists, both in the walk's language (its row asks it),
    /// and each url robots disallows.
    fn walk_of(&self, request: &Request, rule: &Rule, rows: &[parse::IndexRow]) -> Vec<Request> {
        let Some(row) = self.admitted_row(&request.url) else {
            return Vec::new();
        };
        let (Some(walk), Some(range)) = (row.walk.as_ref(), self.walks.get(&request.url)) else {
            return Vec::new();
        };
        if row.index_url != request.url {
            return Vec::new();
        }
        let listed: BTreeSet<u64> = rows
            .iter()
            .filter_map(|listed| {
                let (at, path) = self.rewritten(listed.link.as_deref()?)?;
                (at == row.host)
                    .then(|| release_of(&rule.path_prefix, &path))
                    .flatten()
            })
            .filter(|release| release.language == walk.language)
            .map(|release| release.id)
            .collect();
        range
            .ids()
            .filter(|id| !range.answered().contains(id) && !listed.contains(id))
            .map(|id| walk.url_for(range.date(), id))
            .filter(|url| self.robots_allow(url))
            .map(|url| Request {
                source: SourceId::new(SOURCE),
                target_window: request.target_window.clone(),
                phase: RequestPhase::Document,
                provenance: request.provenance,
                url,
                headers: Vec::new(),
                params: Vec::new(),
                universe_version: empty_roster(),
                listed_date: None,
                hosts_version: request.hosts_version.clone(),
                extract_version: request.extract_version.clone(),
            })
            .collect()
    }

    /// The next page of a paged index as an `Index` follow-up carrying, as `listed_date`, the
    /// listed text of this page's oldest row; never a vendor row (section 11.2).
    fn next_page_of(&self, request: &Request, raw: &RawBlob, rule: &Rule) -> Option<Request> {
        let paged = self.admitted_row(&request.url)?.paged.as_ref()?;
        let page = paged.page_of(&request.url)?;
        let rows = paging::page_rows(&raw.bytes, rule, &self.keys)?;
        let reached = request
            .listed_date
            .as_deref()
            .and_then(|raw| minute_of(raw, &rule.valid_format));
        let basis = self.basis.get(&paged.index_url);
        let (next, oldest) = paging::next_page(page, &rows, reached, basis)?;
        let url = paged.page_url(next);
        self.robots_allow(&url).then(|| Request {
            url,
            listed_date: Some(oldest),
            ..request.clone()
        })
    }

    /// The walk's own url of one release on the request's host: an index row with no listed value
    /// is not one, so it is not dated by its page.
    fn walked(&self, request: &Request, rule: &Rule) -> bool {
        let Some(walk) = self
            .admitted_row(&request.url)
            .and_then(|row| row.walk.as_ref())
        else {
            return false;
        };
        host_and_path(&request.url)
            .and_then(|(_, path)| release_of(&rule.path_prefix, &path))
            .is_some_and(|release| walk.url_for(&release.date, release.id) == request.url)
    }

    /// One article page, which is the one row a document payload holds.
    fn document_of(&self, request: &Request, raw: &RawBlob) -> Extracted {
        let Some(rule) = self.rule_of(request) else {
            return nothing();
        };
        let page = parse::page(&raw.bytes, rule);
        let source_item_id = self.item_key(&request.url).unwrap_or_default();
        // No index row listed a walked id, so its own page dates it (section 11.2).
        let (valid, valid_raw, valid_field, dated_by) =
            match (&request.listed_date, &rule.page_valid) {
                (None, Some(page_valid)) if self.walked(request, rule) => {
                    let text = page.valid.clone().unwrap_or_default();
                    let valid = minute_containing(&text, &page_valid.format)
                        .map_or(Published::Unknown, Published::AtMinute);
                    (valid, text, page_valid.field.clone(), DecidedBy::Page)
                }
                (listed, _) => (
                    self.listed_publication(request),
                    listed.clone().unwrap_or_default(),
                    rule.valid_field.clone(),
                    DecidedBy::IndexRow,
                ),
            };
        let body = page
            .body
            .as_ref()
            .ok()
            .map(|text| ExtractedBody::of(text.clone()));
        let item = CollectedItem {
            source: SourceId::new(SOURCE),
            source_item_id,
            url: request.url.clone(),
            // `doc_id`'s input only (section 11.1). A page naming none is canonical at its url.
            canonical_url: page
                .canonical
                .and_then(|href| absolute(&href, &request.url))
                .unwrap_or_else(|| request.url.clone()),
            title: page.title.unwrap_or_default(),
            valid,
            valid_raw,
            valid_field,
            valid_source: ValidSource::VendorPublished,
            provenance: request.provenance,
            observed: None,
            body,
        };
        // The url came from the index row or the walk; the time from the row unless the page dated it.
        let unusable = if item.source_item_id.is_empty() {
            Some((UnusableReason::BlankSourceItemId, DecidedBy::IndexRow))
        } else if item.valid == Published::Unknown {
            Some((UnusableReason::NoParseableValidTime, dated_by))
        } else {
            page.body.err().map(|reason| (reason, DecidedBy::Page))
        };
        let (items, unusable) = match unusable {
            Some((reason, decided_by)) => (
                Vec::new(),
                vec![UnusableRow {
                    reason,
                    decided_by,
                    item,
                    raw_blob: raw.digest.clone(),
                }],
            ),
            None => (vec![item], Vec::new()),
        };
        Extracted {
            items,
            follow_ups: Vec::new(),
            unusable,
            vendor_rows: 1,
            filtered_out: 0,
            robots_disallowed: 0,
            walk: Vec::new(),
        }
    }

    /// Whether a document's 3xx moves its release to another language on its own host: GNW's
    /// "id exists, language X, not fetched" (owner decision PG-D4, 2026-11-17).
    fn moved_to_another_language(&self, request: &Request, response: &RawResponse) -> bool {
        let (Some(row), Some(location)) = (
            self.admitted_row(&request.url),
            response.headers().first(news_provider::wire::LOCATION),
        ) else {
            return false;
        };
        self.rules
            .get(&row.rule)
            .filter(|rule| rule.story_key == StoryKey::Release)
            .and_then(|rule| {
                walk::moved_to(
                    &self.keys,
                    &row.host,
                    &rule.path_prefix,
                    &request.url,
                    location,
                )
            })
            .is_some()
    }
}

impl Provider for OpenWebFeed {
    fn source(&self) -> SourceId {
        SourceId::new(SOURCE)
    }

    fn transport(&self) -> TransportProfile {
        TransportProfile {
            user_agent: HeaderValue::FromCredential(CredentialRef::new(CONTACT_CREDENTIAL)),
            timeouts: Timeouts {
                global: VENDOR_PATIENCE,
                per_call: VENDOR_PATIENCE,
                resolve: VENDOR_PATIENCE,
                connect: VENDOR_PATIENCE,
                send_request: VENDOR_PATIENCE,
                await_100: VENDOR_PATIENCE,
                send_body: VENDOR_PATIENCE,
                recv_response: VENDOR_PATIENCE,
                recv_body: VENDOR_PATIENCE,
            },
            body_limit: BODY_LIMIT,
            budget: RequestBudget {
                min_interval: None,
                per_host: Some(INTERVAL),
            },
        }
    }

    /// `plan` refuses every window but the run's UTC day, so no other answer is reachable.
    fn provenance(&self, _window: &TargetWindow, _at: ObservationTime) -> Provenance {
        Provenance::LiveIngest
    }

    fn precision_ceiling(&self) -> ValidPrecision {
        ValidPrecision::Minute
    }

    /// No roster filters this feed (section 11.1): the digest of zero bytes.
    fn universe_version(&self) -> FileDigest {
        empty_roster()
    }

    fn cadence(&self) -> Cadence {
        Cadence::EveryUtcDay
    }

    fn follow_up_cap(&self) -> Option<u32> {
        Some(FOLLOW_UP_CAP)
    }

    /// One `Index` request per admitted host's index, and one for page 1 of its paged index, for
    /// the run's UTC day; any other window is refused before any request (section 11.1, §8 R10).
    fn plan(
        &self,
        target: &TargetWindow,
        calendar: Option<&Calendar>,
        listing: Option<&Listing>,
        at: ObservationTime,
    ) -> Result<Plan, PlanRefusal> {
        self.cadence().admits(&self.source(), calendar, listing)?;
        let Some(days) = days_of(target) else {
            return Err(PlanRefusal::TargetWindowUnreadable(
                target.as_str().to_owned(),
            ));
        };
        let run_day = at.inner().date_naive();
        if days != [run_day] {
            return Err(PlanRefusal::NotTheRunDay {
                source_id: SOURCE.to_owned(),
                window: target.as_str().to_owned(),
                run_day,
            });
        }
        let window = TargetWindow::new(run_day.to_string());
        Ok(Plan {
            requests: self
                .hosts
                .admitted()
                .flat_map(|row| {
                    let paged = row.paged.as_ref().map(|paged| &paged.index_url);
                    std::iter::once(&row.index_url)
                        .chain(paged)
                        .map(|url| self.index_request(url, &window))
                })
                .collect(),
            not_planned: Vec::new(),
        })
    }

    /// Bot protection (403, a challenge page) is permanent and never retried in another form
    /// (section 11.1); so is every other 4xx but 408 and 429.
    fn classify(&self, request: &Request, answer: &Result<RawResponse, TransportError>) -> Verdict {
        let response = match answer {
            Ok(response) => response,
            Err(failed) if failed.kind() == TransportKind::BodyOverLimit => {
                return Verdict::PermanentFailure
            }
            Err(_) => return Verdict::TransientFailure,
        };
        let Some(rule) = self.rule_of(request) else {
            return Verdict::PermanentFailure;
        };
        match response.status() {
            OK => match request.phase {
                RequestPhase::Index => match parse::index_rows(response.body(), rule) {
                    None => Verdict::PermanentFailure,
                    Some(rows) if rows.is_empty() => Verdict::Empty,
                    Some(_) => Verdict::Usable,
                },
                RequestPhase::Document => Verdict::Usable,
            },
            status
                if REDIRECTS.contains(&status)
                    && request.phase == RequestPhase::Document
                    && self.moved_to_another_language(request, response) =>
            {
                Verdict::Empty
            }
            TOO_MANY_REQUESTS | REQUEST_TIMEOUT => Verdict::TransientFailure,
            status if SERVER_ERRORS.contains(&status) => Verdict::TransientFailure,
            _ => Verdict::PermanentFailure,
        }
    }

    fn extract(&self, request: &Request, raw: &RawBlob) -> Extracted {
        match request.phase {
            RequestPhase::Index => self.index_rows_of(request, raw),
            RequestPhase::Document => self.document_of(request, raw),
        }
    }

    fn extract_rules_version(&self) -> FileDigest {
        self.rules_version.clone()
    }

    /// Every follow-up is live, and one whose story is in the held set is dropped (section 11.1).
    /// A follow-up with no `story_of`, a next index page among them, is always issued: a skip we
    /// cannot justify is a hole.
    fn follow_ups_to_issue(
        &self,
        follow_ups: Vec<Request>,
        held: &BTreeSet<String>,
    ) -> Vec<Request> {
        follow_ups
            .into_iter()
            .filter(|follow_up| {
                self.story_of(follow_up)
                    .is_none_or(|story| !held.contains(story.hex()))
            })
            .collect()
    }

    /// A document's story; an index page names none.
    fn story_of(&self, follow_up: &Request) -> Option<Digest> {
        (follow_up.phase == RequestPhase::Document)
            .then(|| canon::digest::story_id(SOURCE, &self.item_key(&follow_up.url)?).ok())
            .flatten()
    }

    /// The index row's publication text under the exact minute format of the rule that listed it
    /// (section 11.1), which `document_of` dates the item by.
    fn listed_publication(&self, follow_up: &Request) -> Published {
        match (self.rule_of(follow_up), follow_up.listed_date.as_deref()) {
            (Some(rule), Some(raw)) => {
                minute_of(raw, &rule.valid_format).map_or(Published::Unknown, Published::AtMinute)
            }
            _ => Published::Unknown,
        }
    }

    /// S7-D5: a deferred document built as `index_rows_of` builds a follow-up, under this run's
    /// hosts file and extraction version; `None` off an admitted host, for a url that keys no
    /// story, or one the pinned robots disallow.
    fn carried(
        &self,
        target: &TargetWindow,
        url: &str,
        listed_date: Option<&str>,
    ) -> Option<Request> {
        self.admitted_row(url)?;
        (self.item_key(url).is_some() && self.robots_allow(url)).then(|| Request {
            source: SourceId::new(SOURCE),
            target_window: target.clone(),
            phase: RequestPhase::Document,
            provenance: Provenance::LiveIngest,
            url: url.to_owned(),
            headers: Vec::new(),
            params: Vec::new(),
            universe_version: empty_roster(),
            listed_date: listed_date.map(str::to_owned),
            hosts_version: Some(self.hosts.version.clone()),
            extract_version: Some(self.extract_version.clone()),
        })
    }

    /// The walk's own url of a release with no listed value, as `document_of` dates it.
    fn is_walk_id(&self, request: &Request) -> bool {
        request.phase == RequestPhase::Document
            && request.listed_date.is_none()
            && self
                .rule_of(request)
                .is_some_and(|rule| self.walked(request, rule))
    }
}

/// A url's lowercased host and its path (`/` when empty), without scheme, port, query or
/// fragment. `None` for another scheme, a userinfo or port, or a host `HostKey` refuses.
pub(crate) fn host_and_path(url: &str) -> Option<(String, String)> {
    let (host, target) = host_and_target(url)?;
    let path = target.split('?').next().unwrap_or("/");
    Some((host, path.to_owned()))
}

/// Owner decision 4 (2026-11-17): a path robots.txt is checked against holds no `%`, no non-ASCII
/// byte and no control byte; nor a `#`, which robots.cc 50a0dac cuts from a url (lines 146-148).
pub(crate) fn plain_path(path: &str) -> bool {
    path.bytes()
        .all(|byte| !NOT_PLAIN.contains(&byte) && byte.is_ascii() && !byte.is_ascii_control())
}

/// A document path robots.cc reads as fetched (robots.h:153 wants it %-encoded): printable ASCII
/// with no space, and no segment that is `.` or `..`, written or as `%2e`, which servers walk.
fn followable_path(path: &str) -> bool {
    path.bytes().all(|byte| byte.is_ascii_graphic())
        && path.split('/').all(|segment| {
            let dots = segment.replace("%2e", ".").replace("%2E", ".");
            !matches!(dots.as_str(), "." | "..")
        })
}

/// `host_and_path` with the query kept: the path robots.txt rules are matched against.
pub(crate) fn host_and_target(url: &str) -> Option<(String, String)> {
    let (authority, tail) = authority_and_tail(url)?;
    news_provider::HostKey::new(authority).ok()?;
    let target = tail.split('#').next().unwrap_or("");
    let target = if target.starts_with('/') {
        target.to_owned()
    } else {
        format!("/{target}")
    };
    Some((authority.to_ascii_lowercase(), target))
}

/// The text after an http(s) scheme, split where the authority ends; `None` for another scheme.
fn authority_and_tail(url: &str) -> Option<(&str, &str)> {
    let rest = SCHEMES.iter().find_map(|scheme| {
        let head = url.get(..scheme.len())?;
        head.eq_ignore_ascii_case(scheme)
            .then(|| url.get(scheme.len()..))
            .flatten()
    })?;
    Some(rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len())))
}

/// A canonical href as an absolute url: a scheme- or scheme-relative one as written (`https:`
/// prefixed) when it names any authority, port and userinfo kept; a root-relative one resolved
/// against `base`; anything else `None`. Another host is kept, because `doc_id` is cross-source.
fn absolute(href: &str, base: &str) -> Option<String> {
    let written = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_owned()
    };
    if let Some((authority, _)) = authority_and_tail(&written) {
        return (!authority.is_empty()).then_some(written);
    }
    let (host, _) = host_and_path(base)?;
    href.starts_with('/')
        .then(|| format!("https://{host}{href}"))
}

/// The exact minute form the rule names: the value must print back byte for byte, because
/// chrono's `%Y` reads `26` as the year 26 and `%H` reads `9` (review 2026-11-15). A value with
/// seconds is refused by `Minute::new`. A format with an offset reprints at the value's own one.
pub(crate) fn minute_of(raw: &str, format: &str) -> Option<Minute> {
    Minute::new(read_exactly(raw, format)?).ok()
}

/// The minute holding a value `format` reads to the second, read as exactly as `minute_of` reads:
/// 16 of 16 GNW pages' `og:article:published_time` fell in their RSS `pubDate`'s minute (§11.6).
/// A fractional second is refused, by `Minute::new`.
pub(crate) fn minute_containing(raw: &str, format: &str) -> Option<Minute> {
    use chrono::Timelike;
    Minute::new(read_exactly(raw, format)?.with_second(0)?).ok()
}

fn read_exactly(raw: &str, format: &str) -> Option<DateTime<Utc>> {
    use std::fmt::Write;
    let raw = raw.trim();
    let mut printed = String::new();
    let instant = match DateTime::parse_from_str(raw, format) {
        Ok(stated) => {
            write!(printed, "{}", stated.format(format)).ok()?;
            stated.with_timezone(&Utc)
        }
        Err(_) => {
            let naive = NaiveDateTime::parse_from_str(raw, format).ok()?;
            write!(printed, "{}", naive.format(format)).ok()?;
            naive.and_utc()
        }
    };
    (printed == raw).then_some(instant)
}

fn empty_roster() -> FileDigest {
    FileDigest::of(&[])
}

/// What a payload for a host with no admitted rule yields. `classify` closes such an answer
/// `PermanentFailure` before anything extracts it.
fn nothing() -> Extracted {
    Extracted {
        items: Vec::new(),
        follow_ups: Vec::new(),
        unusable: Vec::new(),
        vendor_rows: 0,
        filtered_out: 0,
        robots_disallowed: 0,
        walk: Vec::new(),
    }
}

/// Refuses a JSON object holding a key outside `known`.
pub(crate) fn known_fields(
    object: &Map<String, Value>,
    known: &[&str],
    path: &Path,
) -> Result<(), Refusal> {
    match object.keys().find(|key| !known.contains(&key.as_str())) {
        Some(field) => Err(Refusal::UnknownField {
            path: path.to_path_buf(),
            field: field.clone(),
        }),
        None => Ok(()),
    }
}

/// Every `cross_check` host of an admitted row's rule is another admitted row's host.
fn cross_checked_hosts_admitted(
    hosts: &Hosts,
    rules: &BTreeMap<String, Rule>,
    path: &Path,
) -> Result<(), Refusal> {
    for keyed in hosts.admitted() {
        let Some(keys) = rules
            .get(&keyed.rule)
            .and_then(|rule| rule.provider_keys.as_ref())
        else {
            continue;
        };
        for (key, host) in &keys.cross_check {
            let admitted = hosts
                .admitted()
                .any(|row| row.host == *host && row.host != keyed.host);
            if !admitted {
                return Err(Refusal::CrossCheckHostNotAdmitted {
                    path: path.to_path_buf(),
                    rule: keyed.rule.clone(),
                    key: key.clone(),
                    host: host.clone(),
                });
            }
        }
    }
    Ok(())
}

/// A site operator needs somebody to reach before blocking us, so an address is required.
fn usable_contact(contact: &str) -> Result<(), Refusal> {
    let addressed = contact.split_whitespace().any(|token| {
        token
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
    });
    if addressed {
        Ok(())
    } else {
        Err(Refusal::ContactMissing)
    }
}
