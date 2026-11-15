//! The hashed extraction rules, one per hosts-file `rule` name. Every field is read by `extract`
//! or `item_key`, so `extract_rules_version` covers the path filter and the host rewrite too
//! (architecture.md section 11.1).

use std::collections::BTreeMap;
use std::path::Path;

use scraper::Selector;
use serde_json::{Map, Value};

use crate::{known_fields, Refusal};

pub(crate) const RULES_SCHEMA: &str = "open-web-feed.rules/1";
const TOP: [&str; 2] = ["schema", "rules"];
const RULE: [&str; 7] = [
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

/// One element's attribute, picked by a CSS selector.
#[derive(Debug, Clone)]
pub(crate) struct Pick {
    pub(crate) selector: Selector,
    pub(crate) attribute: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Rule {
    /// The index row's element holding the publication value, and `CollectedItem.valid_field`.
    pub(crate) valid_field: String,
    /// A chrono format with no seconds field: a value it cannot read is `NoParseableValidTime`.
    pub(crate) valid_format: String,
    /// A link whose path does not start with this is filtered out at the index.
    pub(crate) path_prefix: String,
    /// Host to host, applied before the path filter and inside `item_key` (section 11.2).
    pub(crate) rewrite_hosts: BTreeMap<String, String>,
    pub(crate) title: Pick,
    pub(crate) canonical: Pick,
    /// The article container; its `paragraph` descendants are the body (section 11.6's rule).
    pub(crate) container: Selector,
    pub(crate) paragraph: Selector,
}

pub(crate) fn parse_rules(bytes: &[u8], path: &Path) -> Result<BTreeMap<String, Rule>, Refusal> {
    let unparseable = |why: String| Refusal::RulesUnparseable {
        path: path.to_path_buf(),
        why,
    };
    let read: Value = serde_json::from_slice(bytes).map_err(|why| unparseable(why.to_string()))?;
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
    let mut rewrite_hosts = BTreeMap::new();
    for (from, to) in object("rewrite_hosts")? {
        let to = to
            .as_str()
            .ok_or_else(|| unparseable(format!("rewrite of {from} is not a host")))?;
        rewrite_hosts.insert(from.to_ascii_lowercase(), to.to_ascii_lowercase());
    }
    let body = object("body")?;
    known_fields(body, &BODY, path)?;
    Ok(Rule {
        valid_field: text("valid_field")?,
        valid_format: text("valid_format")?,
        path_prefix: text("path_prefix")?,
        rewrite_hosts,
        title: pick(object("title")?, path, &unparseable)?,
        canonical: pick(object("canonical")?, path, &unparseable)?,
        container: selector(body.get("container"), &unparseable)?,
        paragraph: selector(body.get("paragraph"), &unparseable)?,
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
