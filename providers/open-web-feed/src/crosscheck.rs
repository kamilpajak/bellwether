//! Option C (architecture.md section 11.2): a keyed index's rows under a `cross_check` key,
//! matched by normalised title against the titles the direct indexes listed. It stores nothing.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, TimeDelta, Utc};

use crate::minute_of;
use crate::parse::index_rows;
use crate::rules::{ProviderKeys, Rule};

/// The longest period of `newsvault`'s schedule (`schedule::SLOTS`, 90 min); a `newsvault` test
/// holds this copy to that table (architecture.md section 11.2).
pub const LONGEST_POLL_PERIOD: TimeDelta = TimeDelta::minutes(90);

/// How long before a keyed row's minute a direct answer is still read (owner decision 2026-11-17).
pub const DIRECT_LEAD: TimeDelta = LONGEST_POLL_PERIOD;

/// One stored direct index answer: the url it was asked at, when it arrived, its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectAnswer {
    pub url: String,
    pub received_at: DateTime<Utc>,
    pub bytes: Vec<u8>,
}

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
    /// `cross_checked` rows whose key's own direct index has no parsed answer after their minute
    /// closed: "we did not look", never "not listed".
    pub not_looked: u32,
    /// `cross_checked` rows whose key's host is a candidate row: no direct index to compare with.
    pub no_direct_index: u32,
    pub listed_directly: u32,
    pub unlisted: Vec<Unlisted>,
    /// Distinct normalised titles the readable direct payloads listed.
    pub direct_titles: u32,
    /// Direct answers under an admitted direct index url that parsed as its rule's index shape.
    pub direct_parsed: u32,
}

/// The keyed row's rule, every admitted direct row's index url with its rule, and each
/// `cross_check` key's own direct index url, absent for a key whose host is not admitted.
#[derive(Debug, Clone)]
pub struct CrossChecker {
    index_url: String,
    rule: Rule,
    keys: ProviderKeys,
    direct: BTreeMap<String, Rule>,
    index_of: BTreeMap<String, String>,
}

impl CrossChecker {
    pub(crate) const fn new(
        index_url: String,
        rule: Rule,
        keys: ProviderKeys,
        direct: BTreeMap<String, Rule>,
        index_of: BTreeMap<String, String>,
    ) -> Self {
        Self {
            index_url,
            rule,
            keys,
            direct,
            index_of,
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

    /// The earliest direct answer `check` needs: `DIRECT_LEAD` before the earliest `cross_check`
    /// row's minute.
    #[must_use]
    pub fn read_from(&self, keyed: &[u8]) -> Option<DateTime<Utc>> {
        self.earliest_cross_checked(keyed)?
            .checked_sub_signed(DIRECT_LEAD)
    }

    /// The earliest minute a `cross_check` row of `keyed` with a direct index is listed at.
    #[must_use]
    pub fn earliest_cross_checked(&self, keyed: &[u8]) -> Option<DateTime<Utc>> {
        index_rows(keyed, &self.rule)?
            .iter()
            .filter(|row| {
                row.image
                    .as_deref()
                    .and_then(|image| self.keys.key_of(image))
                    .is_some_and(|key| self.index_of.contains_key(key))
            })
            .filter_map(|row| minute_of(row.valid_raw.as_deref()?, &self.rule.valid_format))
            .map(|minute| minute.start())
            .min()
    }

    /// `None` when `keyed` is not the rule's index shape. A direct answer whose url is no
    /// admitted direct index, or that does not parse, adds no title and counts as no answer.
    #[must_use]
    pub fn check(
        &self,
        keyed: &[u8],
        direct: &[DirectAnswer],
        watched_from: Option<DateTime<Utc>>,
    ) -> Option<CrossCheck> {
        let rows = index_rows(keyed, &self.rule)?;
        let mut listed: BTreeSet<String> = BTreeSet::new();
        let mut newest: BTreeMap<&str, DateTime<Utc>> = BTreeMap::new();
        let mut direct_parsed: u32 = 0;
        for answer in direct {
            let Some(parsed) = self
                .direct
                .get(&answer.url)
                .and_then(|rule| index_rows(&answer.bytes, rule))
            else {
                continue;
            };
            direct_parsed = direct_parsed.saturating_add(1);
            let at = newest
                .entry(answer.url.as_str())
                .or_insert(answer.received_at);
            *at = (*at).max(answer.received_at);
            listed.extend(
                parsed
                    .iter()
                    .filter_map(|row| row.title.as_deref().map(normalised))
                    .filter(|title| !title.is_empty()),
            );
        }
        let mut checked = CrossCheck {
            followed: 0,
            cross_checked: 0,
            other_key: 0,
            no_key: 0,
            not_compared: 0,
            not_looked: 0,
            no_direct_index: 0,
            listed_directly: 0,
            unlisted: Vec::new(),
            direct_titles: u32::try_from(listed.len()).unwrap_or(u32::MAX),
            direct_parsed,
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
            if !self.keys.cross_check.contains_key(key) {
                checked.other_key += 1;
                continue;
            }
            checked.cross_checked += 1;
            let Some(own) = self.index_of.get(key) else {
                checked.no_direct_index += 1;
                continue;
            };
            let minute = row
                .valid_raw
                .as_deref()
                .and_then(|raw| minute_of(raw, &self.rule.valid_format));
            let watched = minute
                .is_some_and(|minute| watched_from.is_some_and(|from| minute.start() >= from));
            let looked_until = newest.get(own.as_str());
            let looked = minute
                .and_then(|minute| minute.closes())
                .is_some_and(|closes| looked_until.is_some_and(|until| *until >= closes));
            if !watched {
                checked.not_compared += 1;
            } else if !looked {
                checked.not_looked += 1;
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
