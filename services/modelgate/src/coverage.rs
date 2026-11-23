//! Prior coverage: how many earlier stories of the same cut carry one of a story's exchange-ticker
//! tags. Code computes it at label time and no request carries it. Grammar, windows and what the
//! count misses: `docs/design/architecture.md` section 3.5, "Prior coverage".

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const RECORD: &str = "prior-coverage/1";
/// Names what the tables below cannot: the parenthesis rule, the separators and the ticker shape.
const GRAMMAR: &str = "ticker-tag/1";
pub const WINDOW_DAYS: [u32; 3] = [1, 7, 30];
/// A family and the spellings read as it, each upper-cased with its spaces removed.
const EXCHANGES: [(&str, &[&str]); 5] = [
    ("NASDAQ", &["NASDAQ", "NASDAQGS", "NASDAQGM", "NASDAQCM"]),
    ("NYSE", &["NYSE"]),
    ("NYSEAMERICAN", &["NYSEAMERICAN", "NYSEMKT", "AMEX"]),
    ("NYSEARCA", &["NYSEARCA"]),
    ("OTC", &["OTC", "OTCQB", "OTCQX", "OTCPINK", "OTCMKTS"]),
];
const MAX_GROUP_BYTES: usize = 120;
const TICKER_LETTERS: std::ops::RangeInclusive<usize> = 1..=5;
const CLASS_LETTERS: std::ops::RangeInclusive<usize> = 1..=2;

/// The token a stored record carries. Built from the tables, so a changed exchange list or window
/// is another token without anyone remembering to rename it.
#[must_use]
pub fn version() -> String {
    let exchanges: Vec<String> = EXCHANGES
        .iter()
        .map(|(family, spellings)| format!("{family}={}", spellings.join(",")))
        .collect();
    let days: Vec<String> = WINDOW_DAYS.iter().map(u32::to_string).collect();
    format!(
        "{RECORD}({GRAMMAR};{};days={})",
        exchanges.join(";"),
        days.join(",")
    )
}

fn letters(text: &str, count: &std::ops::RangeInclusive<usize>) -> bool {
    count.contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_uppercase())
}

fn is_ticker(text: &str) -> bool {
    match text.split_once(['.', '-']) {
        Some((root, class)) => letters(root, &TICKER_LETTERS) && letters(class, &CLASS_LETTERS),
        None => letters(text, &TICKER_LETTERS),
    }
}

/// `EXCHANGE: TICKER` as `FAMILY:TICKER`, or nothing.
fn pair(piece: &str) -> Option<String> {
    let (exchange, ticker) = piece.split_once(':')?;
    let spelled: String = exchange
        .chars()
        .filter(|character| !character.is_whitespace())
        .map(|character| character.to_ascii_uppercase())
        .collect();
    let (family, _) = EXCHANGES
        .iter()
        .find(|(_, spellings)| spellings.contains(&spelled.as_str()))?;
    let ticker = ticker.trim();
    is_ticker(ticker).then(|| format!("{family}:{ticker}"))
}

/// Every tag of `text`: inside one pair of parentheses, pieces split at `;` and `,`.
#[must_use]
pub fn tags(text: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut rest = text;
    while let Some(open) = rest.find('(') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find(')') else { break };
        let group = &rest[..close];
        if group.contains('(') {
            continue;
        }
        if group.len() <= MAX_GROUP_BYTES {
            found.extend(group.split([';', ',']).filter_map(pair));
        }
        rest = &rest[close + 1..];
    }
    found
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowCount {
    pub days: u32,
    /// Distinct earlier stories carrying at least one of the story's tags.
    pub stories: u64,
    /// Whether the cut reaches back to the window's start. A count over less is a floor.
    pub complete: bool,
}

/// `NoTag` is not a count of zero: nothing was looked for, because there was nothing to key on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PriorCoverage {
    NoTag,
    Tagged {
        tags: Vec<String>,
        windows: Vec<WindowCount>,
    },
}

/// Every tagged row of one cut, by tag.
#[derive(Debug, Default)]
pub struct Mentions {
    by_tag: HashMap<String, Vec<(DateTime<Utc>, String)>>,
    earliest: Option<DateTime<Utc>>,
}

impl Mentions {
    pub fn add(&mut self, story_id: &str, observed_at: DateTime<Utc>, body: Option<&str>) {
        self.earliest = Some(self.earliest.map_or(observed_at, |at| at.min(observed_at)));
        for tag in tags(body.unwrap_or_default()) {
            let seen = self.by_tag.entry(tag).or_default();
            seen.push((observed_at, story_id.to_owned()));
        }
    }

    /// The record of a story first observed at `observed_at`: rows observed strictly before it,
    /// and at or after each window's start.
    #[must_use]
    pub fn prior(&self, story_id: &str, observed_at: DateTime<Utc>, body: &str) -> PriorCoverage {
        let own = tags(body);
        if own.is_empty() {
            return PriorCoverage::NoTag;
        }
        let windows = WINDOW_DAYS
            .iter()
            .map(|&days| {
                let from = observed_at - TimeDelta::days(i64::from(days));
                let earlier: BTreeSet<&str> = own
                    .iter()
                    .filter_map(|tag| self.by_tag.get(tag))
                    .flatten()
                    .filter(|(at, id)| *at >= from && *at < observed_at && id != story_id)
                    .map(|(_, id)| id.as_str())
                    .collect();
                WindowCount {
                    days,
                    stories: u64::try_from(earlier.len()).unwrap_or(u64::MAX),
                    complete: self.earliest.is_some_and(|earliest| earliest <= from),
                }
            })
            .collect();
        PriorCoverage::Tagged {
            tags: own.into_iter().collect(),
            windows,
        }
    }
}

/// The envelope's `prior_coverage`: per window, tagged stories with no earlier story, with at
/// least one, the highest count, and how many counts are over an incomplete window.
#[must_use]
pub fn summary(records: &[PriorCoverage]) -> Value {
    let tagged: Vec<&Vec<WindowCount>> = records
        .iter()
        .filter_map(|record| match record {
            PriorCoverage::Tagged { windows, .. } => Some(windows),
            PriorCoverage::NoTag => None,
        })
        .collect();
    let windows: Vec<Value> = WINDOW_DAYS
        .iter()
        .map(|&days| {
            let counts: Vec<&WindowCount> = tagged
                .iter()
                .flat_map(|windows| windows.iter())
                .filter(|window| window.days == days)
                .collect();
            let none = counts.iter().filter(|window| window.stories == 0).count();
            json!({
                "days": days, "none": none, "some": counts.len() - none,
                "max": counts.iter().map(|window| window.stories).max(),
                "incomplete": counts.iter().filter(|window| !window.complete).count(),
            })
        })
        .collect();
    json!({
        "version": version(), "stories": records.len(),
        "no_tag": records.len() - tagged.len(), "tagged": tagged.len(), "windows": windows,
    })
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use serde_json::json;

    use super::{summary, tags, version, Mentions, PriorCoverage, WindowCount};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn found(text: &str) -> Vec<String> {
        tags(text).into_iter().collect()
    }

    fn at(text: &str) -> Result<DateTime<Utc>, chrono::ParseError> {
        text.parse()
    }

    /// The grammar's vectors, every text written here. Mutations 2026-11-23, each red: exchange
    /// names compared without upper-casing; the ticker upper-cased before the shape test; the
    /// `(` test on a group removed; pieces split at `;` only.
    #[test]
    fn a_tag_is_an_accepted_exchange_and_a_ticker_inside_one_pair_of_parentheses() {
        let read: [(&str, &[&str]); 12] = [
            ("Largeplat Corp. (NASDAQ: LRGP) today", &["NASDAQ:LRGP"]),
            (
                "Largeplat (Nasdaq:LRGP) and (NasdaqGS : LRGP)",
                &["NASDAQ:LRGP"],
            ),
            (
                "Holdco (NYSE: HLD.A; NYSE: HLD-B)",
                &["NYSE:HLD-B", "NYSE:HLD.A"],
            ),
            (
                "Smallco (NYSE American: SMQ, TSX: SMQ)",
                &["NYSEAMERICAN:SMQ"],
            ),
            ("Tinyco (OTCQB: TNYQF)", &["OTC:TNYQF"]),
            (
                "Two (NYSE: AAA) names (NASDAQ: BBB)",
                &["NASDAQ:BBB", "NYSE:AAA"],
            ),
            ("Nested (the \"Company\" (NYSE: NST))", &["NYSE:NST"]),
            ("No parentheses NASDAQ: LRGP", &[]),
            ("Brackets [NYSE: BRK]", &[]),
            ("Foreign (TSX: FOR) and (LSE: FOR)", &[]),
            ("Lower case (NASDAQ: lrgp), long (NYSE: TOOLONG)", &[]),
            ("A ratio (NYSE: 3:1) and an open one (NASDAQ: LRGP", &[]),
        ];
        for (text, expected) in read {
            assert_eq!(found(text), expected, "{text}");
        }
    }

    /// The token moves with the exchange table and the windows, and section 3.5 names it.
    /// Mutations 2026-11-23, each red: a spelling added to `EXCHANGES`; a window of 31 days.
    #[test]
    fn the_version_token_is_built_from_the_tables_and_is_the_one_the_design_names() {
        let token = version();
        assert_eq!(
            token,
            "prior-coverage/1(ticker-tag/1;NASDAQ=NASDAQ,NASDAQGS,NASDAQGM,NASDAQCM;NYSE=NYSE;\
             NYSEAMERICAN=NYSEAMERICAN,NYSEMKT,AMEX;NYSEARCA=NYSEARCA;\
             OTC=OTC,OTCQB,OTCQX,OTCPINK,OTCMKTS;days=1,7,30)"
        );
        let design = include_str!("../../../docs/design/architecture.md");
        assert!(design.contains(&token), "{token}");
    }

    fn index() -> Result<Mentions, chrono::ParseError> {
        let mut mentions = Mentions::default();
        let rows = [
            ("old", "2026-10-01T09:00:00Z", Some("Unrelated (NYSE: OLD)")),
            (
                "month",
                "2026-11-01T12:00:00Z",
                Some("Alert (NASDAQ: LRGP)"),
            ),
            ("week", "2026-11-17T12:00:00Z", Some("Note (Nasdaq: LRGP)")),
            ("week", "2026-11-18T12:00:00Z", Some("Note (Nasdaq: LRGP)")),
            (
                "day",
                "2026-11-19T18:00:00Z",
                Some("Both (NASDAQ: LRGP; NYSE: SMQ)"),
            ),
            ("other", "2026-11-19T19:00:00Z", Some("Other (NYSE: OTH)")),
            ("bare", "2026-11-19T20:00:00Z", None),
            (
                "twin",
                "2026-11-20T10:00:00Z",
                Some("Same instant (NASDAQ: LRGP)"),
            ),
            (
                "later",
                "2026-11-20T11:00:00Z",
                Some("Later (NASDAQ: LRGP)"),
            ),
        ];
        for (story, observed_at, body) in rows {
            mentions.add(story, at(observed_at)?, body);
        }
        Ok(mentions)
    }

    /// Point in time. Mutations 2026-11-23, each red: `<` to `<=` on the story's own instant;
    /// the upper bound removed; the window's start removed; the story's own id counted; rows
    /// counted instead of distinct stories.
    #[test]
    fn only_distinct_stories_observed_before_the_story_and_inside_the_window_are_counted(
    ) -> TestResult {
        let mentions = index()?;
        let record = mentions.prior(
            "story",
            at("2026-11-20T10:00:00Z")?,
            "Largeplat (NASDAQ: LRGP)",
        );
        let window = |days, stories, complete| WindowCount {
            days,
            stories,
            complete,
        };
        let expected = PriorCoverage::Tagged {
            tags: vec!["NASDAQ:LRGP".to_owned()],
            windows: vec![window(1, 1, true), window(7, 2, true), window(30, 3, true)],
        };
        assert_eq!(record, expected);
        let own_earlier_row = mentions.prior("day", at("2026-11-20T10:00:00Z")?, "(NASDAQ: LRGP)");
        assert!(
            matches!(&own_earlier_row, PriorCoverage::Tagged { windows, .. } if windows[0].stories == 0),
            "{own_earlier_row:?}"
        );
        Ok(())
    }

    /// Mutations 2026-11-23, each red: `NoTag` replaced by three windows of zero; `complete`
    /// always true.
    #[test]
    fn no_tag_is_not_zero_and_a_window_the_cut_does_not_reach_back_over_is_marked() -> TestResult {
        let mentions = index()?;
        let instant = at("2026-10-20T10:00:00Z")?;
        assert_eq!(
            mentions.prior("story", instant, "Largeplat today released a model."),
            PriorCoverage::NoTag
        );
        let early = mentions.prior("story", instant, "Largeplat (NASDAQ: LRGP)");
        let two_tags = mentions.prior(
            "story",
            at("2026-11-20T10:00:00Z")?,
            "(NYSE: OTH) (NYSE: SMQ)",
        );
        let PriorCoverage::Tagged { windows, .. } = &early else {
            return Err("a tagged story read as untagged".into());
        };
        let read: Vec<(u64, bool)> = windows.iter().map(|w| (w.stories, w.complete)).collect();
        assert_eq!(read, [(0, true), (0, true), (0, false)]);
        let PriorCoverage::Tagged { windows, .. } = &two_tags else {
            return Err("a tagged story read as untagged".into());
        };
        assert_eq!(windows[0].stories, 2);
        assert_eq!(
            summary(&[early.clone(), two_tags.clone(), PriorCoverage::NoTag])["windows"],
            json!([
                {"days": 1, "none": 1, "some": 1, "max": 2, "incomplete": 0},
                {"days": 7, "none": 1, "some": 1, "max": 2, "incomplete": 0},
                {"days": 30, "none": 1, "some": 1, "max": 2, "incomplete": 1},
            ])
        );
        assert_eq!(
            summary(&[PriorCoverage::NoTag])["windows"][0]["max"],
            json!(null)
        );
        Ok(())
    }
}
