//! The observation lag as a hashed input FILE, one entry per (source, provenance).
//!
//! `probed_lag` asks whether a `vendor_archive` window has a measured lag behind it
//! (`docs/design/architecture.md` section 8 row R11) and until this module existed nothing could
//! hand it one on a production path: `EdgarPress::with_observation_lag` had no caller outside a
//! test. A lag is a MEASUREMENT with a probe behind it, so it reaches a run the way the calendar,
//! the quarter listing and the roster do - a file the caller names, digested as read, so the run
//! envelope can say which lag dated its items and a second reader can re-derive it.

use std::collections::BTreeMap;

use canon::FileDigest;
use pit::MeasuredLag;

use crate::provider::Provenance;
use crate::SourceId;

pub const LAG_POLICY_SCHEMA: &str = "observation_lag_policy/1";

const SOURCE_FIELD: &str = "source";
const PROVENANCE_FIELD: &str = "provenance";
const LAGS_FIELD: &str = "lags";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LagPolicyError {
    #[error("the observation-lag policy is not JSON: {0}")]
    Unparseable(String),
    #[error("the observation-lag policy declares schema {0:?}, not {LAG_POLICY_SCHEMA}")]
    UnknownSchema(String),
    /// Requirement R6. An absent or empty `lags` array read as "this source has no lag" would be
    /// an empty fallback, and the one it hides is the backfill refusal R11 exists for.
    #[error("the observation-lag policy names no lag at all under {LAGS_FIELD}")]
    NoLags,
    #[error("lag entry {at} is unusable: {why}")]
    UnreadableEntry { at: usize, why: String },
    /// Two statements about one population cannot both be the lag it is dated by, and this file
    /// has no rule for choosing between them.
    #[error("the policy names {provenance} of {source_id} twice")]
    StatedTwice {
        source_id: String,
        provenance: &'static str,
    },
}

/// Every measured lag a run may date by, keyed the way architecture.md section 3.1 keys a
/// population: by source AND by provenance, because the same adapter reading today's index and an
/// earlier one are two retrieval models and one lag does not describe both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LagPolicy {
    lags: BTreeMap<(String, &'static str), MeasuredLag>,
    version: FileDigest,
}

impl LagPolicy {
    /// The only constructor, so no second path skips the schema check or the R6 check.
    pub fn read(bytes: &[u8]) -> Result<Self, LagPolicyError> {
        let read: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|why| LagPolicyError::Unparseable(why.to_string()))?;
        let schema = read["schema"].as_str().unwrap_or_default();
        if schema != LAG_POLICY_SCHEMA {
            return Err(LagPolicyError::UnknownSchema(schema.to_owned()));
        }
        let entries = read[LAGS_FIELD]
            .as_array()
            .filter(|entries| !entries.is_empty())
            .ok_or(LagPolicyError::NoLags)?;
        let mut lags = BTreeMap::new();
        for (at, entry) in entries.iter().enumerate() {
            let (key, lag) = entry_of(at, entry)?;
            if lags.insert(key.clone(), lag).is_some() {
                return Err(LagPolicyError::StatedTwice {
                    source_id: key.0,
                    provenance: key.1,
                });
            }
        }
        Ok(Self {
            lags,
            version: FileDigest::of(bytes),
        })
    }

    /// The lag this (source, provenance) is dated by, or `None`. `None` is not a lag of zero: the
    /// caller hands it to [`crate::probed_lag`], which refuses the window.
    #[must_use]
    pub fn lag(&self, source: &SourceId, provenance: Provenance) -> Option<&MeasuredLag> {
        self.lags
            .get(&(source.as_str().to_owned(), provenance.name()))
    }

    /// sha256 of the policy bytes as read, which the run envelope carries so a stored item can be
    /// traced to the probe that dated it.
    #[must_use]
    pub const fn version(&self) -> &FileDigest {
        &self.version
    }
}

fn entry_of(
    at: usize,
    entry: &serde_json::Value,
) -> Result<((String, &'static str), MeasuredLag), LagPolicyError> {
    let unreadable = |why: String| LagPolicyError::UnreadableEntry { at, why };
    let source = entry[SOURCE_FIELD]
        .as_str()
        .ok_or_else(|| unreadable(format!("no {SOURCE_FIELD}")))?;
    let named = entry[PROVENANCE_FIELD]
        .as_str()
        .ok_or_else(|| unreadable(format!("no {PROVENANCE_FIELD}")))?;
    let provenance = provenance_named(named).ok_or_else(|| {
        unreadable(format!(
            "{PROVENANCE_FIELD} {named:?} is not one of the three"
        ))
    })?;
    let lag: MeasuredLag = serde_json::from_value(entry.clone())
        .map_err(|why| unreadable(format!("the four MeasuredLag fields do not read: {why}")))?;
    Ok(((source.to_owned(), provenance.name()), lag))
}

/// The enum's own `name` read backwards, so a policy file cannot name a fourth provenance and a
/// renamed variant breaks both directions at once.
fn provenance_named(named: &str) -> Option<Provenance> {
    [
        Provenance::LiveIngest,
        Provenance::VendorArchive,
        Provenance::BulkDataset,
    ]
    .into_iter()
    .find(|provenance| provenance.name() == named)
}

#[cfg(test)]
mod tests {
    use super::{LagPolicy, LagPolicyError, LAG_POLICY_SCHEMA};
    use crate::provider::Provenance;
    use crate::SourceId;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const MEASURED: &str = r#"{"schema": "observation_lag_policy/1",
      "lags": [{"source": "edgar_press", "provenance": "vendor_archive",
                "probe_id": "edgar_index_write-20261113", "probes": 22, "leaks": 0,
                "seconds": 94922}]}"#;

    #[test]
    fn the_lag_is_looked_up_by_source_and_by_provenance_together() -> TestResult {
        let policy = LagPolicy::read(MEASURED.as_bytes())?;
        let edgar = SourceId::new("edgar_press");
        let Some(archive) = policy.lag(&edgar, Provenance::VendorArchive) else {
            return Err("the policy states this population and did not answer".into());
        };
        assert_eq!(archive.seconds, 94_922);
        assert_eq!(archive.probes, 22);
        // The same source read live is the other population and this file says nothing about it.
        assert_eq!(policy.lag(&edgar, Provenance::LiveIngest), None);
        assert_eq!(
            policy.lag(&SourceId::new("rss"), Provenance::VendorArchive),
            None
        );
        Ok(())
    }

    #[test]
    fn a_policy_naming_no_lag_is_refused_rather_than_read_as_no_lag() {
        let empty = format!(r#"{{"schema": "{LAG_POLICY_SCHEMA}", "lags": []}}"#);
        assert_eq!(
            LagPolicy::read(empty.as_bytes()),
            Err(LagPolicyError::NoLags)
        );
        let absent = format!(r#"{{"schema": "{LAG_POLICY_SCHEMA}"}}"#);
        assert_eq!(
            LagPolicy::read(absent.as_bytes()),
            Err(LagPolicyError::NoLags)
        );
    }

    #[test]
    fn an_unknown_schema_is_refused_before_any_entry_is_read() {
        let wrong = r#"{"schema": "calendar/1", "lags": [{"source": "edgar_press"}]}"#;
        assert_eq!(
            LagPolicy::read(wrong.as_bytes()),
            Err(LagPolicyError::UnknownSchema("calendar/1".to_owned()))
        );
    }

    #[test]
    fn one_population_stated_twice_is_refused() {
        let twice = MEASURED.replace(
            "}]}",
            r#"}, {"source": "edgar_press", "provenance": "vendor_archive",
                  "probe_id": "other", "probes": 1, "leaks": 0, "seconds": 1}]}"#,
        );
        assert_eq!(
            LagPolicy::read(twice.as_bytes()),
            Err(LagPolicyError::StatedTwice {
                source_id: "edgar_press".to_owned(),
                provenance: "vendor_archive",
            })
        );
    }

    #[test]
    fn a_provenance_outside_the_enum_is_refused_and_not_dropped() -> TestResult {
        let fourth = MEASURED.replace("vendor_archive", "someone_elses_archive");
        let Err(LagPolicyError::UnreadableEntry { at, why }) = LagPolicy::read(fourth.as_bytes())
        else {
            return Err("a fourth provenance was accepted or dropped".into());
        };
        assert_eq!(at, 0);
        assert!(why.contains("someone_elses_archive"), "{why}");
        Ok(())
    }

    #[test]
    fn an_entry_missing_a_measured_lag_field_is_refused() -> TestResult {
        let no_probes = MEASURED.replace(r#""probes": 22,"#, "");
        let Err(LagPolicyError::UnreadableEntry { at, why }) =
            LagPolicy::read(no_probes.as_bytes())
        else {
            return Err("an entry with no probe count was accepted".into());
        };
        assert_eq!(at, 0);
        assert!(why.contains("probes"), "{why}");
        Ok(())
    }

    #[test]
    fn the_version_is_the_digest_of_the_bytes_as_read() -> TestResult {
        let policy = LagPolicy::read(MEASURED.as_bytes())?;
        assert_eq!(
            policy.version(),
            &canon::FileDigest::of(MEASURED.as_bytes())
        );
        // One byte of whitespace is a different file and therefore a different version.
        let spaced = format!("{MEASURED} ");
        let other = LagPolicy::read(spaced.as_bytes())?;
        assert_ne!(policy.version(), other.version());
        Ok(())
    }
}
