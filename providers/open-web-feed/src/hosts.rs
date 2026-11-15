//! The hosts file: one hashed file per SourceId, whose digest is `hosts_version` (architecture.md
//! sections 2 and 11.1). In step 2 a row holds host, status, index url and rule only; Crawl-delay
//! and the robots digest arrive with their reader in section 11.5 step 3.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use canon::FileDigest;
use news_provider::HostKey;
use serde_json::Value;

use crate::rules::Rule;
use crate::{host_and_path, known_fields, Refusal};

pub(crate) const HOSTS_SCHEMA: &str = "open-web-feed.hosts/1";
const TOP: [&str; 2] = ["schema", "hosts"];
const ROW: [&str; 4] = ["host", "status", "index_url", "rule"];
const ADMITTED: &str = "admitted";
const CANDIDATE: &str = "candidate";

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
    pub(crate) rule: String,
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
    let read: Value = serde_json::from_slice(&bytes).map_err(|why| unparseable(why.to_string()))?;
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
        if host_and_path(&index_url).map(|(at, _)| at) != Some(host.clone()) {
            return Err(Refusal::IndexOffHost {
                path: path.to_path_buf(),
                host,
                url: index_url,
            });
        }
        rows.push(HostRow {
            host,
            status,
            index_url,
            rule,
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
