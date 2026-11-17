//! Option C (architecture.md section 11.2): a keyed index's rows under a `cross_check` key,
//! matched by normalised title against the titles the direct indexes listed. It stores nothing.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

use crate::minute_of;
use crate::parse::index_rows;
use crate::rules::{ProviderKeys, Rule};

/// One keyed index row under a `cross_check` key whose title no direct index listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unlisted {
    pub key: String,
    pub url: String,
    /// As the keyed index spells it; empty when the row has none.
    pub title: String,
    pub listed_raw: String,
}

/// One keyed index payload, read against the direct index payloads handed beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossCheck {
    /// The keyed index's rows by key: a `follow` key, a `cross_check` key, another, none.
    pub followed: u32,
    pub cross_checked: u32,
    pub other_key: u32,
    pub no_key: u32,
    /// `cross_checked` rows dated before `watched_from`, or with no minute the rule reads.
    pub not_compared: u32,
    pub listed_directly: u32,
    pub unlisted: Vec<Unlisted>,
    /// Distinct normalised titles the readable direct payloads listed.
    pub direct_titles: u32,
}

/// The keyed row's rule and every admitted direct row's index url with its rule.
#[derive(Debug, Clone)]
pub struct CrossChecker {
    index_url: String,
    rule: Rule,
    keys: ProviderKeys,
    direct: BTreeMap<String, Rule>,
}

impl CrossChecker {
    pub(crate) const fn new(
        index_url: String,
        rule: Rule,
        keys: ProviderKeys,
        direct: BTreeMap<String, Rule>,
    ) -> Self {
        Self {
            index_url,
            rule,
            keys,
            direct,
        }
    }

    /// The keyed index url, whose answers `check` reads.
    #[must_use]
    pub fn index_url(&self) -> &str {
        &self.index_url
    }

    /// The url of an admitted direct index, whose answers count as listed.
    #[must_use]
    pub fn is_direct_index(&self, url: &str) -> bool {
        self.direct.contains_key(url)
    }

    /// The earliest minute a `cross_check` row of `keyed` is listed at: no direct index answered
    /// before it can have listed any of them.
    #[must_use]
    pub fn earliest_cross_checked(&self, keyed: &[u8]) -> Option<DateTime<Utc>> {
        index_rows(keyed, &self.rule)?
            .iter()
            .filter(|row| {
                row.image
                    .as_deref()
                    .and_then(|image| self.keys.key_of(image))
                    .is_some_and(|key| self.keys.cross_check.contains(key))
            })
            .filter_map(|row| minute_of(row.valid_raw.as_deref()?, &self.rule.valid_format))
            .map(|minute| minute.start())
            .min()
    }

    /// `None` when `keyed` is not the rule's index shape. A direct payload whose url is no
    /// admitted direct index, or that does not parse, adds no title.
    #[must_use]
    pub fn check(
        &self,
        keyed: &[u8],
        direct: &[(String, Vec<u8>)],
        watched_from: Option<DateTime<Utc>>,
    ) -> Option<CrossCheck> {
        let rows = index_rows(keyed, &self.rule)?;
        let listed: BTreeSet<String> = direct
            .iter()
            .filter_map(|(url, bytes)| index_rows(bytes, self.direct.get(url)?))
            .flatten()
            .filter_map(|row| row.title.as_deref().map(normalised))
            .filter(|title| !title.is_empty())
            .collect();
        let mut checked = CrossCheck {
            followed: 0,
            cross_checked: 0,
            other_key: 0,
            no_key: 0,
            not_compared: 0,
            listed_directly: 0,
            unlisted: Vec::new(),
            direct_titles: u32::try_from(listed.len()).unwrap_or(u32::MAX),
        };
        for row in &rows {
            let key = row
                .image
                .as_deref()
                .and_then(|image| self.keys.key_of(image));
            let Some(key) = key else {
                checked.no_key += 1;
                continue;
            };
            if self.keys.follow.contains(key) {
                checked.followed += 1;
                continue;
            }
            if !self.keys.cross_check.contains(key) {
                checked.other_key += 1;
                continue;
            }
            checked.cross_checked += 1;
            let minute = row
                .valid_raw
                .as_deref()
                .and_then(|raw| minute_of(raw, &self.rule.valid_format));
            let watched = minute
                .is_some_and(|minute| watched_from.is_some_and(|from| minute.start() >= from));
            if !watched {
                checked.not_compared += 1;
            } else if row
                .title
                .as_deref()
                .is_some_and(|title| listed.contains(&normalised(title)))
            {
                checked.listed_directly += 1;
            } else {
                checked.unlisted.push(Unlisted {
                    key: key.to_owned(),
                    url: row.link.clone().unwrap_or_default(),
                    title: row.title.clone().unwrap_or_default(),
                    listed_raw: row.valid_raw.clone().unwrap_or_default(),
                });
            }
        }
        Some(checked)
    }
}

/// Section 11.6's title match: lowercase, each run of characters that are not alphanumeric one
/// space, trimmed. XML entities are already decoded by the index reader.
#[must_use]
pub fn normalised(title: &str) -> String {
    title
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<&str>>()
        .join(" ")
}
