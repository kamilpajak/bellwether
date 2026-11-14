//! The archive-reading parity gate (memo §5.1): this reader against `probes/newsvault_parity.py`,
//! over the sealed corpus the lake could not provide. Section 10 of architecture.md records what
//! the 156-of-156 agreement does and does not say; the probe's `PREREGISTRATION.md` is the
//! decision rule, and `probes/newsvault_parity/expected.json` is its committed output, which this
//! file is the only reader of. Nothing here runs Python: the expectation is committed, so CI needs no interpreter.

use std::error::Error;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use newsvault::lake::{LakeError, LakeReader};
use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

/// The five shapes the archive holds none of, named here because the gate is only as good as the
/// corpus: a shape missing from both the lake and this list is still unchecked.
const SHAPES_THE_LAKE_LACKS: [&str; 5] = [
    "2026-01-01", // a null id
    "2026-01-02", // an id that is a real empty string, and a blank one
    "2026-01-03", // a null publication time
    "2026-01-04", // a null ingest time
    "2026-01-05", // an ingest time before the publication value, as an instant and as a day
];
/// The one shape that is a file and not a row.
const A_RUN_FILE_MISSING_A_COLUMN: &str = "2026-01-09";
const MISSING_COLUMN: &str = "tickers";

fn probe_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../probes/newsvault_parity")
}

fn expected() -> Result<Vec<Value>, Box<dyn Error>> {
    let text = std::fs::read_to_string(probe_dir().join("expected.json"))?;
    let doc: Value = serde_json::from_str(&text)?;
    Ok(doc["days"].as_array().cloned().unwrap_or_default())
}

fn day_of(date: &str) -> Result<newsvault::lake::Day, LakeError> {
    LakeReader::new(probe_dir().join("lake")).day(
        date.parse::<NaiveDate>()
            .unwrap_or(NaiveDate::from_yo_opt(2026, 1).unwrap_or_default()),
    )
}

fn text(of: &Value, key: &str) -> String {
    of[key].as_str().unwrap_or_default().to_owned()
}

fn number(of: &Value, key: &str) -> u64 {
    of[key].as_u64().unwrap_or(u64::MAX)
}

/// Every field of every day, the two digests included. The probe computed the right-hand side
/// from the same committed parquet files and from the rules written out in its own docstring.
#[test]
fn the_rust_digests_match_the_probes_committed_expected_output_for_the_sealed_fixture() -> TestResult
{
    let days = expected()?;
    assert!(!days.is_empty(), "the committed expectation is empty");
    for day in &days {
        let date = text(day, "date");
        if !day["refused"].is_null() {
            let Err(why) = day_of(&date) else {
                return Err(
                    format!("{date}: the probe refused this day and this reader did not").into(),
                );
            };
            assert!(
                matches!(&why, LakeError::MissingColumn(column) if *column == text(day, "column")),
                "{date}: {why:?}"
            );
            continue;
        }
        let read = day_of(&date)?;
        assert_eq!(read.runs.len() as u64, number(day, "runs"), "{date} runs");
        assert_eq!(read.raw_rows as u64, number(day, "raw_rows"), "{date} raw");
        assert_eq!(
            read.items.len() as u64,
            number(day, "distinct_items"),
            "{date} items"
        );
        assert_eq!(
            read.unusable_rows as u64,
            number(day, "unusable_rows"),
            "{date} unusable"
        );
        assert_eq!(
            read.valid_time_conflict_events as u64,
            number(day, "valid_time_conflict_events"),
            "{date} conflict events"
        );
        assert_eq!(
            read.items_with_conflicting_valid_time as u64,
            number(day, "items_with_conflicting_valid_time"),
            "{date} conflicting items"
        );
        assert_eq!(read.id_digest(), text(day, "id_digest"), "{date} id digest");
        assert_eq!(
            read.content_digest(),
            text(day, "content_digest"),
            "{date} content digest"
        );
    }
    Ok(())
}

/// Loud means counted or refused, never dropped in silence. Each row shape leaves its partition
/// with a non-zero `unusable_rows` and one good item beside it, so a reader cannot pass this by
/// returning nothing at all; the file shape refuses the whole day.
#[test]
fn a_null_id_an_empty_id_a_null_publication_time_an_inverted_row_and_a_missing_column_each_disagree_loudly(
) -> TestResult {
    for date in SHAPES_THE_LAKE_LACKS {
        let day = day_of(date)?;
        assert!(day.unusable_rows > 0, "{date} counted no unusable row");
        assert_eq!(
            day.raw_rows,
            day.unusable_rows + day.items.len(),
            "{date}: every raw row is either an item or a counted refusal"
        );
        assert!(!day.items.is_empty(), "{date} kept no good row beside it");
    }
    let Err(why) = day_of(A_RUN_FILE_MISSING_A_COLUMN) else {
        return Err("a run file missing a column was read rather than refused".into());
    };
    assert!(
        matches!(&why, LakeError::MissingColumn(column) if *column == MISSING_COLUMN),
        "{why:?}"
    );
    Ok(())
}

/// The field the comparator reads. Section 10 recorded "`newsvault` emits no digest field" as one
/// of the three reasons the gate did not exist.
#[test]
fn the_day_envelope_on_stdout_carries_both_digests() -> TestResult {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .args(["day", "2026-01-08", "--format", "json", "--lake"])
        .arg(probe_dir().join("lake"))
        .output()?;
    assert!(out.status.success(), "{:?}", out.status);
    let envelope: Value = serde_json::from_slice(&out.stdout)?;
    let day = day_of("2026-01-08")?;
    assert_eq!(text(&envelope, "id_digest"), day.id_digest());
    assert_eq!(text(&envelope, "content_digest"), day.content_digest());
    Ok(())
}
