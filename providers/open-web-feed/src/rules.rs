//! The hashed extraction rules, one per hosts-file `rule` name. Every field is read by `extract`
//! or `item_key`, so `extract_rules_version` covers the path filter and the host rewrite too
//! (architecture.md section 11.1).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use scraper::Selector;
use serde_json::{Map, Value};

use chrono::{NaiveDate, NaiveDateTime};
use news_provider::HostKey;

use crate::{known_fields, minute_of, plain_path, strict, Refusal};

pub(crate) const RULES_SCHEMA: &str = "open-web-feed.rules/1";
const TOP: [&str; 2] = ["schema", "rules"];
const RULE: [&str; 9] = [
    "index",
    "provider_keys",
    "valid_field",
    "valid_format",
    "path_prefix",
    "rewrite_hosts",
    "title",
    "canonical",
    "body",
];
const PICK: [&str; 2] = ["selector", "attribute"];
const BODY: [&str; 2] = ["container", "paragraph"];
const PROVIDER_KEYS: [&str; 3] = ["image_key_after", "follow", "cross_check"];
const RSS: &str = "rss";
const NEWS_SITEMAP: &str = "news_sitemap";
/// A news sitemap rule's `valid_field` names a child of `news:news` behind this prefix.
pub(crate) const NEWS_FIELD_PREFIX: &str = "news:";
/// The minute every `valid_format` must print and read back, 2026-11-14T19:07Z, and a second
/// inside it that the format must not print.
const PROBE_MINUTE: (i32, u32, u32, u32, u32) = (2026, 11, 14, 19, 7);
const PROBE_SECOND: u32 = 31;

/// Section 11.2's provider key filter: a news sitemap row's key is the url segment after
/// `image_key_after` in its first `image:loc`. A row is followed only when its key equals a
/// `follow` key; a row under a `cross_check` key is read by `CrossChecker` alone.
#[derive(Debug, Clone)]
pub(crate) struct ProviderKeys {
    pub(crate) image_key_after: String,
    pub(crate) follow: BTreeSet<String>,
    /// Each key to the lowercased host whose admitted index answers for its rows; `open` refuses
    /// a host no other admitted row holds.
    pub(crate) cross_check: BTreeMap<String, String>,
}

impl ProviderKeys {
    /// The segment after the first `image_key_after`, up to the next `/`; `None` when the mark is
    /// absent or no `/` ends a non-empty segment.
    pub(crate) fn key_of<'a>(&self, image: &'a str) -> Option<&'a str> {
        let (_, after) = image.split_once(self.image_key_after.as_str())?;
        let (key, _) = after.split_once('/')?;
        (!key.is_empty()).then_some(key)
    }
}

/// One element's attribute, picked by a CSS selector.
#[derive(Debug, Clone)]
pub(crate) struct Pick {
    pub(crate) selector: Selector,
    pub(crate) attribute: String,
}

/// A host's index shape. A rule with no `index` field is `Rss`, as every rule before it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndexFormat {
    Rss,
    /// A Google news sitemap: `urlset/url/loc` and `url/news:news/<valid_field>`.
    NewsSitemap,
}

#[derive(Debug, Clone)]
pub(crate) struct Rule {
    pub(crate) index: IndexFormat,
    /// The index row's element holding the publication value, and `CollectedItem.valid_field`.
    pub(crate) valid_field: String,
    /// A chrono format with no seconds field: a value it cannot read is `NoParseableValidTime`.
    pub(crate) valid_format: String,
    /// A link whose path does not start with this is filtered out at the index.
    pub(crate) path_prefix: String,
    /// Host to host, applied before the path filter and inside `item_key` (section 11.2).
    pub(crate) rewrite_hosts: BTreeMap<String, String>,
    /// Absent: every row the path filter keeps is followed.
    pub(crate) provider_keys: Option<ProviderKeys>,
    pub(crate) title: Pick,
    pub(crate) canonical: Pick,
    /// The article container; its outermost `paragraph` matches and table rows are the body.
    pub(crate) container: Selector,
    pub(crate) paragraph: Selector,
}

pub(crate) fn parse_rules(bytes: &[u8], path: &Path) -> Result<BTreeMap<String, Rule>, Refusal> {
    let unparseable = |why: String| Refusal::RulesUnparseable {
        path: path.to_path_buf(),
        why,
    };
    let read = strict::read(bytes, path, unparseable)?;
    let top = read
        .as_object()
        .ok_or_else(|| unparseable("not an object".to_owned()))?;
    known_fields(top, &TOP, path)?;
    let schema = top.get("schema").and_then(Value::as_str).unwrap_or("");
    if schema != RULES_SCHEMA {
        return Err(unparseable(format!("schema {schema}")));
    }
    let rules = top
        .get("rules")
        .and_then(Value::as_object)
        .ok_or_else(|| unparseable("no rules object".to_owned()))?;
    let mut parsed = BTreeMap::new();
    for (name, rule) in rules {
        let rule = rule
            .as_object()
            .ok_or_else(|| unparseable(format!("rule {name} is not an object")))?;
        parsed.insert(name.clone(), parse_rule(name, rule, path)?);
    }
    if parsed.is_empty() {
        return Err(unparseable("no rule".to_owned()));
    }
    Ok(parsed)
}

fn parse_rule(name: &str, rule: &Map<String, Value>, path: &Path) -> Result<Rule, Refusal> {
    let unparseable = |why: String| Refusal::RulesUnparseable {
        path: path.to_path_buf(),
        why: format!("rule {name}: {why}"),
    };
    known_fields(rule, &RULE, path)?;
    let text = |key: &str| {
        rule.get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| unparseable(format!("no {key}")))
    };
    let object = |key: &str| {
        rule.get(key)
            .and_then(Value::as_object)
            .ok_or_else(|| unparseable(format!("no {key} object")))
    };
    let not_a_host = |host: &str| Refusal::RewriteNotAHost {
        path: path.to_path_buf(),
        rule: name.to_owned(),
        host: host.to_owned(),
    };
    let mut rewrite_hosts = BTreeMap::new();
    for (from, to) in object("rewrite_hosts")? {
        let to = to.as_str().ok_or_else(|| not_a_host(from))?;
        for host in [from.as_str(), to] {
            HostKey::new(host).map_err(|_| not_a_host(host))?;
        }
        let from = from.to_ascii_lowercase();
        if rewrite_hosts.contains_key(&from) {
            return Err(Refusal::RewriteNamedTwice {
                path: path.to_path_buf(),
                rule: name.to_owned(),
                host: from,
            });
        }
        rewrite_hosts.insert(from, to.to_ascii_lowercase());
    }
    let path_prefix = text("path_prefix")?;
    if !path_prefix.starts_with('/') {
        return Err(Refusal::PathPrefixNotRooted {
            path: path.to_path_buf(),
            rule: name.to_owned(),
            prefix: path_prefix,
        });
    }
    if !plain_path(&path_prefix) {
        return Err(Refusal::PathPrefixNotPlain {
            path: path.to_path_buf(),
            rule: name.to_owned(),
            prefix: path_prefix,
        });
    }
    let valid_format = text("valid_format")?;
    if !reads_a_minute(&valid_format) {
        return Err(Refusal::FormatReadsNoMinute {
            path: path.to_path_buf(),
            rule: name.to_owned(),
            format: valid_format,
        });
    }
    let index = match rule.get("index").map(Value::as_str) {
        None | Some(Some(RSS)) => IndexFormat::Rss,
        Some(Some(NEWS_SITEMAP)) => IndexFormat::NewsSitemap,
        Some(other) => return Err(unparseable(format!("index {other:?}"))),
    };
    let valid_field = text("valid_field")?;
    let news_field = valid_field
        .strip_prefix(NEWS_FIELD_PREFIX)
        .is_some_and(|name| !name.is_empty());
    if index == IndexFormat::NewsSitemap && !news_field {
        return Err(unparseable(format!(
            "valid_field {valid_field} names no {NEWS_FIELD_PREFIX} element"
        )));
    }
    let provider_keys = match rule.get("provider_keys") {
        None => None,
        Some(keys) => {
            let keys = keys
                .as_object()
                .ok_or_else(|| unparseable("provider_keys is not an object".to_owned()))?;
            if index != IndexFormat::NewsSitemap {
                return Err(unparseable(
                    "provider_keys are read from a news sitemap's image:loc only".to_owned(),
                ));
            }
            Some(provider_keys(keys, path, &unparseable)?)
        }
    };
    let body = object("body")?;
    known_fields(body, &BODY, path)?;
    Ok(Rule {
        index,
        valid_field,
        valid_format,
        path_prefix,
        rewrite_hosts,
        provider_keys,
        title: pick(object("title")?, path, &unparseable)?,
        canonical: pick(object("canonical")?, path, &unparseable)?,
        container: selector(body.get("container"), &unparseable)?,
        paragraph: selector(body.get("paragraph"), &unparseable)?,
    })
}

/// `format` prints `PROBE_MINUTE` as text that `minute_of` reads back as that minute, and prints
/// no second: a `%S` format reads only values at second 0 and refuses every other one.
fn reads_a_minute(format: &str) -> bool {
    let (year, month, day, hour, minute) = PROBE_MINUTE;
    let at = |second| {
        NaiveDate::from_ymd_opt(year, month, day)
            .and_then(|date| date.and_hms_opt(hour, minute, second))
    };
    let (Some(probe), Some(inside)) = (at(0), at(PROBE_SECOND)) else {
        return false;
    };
    match (printed(probe, format), printed(inside, format)) {
        (Some(minute_text), Some(second_text)) if minute_text == second_text => {
            minute_of(&minute_text, format).is_some_and(|read| read.start() == probe.and_utc())
        }
        _ => false,
    }
}

/// Whether `reader`'s format reads the probe minute as `printer`'s format prints it: one host's two
/// indexes are told apart by the spelling of a listed value, so neither may read the other's.
pub(crate) fn reads_the_other(printer: &Rule, reader: &Rule) -> bool {
    let (year, month, day, hour, minute) = PROBE_MINUTE;
    NaiveDate::from_ymd_opt(year, month, day)
        .and_then(|date| date.and_hms_opt(hour, minute, 0))
        .and_then(|probe| printed(probe, &printer.valid_format))
        .is_some_and(|text| minute_of(&text, &reader.valid_format).is_some())
}

/// At offset zero, so a format holding `%:z` prints too and reads back as the same instant.
fn printed(at: NaiveDateTime, format: &str) -> Option<String> {
    use std::fmt::Write;
    let mut text = String::new();
    write!(text, "{}", at.and_utc().fixed_offset().format(format)).ok()?;
    Some(text)
}

fn provider_keys(
    keys: &Map<String, Value>,
    path: &Path,
    unparseable: &dyn Fn(String) -> Refusal,
) -> Result<ProviderKeys, Refusal> {
    known_fields(keys, &PROVIDER_KEYS, path)?;
    let image_key_after = keys
        .get("image_key_after")
        .and_then(Value::as_str)
        .filter(|mark| mark.len() > 1 && mark.ends_with('/'))
        .ok_or_else(|| unparseable("image_key_after names no mark ending in /".to_owned()))?;
    let key = |name: &str, key: &str| {
        (!key.is_empty() && !key.contains('/'))
            .then(|| key.to_owned())
            .ok_or_else(|| unparseable(format!("{name} holds {key}, which is no key")))
    };
    let follow = keys
        .get("follow")
        .and_then(Value::as_array)
        .ok_or_else(|| unparseable("provider_keys has no follow list".to_owned()))?
        .iter()
        .map(|listed| key("follow", listed.as_str().unwrap_or_default()))
        .collect::<Result<BTreeSet<String>, Refusal>>()?;
    let cross_check = keys
        .get("cross_check")
        .and_then(Value::as_object)
        .ok_or_else(|| unparseable("provider_keys has no cross_check object".to_owned()))?
        .iter()
        .map(|(named, host)| {
            let host = host
                .as_str()
                .filter(|host| !host.is_empty())
                .ok_or_else(|| unparseable(format!("cross_check maps {named} to no host")))?;
            Ok((key("cross_check", named)?, host.to_ascii_lowercase()))
        })
        .collect::<Result<BTreeMap<String, String>, Refusal>>()?;
    if follow.is_empty() {
        return Err(unparseable("provider_keys follows no key".to_owned()));
    }
    if let Some(both) = follow.iter().find(|key| cross_check.contains_key(*key)) {
        return Err(unparseable(format!(
            "{both} is both followed and cross-checked"
        )));
    }
    Ok(ProviderKeys {
        image_key_after: image_key_after.to_owned(),
        follow,
        cross_check,
    })
}

fn pick(
    object: &Map<String, Value>,
    path: &Path,
    unparseable: &dyn Fn(String) -> Refusal,
) -> Result<Pick, Refusal> {
    known_fields(object, &PICK, path)?;
    let attribute = object
        .get("attribute")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| unparseable("a pick names no attribute".to_owned()))?;
    Ok(Pick {
        selector: selector(object.get("selector"), unparseable)?,
        attribute: attribute.to_owned(),
    })
}

fn selector(
    value: Option<&Value>,
    unparseable: &dyn Fn(String) -> Refusal,
) -> Result<Selector, Refusal> {
    let text = value
        .and_then(Value::as_str)
        .ok_or_else(|| unparseable("a selector is missing".to_owned()))?;
    Selector::parse(text).map_err(|why| unparseable(format!("selector {text}: {why:?}")))
}
