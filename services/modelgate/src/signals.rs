//! What code reads out of two bodies beside their similarity: visible hosts, number tokens and
//! ticker tags. Recorded per candidate pair and judged by nothing yet. Both grammars and what each
//! misses: `docs/design/architecture.md` section 3.5, "Repeated releases".

use std::collections::BTreeSet;

use crate::coverage;

pub const HOST_GRAMMAR: &str = "visible-host/1";
pub const NUMBER_GRAMMAR: &str = "number-token/1";
const MIN_NUMBER_DIGITS: usize = 2;
const HOST_PREFIXES: [&str; 3] = ["https://", "http://", "www."];
const MIN_TLD_LETTERS: usize = 2;
const PPM: u128 = 1_000_000;

/// An ASCII digit for a decimal digit of the four scripts a release is likely to carry.
fn digit(character: char) -> Option<char> {
    let code = u32::from(character);
    let folded = match code {
        0x30..=0x39 => code,
        0x0660..=0x0669 => code - 0x0660 + 0x30,
        0x06F0..=0x06F9 => code - 0x06F0 + 0x30,
        0xFF10..=0xFF19 => code - 0xFF10 + 0x30,
        _ => return None,
    };
    char::from_u32(folded)
}

/// Every maximal run of at least two digits. No separator joins two runs, so `1,234.5`,
/// `1.234,5` and `1 234,5` all read `234`, and a date reads the same in any order of its parts.
#[must_use]
pub fn number_tokens(text: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut run = String::new();
    for character in text.chars().chain([' ']) {
        if let Some(folded) = digit(character) {
            run.push(folded);
            continue;
        }
        if run.len() >= MIN_NUMBER_DIGITS {
            found.insert(std::mem::take(&mut run));
        }
        run.clear();
    }
    found
}

fn host_of(word: &str) -> Option<String> {
    let lowered = word.to_ascii_lowercase();
    let mut rest = lowered.as_str();
    let mut marked = false;
    for prefix in HOST_PREFIXES {
        if let Some(stripped) = rest.strip_prefix(prefix) {
            rest = stripped;
            marked = true;
        }
    }
    let host = rest
        .split(['/', '?', '#', ':'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('.');
    let label = |text: &str| {
        !text.is_empty()
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    };
    let tld = host.rsplit('.').next().unwrap_or_default();
    let shaped = host.contains('.')
        && host.split('.').all(label)
        && tld.len() >= MIN_TLD_LETTERS
        && tld.bytes().all(|byte| byte.is_ascii_lowercase());
    (marked && shaped).then(|| host.to_owned())
}

/// Every host written out in the text after `http://`, `https://` or `www.`, lower-cased and
/// without the `www.`. A link's target is not text and a bare `example.com` is not read.
#[must_use]
pub fn visible_hosts(text: &str) -> BTreeSet<String> {
    let edge = |c: char| c.is_whitespace() || "()[]<>\"',;".contains(c);
    text.split(edge).filter_map(host_of).collect()
}

/// What one body carries, read from the whole body and never from the cut state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Signals {
    pub hosts: BTreeSet<String>,
    pub numbers: BTreeSet<String>,
    pub tickers: BTreeSet<String>,
    pub body_bytes: usize,
}

impl Signals {
    #[must_use]
    pub fn of(body: &str) -> Self {
        Self {
            hosts: visible_hosts(body),
            numbers: number_tokens(body),
            tickers: coverage::tags(body),
            body_bytes: body.len(),
        }
    }

    #[must_use]
    pub fn shared_hosts(&self, other: &Self) -> Vec<String> {
        self.hosts.intersection(&other.hosts).cloned().collect()
    }

    /// The two threshold-free guards: a shared visible host, and no contradiction between two
    /// tagged texts. A text with no visible host passes neither.
    #[must_use]
    pub fn structurally_alike(&self, other: &Self) -> bool {
        let tagged = !self.tickers.is_empty() && !other.tickers.is_empty();
        !self.hosts.is_disjoint(&other.hosts)
            && !(tagged && self.tickers.is_disjoint(&other.tickers))
    }

    /// The shorter body's bytes over the longer's, in parts per million.
    #[must_use]
    pub fn length_ratio_ppm(&self, other: &Self) -> i64 {
        let (short, long) = (
            self.body_bytes.min(other.body_bytes),
            self.body_bytes.max(other.body_bytes),
        );
        if long == 0 {
            return 0;
        }
        i64::try_from(short as u128 * PPM / long as u128).unwrap_or(i64::MAX)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeSet;

    use super::{number_tokens, visible_hosts, Signals};

    /// One invented funding release in six languages, written for this test: same amount, date,
    /// site and ticker in each, in that language's own number and date forms.
    pub(crate) const FUNDING: [(&str, &str); 6] = [
        ("en", "Harrowgate Robotics (NASDAQ: HRWG) said on November 23, 2026 that it raised $12,500,000 in new funding from 14 investors. Details: https://www.harrowgate-robotics.example/news"),
        ("de", "Harrowgate Robotics (NASDAQ: HRWG) teilte am 23.11.2026 mit, dass es 12.500.000 US-Dollar von 14 Investoren eingeworben hat. Einzelheiten: https://www.harrowgate-robotics.example/news"),
        ("fr", "Harrowgate Robotics (NASDAQ: HRWG) a annoncé le 23 novembre 2026 avoir levé 12 500 000 dollars auprès de 14 investisseurs. Détails : https://www.harrowgate-robotics.example/news"),
        ("es", "Harrowgate Robotics (NASDAQ: HRWG) comunicó el 23 de noviembre de 2026 que captó 12.500.000 dólares de 14 inversores. Detalles: https://www.harrowgate-robotics.example/news"),
        ("sk", "Spoločnosť Harrowgate Robotics (NASDAQ: HRWG) 23. 11. 2026 oznámila, že získala 12 500 000 USD od 14 investorov. Podrobnosti: https://www.harrowgate-robotics.example/news"),
        ("pl", "Spółka Harrowgate Robotics (NASDAQ: HRWG) podała 23.11.2026 r., że pozyskała 12 500 000 USD od 14 inwestorów. Szczegóły: www.harrowgate-robotics.example/news"),
    ];
    /// Two invented law firms on one invented lawsuit: one matter, two documents.
    pub(crate) const FIRM_ONE: &str = "Dunmere & Vale LLP reminds investors of Harrowgate Robotics (NASDAQ: HRWG) of the January 12, 2027 lead plaintiff deadline in the class action. No fee unless we win. Call 555 0100 or visit https://www.dunmere-vale.example/hrwg";
    pub(crate) const FIRM_TWO: &str = "The Okonjo Law Group announces a class action against Harrowgate Robotics (NASDAQ: HRWG). Investors have until January 12, 2027 to ask to lead the case. Contact 555 0177 or https://okonjo-law.example/cases/hrwg";
    /// The same issuer three weeks later: same site and wording, other numbers.
    pub(crate) const FOLLOW_UP: &str = "Harrowgate Robotics (NASDAQ: HRWG) said on December 14, 2026 that it raised $31,000,000 in new funding from 9 investors. Details: https://www.harrowgate-robotics.example/news";

    fn set(members: &[&str]) -> BTreeSet<String> {
        members.iter().map(|member| (*member).to_owned()).collect()
    }

    /// The locale table. Mutations, each alone: `.` and `,` between digits joined into one run;
    /// the two-digit minimum dropped (2026-11-23); full-width digits not folded (2026-11-24):
    /// red each.
    #[test]
    fn a_translation_keeps_its_number_tokens_whatever_the_locale_writes_between_digits() {
        let english = number_tokens(FUNDING[0].1);
        assert_eq!(english, set(&["000", "12", "14", "2026", "23", "500"]));
        for (language, text) in &FUNDING[1..] {
            let tokens = number_tokens(text);
            let dated: BTreeSet<String> = tokens.difference(&set(&["11"])).cloned().collect();
            assert_eq!(dated, english, "{language}");
        }
        let forms = [
            ("1,234.5 and 1.234,5 and 1 234,5", vec!["234"]),
            ("2025 2024", vec!["2024", "2025"]),
            (
                "+1 212 555 0100 or +1-212-555-0100",
                vec!["0100", "212", "555"],
            ),
            ("1.5 billion", vec![]),
            ("1.500 Millionen", vec!["500"]),
            ("２０２６", vec!["2026"]),
            ("٢٠٢٦", vec!["2026"]),
            ("۲۰۲۶", vec!["2026"]),
            ("7", vec![]),
        ];
        for (text, expected) in forms {
            assert_eq!(number_tokens(text), set(&expected), "{text}");
        }
    }

    /// Mutations 2026-11-23, each alone: a bare `name.example` read as a host; the path kept;
    /// `www.` kept: red each.
    #[test]
    fn a_host_is_read_only_where_the_text_writes_a_url_out() {
        let found = visible_hosts(
            "See https://www.Harrowgate-Robotics.example/news, (http://ir.example.org:8080/a?b) \
             or www.example.net. Not example.com, not said.The next, not https://localhost/x, \
             not http://10.0.0.1/y, not press@example.org.",
        );
        assert_eq!(
            found,
            set(&[
                "example.net",
                "harrowgate-robotics.example",
                "ir.example.org"
            ])
        );
    }

    /// The hard negatives of section 3.5: what the structural guards say about each. Mutations
    /// 2026-11-24, each alone: an untagged text read as a contradiction; one tagged text enough
    /// for one: red each.
    #[test]
    fn two_firms_on_one_lawsuit_share_a_ticker_and_no_host_and_a_follow_up_shares_both() {
        let release = Signals::of(FUNDING[0].1);
        for (language, text) in &FUNDING[1..] {
            let copy = Signals::of(text);
            assert!(release.structurally_alike(&copy), "{language}");
            assert_eq!(
                release.shared_hosts(&copy),
                ["harrowgate-robotics.example"],
                "{language}"
            );
        }
        let (one, two, later) = (
            Signals::of(FIRM_ONE),
            Signals::of(FIRM_TWO),
            Signals::of(FOLLOW_UP),
        );
        assert_eq!(one.tickers, two.tickers);
        assert!(!one.structurally_alike(&two));
        assert!(release.structurally_alike(&later));
        // 3 of the follow-up's 4 tokens: the thousands group, a count and the year are shared.
        let shared: Vec<&str> = release
            .numbers
            .intersection(&later.numbers)
            .map(String::as_str)
            .collect();
        assert_eq!(
            (shared, later.numbers.len()),
            (vec!["000", "14", "2026"], 4)
        );
        let other =
            Signals::of("Quillfeather Mining (NYSE: QFM) https://harrowgate-robotics.example");
        assert!(!release.structurally_alike(&other));
        // A text with no ticker tag contradicts nothing: 70% of the first 106 stories had none.
        let untagged = Signals::of("Our annual meeting: https://harrowgate-robotics.example/agm");
        assert!(untagged.tickers.is_empty());
        assert!(untagged.structurally_alike(&untagged.clone()));
        assert!(release.structurally_alike(&untagged) && untagged.structurally_alike(&release));
        // Two firms that both print one third party's host are alike: a rule must exclude it.
        let social = " Follow https://www.social-network.example/firm";
        let (one, two) = (
            Signals::of(&format!("{FIRM_ONE}{social}")),
            Signals::of(&format!("{FIRM_TWO}{social}")),
        );
        assert!(one.structurally_alike(&two));
        assert_eq!(release.length_ratio_ppm(&release), 1_000_000);
        assert_eq!(
            Signals::of("ab").length_ratio_ppm(&Signals::of("abcd")),
            500_000
        );
    }
}
