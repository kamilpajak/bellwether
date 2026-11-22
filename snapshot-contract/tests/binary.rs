//! The `snapshot-contract` binary, run the way a reader outside Rust runs it: the exit status is
//! the verdict, stdout carries exactly one JSON value, and `schema` names every finding code.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs::File;
use std::path::Path;
use std::process::{Command, Output};

use canon::FileDigest;
use serde_json::Value;
use snapshot_contract::{
    cutoff_text, file_sha256, snapshot_id_of, DataWriter, Manifest, SnapshotRow, DATA_FILE,
    FINDING_CODES, MANIFEST_FILE, MANIFEST_SCHEMA, PROVENANCES,
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const CUTOFF: &str = "2026-07-05T00:00:00.000000Z";
const BODY: &str = "synthetic body";

fn run(args: &[&str]) -> TestResult<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_snapshot-contract"))
        .args(args)
        .output()?)
}

/// One row, written through the public writer as `newsvault cut` writes it.
fn sound_pair(dir: &Path) -> TestResult {
    let row = SnapshotRow {
        seq: 1,
        item_id: "item-1".to_owned(),
        story_id: "story-1".to_owned(),
        doc_id: None,
        source: "open_web_wires".to_owned(),
        provenance: PROVENANCES[0].to_owned(),
        valid_at: Some("2026-07-01T13:30:00.000000Z".to_owned()),
        valid_minute: None,
        valid_day: None,
        valid_tz: None,
        observed_at: "2026-07-01T13:31:00.000000Z".to_owned(),
        body_blob: Some(FileDigest::of(BODY.as_bytes()).hex().to_owned()),
        body: Some(BODY.to_owned()),
    };
    let cutoff = CUTOFF.parse()?;
    let digest = row.digest()?;
    let id = snapshot_id_of(cutoff, &[digest.hex()]).map_err(|why| why.to_string())?;
    let mut writer = DataWriter::create(File::create_new(dir.join(DATA_FILE))?, id.hex())?;
    writer.push(row)?;
    writer.finish()?;
    let mut counts: BTreeMap<String, u64> = PROVENANCES
        .iter()
        .map(|name| ((*name).to_owned(), 0))
        .collect();
    counts.insert(PROVENANCES[0].to_owned(), 1);
    let manifest = Manifest {
        schema: MANIFEST_SCHEMA.to_owned(),
        snapshot_id: id.hex().to_owned(),
        cutoff: cutoff_text(cutoff),
        seq_high_water: 1,
        items: 1,
        provenance_counts: counts,
        data_sha256: file_sha256(&dir.join(DATA_FILE))?.0,
    };
    std::fs::write(
        dir.join(MANIFEST_FILE),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(())
}

fn verdict(dir: &Path) -> TestResult<(Option<i32>, Option<bool>)> {
    let output = run(&["validate", &dir.display().to_string(), "--format", "json"])?;
    let report: Value = serde_json::from_slice(&output.stdout)?;
    Ok((output.status.code(), report["admissible"].as_bool()))
}

#[test]
fn the_exit_status_is_the_verdict_and_a_missing_directory_is_not_one() -> TestResult {
    let sound = tempfile::TempDir::new()?;
    sound_pair(sound.path())?;
    let empty = tempfile::TempDir::new()?;
    let absent = run(&[
        "validate",
        &empty.path().join("absent").display().to_string(),
    ])?;
    assert_eq!(verdict(sound.path())?, (Some(0), Some(true)));
    assert_eq!(verdict(empty.path())?, (Some(1), Some(false)));
    assert_eq!(
        (absent.status.code(), absent.stdout.is_empty()),
        (Some(4), true)
    );
    Ok(())
}

#[test]
fn schema_names_every_finding_code_and_the_current_rule() -> TestResult {
    let output = run(&["schema", "--format", "json"])?;
    let schema: Value = serde_json::from_slice(&output.stdout)?;
    let codes: Vec<&str> = schema["finding_codes"]
        .as_array()
        .ok_or("no finding codes")?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(codes, FINDING_CODES);
    assert_eq!(
        schema["data_rules"][0]["data_schema"].as_str(),
        Some("snapshot.data/1")
    );
    Ok(())
}
