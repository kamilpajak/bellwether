//! A model id that names one dated build. An alias is refused here, before any row or socket:
//! `docs/design/architecture.md` section 3.5, row `model`.

use chrono::{DateTime, NaiveDate, Utc};

/// The vendor's marker of a floating alias.
const ALIAS_PREFIX: char = '~';
const DATE_DIGITS: usize = 8;
const DATE_FORMAT: &str = "%Y%m%d";
/// Bounds the request envelope beside the state and question budgets (`request.rs`).
pub const MAX_MODEL_ID_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelIdError {
    #[error("the model id is an alias (leading `~`); name a dated build, `<id>-YYYYMMDD`")]
    Alias,
    #[error("the model id does not end in `-YYYYMMDD` with a real date, so it names no one build")]
    Undated,
    #[error("the model id is longer than {MAX_MODEL_ID_BYTES} bytes or holds a character outside `[A-Za-z0-9._/-]`")]
    Malformed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelId {
    text: String,
    build_date: NaiveDate,
}

impl ModelId {
    pub fn parse(text: &str) -> Result<Self, ModelIdError> {
        if text.starts_with(ALIAS_PREFIX) {
            return Err(ModelIdError::Alias);
        }
        let plain = |byte: u8| byte.is_ascii_alphanumeric() || b"._/-".contains(&byte);
        if text.len() > MAX_MODEL_ID_BYTES || !text.bytes().all(plain) {
            return Err(ModelIdError::Malformed);
        }
        let build_date = text
            .rsplit_once('-')
            .filter(|(name, digits)| !name.is_empty() && digits.len() == DATE_DIGITS)
            .and_then(|(_, digits)| NaiveDate::parse_from_str(digits, DATE_FORMAT).ok())
            .ok_or(ModelIdError::Undated)?;
        Ok(Self {
            text: text.to_owned(),
            build_date,
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Whether the build's date is before the UTC day the story was observed on. A build dated
    /// on or after that day may have read the story, so it does not label it (section 3.5).
    /// The date is the id's suffix, not a vendor statement of a knowledge cutoff.
    #[must_use]
    pub fn predates(&self, observed_at: DateTime<Utc>) -> bool {
        self.build_date < observed_at.date_naive()
    }
}

#[cfg(test)]
mod tests {
    use super::{ModelId, ModelIdError};
    use chrono::{DateTime, Utc};

    const DATED: &str = "example/decide-1-20260917";

    /// Section 8 row MG2. Mutation 2026-11-23, the suffix check replaced by a fixed date: red.
    #[test]
    fn an_alias_or_an_undated_model_id_is_refused() {
        let refused = [
            "~example/decide-latest",
            "example/decide-1",
            "example/decide-1-20261340",
            "example/decide-1-2026091",
            "-20260917",
        ]
        .map(|text| ModelId::parse(text).err());
        assert_eq!(
            refused,
            [
                Some(ModelIdError::Alias),
                Some(ModelIdError::Undated),
                Some(ModelIdError::Undated),
                Some(ModelIdError::Undated),
                Some(ModelIdError::Undated),
            ]
        );
        assert_eq!(
            ModelId::parse(DATED).map(|id| id.as_str().to_owned()),
            Ok(DATED.to_owned())
        );
    }

    #[test]
    fn a_model_id_holding_a_space_or_a_quote_is_malformed() {
        let refused = ["example/a b-20260917", "example/\"-20260917"].map(ModelId::parse);
        assert_eq!(
            refused,
            [Err(ModelIdError::Malformed), Err(ModelIdError::Malformed)]
        );
    }

    /// Section 8 row MG2b. Mutation 2026-11-23, `<` to `<=`: red on the same-day case.
    #[test]
    fn a_build_dated_on_or_after_the_observation_day_does_not_predate_the_story(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let build = ModelId::parse(DATED)?;
        let verdicts = [
            "2026-09-16T23:59:59Z",
            "2026-09-17T00:00:00Z",
            "2026-09-17T23:59:59Z",
            "2026-09-18T00:00:00Z",
        ]
        .map(|at| at.parse::<DateTime<Utc>>().map(|at| build.predates(at)));
        assert_eq!(verdicts, [Ok(false), Ok(false), Ok(false), Ok(true)]);
        Ok(())
    }
}
