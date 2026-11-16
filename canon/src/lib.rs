//! The digest rules of `docs/design/architecture.md` section 2: the canonical form, whose six
//! rules live in that section's "Canonical form" bullet and are not restated here, and
//! [`FileDigest`] for the rows whose input is a file's bytes, named by that same bullet. Why
//! the form is a crate of its own rather than code inside the first producer: `CONTRIBUTING.md`,
//! "Everything else" - every digest in section 2 is taken over it, and section 8 row R7 is the
//! row it moves. `canon/vectors/` is what this crate owes a reader in another language.

use chrono::{DateTime, NaiveDate, Timelike, Utc};
use sha2::{Digest as _, Sha256};
use unicode_normalization::UnicodeNormalization;

pub mod digest;

/// Between two fields, with no trailing separator.
const FIELD: char = '\u{1f}';
/// Between the members of one list field, in that field's own stated order.
const LIST: char = '\u{1e}';
/// Between a tag and the value it qualifies. Distinct from [`FIELD`] so that a tagged value
/// cannot render as the two plain text fields that spell the same thing.
const TAG: char = '\u{1d}';
/// The three separators, which is also the set a value may not contain.
const SEPARATORS: [char; 3] = [FIELD, LIST, TAG];
/// Truncating, never rounding: chrono's `%.6f` of a 1,500 ns instant is `.000001` - measured
/// 2026-11-12 against chrono 0.4.45.
const INSTANT_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.6fZ";
const DAY_FORMAT: &str = "%Y-%m-%d";
const INSTANT_TAG: &str = "instant";
const MINUTE_TAG: &str = "minute";
const DAY_TAG: &str = "day";
const CREDENTIAL_TAG: &str = "credential";
/// chrono carries a leap second's extra second in the nanosecond field rather than in `second`.
const LEAP_SECOND_NANOS: u32 = 1_000_000_000;

/// The two ways a field list can fail to have a canonical form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonError {
    /// A value holding a separator forges a field boundary: without this refusal the one field
    /// `a\u{1f}b` and the two fields `a` and `b` have one canonical form and therefore one digest.
    SeparatorInValue,
    /// Measured 2026-11-12: chrono 0.4.45 parses `2026-07-01T23:59:60Z`, renders it
    /// `23:59:60.000000Z`, and its own microsecond round trip returns `2026-07-02T00:00:00Z`
    /// (not equal), while Python's `fromisoformat` raises `second must be in 0..59, not 60`.
    SixtiethSecond,
    /// A `minute` stamp whose seconds or sub-seconds are not zero: `pit::datedness::Minute`
    /// refuses one, and the form must not depend on every caller having gone through it.
    MinuteWithSeconds,
}

impl std::fmt::Display for CanonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SeparatorInValue => write!(f, "a value holding a canonical-form separator"),
            Self::SixtiethSecond => write!(f, "an instant whose seconds field is 60"),
            Self::MinuteWithSeconds => write!(f, "a minute stamp carrying seconds"),
        }
    }
}

/// Which digest a canonical form is for. Its name is the first field of every form, so two
/// digest functions whose remaining fields coincide cannot produce one digest: before this
/// existed, `filter_params` over one literal parameter and `story_id` over a source and an id
/// were the same two text fields and the same sha256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    ItemId,
    StoryId,
    DocId,
    AttemptId,
    FilterParams,
    ResultDigest,
    SnapshotId,
    ExtractVersion,
}

impl Domain {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ItemId => "item_id",
            Self::StoryId => "story_id",
            Self::DocId => "doc_id",
            Self::AttemptId => "attempt_id",
            Self::FilterParams => "filter_params",
            Self::ResultDigest => "result_digest",
            Self::SnapshotId => "snapshot_id",
            Self::ExtractVersion => "extract_version",
        }
    }
}

/// A publication time together with its precision, which is what keeps a day from colliding with
/// an instant (architecture.md section 3.1, `valid_precision`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stamp {
    Instant(DateTime<Utc>),
    /// A minute as its first microsecond: it differs from that instant only by its tag.
    Minute(DateTime<Utc>),
    /// The date as published, never a fabricated instant inside it.
    Day(NaiveDate),
}

/// One field of a canonical form.
#[derive(Debug, Clone, Copy)]
pub enum Field<'a> {
    Text(&'a str),
    /// An absent optional field: it contributes an empty string and keeps its place in the list.
    Absent,
    List(&'a [&'a str]),
    Stamp(Stamp),
    /// A credential's NAME. Its value never enters a digest, because `filter_params`
    /// (architecture.md section 3.2) is written into an archive nothing may delete.
    Credential(&'a str),
}

/// A sha256 over a canonical form. Constructible only by [`sha256_canon`], so a digest in a field
/// of this type was taken over the form and not over something a caller assembled.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Digest(String);

/// A sha256 over a hashed FILE's bytes as committed, for the rows architecture.md section 2's
/// "Canonical form" bullet names. Its own type so that "a [`Digest`] was taken over the form" keeps
/// holding.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileDigest(String);

impl FileDigest {
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(const_hex::encode(Sha256::digest(bytes)))
    }

    #[must_use]
    pub fn hex(&self) -> &str {
        &self.0
    }
}

impl Digest {
    #[must_use]
    pub fn hex(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Every caller-supplied value passes through here before it reaches the form.
fn write_value(value: &str, out: &mut String) -> Result<(), CanonError> {
    if value.contains(SEPARATORS) {
        return Err(CanonError::SeparatorInValue);
    }
    out.extend(value.nfc());
    Ok(())
}

fn render(field: &Field, out: &mut String) -> Result<(), CanonError> {
    match *field {
        Field::Text(text) => write_value(text, out)?,
        Field::Absent => {}
        Field::List(members) => {
            for (position, member) in members.iter().enumerate() {
                if position > 0 {
                    out.push(LIST);
                }
                write_value(member, out)?;
            }
        }
        Field::Stamp(Stamp::Instant(at)) => tagged(INSTANT_TAG, &instant_text(at)?, out)?,
        Field::Stamp(Stamp::Minute(starts)) => tagged(MINUTE_TAG, &minute_text(starts)?, out)?,
        Field::Stamp(Stamp::Day(day)) => {
            tagged(DAY_TAG, &day.format(DAY_FORMAT).to_string(), out)?;
        }
        Field::Credential(name) => tagged(CREDENTIAL_TAG, name, out)?,
    }
    Ok(())
}

fn instant_text(at: DateTime<Utc>) -> Result<String, CanonError> {
    if at.timestamp_subsec_nanos() >= LEAP_SECOND_NANOS {
        return Err(CanonError::SixtiethSecond);
    }
    Ok(at.format(INSTANT_FORMAT).to_string())
}

fn minute_text(starts: DateTime<Utc>) -> Result<String, CanonError> {
    let text = instant_text(starts)?;
    if starts.second() != 0 || starts.nanosecond() != 0 {
        return Err(CanonError::MinuteWithSeconds);
    }
    Ok(text)
}

/// A tag and the value it qualifies, in that order. Section 2 says the precision tag travels
/// *beside* the value and does not say on which side; this is the choice, and it is the same one
/// for a credential reference.
fn tagged(tag: &str, value: &str, out: &mut String) -> Result<(), CanonError> {
    out.push_str(tag);
    out.push(TAG);
    write_value(value, out)
}

/// The canonical form of a field list, under the digest it is for.
pub fn canon(domain: Domain, fields: &[Field]) -> Result<String, CanonError> {
    let mut out = String::from(domain.name());
    for field in fields {
        out.push(FIELD);
        render(field, &mut out)?;
    }
    Ok(out)
}

/// The sha256 of [`canon`]'s output, lower-case hex.
pub fn sha256_canon(domain: Domain, fields: &[Field]) -> Result<Digest, CanonError> {
    let form = canon(domain, fields)?;
    Ok(Digest(const_hex::encode(Sha256::digest(form.as_bytes()))))
}

#[cfg(test)]
mod tests {
    use super::{canon, sha256_canon, CanonError, Domain, Field, FileDigest, Stamp};

    /// Any one domain: these tests are about the form, and the form is the same under each.
    const ANY: Domain = Domain::ItemId;
    use chrono::{DateTime, Utc};

    /// 1,500 ns past a whole microsecond, which `%.6f` truncates rather than rounds.
    const NANOSECOND_BEARING: &str = "2026-07-01T12:00:00.000001500Z";
    const SIXTIETH_SECOND: &str = "2026-07-01T23:59:60Z";
    const A_MINUTE: &str = "2026-11-14T19:00:00Z";
    const THIRTY_SECONDS_INTO_IT: &str = "2026-11-14T19:00:30Z";
    const A_MICROSECOND_INTO_IT: &str = "2026-11-14T19:00:00.000001Z";
    const CREDENTIAL_NAME: &str = "sec_contact";
    const HEADER_NAME: &str = "User-Agent";
    /// German phonebook collation puts `ä` with `ae` and therefore before `z`; raw UTF-8 bytes
    /// put `z` (0x7a) before `ä` (0xc3 0xa4).
    const LOCALE_ORDER: [&str; 2] = ["ärger", "zebra"];
    const CODE_POINT_ORDER: [&str; 2] = ["zebra", "ärger"];

    /// Either the canonical form of one instant field, or why there is none.
    fn canon_of_instant(text: &str) -> String {
        match text.parse::<DateTime<Utc>>() {
            Ok(at) => match canon(ANY, &[Field::Stamp(Stamp::Instant(at))]) {
                Ok(form) => form,
                Err(error) => format!("refused: {error}"),
            },
            Err(error) => format!("unparsed: {error}"),
        }
    }

    fn hex(fields: &[Field]) -> String {
        match sha256_canon(ANY, fields) {
            Ok(digest) => digest.hex().to_string(),
            Err(error) => format!("refused: {error}"),
        }
    }

    /// `printf 'a' | shasum -a 256` on 2026-11-13. A file digest is the plain sha256 of the bytes
    /// and takes no canonical form, so the one value an outside tool can reproduce pins it.
    const SHA256_OF_ONE_LETTER_A: &str =
        "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb";

    #[test]
    fn a_file_digest_is_the_plain_sha256_of_the_bytes_and_moves_with_one_of_them() {
        assert_eq!(FileDigest::of(b"a").hex(), SHA256_OF_ONE_LETTER_A);
        assert_ne!(FileDigest::of(b"a"), FileDigest::of(b"b"));
    }

    #[test]
    fn a_list_sorted_by_a_locale_collation_digests_differently_from_code_point_order() {
        assert_ne!(
            hex(&[Field::List(&LOCALE_ORDER)]),
            hex(&[Field::List(&CODE_POINT_ORDER)])
        );
    }

    #[test]
    fn a_credential_ref_canonicalises_to_the_credentials_name_and_never_its_value() {
        assert_eq!(
            canon(
                ANY,
                &[Field::Text(HEADER_NAME), Field::Credential(CREDENTIAL_NAME)]
            ),
            Ok("item_id\u{1f}User-Agent\u{1f}credential\u{1d}sec_contact".to_string())
        );
    }

    #[test]
    fn a_nanosecond_bearing_instant_canonicalises_to_the_microsecond_it_is_stored_as() {
        assert_eq!(
            canon_of_instant(NANOSECOND_BEARING),
            "item_id\u{1f}instant\u{1d}2026-07-01T12:00:00.000001Z"
        );
    }

    #[test]
    fn an_instant_with_a_sixtieth_second_is_refused_rather_than_canonicalised() {
        assert_eq!(
            canon_of_instant(SIXTIETH_SECOND),
            format!("refused: {}", CanonError::SixtiethSecond)
        );
    }

    /// Review 2026-11-15: before this refusal `Stamp::Minute` of 19:00:30 digested as a minute.
    #[test]
    fn a_minute_stamp_carrying_seconds_is_refused_and_a_whole_minute_is_not() {
        let refused = [A_MINUTE, THIRTY_SECONDS_INTO_IT, A_MICROSECOND_INTO_IT].map(|text| {
            text.parse::<DateTime<Utc>>()
                .ok()
                .map(|at| canon(ANY, &[Field::Stamp(Stamp::Minute(at))]).err())
        });
        assert_eq!(
            refused,
            [
                Some(None),
                Some(Some(CanonError::MinuteWithSeconds)),
                Some(Some(CanonError::MinuteWithSeconds))
            ]
        );
    }
}
