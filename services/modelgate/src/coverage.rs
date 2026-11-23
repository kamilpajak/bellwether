//! Prior coverage: how many earlier stories of the same cut carry one of a story's exchange-ticker
//! tags. Code computes it at label time and no request carries it. Grammar, windows and what the
//! count misses: `docs/design/architecture.md` section 3.5, "Prior coverage".

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const RECORD: &str = "prior-coverage/1";
/// Names what the tables below cannot: the parenthesis rule, the separators, the first-word rule.
const GRAMMAR: &str = "ticker-tag/1";
pub const WINDOW_DAYS: [u32; 3] = [1, 7, 30];
/// Spellings that mark a US listing, each upper-cased with its spaces and `-` removed. A filter
/// only: the tag is the ticker, so one company under two of them is one tag.
const EXCHANGES: [&str; 16] = [
    "NASDAQ",
    "NASDAQGS",
    "NASDAQGM",
    "NASDAQCM",
    "NYSE",
    "NYSEAMERICAN",
    "NYSEMKT",
    "AMEX",
    "NYSEARCA",
    "OTC",
    "OTCQB",
    "OTCQX",
    "OTCPINK",
    "OTCMKTS",
    "OTCMARKETS",
    "OTHEROTC",
];
const TICKER_LETTERS: std::ops::RangeInclusive<usize> = 1..=5;
const CLASS_LETTERS: std::ops::RangeInclusive<usize> = 1..=2;

/// The token a stored record carries. Built from the tables, so a changed exchange list, ticker
/// length or window is another token without anyone remembering to rename it.
#[must_use]
pub fn version() -> String {
    let days: Vec<String> = WINDOW_DAYS.iter().map(u32::to_string).collect();
    format!(
        "{RECORD}({GRAMMAR};exchanges={};ticker={}-{};class={}-{};days={})",
        EXCHANGES.join(","),
        TICKER_LETTERS.start(),
        TICKER_LETTERS.end(),
        CLASS_LETTERS.start(),
        CLASS_LETTERS.end(),
        days.join(",")
    )
}

fn letters(text: &str, count: &std::ops::RangeInclusive<usize>) -> bool {
    count.contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_uppercase())
}

/// `word` as a tag, a class share written `ROOT.X` whichever of `.` and `-` the text used.
fn ticker(word: &str) -> Option<String> {
    let shaped = match word.split_once(['.', '-']) {
        Some((root, class)) => letters(root, &TICKER_LETTERS) && letters(class, &CLASS_LETTERS),
        None => letters(word, &TICKER_LETTERS),
    };
    shaped.then(|| word.replace('-', "."))
}

fn is_exchange(text: &str) -> bool {
    let spelled: String = text
        .chars()
        .filter(|character| !character.is_whitespace() && *character != '-')
        .map(|character| character.to_ascii_uppercase())
        .collect();
    EXCHANGES.contains(&spelled.as_str())
}

/// Every tag of one group: `EXCHANGE: TICKER` takes the first word after the colon, and a bare
/// ticker is read only straight after a piece that named an accepted exchange.
fn group_tags(group: &str, found: &mut BTreeSet<String>) {
    let mut listed = false;
    for piece in group.split([';', ',']) {
        let word = match piece.split_once(':') {
            Some((exchange, rest)) => {
                listed = is_exchange(exchange);
                rest.split_whitespace().next()
            }
            None => Some(piece.trim()),
        };
        if let Some(tag) = word.filter(|_| listed).and_then(ticker) {
            found.insert(tag);
        }
    }
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
        group_tags(group, &mut found);
        rest = &rest[close + 1..];
    }
    found
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowCount {
    pub days: u32,
    /// Distinct earlier stories carrying at least one of the story's tags.
    pub stories: u64,
    /// Whether the cut reaches back to the window's start. A count over less is a floor.
    pub complete: bool,
    /// The same count for each tag alone, so a partner's coverage is not read as the issuer's.
    pub by_tag: BTreeMap<String, u64>,
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

/// Per tag and story, the first instant a row of that story carried the tag in one cut.
#[derive(Debug, Default)]
pub struct Mentions {
    first: HashMap<String, HashMap<String, DateTime<Utc>>>,
    earliest: Option<DateTime<Utc>>,
}

fn count(stories: usize) -> u64 {
    u64::try_from(stories).unwrap_or(u64::MAX)
}

impl Mentions {
    pub fn add(&mut self, story_id: &str, observed_at: DateTime<Utc>, body: Option<&str>) {
        self.earliest = Some(self.earliest.map_or(observed_at, |at| at.min(observed_at)));
        for tag in tags(body.unwrap_or_default()) {
            let seen = self.first.entry(tag).or_default();
            let kept = seen.entry(story_id.to_owned()).or_insert(observed_at);
            *kept = observed_at.min(*kept);
        }
    }

    /// The record of a story first observed at `observed_at`: other stories whose first tagged
    /// row is strictly before it, and at or after each window's start.
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
                let mut pooled: BTreeSet<&str> = BTreeSet::new();
                let mut by_tag = BTreeMap::new();
                for tag in &own {
                    let earlier: Vec<&str> = self
                        .first
                        .get(tag)
                        .into_iter()
                        .flatten()
                        .filter(|(id, at)| **at >= from && **at < observed_at && *id != story_id)
                        .map(|(id, _)| id.as_str())
                        .collect();
                    by_tag.insert(tag.clone(), count(earlier.len()));
                    pooled.extend(earlier);
                }
                WindowCount {
                    days,
                    stories: count(pooled.len()),
                    complete: self.earliest.is_some_and(|earliest| earliest <= from),
                    by_tag,
                }
            })
            .collect();
        PriorCoverage::Tagged {
            tags: own.into_iter().collect(),
            windows,
        }
    }
}

/// The envelope's `prior_coverage`. Per window: tagged stories with no earlier story, with at
/// least one, with none for at least one of their tags, the highest count, and how many counts
/// are over an incomplete window.
#[must_use]
pub fn summary(records: &[PriorCoverage]) -> Value {
    let tagged: Vec<(&Vec<String>, &Vec<WindowCount>)> = records
        .iter()
        .filter_map(|record| match record {
            PriorCoverage::Tagged { tags, windows } => Some((tags, windows)),
            PriorCoverage::NoTag => None,
        })
        .collect();
    let windows: Vec<Value> = WINDOW_DAYS
        .iter()
        .map(|&days| {
            let counts: Vec<&WindowCount> = tagged
                .iter()
                .flat_map(|(_, windows)| windows.iter())
                .filter(|window| window.days == days)
                .collect();
            let none = counts.iter().filter(|window| window.stories == 0).count();
            let a_tag_bare = |window: &&&WindowCount| window.by_tag.values().any(|&n| n == 0);
            json!({
                "days": days, "none": none, "some": counts.len() - none,
                "none_for_a_tag": counts.iter().filter(a_tag_bare).count(),
                "max": counts.iter().map(|window| window.stories).max(),
                "incomplete": counts.iter().filter(|window| !window.complete).count(),
            })
        })
        .collect();
    json!({
        "version": version(), "stories": records.len(),
        "no_tag": records.len() - tagged.len(), "tagged": tagged.len(),
        "several_tags": tagged.iter().filter(|(tags, _)| tags.len() > 1).count(),
        "windows": windows,
    })
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use serde_json::json;

    use super::{summary, tags, version, Mentions, PriorCoverage, WindowCount};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const STORY_AT: &str = "2026-11-20T10:00:00Z";

    fn found(text: &str) -> Vec<String> {
        tags(text).into_iter().collect()
    }

    fn at(text: &str) -> Result<DateTime<Utc>, chrono::ParseError> {
        text.parse()
    }

    /// The grammar's vectors, every text written here; the last block is what it misses or reads
    /// wrongly (section 3.5). Mutations 2026-11-23: see section 8 row MG12.
    #[test]
    fn a_tag_is_a_ticker_after_an_accepted_exchange_inside_one_pair_of_parentheses() {
        let read: [(&str, &[&str]); 27] = [
            ("Largeplat Corp. (NASDAQ: LRGP) today", &["LRGP"]),
            ("Largeplat (Nasdaq:LRGP) and (NasdaqGS : LRGP)", &["LRGP"]),
            ("One company (NASDAQ: LRGP) (NYSE: LRGP)", &["LRGP"]),
            (
                "Hyphens (NYSE-MKT: SMQ) (NASDAQ-GS: LRGP)",
                &["LRGP", "SMQ"],
            ),
            (
                "Tinyco (OTC Markets: TNYQY) (Other OTC: TNYQF)",
                &["TNYQF", "TNYQY"],
            ),
            ("Holdco (NYSE: HLD.A; NYSE: HLD-B)", &["HLD.A", "HLD.B"]),
            ("Two classes (NASDAQ: GGL, GGLA)", &["GGL", "GGLA"]),
            ("Smallco (NYSE American: SMQ, TSX: SMQ, XYZ)", &["SMQ"]),
            ("Two venues (NASDAQ: ABC and TSX: ABC)", &["ABC"]),
            ("Defined (NYSE: ABC or the \"Company\")", &["ABC"]),
            (
                "Tinyco (\"TN\" or the \"Company\", TSX.V: TNY; OTCQB: TNYQF)",
                &["TNYQF"],
            ),
            ("Two (NYSE: AAA) names (NASDAQ: BBB)", &["AAA", "BBB"]),
            ("Nested (the \"Company\" (NYSE: NST))", &["NST"]),
            ("Five letters (NYSE: ABCDE), six (NYSE: ABCDEF)", &["ABCDE"]),
            (
                "Class of two (NYSE: AB.CD), of three (NYSE: AB.CDE)",
                &["AB.CD"],
            ),
            ("No parentheses NASDAQ: LRGP", &[]),
            ("Brackets [NYSE: BRK]", &[]),
            ("Foreign (TSX: FOR) and (LSE: FOR)", &[]),
            ("A bare ticker after a foreign venue (TSX: FOR, FORX)", &[]),
            (
                "Lower case (NASDAQ: lrgp), a bare word (NYSE: ABC, Inc)",
                &["ABC"],
            ),
            ("A ratio (NYSE: 3:1) and an open one (NASDAQ: LRGP", &[]),
            ("A long name (Nasdaq Global Select Market: LRGP)", &[]),
            ("Joined venues (NASDAQ & TSX: ABC)", &[]),
            ("The foreign venue first (TSX: ABC and NASDAQ: ABC)", &[]),
            ("A full-width colon (NASDAQ\u{ff1a}LRGP)", &[]),
            ("A fund, read as a company (NASDAQ: QQQ)", &["QQQ"]),
            ("A word, read as a ticker (NYSE: A new listing)", &["A"]),
        ];
        for (text, expected) in read {
            assert_eq!(found(text), expected, "{text}");
        }
        let many: Vec<String> = ('A'..='L').map(|last| format!("NYSE: AB{last}")).collect();
        let group = format!("({})", many.join("; "));
        assert!(group.len() > 120, "{}", group.len());
        assert_eq!(found(&group).len(), 12);
    }

    /// The token moves with the exchange list, the ticker shape and the windows, and section 3.5
    /// names it. Mutations 2026-11-23: see section 8 row MG12.
    #[test]
    fn the_version_token_is_built_from_the_tables_and_is_the_one_the_design_names() {
        let token = version();
        assert_eq!(
            token,
            "prior-coverage/1(ticker-tag/1;exchanges=NASDAQ,NASDAQGS,NASDAQGM,NASDAQCM,NYSE,\
             NYSEAMERICAN,NYSEMKT,AMEX,NYSEARCA,OTC,OTCQB,OTCQX,OTCPINK,OTCMKTS,OTCMARKETS,\
             OTHEROTC;ticker=1-5;class=1-2;days=1,7,30)"
        );
        let design = include_str!("../../../docs/design/architecture.md");
        assert!(design.contains(&token), "{token}");
    }

    /// One row on each window's first instant and one a microsecond before it; `old` is seen
    /// again inside the day; `reach`, with no body, is the earliest row of the cut.
    fn index() -> Result<Mentions, chrono::ParseError> {
        const LRGP: Option<&str> = Some("Alert (NASDAQ: LRGP)");
        let mut mentions = Mentions::default();
        let rows = [
            ("reach", "2026-10-01T09:00:00Z", None),
            ("before30", "2026-10-21T09:59:59.999999Z", LRGP),
            ("at30", "2026-10-21T10:00:00Z", LRGP),
            ("old", "2026-10-25T10:00:00Z", LRGP),
            ("before7", "2026-11-13T09:59:59.999999Z", LRGP),
            ("at7", "2026-11-13T10:00:00Z", LRGP),
            ("before1", "2026-11-19T09:59:59.999999Z", LRGP),
            ("at1", "2026-11-19T10:00:00Z", LRGP),
            (
                "both",
                "2026-11-19T18:00:00Z",
                Some("Both (NASDAQ: LRGP; NYSE: SMQ)"),
            ),
            ("other", "2026-11-19T19:00:00Z", Some("Other (NYSE: OTH)")),
            ("story", "2026-11-19T20:00:00Z", LRGP),
            ("old", "2026-11-20T08:00:00Z", LRGP),
            ("twin", STORY_AT, LRGP),
            ("later", "2026-11-20T11:00:00Z", LRGP),
        ];
        for (story, observed_at, body) in rows {
            mentions.add(story, at(observed_at)?, body);
        }
        Ok(mentions)
    }

    /// One window: pooled count, complete, count per tag in tag order.
    type Read = (u64, bool, Vec<u64>);

    fn read(record: &PriorCoverage) -> Result<Vec<Read>, &'static str> {
        let PriorCoverage::Tagged { windows, .. } = record else {
            return Err("a tagged story read as untagged");
        };
        let one = |w: &WindowCount| (w.stories, w.complete, w.by_tag.values().copied().collect());
        Ok(windows.iter().map(one).collect())
    }

    /// Point in time, and a window counts the stories the cut first held inside it. Mutations
    /// 2026-11-23: see section 8 row MG12.
    #[test]
    fn a_window_counts_other_stories_first_seen_inside_it_and_before_the_story() -> TestResult {
        let mentions = index()?;
        let record = mentions.prior("story", at(STORY_AT)?, "Largeplat (NASDAQ: LRGP)");
        assert_eq!(
            read(&record)?,
            [(2, true, vec![2]), (4, true, vec![4]), (7, true, vec![7])]
        );
        assert!(matches!(&record, PriorCoverage::Tagged { tags, .. } if tags == &["LRGP"]));
        Ok(())
    }

    /// A partner's coverage stays apart from the issuer's, and a story under two of the tags is
    /// one story. Mutations 2026-11-23: see section 8 row MG12.
    #[test]
    fn each_tag_keeps_its_own_count_beside_the_pooled_one() -> TestResult {
        let mentions = index()?;
        let partner = "Quietco (NYSE: QUI) signs with Largeplat (NASDAQ: LRGP)";
        let named = mentions.prior("story", at(STORY_AT)?, partner);
        assert!(matches!(&named, PriorCoverage::Tagged { tags, .. } if tags == &["LRGP", "QUI"]));
        assert_eq!(read(&named)?[0], (2, true, vec![2, 0]));
        let shared = mentions.prior("story", at(STORY_AT)?, "(NASDAQ: LRGP) (NYSE: SMQ)");
        assert_eq!(read(&shared)?[0], (2, true, vec![2, 1]));
        let apart = mentions.prior("story", at(STORY_AT)?, "(NYSE: OTH) (NYSE: SMQ)");
        assert_eq!(read(&apart)?[0], (2, true, vec![1, 1]));
        assert_eq!(
            summary(&[named, shared, apart, PriorCoverage::NoTag]),
            json!({
                "version": version(), "stories": 4, "no_tag": 1, "tagged": 3, "several_tags": 3,
                "windows": [
                    {"days": 1, "none": 0, "some": 3, "none_for_a_tag": 1, "max": 2, "incomplete": 0},
                    {"days": 7, "none": 0, "some": 3, "none_for_a_tag": 1, "max": 4, "incomplete": 0},
                    {"days": 30, "none": 0, "some": 3, "none_for_a_tag": 1, "max": 7, "incomplete": 0},
                ],
            })
        );
        Ok(())
    }

    /// `complete` reads the earliest row of the cut, tagged or not, and a cut that starts on the
    /// window's first instant reaches it. Mutations 2026-11-23: see section 8 row MG12.
    #[test]
    fn no_tag_is_not_zero_and_a_window_the_cut_does_not_reach_back_over_is_marked() -> TestResult {
        let mentions = index()?;
        let instant = at("2026-10-20T10:00:00Z")?;
        assert_eq!(
            mentions.prior("story", instant, "Largeplat today released a model."),
            PriorCoverage::NoTag
        );
        let early = mentions.prior("story", instant, "Largeplat (NASDAQ: LRGP)");
        assert_eq!(
            read(&early)?,
            [(0, true, vec![0]), (0, true, vec![0]), (0, false, vec![0])]
        );
        let on_the_edge = mentions.prior("story", at("2026-10-02T09:00:00Z")?, "(NASDAQ: LRGP)");
        assert_eq!(read(&on_the_edge)?[0], (0, true, vec![0]));
        assert_eq!(read(&on_the_edge)?[1], (0, false, vec![0]));
        assert_eq!(
            summary(&[early, PriorCoverage::NoTag])["windows"][2],
            json!({"days": 30, "none": 1, "some": 0, "none_for_a_tag": 1, "max": 0, "incomplete": 1})
        );
        assert_eq!(
            summary(&[PriorCoverage::NoTag])["windows"][0]["max"],
            json!(null)
        );
        Ok(())
    }
}
