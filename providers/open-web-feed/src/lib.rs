//! Our own forward news feed: dated indexes and article pages from admitted hosts, collected live
//! for the run's UTC day only. The design is `docs/design/architecture.md` section 11; GNW is the
//! one admitted host (section 11.5 step 2). It declares no HTTP dependency and holds no clock and
//! no `Sink`.

mod depth;
mod hosts;
mod parse;
mod rules;
mod strict;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use canon::{Digest, FileDigest};
use chrono::NaiveDateTime;
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

pub use crate::hosts::RobotsPin;
use crate::hosts::{read_hosts, HostRow, Hosts, Status, HOSTS_SCHEMA};
use crate::rules::{parse_rules, Rule, RULES_SCHEMA};

pub const SOURCE: &str = "open_web_wires";
/// The User-Agent, a product token plus a contact address, travels by NAME as SEC's does
/// (architecture.md section 3.2's `filter_params` row); its own name, not another vendor's.
pub const CONTACT_CREDENTIAL: &str = "open_web_contact";

const OK: u16 = 200;
const REQUEST_TIMEOUT: u16 = 408;
const TOO_MANY_REQUESTS: u16 = 429;
const SERVER_ERRORS: std::ops::RangeInclusive<u16> = 500..=599;
const SCHEMES: [&str; 2] = ["https://", "http://"];
/// An ASSUMPTION taken by default, as edgar-press's is: nothing here has timed a GNW response.
const VENDOR_PATIENCE: Duration = Duration::from_secs(30);
/// The largest page of the 2026-11-14 probes was 979,950 B (cnbc.com), the GNW article 71,657 B
/// and the GNW RSS 36,687 B, all as decompressed by curl (§11.6's method).
const BODY_LIMIT: u64 = 2 * 1_024 * 1_024;
/// Section 11.1's floor; per IP the same, because no measurement places a smaller one. A host's
/// robots Crawl-delay raises `per_host` above it (`with_crawl_delays`), never below.
const INTERVAL: Duration = Duration::from_secs(10);

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
    #[error(
        "the rules at {path} read rule {rule}'s valid time as {format}, which reads no minute"
    )]
    FormatReadsNoMinute {
        path: PathBuf,
        rule: String,
        format: String,
    },
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

#[derive(Debug)]
pub struct OpenWebFeed {
    hosts: Hosts,
    rules: BTreeMap<String, Rule>,
    rules_version: FileDigest,
    /// Every rule's host rewrite, which `item_key` applies whatever host a url names.
    rewrites: BTreeMap<String, String>,
    /// Per host, the Crawl-delay `newsvault` read from the pinned robots.txt (section 11.2).
    crawl_delays: BTreeMap<String, Duration>,
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
        Ok(Self {
            hosts: read_hosts(hosts, &parsed)?,
            rules: parsed,
            rules_version: FileDigest::of(&rules_bytes),
            rewrites,
            crawl_delays: BTreeMap::new(),
        })
    }

    /// The Crawl-delay per host, read by `newsvault` from each pinned robots.txt. `transport()`
    /// paces at the largest an ADMITTED host states when it is above 10 s; one `per_host` serves
    /// every host (§8 row R19 hole 7), so one slow host slows them all.
    #[must_use]
    pub fn with_crawl_delays(self, crawl_delays: BTreeMap<String, Duration>) -> Self {
        Self {
            crawl_delays,
            ..self
        }
    }

    /// Every path an admitted row asks of its host (section 11.2), in hosts-file order.
    #[must_use]
    pub fn admitted_paths(&self) -> Vec<AdmittedPath> {
        self.hosts
            .admitted()
            .flat_map(|row| {
                let prefix = self.rules.get(&row.rule).map(|rule| &rule.path_prefix);
                std::iter::once(&row.index_target)
                    .chain(prefix)
                    .map(|path| AdmittedPath {
                        host: row.host.clone(),
                        path: path.clone(),
                    })
            })
            .collect()
    }

    /// The one per-host interval: 10 s, or the largest admitted host's Crawl-delay above it.
    fn per_host_interval(&self) -> Duration {
        self.hosts
            .admitted()
            .filter_map(|row| self.crawl_delays.get(&row.host))
            .fold(INTERVAL, |slowest, delay| slowest.max(*delay))
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

    /// `hosts_version`: the sha256 of the hosts file as read.
    #[must_use]
    pub const fn hosts_version(&self) -> &FileDigest {
        &self.hosts.version
    }

    /// The one function behind `source_item_id` and `story_of` (section 11.1): the host rewrite,
    /// then the lowercased host plus the path, with no scheme, query or fragment. `None` for a
    /// url with no host this crate can read. A page's canonical link never reaches it.
    #[must_use]
    pub fn item_key(&self, url: &str) -> Option<String> {
        let (host, path) = self.rewritten(url)?;
        Some(format!("{host}{path}"))
    }

    fn rewritten(&self, url: &str) -> Option<(String, String)> {
        let (host, path) = host_and_path(url)?;
        Some((self.rewrites.get(&host).cloned().unwrap_or(host), path))
    }

    fn admitted_row(&self, url: &str) -> Option<&HostRow> {
        let (host, _) = host_and_path(url)?;
        self.hosts.admitted().find(|row| row.host == host)
    }

    fn rule_for(&self, url: &str) -> Option<&Rule> {
        self.rules.get(&self.admitted_row(url)?.rule)
    }

    fn index_request(&self, row: &HostRow, window: &TargetWindow) -> Request {
        Request {
            source: SourceId::new(SOURCE),
            target_window: window.clone(),
            phase: RequestPhase::Index,
            provenance: Provenance::LiveIngest,
            url: row.index_url.clone(),
            headers: Vec::new(),
            params: Vec::new(),
            universe_version: empty_roster(),
            listed_date: None,
            hosts_version: Some(self.hosts.version.clone()),
        }
    }

    /// Each RSS item whose rewritten link stays on the index's own host, passes the path filter
    /// and keys a story no earlier row of this index keyed is one follow-up; the raw publication
    /// value rides `listed_date`, never sent.
    fn index_rows_of(&self, request: &Request, raw: &RawBlob) -> Extracted {
        let (Some(rule), Some((host, _))) =
            (self.rule_for(&request.url), host_and_path(&request.url))
        else {
            return nothing();
        };
        // An unreadable index is `classify`'s `PermanentFailure`, so nothing extracts it.
        let rows = parse::index_rows(&raw.bytes, rule).unwrap_or_default();
        let mut follow_ups = Vec::new();
        let mut filtered_out = 0;
        let mut keyed = BTreeSet::new();
        for row in &rows {
            let kept = row.link.as_deref().and_then(|link| {
                let (at, path) = self.rewritten(link)?;
                let walks = path.split('/').any(|segment| matches!(segment, "." | ".."));
                (at == host
                    && path.starts_with(&rule.path_prefix)
                    && !walks
                    && keyed.insert(format!("{at}{path}")))
                .then(|| format!("https://{at}{path}"))
            });
            match kept {
                Some(url) => follow_ups.push(Request {
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
                }),
                None => filtered_out += 1,
            }
        }
        Extracted {
            items: Vec::new(),
            follow_ups,
            unusable: Vec::new(),
            vendor_rows: u32::try_from(rows.len()).unwrap_or(u32::MAX),
            filtered_out,
        }
    }

    /// One article page, which is the one row a document payload holds.
    fn document_of(&self, request: &Request, raw: &RawBlob) -> Extracted {
        let Some(rule) = self.rule_for(&request.url) else {
            return nothing();
        };
        let page = parse::page(&raw.bytes, rule);
        let source_item_id = self.item_key(&request.url).unwrap_or_default();
        let valid_raw = request.listed_date.clone().unwrap_or_default();
        let valid = minute_of(&valid_raw, &rule.valid_format)
            .map_or(Published::Unknown, Published::AtMinute);
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
            valid_field: rule.valid_field.clone(),
            valid_source: ValidSource::VendorPublished,
            provenance: request.provenance,
            observed: None,
            body,
        };
        // The url and the listed time both came from the index row; only the body is the page's.
        let unusable = if item.source_item_id.is_empty() {
            Some((UnusableReason::BlankSourceItemId, DecidedBy::IndexRow))
        } else if item.valid == Published::Unknown {
            Some((UnusableReason::NoParseableValidTime, DecidedBy::IndexRow))
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
        }
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
                min_interval: INTERVAL,
                per_host: Some(self.per_host_interval()),
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

    /// One `Index` request per admitted host for the run's UTC day; any other window is refused
    /// before any request (section 11.1, §8 row R10).
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
                .map(|row| self.index_request(row, &window))
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
        let Some(rule) = self.rule_for(&request.url) else {
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
    /// A follow-up with no `story_of` is always issued: a skip we cannot justify is a hole.
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

    fn story_of(&self, follow_up: &Request) -> Option<Digest> {
        canon::digest::story_id(SOURCE, &self.item_key(&follow_up.url)?).ok()
    }
}

/// A url's lowercased host and its path (`/` when empty), without scheme, port, query or
/// fragment. `None` for another scheme, a userinfo or port, or a host `HostKey` refuses.
pub(crate) fn host_and_path(url: &str) -> Option<(String, String)> {
    let (host, target) = host_and_target(url)?;
    let path = target.split('?').next().unwrap_or("/");
    Some((host, path.to_owned()))
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
/// seconds is refused by `Minute::new`.
pub(crate) fn minute_of(raw: &str, format: &str) -> Option<Minute> {
    use std::fmt::Write;
    let raw = raw.trim();
    let naive = NaiveDateTime::parse_from_str(raw, format).ok()?;
    let mut printed = String::new();
    write!(printed, "{}", naive.format(format)).ok()?;
    (printed == raw).then_some(())?;
    Minute::new(naive.and_utc()).ok()
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
