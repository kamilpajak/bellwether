//! The hosts file: one hashed file per SourceId, whose digest is `hosts_version` (architecture.md
//! sections 2 and 11.1). A row holds host, status, index url, rule and the robots pin.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use canon::FileDigest;
use news_provider::HostKey;
use serde_json::Value;

use crate::rules::Rule;
use crate::{host_and_target, known_fields, strict, Refusal};

pub(crate) const HOSTS_SCHEMA: &str = "open-web-feed.hosts/2";
const TOP: [&str; 2] = ["schema", "hosts"];
const ROW: [&str; 5] = ["host", "status", "index_url", "rule", "robots"];
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
        rows.push(HostRow {
            host,
            status,
            index_url,
            index_target,
            rule,
            robots,
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
