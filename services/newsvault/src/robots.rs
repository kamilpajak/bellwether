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
const NANOS_DIGITS: usize = 9;

/// Why a pinned robots body cannot be read; each refuses the run as `robots_refused` (section 5.1).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RobotsError {
    #[error("the body is not UTF-8")]
    NotUtf8,
    /// A 200 with no group at all is most often an HTML page served at the robots path.
    #[error("the body names no user-agent")]
    NoUserAgentLine,
    #[error("the * group's Crawl-delay {0:?} is not a non-negative number of seconds")]
    CrawlDelayNotANumber(String),
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
}

impl Robots {
    /// RFC 9309 grouping: a run of `user-agent` lines opens a group and the next rule line closes
    /// the run; a rule before any `user-agent` line belongs to no group. Unknown lines are ignored.
    pub fn parse(body: &[u8]) -> Result<Self, RobotsError> {
        let text = std::str::from_utf8(body).map_err(|_| RobotsError::NotUtf8)?;
        let mut robots = Self {
            rules: Vec::new(),
            crawl_delay: None,
        };
        let (mut named_an_agent, mut in_agent_run, mut every_agent) = (false, false, false);
        for line in text.lines() {
            let line = line.split(COMMENT).next().unwrap_or("");
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim().to_ascii_lowercase().as_str() {
                USER_AGENT => {
                    named_an_agent = true;
                    if !in_agent_run {
                        every_agent = false;
                        in_agent_run = true;
                    }
                    every_agent |= value == EVERY_AGENT;
                }
                rule @ (ALLOW | DISALLOW) => {
                    in_agent_run = false;
                    if every_agent && !value.is_empty() {
                        robots.rules.push(PathRule {
                            allow: rule == ALLOW,
                            pattern: value.to_owned(),
                        });
                    }
                }
                CRAWL_DELAY => {
                    in_agent_run = false;
                    if every_agent {
                        let delay = seconds(value)?;
                        robots.crawl_delay =
                            Some(robots.crawl_delay.map_or(delay, |d| d.max(delay)));
                    }
                }
                _ => {}
            }
        }
        if named_an_agent {
            Ok(robots)
        } else {
            Err(RobotsError::NoUserAgentLine)
        }
    }

    /// The largest Crawl-delay the `*` groups state; `None` when they state none.
    #[must_use]
    pub const fn crawl_delay(&self) -> Option<Duration> {
        self.crawl_delay
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
    use super::{Robots, RobotsError};
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

    fn gnw() -> Result<Robots, Box<dyn std::error::Error>> {
        let body = std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
            "../../providers/open-web-feed/tests/fixtures/gnw/robots_rss_globenewswire_com.txt",
        ))?;
        Ok(Robots::parse(&body)?)
    }

    /// Mutation 2026-11-15, the first group read instead of `*`: red (nasdaq's first is bingbot).
    #[test]
    fn the_star_group_is_read_and_a_named_bot_group_is_not() -> TestResult {
        let delays = [
            ("www.nasdaq.com.txt", Some(30)),
            ("www.accessnewswire.com.txt", Some(5)),
            ("www.prnewswire.com.txt", None),
            ("synthetic_star_shares_a_group.txt", Some(20)),
        ]
        .map(|(name, secs)| (name, secs.map(Duration::from_secs)));
        for (name, expected) in delays {
            assert_eq!(
                Robots::parse(&fixture(name)?)?.crawl_delay(),
                expected,
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

    #[test]
    fn a_robots_200_with_no_user_agent_line_is_refused() -> TestResult {
        assert_eq!(
            Robots::parse(&fixture("synthetic_html_page.txt")?),
            Err(RobotsError::NoUserAgentLine)
        );
        assert_eq!(Robots::parse(b""), Err(RobotsError::NoUserAgentLine));
        assert_eq!(
            Robots::parse(&[b'U', 0xff, b'\n']),
            Err(RobotsError::NotUtf8)
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
}
