//! The robots.txt subset `collect` reads from a pinned body (architecture.md section 11.2): the
//! `user-agent: *` groups, merged, their `Allow`/`Disallow` rules and their `Crawl-delay`. A group
//! naming our own product token is never read: the token is `BELLWETHER_FEED_CONTACT`'s, not code's.
//! Percent-encoding is not normalised, so `/a%2Fb` and `/a/b` are two paths here.

use std::time::Duration;

const USER_AGENT: &str = "user-agent";
const ALLOW: &str = "allow";
const DISALLOW: &str = "disallow";
const CRAWL_DELAY: &str = "crawl-delay";
const EVERY_AGENT: &str = "*";
const WILDCARD: char = '*';
const END_ANCHOR: char = '$';
const COMMENT: char = '#';
const BYTE_ORDER_MARK: char = '\u{feff}';
const MARKUP: char = '<';
/// RFC 9309 section 2.2's EOL is CR, LF or CRLF; `str::lines` splits on LF alone.
const LINE_ENDS: [char; 2] = ['\r', '\n'];
const NANOS_DIGITS: usize = 9;

/// Why a pinned robots body cannot be read; each refuses the run as `robots_refused` (section 5.1).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RobotsError {
    #[error("the body is not UTF-8")]
    NotUtf8,
    #[error("the body is an HTML page: its first non-whitespace character is <")]
    HtmlPage,
    /// Text that is not empty or comments only, with neither a `user-agent` line nor a rule.
    #[error("the body names no user-agent")]
    NoUserAgentLine,
    #[error("the * group's Crawl-delay {0:?} is not a non-negative number of seconds")]
    CrawlDelayNotANumber(String),
}

impl RobotsError {
    /// The parse verdict `robots refresh` prints for a body refused here.
    #[must_use]
    pub const fn verdict(&self) -> &'static str {
        match self {
            Self::NotUtf8 => "refused_not_utf8",
            Self::HtmlPage => "refused_html",
            Self::NoUserAgentLine => "refused_no_user_agent",
            Self::CrawlDelayNotANumber(_) => "refused_crawl_delay",
        }
    }
}

/// How a body that parsed was read (owner decision 3, 2026-11-16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// At least one `user-agent` line and no rule before the first of them.
    Rules,
    /// Empty, or comments and blank lines only: no rule, every path allowed.
    NoRulesEmpty,
    /// An `Allow`, `Disallow` or Crawl-delay before the first `user-agent` line, read as `*`'s.
    OrphanRulesAppliedToStar,
}

impl Reading {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Rules => "rules",
            Self::NoRulesEmpty => "no_rules_empty",
            Self::OrphanRulesAppliedToStar => "orphan_rules_applied_to_star",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathRule {
    allow: bool,
    pattern: String,
}

/// What the `*` groups of one robots.txt say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Robots {
    rules: Vec<PathRule>,
    crawl_delay: Option<Duration>,
    reading: Reading,
}

/// One group as it is read. Its Crawl-delays wait for the group's end, because a later
/// `user-agent: *` line of the same run still joins it.
#[derive(Default)]
struct Group {
    every_agent: bool,
    delays: Vec<String>,
}

impl Robots {
    /// RFC 9309 section 2.2 grouping: a run of `user-agent` lines opens a group and the next
    /// `Allow`/`Disallow` closes the run; Crawl-delay and unknown lines close nothing (section
    /// 2.2.4). A rule before any `user-agent` line is read as a `*` rule (owner decision 3c).
    pub fn parse(body: &[u8]) -> Result<Self, RobotsError> {
        let text = std::str::from_utf8(body).map_err(|_| RobotsError::NotUtf8)?;
        let text = text.strip_prefix(BYTE_ORDER_MARK).unwrap_or(text);
        if text.trim_start().starts_with(MARKUP) {
            return Err(RobotsError::HtmlPage);
        }
        let mut robots = Self {
            rules: Vec::new(),
            crawl_delay: None,
            reading: Reading::Rules,
        };
        let mut group = Group::default();
        let (mut named_an_agent, mut in_agent_run) = (false, false);
        let (mut orphan, mut content) = (false, false);
        for line in text.split(LINE_ENDS) {
            let line = line.split(COMMENT).next().unwrap_or("");
            content |= !line.trim().is_empty();
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim().to_ascii_lowercase().as_str() {
                USER_AGENT => {
                    if !in_agent_run {
                        robots.end(&std::mem::take(&mut group))?;
                        in_agent_run = true;
                    }
                    named_an_agent = true;
                    group.every_agent |= value == EVERY_AGENT;
                }
                rule @ (ALLOW | DISALLOW) => {
                    in_agent_run = false;
                    orphan |= !named_an_agent;
                    if (group.every_agent || !named_an_agent) && !value.is_empty() {
                        robots.rules.push(PathRule {
                            allow: rule == ALLOW,
                            pattern: value.to_owned(),
                        });
                    }
                }
                CRAWL_DELAY => {
                    orphan |= !named_an_agent;
                    group.every_agent |= !named_an_agent;
                    group.delays.push(value.to_owned());
                }
                _ => {}
            }
        }
        robots.end(&group)?;
        robots.reading = match (named_an_agent, orphan, content) {
            (_, true, _) => Reading::OrphanRulesAppliedToStar,
            (true, false, _) => Reading::Rules,
            (false, false, false) => Reading::NoRulesEmpty,
            (false, false, true) => return Err(RobotsError::NoUserAgentLine),
        };
        Ok(robots)
    }

    /// A group's Crawl-delays count only when it names `*`; the largest of every `*` group wins.
    fn end(&mut self, group: &Group) -> Result<(), RobotsError> {
        if group.every_agent {
            for value in &group.delays {
                let delay = seconds(value)?;
                self.crawl_delay = Some(self.crawl_delay.map_or(delay, |d| d.max(delay)));
            }
        }
        Ok(())
    }

    /// The largest Crawl-delay the `*` groups state; `None` when they state none.
    #[must_use]
    pub const fn crawl_delay(&self) -> Option<Duration> {
        self.crawl_delay
    }

    #[must_use]
    pub const fn reading(&self) -> Reading {
        self.reading
    }

    /// RFC 9309 section 2.2.2: the longest matching pattern decides, `Allow` wins a tie, and a
    /// path no rule matches is allowed. `path` is matched as written, query included.
    #[must_use]
    pub fn allows(&self, path: &str) -> bool {
        self.rules
            .iter()
            .filter(|rule| matches(&rule.pattern, path))
            .max_by(|a, b| {
                a.pattern
                    .len()
                    .cmp(&b.pattern.len())
                    .then(a.allow.cmp(&b.allow))
            })
            .is_none_or(|rule| rule.allow)
    }
}

/// A pattern matches a path prefix; `*` matches any run of characters, a final `$` the end.
fn matches(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = pattern
        .strip_suffix(END_ANCHOR)
        .map_or((pattern, false), |head| (head, true));
    let mut pieces = pattern.split(WILDCARD);
    let Some(mut rest) = path.strip_prefix(pieces.next().unwrap_or("")) else {
        return false;
    };
    let pieces: Vec<&str> = pieces.collect();
    let Some((last, middle)) = pieces.split_last() else {
        return !anchored || rest.is_empty();
    };
    for piece in middle {
        match rest.find(piece) {
            Some(at) => rest = &rest[at + piece.len()..],
            None => return false,
        }
    }
    if anchored {
        rest.ends_with(last)
    } else {
        rest.contains(last)
    }
}

/// Decimal seconds: digits, optionally a point and more digits; nothing else, no sign, no exponent.
fn seconds(value: &str) -> Result<Duration, RobotsError> {
    let refused = || RobotsError::CrawlDelayNotANumber(value.to_owned());
    let (whole, fraction) = value.split_once('.').unwrap_or((value, "0"));
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !digits(whole) || !digits(fraction) {
        return Err(refused());
    }
    let secs: u64 = whole.parse().map_err(|_| refused())?;
    let nanos: String = fraction
        .chars()
        .chain(std::iter::repeat('0'))
        .take(NANOS_DIGITS)
        .collect();
    let nanos: u32 = nanos.parse().map_err(|_| refused())?;
    Ok(Duration::new(secs, nanos))
}

#[cfg(test)]
mod tests {
    use super::{Reading, Robots, RobotsError};
    use std::path::PathBuf;
    use std::time::Duration;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn fixture(name: &str) -> Result<Vec<u8>, std::io::Error> {
        std::fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/robots")
                .join(name),
        )
    }

    fn provider_fixture(name: &str) -> Result<Vec<u8>, std::io::Error> {
        std::fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../providers/open-web-feed/tests/fixtures")
                .join(name),
        )
    }

    fn gnw() -> Result<Robots, Box<dyn std::error::Error>> {
        let body = provider_fixture("gnw/robots_rss_globenewswire_com.txt")?;
        Ok(Robots::parse(&body)?)
    }

    /// Mutation 2026-11-15, the first group read instead of `*`: red (nasdaq's first is bingbot).
    #[test]
    fn the_star_group_is_read_and_a_named_bot_group_is_not() -> TestResult {
        let delays = [
            (
                "www.nasdaq.com.txt",
                fixture("www.nasdaq.com.txt")?,
                Some(30),
            ),
            (
                "www.accessnewswire.com.txt",
                fixture("www.accessnewswire.com.txt")?,
                Some(5),
            ),
            (
                "prn/robots_www_prnewswire_com.txt",
                provider_fixture("prn/robots_www_prnewswire_com.txt")?,
                None,
            ),
            (
                "synthetic_star_shares_a_group.txt",
                fixture("synthetic_star_shares_a_group.txt")?,
                Some(20),
            ),
        ];
        for (name, body, secs) in delays {
            assert_eq!(
                Robots::parse(&body)?.crawl_delay(),
                secs.map(Duration::from_secs),
                "{name}"
            );
        }
        // nasdaq's `*` group disallows /core/ and allows its CSS; bingbot's group has no rule.
        let nasdaq = Robots::parse(&fixture("www.nasdaq.com.txt")?)?;
        assert_eq!(
            [
                nasdaq.allows("/core/x"),
                nasdaq.allows("/core/a.css"),
                nasdaq.allows("/news")
            ],
            [false, true, true]
        );
        Ok(())
    }

    /// Mutation 2026-11-15, `Disallow` winning a tie: red.
    #[test]
    fn allow_wins_a_tie_and_the_longest_match_decides() -> TestResult {
        let robots = Robots::parse(&fixture("synthetic_tie_and_longest.txt")?)?;
        let cases = [
            ("/tie", true),
            ("/tie/deeper", true),
            ("/a/", false),
            ("/a/b/c", true),
            ("/a/b/private", false),
            ("/elsewhere", true),
        ];
        for (path, allowed) in cases {
            assert_eq!(robots.allows(path), allowed, "{path}");
        }
        Ok(())
    }

    /// GNW's `*/news-release/*.html?pdf=1` blocks a release's PDF form and not the prefix.
    #[test]
    fn wildcard_and_end_anchor_match_as_rfc_9309_states() -> TestResult {
        let gnw = gnw()?;
        let robots = Robots::parse(&fixture("synthetic_wildcard_and_anchor.txt")?)?;
        let cases = [
            (&gnw, "/news-release/", true),
            (&gnw, "/news-release/2026/11/14/1/0/en/x.html", true),
            (&gnw, "/news-release/2026/11/14/1/0/en/x.html?pdf=1", false),
            (&gnw, "/en/news-release/x.html?pdf=1", false),
            (&gnw, "/Tracker?data=abc", false),
            (&robots, "/page.php", false),
            (&robots, "/page.php?x=1", true),
            (&robots, "/dir/page.php", false),
            (&robots, "/x/y/z/private", false),
            (&robots, "/x/private", false),
            (&robots, "/private", true),
            (&robots, "/exact", false),
            (&robots, "/exact/more", true),
        ];
        for (robots, path, allowed) in cases {
            assert_eq!(robots.allows(path), allowed, "{path}");
        }
        Ok(())
    }

    #[test]
    fn a_crawl_delay_that_is_not_a_number_is_refused() -> TestResult {
        for value in [
            "ten",
            "-1",
            "",
            "1e3",
            "NaN",
            "1.",
            ".5",
            "1.5.0",
            "99999999999999999999",
        ] {
            let body = format!("User-agent: *\nCrawl-delay: {value}\n");
            assert_eq!(
                Robots::parse(body.as_bytes()),
                Err(RobotsError::CrawlDelayNotANumber(value.to_owned())),
                "{value:?}"
            );
        }
        assert_eq!(
            Robots::parse(&fixture("synthetic_bad_crawl_delay.txt")?)
                .map(|robots| robots.crawl_delay()),
            Err(RobotsError::CrawlDelayNotANumber("soon".to_owned()))
        );
        let read = Robots::parse(b"User-agent: *\nCrawl-delay: 0.5\nCrawl-delay: 2.25\n")?;
        assert_eq!(read.crawl_delay(), Some(Duration::from_millis(2_250)));
        // A named bot's malformed delay is never read, so it refuses nothing.
        let named = Robots::parse(b"User-agent: bingbot\nCrawl-delay: soon\n")?;
        assert_eq!(named.crawl_delay(), None);
        Ok(())
    }

    /// Owner decision 3b: a body whose first non-whitespace character is `<`, a BOM before it
    /// included (the Fed's 404 page of 2026-11-16 starts so). Text with no group and no rule
    /// stays refused. Mutation 2026-11-16, the `<` check removed: red.
    #[test]
    fn an_html_body_is_refused_and_text_with_no_group_or_rule_too() -> TestResult {
        let refused = [
            fixture("synthetic_html_page.txt")?,
            "\u{feff} \n\t<!doctype html>\n<html></html>"
                .as_bytes()
                .to_vec(),
            b"Sitemap: https://example.test/s.xml\n".to_vec(),
            b"Service unavailable\n".to_vec(),
            vec![b'U', 0xff, b'\n'],
        ]
        .iter()
        .map(|body| Robots::parse(body).map(|robots| robots.reading()))
        .collect::<Vec<_>>();
        assert_eq!(
            refused,
            [
                Err(RobotsError::HtmlPage),
                Err(RobotsError::HtmlPage),
                Err(RobotsError::NoUserAgentLine),
                Err(RobotsError::NoUserAgentLine),
                Err(RobotsError::NotUtf8),
            ]
        );
        Ok(())
    }

    /// Owner decision 3a: an empty or comment-only 200 is no rules, every path allowed, no delay.
    /// Mutation 2026-11-16, the empty arm made `NoUserAgentLine`: red.
    #[test]
    fn an_empty_or_comment_only_body_reads_as_no_rules() -> TestResult {
        for body in [
            &b""[..],
            b"\n\r\n  \t\n",
            b"# nothing here\n\n   # nor here\r\n",
            "\u{feff}# a BOM, then a comment\n".as_bytes(),
        ] {
            let robots = Robots::parse(body)?;
            assert_eq!(
                (robots.reading(), robots.allows("/"), robots.crawl_delay()),
                (Reading::NoRulesEmpty, true, None),
                "{body:?}"
            );
        }
        Ok(())
    }

    /// Owner decision 3c: a rule or Crawl-delay before the first `user-agent` line is a `*` rule;
    /// RFC 9309 section 2.2.2 says SHOULD ignore it. Mutations 2026-11-16, orphan rules skipped:
    /// red; an orphan Crawl-delay skipped: red; `orphan` never set: red.
    #[test]
    fn a_rule_before_the_first_user_agent_is_read_as_a_star_rule() -> TestResult {
        let robots = Robots::parse(
            b"Disallow: /orphan/\nCrawl-delay: 12\n\nUser-agent: bingbot\nDisallow: /named/\n",
        )?;
        let alone = Robots::parse(b"# no group at all\nDisallow: /orphan/\n")?;
        assert_eq!(
            (
                robots.reading(),
                robots.allows("/orphan/x"),
                robots.allows("/named/x"),
                robots.crawl_delay(),
                alone.reading(),
                alone.allows("/orphan/x"),
            ),
            (
                Reading::OrphanRulesAppliedToStar,
                false,
                true,
                Some(Duration::from_secs(12)),
                Reading::OrphanRulesAppliedToStar,
                false,
            )
        );
        assert_eq!(
            Robots::parse(b"Crawl-delay: soon\nUser-agent: *\n"),
            Err(RobotsError::CrawlDelayNotANumber("soon".to_owned()))
        );
        Ok(())
    }

    /// Owner decision 2 (B2): a Crawl-delay line does not end a `user-agent` run, as RFC 9309
    /// section 2.2.4 and Google's robotstxt parser read it. Mutations 2026-11-16, the run ended at
    /// a Crawl-delay again: red; delays applied before the group ends: red (the bingbot-first case).
    #[test]
    fn a_crawl_delay_does_not_end_a_user_agent_run() -> TestResult {
        let star_first = Robots::parse(
            b"User-agent: *\nCrawl-delay: 30\nUser-agent: bingbot\nDisallow: /prywatne/\n",
        )?;
        let star_second = Robots::parse(
            b"User-agent: bingbot\nCrawl-delay: 30\nUser-agent: *\nDisallow: /prywatne/\n",
        )?;
        let read = [&star_first, &star_second].map(|robots| {
            (
                robots.reading(),
                robots.crawl_delay(),
                robots.allows("/prywatne/x"),
                robots.allows("/public"),
            )
        });
        let expected = (Reading::Rules, Some(Duration::from_secs(30)), false, true);
        assert_eq!(read, [expected, expected]);
        // Only a blank line and the delay sit between `*` and googlebot, so its `Disallow: /`
        // is `*`'s too; before B2 it was googlebot's alone.
        let shared = Robots::parse(&fixture("synthetic_star_shares_a_group.txt")?)?;
        assert!(!shared.allows("/news"));
        Ok(())
    }

    #[test]
    fn every_verdict_has_its_own_name() {
        let names = [
            RobotsError::NotUtf8.verdict(),
            RobotsError::HtmlPage.verdict(),
            RobotsError::NoUserAgentLine.verdict(),
            RobotsError::CrawlDelayNotANumber(String::new()).verdict(),
            Reading::Rules.name(),
            Reading::NoRulesEmpty.name(),
            Reading::OrphanRulesAppliedToStar.name(),
        ];
        assert_eq!(
            names,
            [
                "refused_not_utf8",
                "refused_html",
                "refused_no_user_agent",
                "refused_crawl_delay",
                "rules",
                "no_rules_empty",
                "orphan_rules_applied_to_star"
            ]
        );
    }

    /// Read with `str::lines` and no BOM strip, each body parsed Ok with no `*` rule and no delay.
    /// Mutations 2026-11-15, `lines()` restored / the strip removed: each red.
    #[test]
    fn a_cr_only_or_bom_led_body_keeps_its_star_rules_and_crawl_delay() -> TestResult {
        let mut read = Vec::new();
        for name in ["synthetic_cr_line_ends.txt", "synthetic_bom_led.txt"] {
            let robots = Robots::parse(&fixture(name)?)?;
            read.push((name, robots.allows("/rssfeed/"), robots.crawl_delay()));
        }
        let held = Some(Duration::from_secs(60));
        assert_eq!(
            read,
            [
                ("synthetic_cr_line_ends.txt", false, held),
                ("synthetic_bom_led.txt", false, held)
            ]
        );
        Ok(())
    }

    /// A named group after `*` resets the run, so bingbot's tying `Allow` is not a `*` rule.
    /// Mutation 2026-11-15, the `every_agent = false` reset deleted: red.
    #[test]
    fn a_named_group_after_the_star_group_adds_no_star_rule() -> TestResult {
        let robots = Robots::parse(&fixture("synthetic_named_after_star.txt")?)?;
        assert!(!robots.allows("/rssfeed/"));
        Ok(())
    }

    /// Mutation 2026-11-15, the comment cut removed: red (the delay `5 # seconds` is refused).
    #[test]
    fn a_trailing_comment_is_cut_from_a_rule_and_a_crawl_delay() -> TestResult {
        let robots = Robots::parse(&fixture("synthetic_inline_comment.txt")?)?;
        assert_eq!(
            (robots.allows("/rssfeed/"), robots.crawl_delay()),
            (false, Some(Duration::from_secs(5)))
        );
        Ok(())
    }

    /// Mutation 2026-11-15, the empty-value skip removed: red (`/news` disallowed).
    #[test]
    fn an_empty_disallow_disallows_nothing() -> TestResult {
        let robots = Robots::parse(&fixture("synthetic_empty_disallow.txt")?)?;
        assert_eq!(
            (robots.allows("/news"), robots.allows("/private/x")),
            (true, false)
        );
        Ok(())
    }

    #[test]
    fn the_gnw_robots_allows_the_rssfeed_index_and_the_news_release_prefix() -> TestResult {
        let gnw = gnw()?;
        assert_eq!(
            (
                gnw.allows("/rssfeed/"),
                gnw.allows("/news-release/"),
                gnw.allows("/search"),
                gnw.crawl_delay()
            ),
            (true, true, false, None)
        );
        Ok(())
    }

    /// PRN's 8 `Disallow` lines sit in its `*` group; `/multivu/` is one of them. Mutations
    /// 2026-11-16, every `Disallow` read as `Allow`: red; `Disallow` lines skipped: red.
    #[test]
    fn the_prn_robots_allows_the_rss_index_and_the_news_releases_prefix() -> TestResult {
        let prn = Robots::parse(&provider_fixture("prn/robots_www_prnewswire_com.txt")?)?;
        assert_eq!(
            (
                prn.allows("/rss/news-releases-list.rss"),
                prn.allows("/news-releases/"),
                prn.allows("/multivu/"),
                prn.crawl_delay()
            ),
            (true, true, false, None)
        );
        Ok(())
    }
}
