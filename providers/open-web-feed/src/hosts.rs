//! The hosts file: one hashed file per SourceId, whose digest is `hosts_version` (architecture.md
//! sections 2 and 11.1). A row holds host, status, index url, rule and the robots pin.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use canon::FileDigest;
use news_provider::HostKey;
use serde_json::Value;

use crate::rules::{reads_the_other, IndexFormat, Rule, StoryKey};
use crate::walk::{release_of, IdWalk, DATE, ID};
use crate::{host_and_target, known_fields, plain_path, strict, Refusal};

pub(crate) const HOSTS_SCHEMA: &str = "open-web-feed.hosts/2";
const TOP: [&str; 2] = ["schema", "hosts"];
const ROW: [&str; 7] = [
    "host",
    "status",
    "index_url",
    "rule",
    "robots",
    "paged_index",
    "id_walk",
];
const PAGED: [&str; 2] = ["index_url", "rule"];
const WALK: [&str; 1] = ["url"];
/// The date and id an `id_walk.url` is expanded with to check it names a release.
const PROBE_DATE: &str = "2026/11/14";
const PROBE_ID: u64 = 3_403_075;
/// A paged index is named at its first page; page N is this query pair with N in place of 1.
const FIRST_PAGE: &str = "page=1";
const ADMITTED: &str = "admitted";
const CANDIDATE: &str = "candidate";
/// A robots.txt 404 is "no robots" and pins no digest (section 11.1).
const ABSENT: &str = "absent";
const SHA256_HEX: usize = 64;

/// What a hosts row pins its robots.txt to: the sha256 of the body as received after content
/// decoding, or a 404. One field, so a state and a digest cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RobotsPin {
    Sha256(String),
    Absent,
}

impl RobotsPin {
    fn read(text: &str) -> Option<Self> {
        if text == ABSENT {
            return Some(Self::Absent);
        }
        let hex = text.len() == SHA256_HEX
            && text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        hex.then(|| Self::Sha256(text.to_owned()))
    }

    /// The pin as the hosts file spells it.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Sha256(hex) => hex,
            Self::Absent => ABSENT,
        }
    }
}

/// Section 11's **Admitted** term. Only `Admitted` rows are planned (§8 row R19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Admitted,
    /// Named in the file and never asked: its robots fetch or its probe has not landed.
    Candidate,
}

#[derive(Debug, Clone)]
pub(crate) struct HostRow {
    /// Lowercased, and a valid `HostKey`.
    pub(crate) host: String,
    pub(crate) status: Status,
    pub(crate) index_url: String,
    /// The index url's path with its query, which robots.txt must allow (section 11.2).
    pub(crate) index_target: String,
    pub(crate) rule: String,
    /// Required on an admitted row, optional on a candidate one.
    pub(crate) robots: Option<RobotsPin>,
    /// A second index on the same host, read page by page (architecture.md section 11.2).
    pub(crate) paged: Option<PagedIndex>,
    /// The release ids asked below the row's index frontier (architecture.md section 11.2).
    pub(crate) walk: Option<IdWalk>,
}

/// A news sitemap whose page N+1 is asked only while page N holds no key of the previous poll.
#[derive(Debug, Clone)]
pub(crate) struct PagedIndex {
    /// Page 1's url, which `plan` asks.
    pub(crate) index_url: String,
    pub(crate) index_target: String,
    pub(crate) rule: String,
    /// `index_url` without its final `1`: page N is this stem followed by N.
    pub(crate) stem: String,
}

impl PagedIndex {
    /// The page `url` names: the stem, then a decimal with no sign and no leading zero.
    pub(crate) fn page_of(&self, url: &str) -> Option<u32> {
        let number = url.strip_prefix(&self.stem)?;
        let digits = !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit());
        (digits && !number.starts_with('0'))
            .then(|| number.parse().ok())
            .flatten()
    }

    pub(crate) fn page_url(&self, page: u32) -> String {
        format!("{}{page}", self.stem)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Hosts {
    pub(crate) rows: Vec<HostRow>,
    pub(crate) version: FileDigest,
}

impl Hosts {
    pub(crate) fn admitted(&self) -> impl Iterator<Item = &HostRow> {
        self.rows
            .iter()
            .filter(|row| row.status == Status::Admitted)
    }
}

pub(crate) fn read_hosts(path: &Path, rules: &BTreeMap<String, Rule>) -> Result<Hosts, Refusal> {
    let bytes = std::fs::read(path).map_err(|_| Refusal::HostsAbsent(path.to_path_buf()))?;
    let unparseable = |why: String| Refusal::HostsUnparseable {
        path: path.to_path_buf(),
        why,
    };
    let read = strict::read(&bytes, path, unparseable)?;
    let top = read
        .as_object()
        .ok_or_else(|| unparseable("not an object".to_owned()))?;
    known_fields(top, &TOP, path)?;
    let schema = top.get("schema").and_then(Value::as_str).unwrap_or("");
    if schema != HOSTS_SCHEMA {
        return Err(unparseable(format!("schema {schema}")));
    }
    let listed = top
        .get("hosts")
        .and_then(Value::as_array)
        .ok_or_else(|| unparseable("no hosts array".to_owned()))?;
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, row) in listed.iter().enumerate() {
        let row = row
            .as_object()
            .ok_or_else(|| unparseable(format!("row {index} is not an object")))?;
        known_fields(row, &ROW, path)?;
        let field = |key: &str| {
            row.get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| unparseable(format!("row {index} has no {key}")))
        };
        let (named, status, index_url, rule) = (
            field("host")?,
            field("status")?,
            field("index_url")?,
            field("rule")?,
        );
        if HostKey::new(&named).is_err() {
            return Err(Refusal::NotAHost {
                path: path.to_path_buf(),
                host: named,
            });
        }
        let host = named.to_ascii_lowercase();
        if !seen.insert(host.clone()) {
            return Err(Refusal::DuplicateHost {
                path: path.to_path_buf(),
                host,
            });
        }
        let status = match status.as_str() {
            ADMITTED => Status::Admitted,
            CANDIDATE => Status::Candidate,
            other => return Err(unparseable(format!("{host} has status {other}"))),
        };
        if !rules.contains_key(&rule) {
            return Err(Refusal::UnknownRule {
                path: path.to_path_buf(),
                host,
                rule,
            });
        }
        let index_target = match host_and_target(&index_url) {
            Some((at, target)) if at == host => target,
            _ => {
                return Err(Refusal::IndexOffHost {
                    path: path.to_path_buf(),
                    host,
                    url: index_url,
                })
            }
        };
        if !plain_path(&index_target) {
            return Err(Refusal::IndexTargetNotPlain {
                path: path.to_path_buf(),
                host,
                target: index_target,
            });
        }
        let robots = match row.get("robots") {
            None => None,
            Some(pin) => Some(pin.as_str().and_then(RobotsPin::read).ok_or_else(|| {
                unparseable(format!(
                    "{host} pins robots to {pin}, neither 64 lowercase hex nor {ABSENT}"
                ))
            })?),
        };
        if status == Status::Admitted && robots.is_none() {
            return Err(unparseable(format!(
                "{host} is admitted and pins no robots"
            )));
        }
        let paged = match row.get("paged_index") {
            None => None,
            Some(paged) => Some(paged_index(paged, &host, &rule, rules, path)?),
        };
        let walk = match row.get("id_walk") {
            None => None,
            Some(walk) => Some(id_walk(walk, &host, &rule, rules, path)?),
        };
        rows.push(HostRow {
            host,
            status,
            index_url,
            index_target,
            rule,
            robots,
            paged,
            walk,
        });
    }
    let hosts = Hosts {
        rows,
        version: FileDigest::of(&bytes),
    };
    if hosts.admitted().next().is_none() {
        return Err(Refusal::NoHostAdmitted(path.to_path_buf()));
    }
    Ok(hosts)
}

/// A row's `id_walk`: a plain url on the row's host holding `{date}` and `{id}` once each, which
/// expanded names a release under the row's rule, a rule keying stories by release and dating a
/// page by its own time.
fn id_walk(
    walk: &Value,
    host: &str,
    rule: &str,
    rules: &BTreeMap<String, Rule>,
    path: &Path,
) -> Result<IdWalk, Refusal> {
    let unparseable = |why: String| Refusal::HostsUnparseable {
        path: path.to_path_buf(),
        why: format!("{host}'s id_walk {why}"),
    };
    let object = walk
        .as_object()
        .ok_or_else(|| unparseable("is not an object".to_owned()))?;
    known_fields(object, &WALK, path)?;
    let url = object
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| unparseable("has no url".to_owned()))?
        .to_owned();
    let read = rules.get(rule);
    if !read.is_some_and(|read| read.story_key == StoryKey::Release && read.page_valid.is_some()) {
        return Err(Refusal::IdWalkRuleNotReleaseKeyed {
            path: path.to_path_buf(),
            host: host.to_owned(),
            rule: rule.to_owned(),
        });
    }
    let once = |mark: &str| url.matches(mark).count() == 1;
    let probe = url
        .replace(DATE, PROBE_DATE)
        .replace(ID, &PROBE_ID.to_string());
    let named = match (host_and_target(&probe), read) {
        (Some((at, target)), Some(read)) if at == host && plain_path(&target) => {
            release_of(&read.path_prefix, &target)
                .filter(|release| release.id == PROBE_ID && release.date == PROBE_DATE)
        }
        _ => None,
    };
    match named {
        Some(release) if once(DATE) && once(ID) => Ok(IdWalk {
            url,
            language: release.language,
        }),
        _ => Err(Refusal::IdWalkNotARelease {
            path: path.to_path_buf(),
            host: host.to_owned(),
            url,
        }),
    }
}

/// A row's `paged_index`: on the row's host, plain, a news sitemap named at page 1, read under a
/// rule whose valid format and the row's own cannot both read one listed value.
fn paged_index(
    paged: &Value,
    host: &str,
    row_rule: &str,
    rules: &BTreeMap<String, Rule>,
    path: &Path,
) -> Result<PagedIndex, Refusal> {
    let unparseable = |why: String| Refusal::HostsUnparseable {
        path: path.to_path_buf(),
        why: format!("{host}'s paged_index {why}"),
    };
    let object = paged
        .as_object()
        .ok_or_else(|| unparseable("is not an object".to_owned()))?;
    known_fields(object, &PAGED, path)?;
    let field = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| unparseable(format!("has no {key}")))
    };
    let (index_url, rule) = (field("index_url")?, field("rule")?);
    let (Some(read), Some(own)) = (rules.get(&rule), rules.get(row_rule)) else {
        return Err(Refusal::UnknownRule {
            path: path.to_path_buf(),
            host: host.to_owned(),
            rule,
        });
    };
    let index_target = match host_and_target(&index_url) {
        Some((at, target)) if at == host => target,
        _ => {
            return Err(Refusal::IndexOffHost {
                path: path.to_path_buf(),
                host: host.to_owned(),
                url: index_url,
            })
        }
    };
    if !plain_path(&index_target) {
        return Err(Refusal::IndexTargetNotPlain {
            path: path.to_path_buf(),
            host: host.to_owned(),
            target: index_target,
        });
    }
    let last_pair = index_target
        .split_once('?')
        .and_then(|(_, query)| query.rsplit('&').next());
    let stem = match (last_pair, index_url.strip_suffix('1')) {
        (Some(FIRST_PAGE), Some(stem)) => stem.to_owned(),
        _ => {
            return Err(Refusal::PagedIndexNotPageOne {
                path: path.to_path_buf(),
                host: host.to_owned(),
                url: index_url,
            })
        }
    };
    if read.index != IndexFormat::NewsSitemap {
        return Err(Refusal::PagedIndexNotASitemap {
            path: path.to_path_buf(),
            host: host.to_owned(),
            rule,
        });
    }
    if reads_the_other(read, own) || reads_the_other(own, read) {
        return Err(Refusal::IndexFormatsOverlap {
            path: path.to_path_buf(),
            host: host.to_owned(),
            rules: format!("{row_rule} and {rule}"),
        });
    }
    Ok(PagedIndex {
        index_url,
        index_target,
        rule,
        stem,
    })
}
