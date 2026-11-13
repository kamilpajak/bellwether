//! `newsvault ledger` over the hand-written store under `tests/fixtures/ledger/`. Every test
//! runs the binary, because the exit status is half of what this command answers.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use news_provider::RunId;
use newsvault::store::{Committer, COMMIT_LOG, RUNS_DIR};
use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

/// Past every instant the fixture holds, so an age is a positive number and `--at` is the only
/// clock in the answer.
const AT: &str = "2026-07-31T00:00:00Z";
const FIRST_DAY: &str = "2026-07-01";
const LAST_DAY_OF_THE_GOLDEN: &str = "2026-07-05";
const LAST_DAY: &str = "2026-07-30";
/// The fixture's one run past `AT`.
const AFTER_THE_CUTOFF: &str = "2026-08-01";
const DAYS_IN_THE_MONTH_RANGE: usize = 30;
const EMPTY_DAY: &str = "2026-07-02";
const FAILED_DAY: &str = "2026-07-03";
const ZERO_ITEM_DAY: &str = "2026-07-04";
const TRUNCATED_ENVELOPE: &str = "run-0705-a.json";
/// JSON that stops mid-object, which is what a kill during the envelope rewrite leaves.
const TRUNCATED_JSON: &str = "{\"schema\":\"newsvault.run/1\",\"run_id\":\"run-0705";
const WRONG_SCHEMA_ENVELOPE: &str = "run-0706-z.json";
/// Parses, and is not what the contract says - the other half of the registry's two input codes.
const WRONG_SCHEMA_JSON: &str = "{\"schema\":\"newsvault.run/2\",\"run_id\":\"run-0706-z\",\
\"run_date\":\"2026-07-06\",\"expected_runs\":1,\"started_at\":\"2026-07-06T00:00:00Z\",\
\"finished_at\":null}";
const EDGAR: &str = "edgar_press";
const POLYGON: &str = "polygon";

const EXIT_OK: i32 = 0;
const EXIT_NO: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_LEASE_HELD: i32 = 7;
const EXIT_INCOMPLETE: i32 = 8;

/// Computed OUTSIDE this tree from architecture.md section 2's form on 2026-11-13:
/// `sha256("result_digest" + 0x1f + ids joined by 0x1e)` over the byte-sorted deduplicated id
/// set, in Python. A digest this crate also produced would only agree with itself.
const DIGEST_OF_A_AND_B: &str = "e634e6f6d420f7f9b13d6de957989d2acbc7bf041637c366eb066064cb3ccc6c";
const DIGEST_OF_C: &str = "edab74c15466efba273e3663b16653b1bc3ebf329d22e4eedb2700c26748e51e";

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ledger")
}

/// A store holding the fixture. The schema comes from the writer rather than from the fixture
/// file, so the two cannot drift apart.
fn fixture() -> Result<TempDir, Box<dyn Error>> {
    let root = TempDir::new()?;
    let store = Committer::open(root.path(), &RunId::new("fixture"))?;
    drop(store);
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    db.execute_batch(&fs::read_to_string(fixture_dir().join("store.sql"))?)?;
    drop(db);
    let runs = root.path().join(RUNS_DIR);
    fs::create_dir_all(&runs)?;
    for entry in fs::read_dir(fixture_dir().join("runs"))? {
        let entry = entry?;
        fs::copy(entry.path(), runs.join(entry.file_name()))?;
    }
    Ok(root)
}

struct Answer {
    code: i32,
    stdout: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Result<Value, Box<dyn Error>> {
        Ok(serde_json::from_slice(&self.stdout)?)
    }
}

fn run(root: &TempDir, args: &[&str]) -> Result<Answer, Box<dyn Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("ledger")
        .args(["--store", &root.path().to_string_lossy()])
        .args(["--at", AT])
        .args(["--format", "json"])
        .args(args)
        .output()?;
    Ok(Answer {
        code: output.status.code().unwrap_or(-1),
        stdout: output.stdout,
    })
}

fn over(root: &TempDir, from: &str, to: &str) -> Result<Answer, Box<dyn Error>> {
    run(root, &["--from", from, "--to", to])
}

fn day<'a>(report: &'a Value, run_date: &str) -> Option<&'a Value> {
    report
        .get("days")?
        .as_array()?
        .iter()
        .find(|day| day.get("run_date").and_then(Value::as_str) == Some(run_date))
}

fn per_source<'a>(report: &'a Value, run_date: &str, source: &str) -> Option<&'a Value> {
    day(report, run_date)?
        .get("per_source")?
        .as_array()?
        .iter()
        .find(|entry| entry.get("source").and_then(Value::as_str) == Some(source))
}

fn codes(report: &Value) -> Vec<String> {
    report
        .get("check")
        .and_then(|check| check.get("findings"))
        .and_then(Value::as_array)
        .map(|findings| {
            findings
                .iter()
                .filter_map(|finding| finding.get("code").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn three_runs_observed_against_six_expected_reads_as_a_shortfall_and_not_a_quiet_day() -> TestResult
{
    let root = fixture()?;
    let answer = over(&root, FIRST_DAY, FIRST_DAY)?;
    assert_eq!(answer.code, EXIT_OK);
    let report = answer.json()?;
    let first = day(&report, FIRST_DAY).ok_or("the day is missing")?;
    assert_eq!(first.get("runs_observed"), Some(&Value::from(3)));
    assert_eq!(first.get("runs_expected"), Some(&Value::from(6)));
    assert_eq!(first.get("runs_shortfall"), Some(&Value::from(3)));
    Ok(())
}

#[test]
fn a_run_with_no_finished_at_and_an_attempt_with_no_answered_are_both_reported() -> TestResult {
    let root = fixture()?;
    let report = over(&root, FIRST_DAY, FIRST_DAY)?.json()?;
    let first = day(&report, FIRST_DAY).ok_or("the day is missing")?;
    assert_eq!(
        first.get("runs_unfinished"),
        Some(&Value::from(vec!["run-0701-1230"]))
    );
    let edgar = per_source(&report, FIRST_DAY, EDGAR).ok_or("the source is missing")?;
    assert_eq!(
        edgar.get("opened_without_answered"),
        Some(&Value::from(vec!["a-0701-edgar-2"]))
    );
    Ok(())
}

#[test]
fn an_attempt_answered_and_never_finalised_is_its_own_counted_field() -> TestResult {
    let root = fixture()?;
    let report = over(&root, FIRST_DAY, FIRST_DAY)?.json()?;
    let edgar = per_source(&report, FIRST_DAY, EDGAR).ok_or("the source is missing")?;
    assert_eq!(
        edgar.get("answered_without_finalised"),
        Some(&Value::from(vec!["a-0701-edgar-3"]))
    );
    Ok(())
}

#[test]
fn a_day_where_every_source_closed_empty_differs_from_one_where_every_source_failed() -> TestResult
{
    let root = fixture()?;
    let answer = run(&root, &["--from", EMPTY_DAY, "--to", FAILED_DAY, "--check"])?;
    let report = answer.json()?;
    for source in [EDGAR, POLYGON] {
        let quiet = per_source(&report, EMPTY_DAY, source).ok_or("the quiet source is missing")?;
        let broken =
            per_source(&report, FAILED_DAY, source).ok_or("the failed source is missing")?;
        assert_eq!(quiet.get("empty"), Some(&Value::from(1)), "{source}");
        assert_eq!(quiet.get("failed"), Some(&Value::from(0)), "{source}");
        assert_eq!(broken.get("empty"), Some(&Value::from(0)), "{source}");
        assert_eq!(broken.get("failed"), Some(&Value::from(1)), "{source}");
    }
    assert_eq!(answer.code, EXIT_NO);
    assert_eq!(codes(&report), vec!["holds_a_failure", "holds_a_failure"]);
    Ok(())
}

#[test]
fn both_clocks_are_present_and_differ_and_neither_is_computed_from_a_wall_clock() -> TestResult {
    let root = fixture()?;
    let report = over(&root, FIRST_DAY, LAST_DAY)?.json()?;
    let freshness = report.get("freshness").ok_or("freshness is missing")?;
    let asking = freshness.get("asking_clock").ok_or("the asking clock")?;
    let publication = freshness
        .get("publication_clock")
        .ok_or("the other clock")?;
    // 2026-07-31T00:00:00Z minus the fixture's newest request (13:00:00 on 07-30) and minus its
    // newest publication value (12:30:00), which differ by exactly half an hour.
    assert_eq!(asking.get("age_seconds"), Some(&Value::from(39_600)));
    assert_eq!(publication.get("age_seconds"), Some(&Value::from(41_400)));
    assert_eq!(
        asking.get("newest"),
        Some(&Value::from("2026-07-30T13:00:00.000000Z"))
    );
    assert_eq!(
        publication.get("newest"),
        Some(&Value::from("2026-07-30T12:30:00.000000Z"))
    );
    Ok(())
}

#[test]
fn two_runs_of_the_same_output_over_one_fixture_are_byte_identical() -> TestResult {
    let root = fixture()?;
    let once = over(&root, FIRST_DAY, LAST_DAY_OF_THE_GOLDEN)?;
    let twice = over(&root, FIRST_DAY, LAST_DAY_OF_THE_GOLDEN)?;
    assert_eq!(once.stdout, twice.stdout);
    assert!(!once.stdout.is_empty());
    Ok(())
}

#[test]
fn the_json_output_over_the_fixture_matches_the_committed_golden_file() -> TestResult {
    let root = fixture()?;
    let answer = over(&root, FIRST_DAY, LAST_DAY_OF_THE_GOLDEN)?;
    assert_eq!(answer.code, EXIT_OK);
    let golden = fs::read_to_string(fixture_dir().join("expected.json"))?;
    assert_eq!(String::from_utf8(answer.stdout)?, golden);
    Ok(())
}

#[test]
fn an_unreadable_run_envelope_is_named_and_counted_rather_than_blanking_the_month() -> TestResult {
    let root = fixture()?;
    fs::write(
        root.path().join(RUNS_DIR).join(TRUNCATED_ENVELOPE),
        TRUNCATED_JSON,
    )?;
    fs::write(
        root.path().join(RUNS_DIR).join(WRONG_SCHEMA_ENVELOPE),
        WRONG_SCHEMA_JSON,
    )?;
    let answer = over(&root, FIRST_DAY, LAST_DAY_OF_THE_GOLDEN)?;
    assert_eq!(answer.code, EXIT_INCOMPLETE);
    assert!(!answer.stdout.is_empty(), "exit 8 still reports");
    let report = answer.json()?;
    let unreadable = report
        .get("unreadable_runs")
        .and_then(Value::as_array)
        .ok_or("unreadable_runs is missing")?;
    assert_eq!(
        unreadable,
        serde_json::json!([
            {"path": format!("{RUNS_DIR}/{TRUNCATED_ENVELOPE}"), "code": "input_unparseable"},
            {"path": format!("{RUNS_DIR}/{WRONG_SCHEMA_ENVELOPE}"), "code": "input_schema_mismatch"},
        ])
        .as_array()
        .ok_or("the expectation is an array")?,
    );
    let orphans = report
        .get("attempts_without_a_run_envelope")
        .and_then(Value::as_array)
        .ok_or("the orphan field is missing")?;
    assert_eq!(
        orphans[0].get("attempt_id"),
        Some(&Value::from("a-0705-edgar-0"))
    );
    Ok(())
}

#[test]
fn a_truncated_envelope_does_not_stop_the_other_twenty_nine_days_being_reported() -> TestResult {
    let root = fixture()?;
    fs::write(
        root.path().join(RUNS_DIR).join(TRUNCATED_ENVELOPE),
        TRUNCATED_JSON,
    )?;
    let answer = over(&root, FIRST_DAY, LAST_DAY)?;
    let report = answer.json()?;
    let days = report
        .get("days")
        .and_then(Value::as_array)
        .ok_or("days is missing")?;
    assert_eq!(days.len(), DAYS_IN_THE_MONTH_RANGE);
    assert_eq!(
        day(&report, LAST_DAY)
            .and_then(|last| last.get("runs_observed"))
            .and_then(Value::as_u64),
        Some(1)
    );
    Ok(())
}

#[test]
fn distinct_result_digests_for_one_source_and_window_are_reported_with_their_attempt_ids(
) -> TestResult {
    let root = fixture()?;
    let report = over(&root, FIRST_DAY, FIRST_DAY)?.json()?;
    let drift = report
        .get("result_digest_drift")
        .and_then(Value::as_array)
        .ok_or("the drift field is missing")?;
    assert_eq!(drift.len(), 1, "{drift:?}");
    assert_eq!(drift[0].get("source"), Some(&Value::from(EDGAR)));
    assert_eq!(drift[0].get("target_window"), Some(&Value::from(FIRST_DAY)));
    assert_eq!(drift[0].get("distinct"), Some(&Value::from(2)));
    assert_eq!(
        drift[0].get("attempts"),
        Some(&serde_json::json!([
            {"attempt_id": "a-0701-edgar-0", "result_digest": DIGEST_OF_A_AND_B},
            {"attempt_id": "a-0701-edgar-1", "result_digest": DIGEST_OF_C},
        ]))
    );
    Ok(())
}

#[test]
fn check_exits_one_on_a_shortfall_and_zero_when_every_day_meets_its_expected_runs() -> TestResult {
    let root = fixture()?;
    let short = run(&root, &["--from", FIRST_DAY, "--to", FIRST_DAY, "--check"])?;
    assert_eq!(short.code, EXIT_NO);
    assert_eq!(codes(&short.json()?), vec!["runs_below_expected"]);
    let met = run(&root, &["--from", EMPTY_DAY, "--to", EMPTY_DAY, "--check"])?;
    assert_eq!(met.code, EXIT_OK);
    assert!(codes(&met.json()?).is_empty());
    assert!(!met.stdout.is_empty(), "a quiet day is still a result");
    Ok(())
}

#[test]
fn check_exits_one_when_a_source_returns_zero_items_against_a_non_zero_vendor_row_count(
) -> TestResult {
    let root = fixture()?;
    let answer = run(
        &root,
        &["--from", ZERO_ITEM_DAY, "--to", ZERO_ITEM_DAY, "--check"],
    )?;
    assert_eq!(answer.code, EXIT_NO);
    let report = answer.json()?;
    assert_eq!(
        codes(&report),
        vec!["rows_committed_zero_against_vendor_rows"]
    );
    let edgar = per_source(&report, ZERO_ITEM_DAY, EDGAR).ok_or("the source is missing")?;
    assert_eq!(edgar.get("vendor_rows"), Some(&Value::from(59)));
    assert_eq!(edgar.get("rows_committed"), Some(&Value::from(0)));
    Ok(())
}

#[test]
fn a_limited_ledger_states_matched_and_returned_and_truncated() -> TestResult {
    let root = fixture()?;
    let whole = over(&root, FIRST_DAY, LAST_DAY_OF_THE_GOLDEN)?.json()?;
    assert_eq!(whole.get("matched"), Some(&Value::from(8)));
    assert_eq!(whole.get("returned"), Some(&Value::from(8)));
    assert_eq!(whole.get("truncated"), Some(&Value::from(false)));
    let capped = run(
        &root,
        &[
            "--from",
            FIRST_DAY,
            "--to",
            LAST_DAY_OF_THE_GOLDEN,
            "--limit",
            "1",
        ],
    )?
    .json()?;
    assert_eq!(capped.get("matched"), Some(&Value::from(8)));
    assert_eq!(capped.get("returned"), Some(&Value::from(5)));
    assert_eq!(capped.get("truncated"), Some(&Value::from(true)));
    assert_eq!(
        capped.get("days").and_then(Value::as_array).map(Vec::len),
        Some(5),
        "a cap bounds the rows within a day and never the days"
    );
    Ok(())
}

#[test]
fn an_unpublished_seq_range_is_reported_by_the_ledger_and_exported_by_the_next_run() -> TestResult {
    let root = fixture()?;
    let before = over(&root, FIRST_DAY, LAST_DAY)?.json()?;
    // `seq` 6 is absent from the fixture, so the spans break across it rather than claiming it.
    assert_eq!(
        before.get("unpublished_seq_ranges"),
        Some(&serde_json::json!([
            {"seq_lo": 1, "seq_hi": 5},
            {"seq_lo": 7, "seq_hi": 10},
        ]))
    );
    let exported = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("publish")
        .args(["--store", &root.path().to_string_lossy()])
        .args(["--at", AT])
        .args(["--format", "json"])
        .output()?;
    assert_eq!(exported.status.code(), Some(EXIT_OK));
    let after = over(&root, FIRST_DAY, LAST_DAY)?.json()?;
    assert_eq!(
        after.get("unpublished_seq_ranges"),
        Some(&serde_json::json!([]))
    );
    Ok(())
}

/// A cap reports fewer rows; it may not change the answer. The finding this day carries is a
/// per-source one, so a cap applied before the check would silence it.
#[test]
fn a_cap_does_not_hide_a_check_finding() -> TestResult {
    let root = fixture()?;
    let answer = run(
        &root,
        &[
            "--from",
            ZERO_ITEM_DAY,
            "--to",
            ZERO_ITEM_DAY,
            "--check",
            "--limit",
            "0",
        ],
    )?;
    assert_eq!(answer.code, EXIT_NO);
    assert_eq!(
        codes(&answer.json()?),
        vec!["rows_committed_zero_against_vendor_rows"]
    );
    Ok(())
}

/// The fixture holds a run and an attempt dated after `AT`. A reader that ignored the cutoff
/// would report them, which is the half-question this repository exists to refuse.
#[test]
fn a_run_after_the_cutoff_is_in_no_day_of_the_answer() -> TestResult {
    let root = fixture()?;
    let report = over(&root, AFTER_THE_CUTOFF, AFTER_THE_CUTOFF)?.json()?;
    let august = day(&report, AFTER_THE_CUTOFF).ok_or("the day is missing")?;
    assert_eq!(august.get("runs_observed"), Some(&Value::from(0)));
    assert_eq!(
        august.get("per_source"),
        Some(&Value::from(Vec::<Value>::new()))
    );
    Ok(())
}

#[test]
fn a_source_filter_leaves_only_that_sources_rows() -> TestResult {
    let root = fixture()?;
    let report = run(
        &root,
        &["--from", FIRST_DAY, "--to", FIRST_DAY, "--source", POLYGON],
    )?
    .json()?;
    assert!(per_source(&report, FIRST_DAY, EDGAR).is_none());
    assert!(per_source(&report, FIRST_DAY, POLYGON).is_some());
    assert_eq!(report.get("matched"), Some(&Value::from(1)));
    Ok(())
}

/// Exit 7's first member in this tree, and the only retryable one: a run holding the flock makes
/// a later run the right answer rather than an error (accepted attack A3.13).
#[test]
fn publishing_while_another_run_holds_the_lease_exits_seven() -> TestResult {
    let root = fixture()?;
    let held = Committer::open(root.path(), &RunId::new("a-run-already-going"))?;
    let output = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("publish")
        .args(["--store", &root.path().to_string_lossy()])
        .args(["--at", AT])
        .output()?;
    drop(held);
    assert_eq!(output.status.code(), Some(EXIT_LEASE_HELD));
    assert!(output.stdout.is_empty());
    Ok(())
}

/// Decision D1's other half. Before it, every non-usage failure of this binary landed on `1`,
/// which is the exit the `--check` answer now owns - architecture.md section 5.1's three bold
/// cells. Measured the same way they were: a 7-byte file at a day partition's path.
#[test]
fn a_day_whose_parquet_cannot_be_read_exits_two_and_never_one() -> TestResult {
    let root = TempDir::new()?;
    let partition = root.path().join(format!("session_date={FIRST_DAY}"));
    fs::create_dir_all(&partition)?;
    fs::write(partition.join("run=000.parquet"), [0_u8; 7])?;
    let output = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .args(["day", FIRST_DAY])
        .args(["--lake", &root.path().to_string_lossy()])
        .args(["--at", AT])
        .output()?;
    assert_eq!(output.status.code(), Some(EXIT_USAGE));
    assert!(output.stdout.is_empty());
    Ok(())
}

#[test]
fn a_ledger_without_at_is_a_usage_error() -> TestResult {
    let root = fixture()?;
    let output = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("ledger")
        .args(["--store", &root.path().to_string_lossy()])
        .args(["--from", FIRST_DAY, "--to", FIRST_DAY])
        .output()?;
    assert_eq!(output.status.code(), Some(EXIT_USAGE));
    assert!(output.stdout.is_empty());
    Ok(())
}
