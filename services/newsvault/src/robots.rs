//! The robots.txt subset `collect` reads from a pinned body (architecture.md section 11.2): the
//! `user-agent: *` groups, merged, their `Allow`/`Disallow` rules and their `Crawl-delay`. A group
//! naming our own product token is never read: the token is `BELLWETHER_FEED_CONTACT`'s, not code's.
//! The body's bytes, lines, keys, pattern escaping and matching are read as Google's robots.cc
//! (github.com/google/robotstxt, 50a0dac) reads them, reimplemented; a path is matched as given.
//! The exceptions are in architecture.md section 10, row "key and line spellings".

use std::time::Duration;

/// robots.cc 50a0dac matches a key by case-insensitive prefix: lines 219-224 (user-agent and two
/// typos), 230 (allow, no typo), 235-243 (disallow and five typos).
const USER_AGENT_KEYS: [&str; 3] = ["user-agent", "useragent", "user agent"];
const ALLOW_KEY: &str = "allow";
const DISALLOW_KEYS: [&str; 6] = [
    "disallow",
    "dissallow",
    "dissalow",
    "disalow",
    "diasllow",
    "disallaw",
];
/// robots.cc names no Crawl-delay key, so ours is matched whole, case aside (owner decision 1).
const CRAWL_DELAY_KEY: &str = "crawl-delay";
/// absl's ASCII whitespace, which robots.cc line 296 strips; Rust's `is_ascii_whitespace` lacks VT.
const ASCII_SPACE: [u8; 6] = *b" \t\n\x0b\x0c\r";
/// robots.cc line 327: the blanks that may stand in for a missing colon.
const BLANKS: [u8; 2] = *b" \t";
const SEPARATOR: u8 = b':';
const WILDCARD: char = '*';
const END_ANCHOR: char = '$';
const COMMENT: u8 = b'#';
const BYTE_ORDER_MARK: [u8; 3] = *b"\xef\xbb\xbf";
const MARKUP: u8 = b'<';
/// RFC 9309 section 2.2's EOL is CR, LF or CRLF; `str::lines` splits on LF alone.
const LINE_ENDS: [u8; 2] = *b"\r\n";
/// robots.cc 418-447: a line buffer of 2083 × 8 bytes whose last byte holds the terminator.
const MAX_LINE_BYTES: usize = 2083 * 8 - 1;
/// robots.cc reads a line as a C string, so it ends at the first NUL.
const NUL: u8 = 0;
const PERCENT: u8 = b'%';
/// robots.cc 655-668: an unmatched `Allow` whose text from its last `/` starts so also allows
/// `<text up to that slash>/$`.
const INDEX_PAGE: &str = "/index.htm";
const NANOS_DIGITS: usize = 9;

/// Why a pinned robots body cannot be read; each refuses the run as `robots_refused` (section 5.1).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RobotsError {
    #[error("the body is an HTML page: its first non-whitespace character is <")]
    HtmlPage,
    /// The value's bytes as `<[u8]>::escape_ascii` writes them, so `5\xe4` stays `5\xe4`.
    #[error("the * group's Crawl-delay \"{0}\" is not a non-negative number of seconds")]
    CrawlDelayNotANumber(String),
}

impl RobotsError {
    /// The parse verdict `robots refresh` prints for a body refused here.
    #[must_use]
    pub const fn verdict(&self) -> &'static str {
        match self {
            Self::HtmlPage => "refused_html",
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
    /// Text and no `user-agent` line, so no group: no rule, every path allowed (RFC 9309 section
    /// 2.2.1; recommendation #2, 2026-11-16).
    NoRulesNoGroup,
    /// A `user-agent` line, and an `Allow`, `Disallow` or Crawl-delay before the first of them:
    /// every such line is ignored (owner decision 2026-11-16, Google's standard).
    OrphanRulesIgnored,
}

impl Reading {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Rules => "rules",
            Self::NoRulesEmpty => "no_rules_empty",
            Self::NoRulesNoGroup => "no_rules_no_group",
            Self::OrphanRulesIgnored => "orphan_rules_ignored",
        }
    }
}

/// `pattern` is escaped as robots.cc 167-213 escapes it; `index_directory` is set on an `Allow`
/// only, as `INDEX_PAGE` says.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PathRule {
    allow: bool,
    pattern: String,
    index_directory: Option<String>,
}

impl PathRule {
    fn new(allow: bool, value: &[u8]) -> Self {
        let pattern = escape_pattern(value);
        let index_directory = allow.then(|| index_directory(&pattern)).flatten();
        Self {
            allow,
            pattern,
            index_directory,
        }
    }

    /// The matched pattern's length, which is the rule's priority; `None` when it does not match.
    fn priority(&self, path: &str) -> Option<usize> {
        if matches(&self.pattern, path) {
            return Some(self.pattern.len());
        }
        self.index_directory
            .as_deref()
            .filter(|directory| matches(directory, path))
            .map(str::len)
    }
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
    /// 2.2.4). Every line before the first `user-agent` line belongs to no group and is ignored,
    /// as section 2.2.2 says and Google's robots.cc (50a0dac) does, its Crawl-delay included.
    pub fn parse(body: &[u8]) -> Result<Self, RobotsError> {
        let body = skip_byte_order_mark(body);
        if starts_with_markup(body) {
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
        for raw in body.split(|byte| LINE_ENDS.contains(byte)) {
            if let Some(value) = crawl_delay_value(raw) {
                content = true;
                orphan |= !named_an_agent && !value.is_empty();
                group.delays.push(value.escape_ascii().to_string());
                continue;
            }
            let line = read_line(raw);
            content |= !line.is_empty();
            let Some((key, value)) = directive(line) else {
                continue;
            };
            match Key::of(key) {
                Key::UserAgent => {
                    if !in_agent_run {
                        robots.end(&std::mem::take(&mut group))?;
                        in_agent_run = true;
                    }
                    named_an_agent = true;
                    group.every_agent |= names_every_agent(value);
                }
                rule @ (Key::Allow | Key::Disallow) => {
                    in_agent_run = false;
                    orphan |= !named_an_agent && !value.is_empty();
                    if group.every_agent && !value.is_empty() {
                        robots.rules.push(PathRule::new(rule == Key::Allow, value));
                    }
                }
                Key::CrawlDelay | Key::Other => {}
            }
        }
        robots.end(&group)?;
        robots.reading = match (named_an_agent, orphan, content) {
            (true, true, _) => Reading::OrphanRulesIgnored,
            (true, false, _) => Reading::Rules,
            (false, _, false) => Reading::NoRulesEmpty,
            (false, _, true) => Reading::NoRulesNoGroup,
        };
        Ok(robots)
    }

    /// A group's Crawl-delays count only when it names `*`; the largest of every `*` group wins.
    /// The lines before the first `user-agent` end as a group naming nobody.
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
    /// path no rule matches is allowed. `path` is matched as written, query included, unescaped.
    #[must_use]
    pub fn allows(&self, path: &str) -> bool {
        self.rules
            .iter()
            .filter_map(|rule| rule.priority(path).map(|priority| (priority, rule.allow)))
            .max()
            .is_none_or(|(_, allow)| allow)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    UserAgent,
    Allow,
    Disallow,
    CrawlDelay,
    Other,
}

impl Key {
    /// robots.cc 50a0dac lines 488-500 try user-agent, then allow, then disallow.
    fn of(key: &[u8]) -> Self {
        let starts = |prefix: &&str| {
            key.get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
        };
        if USER_AGENT_KEYS.iter().any(starts) {
            Self::UserAgent
        } else if starts(&ALLOW_KEY) {
            Self::Allow
        } else if DISALLOW_KEYS.iter().any(starts) {
            Self::Disallow
        } else if key.eq_ignore_ascii_case(CRAWL_DELAY_KEY.as_bytes()) {
            Self::CrawlDelay
        } else {
            Self::Other
        }
    }
}

/// robots.cc 50a0dac lines 323-345 over a stripped line with its comment cut: the first colon
/// splits it; with none, one run of blanks does when exactly two words remain. No key, no line.
fn directive(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut halves = line.splitn(2, |byte| *byte == SEPARATOR);
    let head = halves.next()?;
    let (key, value) = match halves.next() {
        Some(value) => (head, value),
        None => {
            let mut words = line.splitn(2, |byte| BLANKS.contains(byte));
            let (key, value) = (words.next()?, trim(words.next()?, &BLANKS));
            if value.iter().any(|byte| BLANKS.contains(byte)) {
                return None;
            }
            (key, value)
        }
    };
    let key = trim(key, &ASCII_SPACE);
    (!key.is_empty()).then(|| (key, trim(value, &ASCII_SPACE)))
}

/// robots.cc 50a0dac 434-441 skips each leading byte that continues the BOM, so `EF BB` alone goes.
fn skip_byte_order_mark(body: &[u8]) -> &[u8] {
    let skipped = body
        .iter()
        .zip(BYTE_ORDER_MARK)
        .take_while(|(byte, mark)| **byte == *mark)
        .count();
    body.get(skipped..).unwrap_or(body)
}

/// Owner decision 3b over bytes: past ASCII whitespace, NUL and every byte at or above 0x80 (any
/// BOM, UTF-8 or Latin-1 whitespace, a UTF-16 unit's zero half), the first byte is `<`.
fn starts_with_markup(body: &[u8]) -> bool {
    body.iter()
        .find(|byte| !(ASCII_SPACE.contains(byte) || **byte == NUL || !byte.is_ascii()))
        == Some(&MARKUP)
}

/// One physical line as robots.cc 50a0dac reads it: its first `MAX_LINE_BYTES` bytes, up to its
/// first NUL, its comment cut (line 302), its ASCII whitespace stripped (296).
fn read_line(line: &[u8]) -> &[u8] {
    let kept = line.get(..MAX_LINE_BYTES).unwrap_or(line);
    let kept = kept.split(|byte| *byte == NUL).next().unwrap_or(kept);
    let kept = kept.split(|byte| *byte == COMMENT).next().unwrap_or(kept);
    trim(kept, &ASCII_SPACE)
}

/// A Crawl-delay line's value as ours read it before 2026-11-17 (owner decision 1): the whole
/// line, NUL and bytes past `MAX_LINE_BYTES` kept, its comment cut. `None` for any other line.
fn crawl_delay_value(line: &[u8]) -> Option<&[u8]> {
    let kept = line.split(|byte| *byte == COMMENT).next().unwrap_or(line);
    let (key, value) = directive(trim(kept, &ASCII_SPACE))?;
    (Key::of(key) == Key::CrawlDelay).then_some(value)
}

fn trim<'a>(mut bytes: &'a [u8], strip: &[u8]) -> &'a [u8] {
    while let [first, rest @ ..] = bytes {
        if !strip.contains(first) {
            break;
        }
        bytes = rest;
    }
    while let [rest @ .., last] = bytes {
        if !strip.contains(last) {
            break;
        }
        bytes = rest;
    }
    bytes
}

/// robots.cc 50a0dac line 625: `*` alone, or followed by whitespace and more, names every agent.
fn names_every_agent(value: &[u8]) -> bool {
    match value.split_first() {
        Some((first, rest)) if char::from(*first) == WILDCARD => {
            rest.first().is_none_or(|next| ASCII_SPACE.contains(next))
        }
        _ => false,
    }
}

/// robots.cc 50a0dac 167-213: a byte at or above 0x80 becomes `%XX`, and a `%` with two hex
/// digits after it keeps them, uppercased; every other byte is kept.
fn escape_pattern(value: &[u8]) -> String {
    let mut escaped = String::with_capacity(value.len());
    let mut rest = value;
    while let Some((&byte, tail)) = rest.split_first() {
        rest = tail;
        match tail {
            [high, low, after @ ..]
                if byte == PERCENT && high.is_ascii_hexdigit() && low.is_ascii_hexdigit() =>
            {
                escaped.push(char::from(PERCENT));
                escaped.push(char::from(high.to_ascii_uppercase()));
                escaped.push(char::from(low.to_ascii_uppercase()));
                rest = after;
            }
            _ if !byte.is_ascii() => escaped.push_str(&format!("%{byte:02X}")),
            _ => escaped.push(char::from(byte)),
        }
    }
    escaped
}

/// `INDEX_PAGE`'s directory pattern for an escaped `Allow` pattern, when it has one.
fn index_directory(pattern: &str) -> Option<String> {
    let (directory, last) = pattern.split_at(pattern.rfind('/')?);
    last.starts_with(INDEX_PAGE)
        .then(|| format!("{directory}/{END_ANCHOR}"))
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
    let past_nanos = fraction
        .bytes()
        .skip(NANOS_DIGITS)
        .any(|digit| digit != b'0');
    Duration::new(secs, nanos)
        .checked_add(Duration::from_nanos(u64::from(past_nanos)))
        .ok_or_else(refused)
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
        // A digit past the ninth rounds up, so a delay just above 10 s never reads as 10 s.
        let past_nanos = Robots::parse(b"User-agent: *\nCrawl-delay: 10.0000000001\n")?;
        assert_eq!(past_nanos.crawl_delay(), Some(Duration::new(10, 1)));
        let zeros = Robots::parse(b"User-agent: *\nCrawl-delay: 10.0000000000\n")?;
        assert_eq!(zeros.crawl_delay(), Some(Duration::from_secs(10)));
        // A named bot's malformed delay is never read unless `*` shares its run (decision 2).
        let named = Robots::parse(b"User-agent: bingbot\nCrawl-delay: soon\n")?;
        assert_eq!(named.crawl_delay(), None);
        assert_eq!(
            Robots::parse(b"User-agent: bingbot\nCrawl-delay: soon\nUser-agent: *\nDisallow: /p\n"),
            Err(RobotsError::CrawlDelayNotANumber("soon".to_owned()))
        );
        Ok(())
    }

    /// Owner decision 1 keeps our Crawl-delay reading: robots.cc reads no Crawl-delay, so its line
    /// is read whole, never cut at a NUL or at 16,663 bytes as a rule line is.
    #[test]
    fn a_crawl_delay_line_is_read_whole_and_not_cut_as_googles_parser_cuts_a_rule_line() {
        let blanks = " ".repeat(16_700);
        let read = [
            b"User-agent: *\nCrawl-delay: 5\0x\n".to_vec(),
            format!("User-agent: *\nCrawl-delay: 5{blanks}x\n").into_bytes(),
            format!("User-agent: *\nCrawl-delay:{blanks}5\n").into_bytes(),
            b"User-agent: *\nCrawl-delay 5\0 x\n".to_vec(),
        ]
        .map(|body| {
            Robots::parse(&body)
                .map(|robots| robots.crawl_delay())
                .map_err(|refused| refused.verdict())
        });
        assert_eq!(
            read,
            [
                Err("refused_crawl_delay"),
                Err("refused_crawl_delay"),
                Ok(Some(Duration::from_secs(5))),
                Ok(None),
            ]
        );
    }

    /// Owner decision 3b: a body whose first non-whitespace character is `<`, a BOM before it
    /// included (the Fed's 404 page of 2026-11-16 starts so). Google's parser would read such a
    /// page for rules instead. Mutation 2026-11-16, the `<` check removed: red.
    #[test]
    fn an_html_body_is_refused() -> TestResult {
        let refused = [
            fixture("synthetic_html_page.txt")?,
            "\u{feff} \n\t<!doctype html>\n<html></html>"
                .as_bytes()
                .to_vec(),
            " \u{feff}<html></html>".as_bytes().to_vec(),
            b"\xef\xbb <html>\xff".to_vec(),
            b"\xff<html>".to_vec(),
        ]
        .iter()
        .map(|body| Robots::parse(body).map(|robots| robots.reading()))
        .collect::<Vec<_>>();
        assert_eq!(refused, vec![Err(RobotsError::HtmlPage); 5]);
        Ok(())
    }

    fn utf16(text: &str, little_endian: bool) -> Vec<u8> {
        text.encode_utf16()
            .flat_map(|unit| {
                if little_endian {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                }
            })
            .collect()
    }

    /// Review of 2026-11-17: an HTML page that is not UTF-8 was read for rules, so every path was
    /// allowed; Unicode whitespace before `<` is skipped too. Each body stands as a soft-404 page.
    #[test]
    fn an_html_body_that_is_not_utf8_or_starts_with_unicode_space_is_refused() {
        let page = "<!doctype html>\n<html><body>Not found</body></html>\n";
        let refused = [
            [&b"\xff\xfe"[..], &utf16(page, true)].concat(),
            [&b"\xfe\xff"[..], &utf16(page, false)].concat(),
            utf16(page, false),
            utf16(page, true),
            [&b"\xa0"[..], page.as_bytes()].concat(),
            format!("\u{a0}\u{2003}\u{3000}\u{85}\u{feff}{page}").into_bytes(),
        ]
        .map(|body| Robots::parse(&body).map(|robots| robots.reading()));
        assert_eq!(refused.to_vec(), vec![Err(RobotsError::HtmlPage); 6]);
        let read = [
            &b"\xa0User-agent: *\nDisallow: /x\n"[..],
            "\u{3000}\nUser-agent: *\nDisallow: /x # <y>\n".as_bytes(),
        ]
        .map(|body| Robots::parse(body).map(|robots| robots.allows("/x")));
        assert_eq!(read, [Ok(true), Ok(false)]);
    }

    /// Owner decision 3a: an empty or comment-only 200 is no rules, every path allowed, no delay.
    /// Mutation 2026-11-16, the empty arm made `NoRulesNoGroup`: red.
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

    /// Owner decision 2026-11-16 (Google's standard): an `Allow`, `Disallow` or Crawl-delay before
    /// the first `user-agent` line is ignored, as RFC 9309 section 2.2.2 says and Google's
    /// robots.cc 50a0dac does. Each body gives the reading, `/news/a`, `/prywatne/a`, `/x`, delay.
    #[test]
    fn a_rule_before_the_first_user_agent_is_ignored() -> TestResult {
        let read = [
            &b"Allow: /news/\nUser-agent: *\nDisallow: /\n"[..],
            b"Disallow: /prywatne/\nUser-agent: *\nAllow: /\n",
            b"Crawl-delay: 30\nUser-agent: *\nDisallow: /x\n",
            b"Crawl-delay: soon\nUser-agent: *\n",
            b"Disallow:\nAllow:\nCrawl-delay:\nUser-agent: *\n",
        ]
        .iter()
        .map(|body| {
            Robots::parse(body).map(|robots| {
                (
                    robots.reading(),
                    robots.allows("/news/a"),
                    robots.allows("/prywatne/a"),
                    robots.allows("/x"),
                    robots.crawl_delay(),
                )
            })
        })
        .collect::<Vec<_>>();
        let ignored = Reading::OrphanRulesIgnored;
        assert_eq!(
            read,
            [
                Ok((ignored, false, false, false, None)),
                Ok((ignored, true, true, true, None)),
                Ok((ignored, true, true, false, None)),
                Ok((ignored, true, true, true, None)),
                Ok((Reading::Rules, true, true, true, None)),
            ]
        );
        Ok(())
    }

    /// The bodies that widened `*` under decision 3c's builds (79a79b8; 41fcbf9, an orphan
    /// Crawl-delay before the `Allow`): every orphan line is ignored, so `*`'s `Disallow: /` holds.
    #[test]
    fn an_orphan_allow_widens_nothing() -> TestResult {
        let mut read = Vec::new();
        for (body, path) in [
            (
                &b"Allow: /news/\nUser-agent: *\nDisallow: /\n"[..],
                "/news/x",
            ),
            (
                b"Crawl-delay: 5\nAllow: /news/\nUser-agent: *\nDisallow: /\n",
                "/news/x",
            ),
            (
                b"Crawl-delay: 5\nDisallow: /a\nAllow: /a/b\nUser-agent: *\nDisallow: /\n",
                "/a/b/c",
            ),
        ] {
            let robots = Robots::parse(body)?;
            read.push((robots.reading(), robots.allows(path), robots.crawl_delay()));
        }
        assert_eq!(read, [(Reading::OrphanRulesIgnored, false, None); 3]);
        Ok(())
    }

    /// Recommendation #2 (2026-11-16): RFC 9309 section 2.2.1, "or no groups are present at all, no
    /// rules apply", and Google's robots.cc 50a0dac, whose matcher drops every rule before an agent.
    /// Each body gives the reading, `/`, `/orphan/x`, the delay.
    #[test]
    fn text_with_no_user_agent_line_reads_as_no_rules() -> TestResult {
        let read = [
            &b"Sitemap: https://example.test/s.xml\n"[..],
            b"Service unavailable\n",
            b"Disallow: /orphan/\n",
            b"# no group at all\nCrawl-delay: 12\nAllow: /\n",
            b"Crawl-delay: soon\nDisallow: /\n",
            b"user_agent: *\nDisallow: /orphan/\n",
        ]
        .iter()
        .map(|body| {
            Robots::parse(body).map(|robots| {
                (
                    robots.reading(),
                    robots.allows("/"),
                    robots.allows("/orphan/x"),
                    robots.crawl_delay(),
                )
            })
        })
        .collect::<Vec<_>>();
        assert_eq!(
            read,
            vec![Ok((Reading::NoRulesNoGroup, true, true, None)); 6]
        );
        Ok(())
    }

    /// The `*` group's `Disallow: /x` under each key spelling; true when `/x` stays allowed.
    fn star_disallows_x(user_agent: &str, disallow: &str) -> Result<bool, RobotsError> {
        let body = format!("{user_agent}: *\n{disallow}: /x\n");
        Robots::parse(body.as_bytes()).map(|robots| robots.allows("/x"))
    }

    /// Recommendation #1: Google's robots.cc 50a0dac matches a key by its case-insensitive prefix.
    /// Lines 219-220 and 224: `user-agent`, `useragent`, `user agent`; line 230: `allow`, no typo;
    /// lines 235-239 and 243: `disallow` and five typos.
    #[test]
    fn every_key_spelling_google_accepts_is_read() -> TestResult {
        let user_agents = [
            "User-agent",
            "USER-AGENT",
            "useragent",
            "UserAgent",
            "user agent",
            "User-agents",
        ];
        let disallows = [
            "Disallow",
            "DISALLOW",
            "dissallow",
            "dissalow",
            "disalow",
            "diasllow",
            "disallaw",
            "Disallowed",
        ];
        for user_agent in user_agents {
            assert_eq!(
                star_disallows_x(user_agent, "Disallow"),
                Ok(false),
                "{user_agent}"
            );
        }
        for disallow in disallows {
            assert_eq!(
                star_disallows_x("User-agent", disallow),
                Ok(false),
                "{disallow}"
            );
        }
        for allow in ["Allow", "ALLOW", "Allowed"] {
            let body = format!("User-agent: *\nDisallow: /\n{allow}: /x\n");
            assert!(Robots::parse(body.as_bytes())?.allows("/x"), "{allow}");
        }
        Ok(())
    }

    /// The controls: a key no line of robots.cc 217-243 names is an unknown line.
    #[test]
    fn a_key_spelling_google_does_not_accept_is_not_read() -> TestResult {
        for user_agent in ["user_agent", "agent", "user-agnet"] {
            let robots = Robots::parse(format!("{user_agent}: *\nDisallow: /x\n").as_bytes())?;
            assert_eq!(robots.reading(), Reading::NoRulesNoGroup, "{user_agent}");
        }
        for disallow in ["disalw", "dis-allow", "nodisallow"] {
            assert_eq!(
                star_disallows_x("User-agent", disallow),
                Ok(true),
                "{disallow}"
            );
        }
        for allow in ["alow", "allw", "a llow"] {
            let body = format!("User-agent: *\nDisallow: /\n{allow}: /x\n");
            assert!(!Robots::parse(body.as_bytes())?.allows("/x"), "{allow}");
        }
        Ok(())
    }

    /// robots.cc 50a0dac lines 323-340: with no colon, a run of spaces or tabs separates the key
    /// when the line holds exactly two words; three words are no line. Line 625: `*` followed by
    /// whitespace and more still names every agent.
    #[test]
    fn a_key_and_value_split_by_whitespace_alone_is_read() -> TestResult {
        let read = [
            &b"User-agent *\nDisallow /x\n"[..],
            b"useragent\t*\nDisallow \t /x\n",
            b"User-agent * bot\nDisallow: /x\n",
            b"User-agent: * and more\nDisallow: /x\n",
            b"User-agent: *bot\nDisallow: /x\n",
            b"User-agent: *\tbot\nDisallow: /x\n",
            b"User-agent: *\x0bbot\nDisallow: /x\n",
        ]
        .iter()
        .map(|body| Robots::parse(body).map(|robots| robots.allows("/x")))
        .collect::<Vec<_>>();
        assert_eq!(
            read,
            [
                Ok(false),
                Ok(false),
                Ok(true),
                Ok(false),
                Ok(true),
                Ok(false),
                Ok(false)
            ]
        );
        Ok(())
    }

    /// robots.cc 50a0dac line 296 strips absl's ASCII whitespace, vertical tab included, and no
    /// other: a trailing VT leaves `/x`, and `*` with a no-break space is no `*`.
    #[test]
    fn only_ascii_whitespace_is_stripped() -> TestResult {
        let read = [
            &b"User-agent: *\nDisallow: /x\x0b\n"[..],
            "User-agent: *\u{a0}\nDisallow: /x\n".as_bytes(),
        ]
        .iter()
        .map(|body| Robots::parse(body).map(|robots| robots.allows("/x")))
        .collect::<Vec<_>>();
        assert_eq!(read, [Ok(false), Ok(true)]);
        Ok(())
    }

    /// Google defines no Crawl-delay key (robots.cc 217-252 names none), so ours is matched whole,
    /// case aside; the colon-less split is the line's, so `Crawl-delay 20` reads.
    #[test]
    fn only_the_crawl_delay_key_itself_is_read() -> TestResult {
        let read = [
            "Crawl-delay: 20",
            "CRAWL-DELAY: 20",
            "Crawl-delay 20",
            "crawl delay: 20",
            "crawldelay: 20",
            "Crawl-delays: 20",
        ]
        .iter()
        .map(|line| {
            Robots::parse(format!("User-agent: *\n{line}\n").as_bytes())
                .map(|robots| robots.crawl_delay())
        })
        .collect::<Vec<_>>();
        let twenty = Ok(Some(Duration::from_secs(20)));
        assert_eq!(
            read,
            [
                twenty.clone(),
                twenty.clone(),
                twenty,
                Ok(None),
                Ok(None),
                Ok(None)
            ]
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
        // Nor does an unknown line. Mutation 2026-11-16, the `_` arm ending the run: red.
        let unknown = Robots::parse(
            b"User-agent: *\nUser-agent: bingbot\nSitemap: x\nUser-agent: googlebot\nDisallow: /g\n",
        )?;
        assert!(!unknown.allows("/g"));
        Ok(())
    }

    /// A parity row: a body, a path, and whether Google's `robots.cc` allowed it. Every verdict
    /// below came from robots.cc 50a0dac compiled with clang++ and run 2026-11-17 for agent
    /// `bellwether` on `https://example.test<path>`.
    type Row = (Vec<u8>, String, bool);

    fn star(rules: &[u8], path: &str, google: bool) -> Row {
        (
            [&b"User-agent: *\n"[..], rules].concat(),
            path.to_owned(),
            google,
        )
    }

    /// The rows ours reads differently from Google, named by index and path.
    fn disagreements(rows: &[Row]) -> Result<Vec<String>, RobotsError> {
        let mut differ = Vec::new();
        for (index, (body, path, google)) in rows.iter().enumerate() {
            if Robots::parse(body)?.allows(path) != *google {
                differ.push(format!("row {index}: {path:.40}"));
            }
        }
        Ok(differ)
    }

    /// The `a` run that, after `Disallow: /`, fills a line to 16,663 bytes, the last one kept.
    const KEPT_RUN: usize = 16_652;

    fn run_of_a(length: usize) -> String {
        "a".repeat(length)
    }

    /// §10 (b), robots.cc 167-213: a pattern's non-ASCII bytes become `%XX` and its `%xx` gets
    /// uppercase hex; a path is matched as given; the escaped length is the priority.
    #[test]
    fn a_pattern_is_escaped_as_googles_parser_escapes_it() -> TestResult {
        let rows = [
            star(b"Disallow:\n", "/", true),
            star(b"Disallow:\n", "/x", true),
            star(b"Disallow: /JSWidget/\n", "/JSWidget/x", false),
            star(b"Disallow: /JSWidget/\n", "/rssfeed/", true),
            star("Disallow: /ä\n".as_bytes(), "/ä", true),
            star("Disallow: /ä\n".as_bytes(), "/%C3%A4", false),
            star("Disallow: /ä\n".as_bytes(), "/%c3%a4", true),
            star(b"Disallow: /%C3%A4\n", "/ä", true),
            star(b"Disallow: /%C3%A4\n", "/%C3%A4", false),
            star(b"Disallow: /%C3%A4\n", "/%c3%a4", true),
            star(b"Disallow: /%c3%a4\n", "/ä", true),
            star(b"Disallow: /%c3%a4\n", "/%C3%A4", false),
            star(b"Disallow: /%c3%a4\n", "/%c3%a4", true),
            star(b"Disallow: /a%2Fb\n", "/a/b", true),
            star(b"Disallow: /a%2Fb\n", "/a%2Fb", false),
            star(b"Disallow: /a%2Fb\n", "/a%2fb", true),
            star(b"Disallow: /~joe\n", "/~joe", false),
            star(b"Disallow: /~joe\n", "/%7Ejoe", true),
            star(b"Disallow: /%62az\n", "/baz", true),
            star(b"Disallow: /%62az\n", "/%62az", false),
            star(b"Disallow: /a%2Ab\n", "/a*b", true),
            star(b"Disallow: /a%2Ab\n", "/a%2Ab", false),
            star(b"Disallow: /x%\n", "/x%", false),
            star(b"Disallow: /x%\n", "/x%25", false),
            star(b"Disallow: /x%a\n", "/x%a", false),
            star(b"Disallow: /x%a\n", "/x%A", true),
            star(b"Disallow: /x%ag\n", "/x%ag", false),
            star(b"Disallow: /x%ag\n", "/x%AG", true),
            star(b"Disallow: /w%3D%3D\n", "/w%3d%3d", true),
            star(b"Disallow: /w%3D%3D\n", "/w%3D%3D", false),
            star(b"Disallow: /w%3d\n", "/w%3D", false),
            star(b"Disallow: /w%3d\n", "/w%3d", true),
            star("Allow: /ä\nDisallow: /%C3\n".as_bytes(), "/%C3%A4", true),
            star("Allow: /ä\nDisallow: /%C3%A4\n".as_bytes(), "/%C3%A4", true),
            star(b"Allow: /%c3\nDisallow: /%C3\n", "/%C3x", true),
            star("Disallow: /*ä$\n".as_bytes(), "/x/%C3%A4", false),
            star("Disallow: /*ä$\n".as_bytes(), "/x/ä", true),
        ];
        assert_eq!(disagreements(&rows)?, Vec::<String>::new());
        Ok(())
    }

    /// §10 (a), robots.cc 655-668: an `Allow` that does not match, whose text from its last `/`
    /// starts with `/index.htm`, allows `<that prefix>/$` at that pattern's length.
    #[test]
    fn an_index_page_allow_also_allows_its_directory_as_google_reads_it() -> TestResult {
        let index = b"Allow: /foo/index.html\nDisallow: /\n";
        let rows = [
            star(index, "/foo/", true),
            star(index, "/foo/index.html", true),
            star(index, "/foo/index.htm", false),
            star(index, "/foo/x", false),
            star(index, "/foo/?q=1", false),
            star(b"Allow: /index.html\nDisallow: /\n", "/", true),
            star(b"Allow: /index.html\nDisallow: /\n", "/?x=1", false),
            star(b"Allow: /index.html\nDisallow: /\n", "/index.html", true),
            star(b"Allow: /foo/index.htmlx\nDisallow: /\n", "/foo/", true),
            star(b"Allow: /*/index.html\nDisallow: /\n", "/a/", true),
            star(b"Allow: /*/index.html\nDisallow: /\n", "/a/b/", true),
            star(b"Allow: /*/index.html\nDisallow: /\n", "/", false),
            star(b"Allow: /foo/index.html\nDisallow: /foo/$\n", "/foo/", true),
            star(
                b"Allow: /foo/index.html\nDisallow: /foo/$x\n",
                "/foo/$x",
                false,
            ),
            star(b"Disallow: /foo/index.html\n", "/foo/", true),
            star(b"Allow: /foo/index.php\nDisallow: /\n", "/foo/", false),
            star(b"Allow: /foo/INDEX.HTML\nDisallow: /\n", "/foo/", false),
            star(
                "Allow: /ä/index.htm\nDisallow: /\n".as_bytes(),
                "/%C3%A4/",
                true,
            ),
            star(
                "Allow: /ä/index.htm\nDisallow: /\n".as_bytes(),
                "/ä/",
                false,
            ),
            star(b"Allow: index.html\nDisallow: /\n", "/", false),
            star(b"Allow: index.html\nDisallow: /\n", "/index.html", false),
            star(
                b"Allow: /foo/index.html\nDisallow: /f*o*/$\n",
                "/foo/",
                false,
            ),
            star(
                b"Allow: /foo/index.html\nDisallow: /f*o*/$\n",
                "/foo/x",
                true,
            ),
            star(b"Allow: /foo/index.html\nDisallow: /foo\n", "/foo/", true),
            star(b"Allow: /foo/index.html\nDisallow: /foo\n", "/foo/x", false),
        ];
        assert_eq!(disagreements(&rows)?, Vec::<String>::new());
        Ok(())
    }

    /// §10 (c), robots.cc 418-447: a line keeps its first 16,663 bytes, after the BOM and before
    /// its line end, and is still read.
    #[test]
    fn a_long_line_is_cut_where_googles_parser_cuts_it() -> TestResult {
        let disallow = |run: usize, tail: &str| format!("Disallow: /{}{tail}\n", run_of_a(run));
        let past = format!("Disallow: /{}b\n", run_of_a(16_700));
        let star_after = |blanks: usize| format!("User-agent:{}*bot\n", " ".repeat(blanks));
        let rows = [
            star(
                past.as_bytes(),
                &format!("/{}", run_of_a(KEPT_RUN - 1)),
                true,
            ),
            star(past.as_bytes(), &format!("/{}", run_of_a(KEPT_RUN)), false),
            star(
                past.as_bytes(),
                &format!("/{}c", run_of_a(KEPT_RUN + 1)),
                false,
            ),
            star(
                format!("Disallow: /\nAllow: /{}b\n", run_of_a(16_700)).as_bytes(),
                &format!("/{}zzz", run_of_a(16_655)),
                true,
            ),
            star(
                format!("#{}\nDisallow: /y\n", "x".repeat(16_700)).as_bytes(),
                "/y",
                false,
            ),
            star(
                disallow(KEPT_RUN, "b\r").as_bytes(),
                &format!("/{}c", run_of_a(KEPT_RUN)),
                false,
            ),
            star(
                disallow(KEPT_RUN, "b\r").as_bytes(),
                &format!("/{}c", run_of_a(KEPT_RUN - 1)),
                true,
            ),
            star(
                disallow(KEPT_RUN - 1, "b").as_bytes(),
                &format!("/{}c", run_of_a(KEPT_RUN - 1)),
                true,
            ),
            star(
                disallow(KEPT_RUN - 1, "b").as_bytes(),
                &format!("/{}b", run_of_a(KEPT_RUN - 1)),
                false,
            ),
            star(
                disallow(KEPT_RUN - 1, "ä").as_bytes(),
                &format!("/{}%C3%A4", run_of_a(KEPT_RUN - 1)),
                false,
            ),
            star(
                disallow(KEPT_RUN - 1, "ä").as_bytes(),
                &format!("/{}%C3x", run_of_a(KEPT_RUN - 1)),
                false,
            ),
            star(
                format!("  {}", disallow(KEPT_RUN, "b")).as_bytes(),
                &format!("/{}c", run_of_a(KEPT_RUN - 1)),
                false,
            ),
            star(
                format!("  {}", disallow(KEPT_RUN, "b")).as_bytes(),
                &format!("/{}c", run_of_a(KEPT_RUN - 3)),
                true,
            ),
            star(
                format!("Allow: /{}\nDisallow: /a\n", run_of_a(16_700)).as_bytes(),
                &format!("/{}", run_of_a(16_655)),
                true,
            ),
            star(
                format!("Allow: /{}\nDisallow: /a\n", run_of_a(16_700)).as_bytes(),
                &format!("/{}", run_of_a(100)),
                false,
            ),
            (
                format!("\u{feff}{}Disallow: /x\n", star_after(16_651)).into_bytes(),
                "/x".to_owned(),
                false,
            ),
            (
                format!("{}Disallow: /x\n", star_after(16_651)).into_bytes(),
                "/x".to_owned(),
                false,
            ),
            (
                format!("{}Disallow: /x\n", star_after(16_652)).into_bytes(),
                "/x".to_owned(),
                true,
            ),
        ];
        assert_eq!(disagreements(&rows)?, Vec::<String>::new());
        Ok(())
    }

    /// §10 (d): robots.cc reads each line as a C string, so a line ends at its first NUL byte,
    /// whether that sits in a key, before the colon or in a value.
    #[test]
    fn a_line_ends_at_its_first_nul_as_googles_parser_reads_it() -> TestResult {
        let rows = [
            star(b"Disallow: /x\0y\n", "/x", false),
            star(b"Disallow: /x\0y\n", "/xy", false),
            star(b"Disallow: /x\0y\n", "/x/z", false),
            star(b"Disallow\0: /x\n", "/x", true),
            (
                b"User-agent: *\0bot\nDisallow: /x\n".to_vec(),
                "/x".to_owned(),
                false,
            ),
            star(b"Disallow: /\0\nAllow: /x\n", "/x", true),
            star(b"Disallow: /\0\nAllow: /x\n", "/y", false),
            star(b"Disallow: /x\x01y\n", "/x", true),
            star(b"Disallow: /x\x01y\n", "/x\x01y", false),
            star(b"Disallow: /x\x7fy\n", "/x", true),
            star(b"Disallow: /x\0# hidden\n", "/x", false),
            star(b"Disal\0low: /x\n", "/x", true),
            star(b"\0Disallow: /x\n", "/x", true),
            (
                b"User\0-agent: *\nDisallow: /x\n".to_vec(),
                "/x".to_owned(),
                true,
            ),
            star(b"Disallow: /x \0/y\n", "/x", false),
            star(b"Disallow: /x \0/y\n", "/x /y", false),
        ];
        assert_eq!(disagreements(&rows)?, Vec::<String>::new());
        Ok(())
    }

    /// Owner decision 2026-11-17: a non-UTF-8 body is read as bytes, as robots.cc reads it (§10 (e)).
    /// Mutations 2026-11-17, the UTF-8 refusal restored: red; only a whole BOM skipped: red.
    #[test]
    fn a_body_that_is_not_utf8_is_read_as_bytes_as_googles_parser_reads_it() -> TestResult {
        let latin1 = b"# Datei f\xfcr Roboter\nUser-agent: *\nDisallow: /caf\xe9/\nAllow: /caf\xe9/men\xfc\n";
        let row = |body: &[u8], path: &str, google| (body.to_vec(), path.to_owned(), google);
        let rows = [
            star(b"Disallow: /\xe4\n", "/%E4", false),
            star(b"Disallow: /\xe4\n", "/%e4", true),
            star(b"Disallow: /\xe4\n", "/\u{e4}", true),
            star(b"Disallow: /\xe4\n", "/x", true),
            star(b"Disallow: /\xe4\xfc\n", "/%E4%FC", false),
            star(b"Disallow: /\xe4\xfc\n", "/%E4", true),
            star(b"Disallow: /%e4\xe4\n", "/%E4%E4", false),
            star(b"Disallow: /%e4\xe4\n", "/%e4%E4", true),
            star(b"Disallow: /\xe4%\n", "/%E4%25", false),
            star(b"Allow: /\xe4\nDisallow: /%E4\n", "/%E4", true),
            star(b"Disallow: /\x80\x80*\xff$\n", "/%80%80a%FF", false),
            star(b"Disallow: /\x80\x80*\xff$\n", "/%80%80a%FFb", true),
            star(b"Allow: /\xe4/index.htm\nDisallow: /\n", "/%E4/", true),
            star(b"Allow: /\xe4/index.htm\nDisallow: /\n", "/%E4/x", false),
            row(latin1, "/caf%E9/x", false),
            row(latin1, "/caf%E9/men%FC", true),
            row(latin1, "/caf%C3%A9/x", true),
            row(latin1, "/x", true),
            star(b"Disallow: /\xc3\0\xa4\n", "/%C3", false),
            star(b"Disallow: /\xc3\0\xa4\n", "/%C3x", false),
            star(b"Disallow: /\xc3\0\xa4\n", "/x", true),
            row(b"User-agent: *\xa0x\nDisallow: /x\n", "/x", true),
            row(b"User-agent: *\xe4\nDisallow: /x\n", "/x", true),
            row(b"User-agent: \xe4*\nDisallow: /x\n", "/x", true),
            row(b"User-agent: b\xe4t\nDisallow: /x\n", "/x", true),
            row(
                b"User-agent: *\nUser-agent: b\xe4t\nDisallow: /x\n",
                "/x",
                false,
            ),
            row(b"Us\xe4r-agent: *\nDisallow: /x\n", "/x", true),
            row(b"User-agent\xe4: *\nDisallow: /x\n", "/x", false),
            star(b"Disallow\xe4: /x\n", "/x", false),
            row(b"\xef\xbbUser-agent: *\nDisallow: /x\n", "/x", false),
            row(b"\xefUser-agent: *\nDisallow: /x\n", "/x", false),
            row(b"\xbbUser-agent: *\nDisallow: /x\n", "/x", true),
            row(b"\xffUser-agent: *\nDisallow: /x\n", "/x", true),
            row(
                b"\xef\xbb\xbf\xef\xbb\xbfUser-agent: *\nDisallow: /x\n",
                "/x",
                true,
            ),
        ];
        assert_eq!(disagreements(&rows)?, Vec::<String>::new());
        Ok(())
    }

    /// The owner's exceptions to Google (2026-11-17): an HTML body and a bad `*` Crawl-delay are
    /// refused where Google allows `/y`; a non-ASCII Crawl-delay is no number, where Google
    /// disallows `/x` (compiled robots.cc, 2026-11-17).
    #[test]
    fn the_bodies_google_reads_and_ours_refuses_are_the_named_exceptions() {
        let refused = [
            &b"<html>\nUser-agent: *\nDisallow: /x\n"[..],
            b"User-agent: *\nCrawl-delay: soon\nDisallow: /x\n",
            b"User-agent: *\nCrawl-delay: 5\xe4\nDisallow: /x\n",
        ]
        .map(|body| Robots::parse(body).map(|robots| robots.allows("/y")));
        assert_eq!(
            refused,
            [
                Err(RobotsError::HtmlPage),
                Err(RobotsError::CrawlDelayNotANumber("soon".to_owned())),
                Err(RobotsError::CrawlDelayNotANumber(r"5\xe4".to_owned())),
            ]
        );
        assert_eq!(
            refused
                .get(2)
                .map(|verdict| verdict.as_ref().err().map(ToString::to_string)),
            Some(Some(
                r#"the * group's Crawl-delay "5\xe4" is not a non-negative number of seconds"#
                    .to_owned()
            ))
        );
    }

    #[test]
    fn every_verdict_has_its_own_name() {
        let names = [
            RobotsError::HtmlPage.verdict(),
            RobotsError::CrawlDelayNotANumber(String::new()).verdict(),
            Reading::Rules.name(),
            Reading::NoRulesEmpty.name(),
            Reading::NoRulesNoGroup.name(),
            Reading::OrphanRulesIgnored.name(),
        ];
        assert_eq!(
            names,
            [
                "refused_html",
                "refused_crawl_delay",
                "rules",
                "no_rules_empty",
                "no_rules_no_group",
                "orphan_rules_ignored"
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

    /// CNW's one `Disallow` sits in its `*` group; its `Sitemap` lines are no rules. Mutations
    /// 2026-11-17, every `Disallow` read as `Allow`: red; `Disallow` lines skipped: red.
    #[test]
    fn the_cnw_robots_allows_the_news_sitemap_and_the_news_releases_prefix() -> TestResult {
        let cnw = Robots::parse(&provider_fixture("cnw/robots_www_newswire_ca.txt")?)?;
        assert_eq!(
            (
                cnw.allows("/sitemap-news.xml?page=1"),
                cnw.allows("/news-releases/"),
                cnw.allows("/templates/get-bitly-url.txt"),
                cnw.crawl_delay()
            ),
            (true, true, false, None)
        );
        Ok(())
    }

    /// Yahoo's `*` group allows the sitemap and `/`, the path filter collect checks, and names no
    /// Crawl-delay; it disallows paths below `/`, which no followed row was under on 2026-11-16.
    /// Mutations 2026-11-17, every `Disallow` read as `Allow`; `Disallow` lines skipped: each red.
    #[test]
    fn the_yahoo_robots_allows_the_news_sitemap_and_every_path_but_its_disallowed_ones(
    ) -> TestResult {
        let yahoo = Robots::parse(&provider_fixture("yahoo/robots_finance_yahoo_com.txt")?)?;
        assert_eq!(
            (
                yahoo.allows("/news-sitemap.xml"),
                yahoo.allows("/"),
                yahoo.allows("/healthcare/articles/laverock-therapeutics-091700844.html"),
                yahoo.allows("/m/an-article.html"),
                yahoo.crawl_delay()
            ),
            (true, true, true, false, None)
        );
        Ok(())
    }
}
