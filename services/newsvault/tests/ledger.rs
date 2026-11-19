//! `newsvault ledger` over the hand-written store under `tests/fixtures/ledger/`. Every test
//! runs the binary, because the exit status is half of what this command answers.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use news_provider::RunId;
use newsvault::store::{Committer, COMMIT_LOG, RUNS_DIR, SCHEMA_VERSION};
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

/// The watchdog's rendering (`--format prometheus`) exits as the JSON one does and carries the
/// same finding, on stdout, under its schema line.
#[test]
fn the_prometheus_rendering_exits_as_json_does_and_counts_the_same_finding() -> TestResult {
    let root = fixture()?;
    let output = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("ledger")
        .args(["--store", &root.path().to_string_lossy()])
        .args(["--at", AT, "--format", "prometheus", "--check"])
        .args(["--from", ZERO_ITEM_DAY, "--to", ZERO_ITEM_DAY])
        .output()?;
    let text = String::from_utf8(output.stdout)?;
    let finding = text
        .lines()
        .find(|line| line.starts_with("newsvault_check_findings{"))
        .map(str::to_owned);
    assert_eq!(
        (
            output.status.code(),
            text.lines().next().map(str::to_owned),
            finding.map(
                |line| line.contains("code=\"rows_committed_zero_against_vendor_rows\"")
                    && line.ends_with(" 1")
            ),
        ),
        (
            Some(EXIT_NO),
            Some(format!("# {}", newsvault::cmd_ledger::PROMETHEUS_SCHEMA)),
            Some(true),
        )
    );
    Ok(())
}

/// Section 11.1: `held` is subtracted, so 58 of 59 held still leaves one row nothing answered, and
/// the finding names both numbers. Mutation 2026-11-15, any non-zero `held` suppressing the
/// finding: red.
#[test]
fn check_still_flags_zero_rows_when_held_covers_only_part_of_the_vendor_rows() -> TestResult {
    let root = fixture()?;
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    db.execute(
        "UPDATE attempts SET held = 58 WHERE attempt_id = 'a-0704-edgar-0'",
        [],
    )?;
    drop(db);
    let answer = run(
        &root,
        &["--from", ZERO_ITEM_DAY, "--to", ZERO_ITEM_DAY, "--check"],
    )?;
    let report = answer.json()?;
    let details: Vec<&str> = report
        .get("check")
        .and_then(|check| check.get("findings"))
        .and_then(Value::as_array)
        .map(|findings| {
            findings
                .iter()
                .filter_map(|finding| finding.get("detail").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let held = per_source(&report, ZERO_ITEM_DAY, EDGAR).and_then(|edgar| edgar.get("held"));
    assert_eq!(
        (answer.code, details, held),
        (
            EXIT_NO,
            vec!["59 vendor rows, 58 held, and no item row committed"],
            Some(&Value::from(58))
        )
    );
    Ok(())
}

/// The human renderer carries `held` as the JSON row does. Mutation 2026-11-15, `held` dropped
/// from `render_human`: red.
#[test]
fn the_human_ledger_states_the_held_count_of_the_json_row() -> TestResult {
    let root = fixture()?;
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    db.execute(
        "UPDATE attempts SET held = 58 WHERE attempt_id = 'a-0704-edgar-0'",
        [],
    )?;
    drop(db);
    let json = over(&root, ZERO_ITEM_DAY, ZERO_ITEM_DAY)?.json()?;
    let held = per_source(&json, ZERO_ITEM_DAY, EDGAR)
        .and_then(|edgar| edgar.get("held"))
        .and_then(Value::as_i64)
        .ok_or("the JSON row has no held")?;
    let human = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("ledger")
        .args(["--store", &root.path().to_string_lossy()])
        .args(["--at", AT, "--format", "human"])
        .args(["--from", ZERO_ITEM_DAY, "--to", ZERO_ITEM_DAY])
        .output()?
        .stdout;
    let line = String::from_utf8(human)?
        .lines()
        .find(|line| line.trim_start().starts_with(EDGAR))
        .map(str::to_owned)
        .ok_or("no human line names the source")?;
    let word_after = |name: &str| {
        let words: Vec<&str> = line.split_whitespace().collect();
        words
            .iter()
            .position(|word| *word == name)
            .and_then(|at| words.get(at + 1))
            .map(|word| (*word).to_owned())
    };
    assert_eq!((word_after("held"), held), (Some("58".to_owned()), 58));
    Ok(())
}

/// The human renderer carries `listing_unusable` as the JSON row does. Mutation 2026-11-15, the
/// field dropped from `render_human`: red.
#[test]
fn the_human_ledger_states_the_listing_unusable_count_of_the_json_row() -> TestResult {
    let root = fixture()?;
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    db.execute(
        "UPDATE attempts SET listing_unusable = 7 WHERE attempt_id = 'a-0704-edgar-0'",
        [],
    )?;
    drop(db);
    let json = over(&root, ZERO_ITEM_DAY, ZERO_ITEM_DAY)?.json()?;
    let counted = per_source(&json, ZERO_ITEM_DAY, EDGAR)
        .and_then(|edgar| edgar.get("listing_unusable"))
        .and_then(Value::as_i64)
        .ok_or("the JSON row has no listing_unusable")?;
    let human = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("ledger")
        .args(["--store", &root.path().to_string_lossy()])
        .args(["--at", AT, "--format", "human"])
        .args(["--from", ZERO_ITEM_DAY, "--to", ZERO_ITEM_DAY])
        .output()?
        .stdout;
    let line = String::from_utf8(human)?
        .lines()
        .find(|line| line.trim_start().starts_with(EDGAR))
        .map(str::to_owned)
        .ok_or("no human line names the source")?;
    let words: Vec<&str> = line.split_whitespace().collect();
    let shown = words
        .iter()
        .position(|word| *word == "listing_unusable")
        .and_then(|at| words.get(at + 1))
        .map(|word| (*word).to_owned());
    assert_eq!((shown, counted), (Some("7".to_owned()), 7));
    Ok(())
}

/// The human renderer carries `robots_disallowed` as the JSON row does. Mutation 2026-11-17, the
/// field printed as 0 in `render_human`: red.
#[test]
fn the_human_ledger_states_the_robots_disallowed_count_of_the_json_row() -> TestResult {
    let root = fixture()?;
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    db.execute(
        "UPDATE attempts SET robots_disallowed = 5 WHERE attempt_id = 'a-0704-edgar-0'",
        [],
    )?;
    drop(db);
    let json = over(&root, ZERO_ITEM_DAY, ZERO_ITEM_DAY)?.json()?;
    let counted = per_source(&json, ZERO_ITEM_DAY, EDGAR)
        .and_then(|edgar| edgar.get("robots_disallowed"))
        .and_then(Value::as_i64)
        .ok_or("the JSON row has no robots_disallowed")?;
    let human = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("ledger")
        .args(["--store", &root.path().to_string_lossy()])
        .args(["--at", AT, "--format", "human"])
        .args(["--from", ZERO_ITEM_DAY, "--to", ZERO_ITEM_DAY])
        .output()?
        .stdout;
    let line = String::from_utf8(human)?
        .lines()
        .find(|line| line.trim_start().starts_with(EDGAR))
        .map(str::to_owned)
        .ok_or("no human line names the source")?;
    let words: Vec<&str> = line.split_whitespace().collect();
    let shown = words
        .iter()
        .position(|word| *word == "robots_disallowed")
        .and_then(|at| words.get(at + 1))
        .map(|word| (*word).to_owned());
    assert_eq!((shown, counted), (Some("5".to_owned()), 5));
    Ok(())
}

/// Section 11.2: each `attempts.continuity` value is counted under its own name, in the JSON row
/// and on the human line, and an attempt with none counts nowhere; 1 to 5 of each, so a swap shows.
/// Mutations 2026-11-17, two `SUM`s swapped and the human line printing `complete` twice: each red.
#[test]
fn every_continuity_value_is_counted_under_its_own_name_in_json_and_human() -> TestResult {
    let root = fixture()?;
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    db.execute_batch(
        "CREATE TEMP TABLE copied AS SELECT * FROM attempts WHERE attempt_id = 'a-0701-edgar-3';",
    )?;
    for copy in 4..16 {
        db.execute(
            "UPDATE copied SET attempt_id = ?1",
            [format!("a-0701-edgar-{copy}")],
        )?;
        db.execute("INSERT INTO attempts SELECT * FROM copied", [])?;
    }
    let names = [
        "complete",
        "boundary_no_shared_key",
        "no_previous_key",
        "first_poll",
        "next_page_unread",
    ];
    let mut attempt = 0;
    for (count, name) in (1..).zip(names) {
        for _ in 0..count {
            db.execute(
                "UPDATE attempts SET continuity = ?1 WHERE attempt_id = ?2",
                [name.to_owned(), format!("a-0701-edgar-{attempt}")],
            )?;
            attempt += 1;
        }
    }
    drop(db);
    let day = "2026-07-01";
    let json = over(&root, day, day)?.json()?;
    let counted: Vec<Option<i64>> = names
        .iter()
        .map(|name| {
            per_source(&json, day, EDGAR)
                .and_then(|edgar| edgar.get("continuity"))
                .and_then(|counts| counts.get(*name))
                .and_then(Value::as_i64)
        })
        .collect();
    let human = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("ledger")
        .args(["--store", &root.path().to_string_lossy()])
        .args(["--at", AT, "--format", "human"])
        .args(["--from", day, "--to", day])
        .output()?
        .stdout;
    let line = String::from_utf8(human)?
        .lines()
        .find(|line| line.trim_start().starts_with(EDGAR))
        .map(str::to_owned)
        .ok_or("no human line names the source")?;
    let words: Vec<&str> = line.split_whitespace().collect();
    let shown: Vec<Option<i64>> = names
        .iter()
        .map(|name| {
            words
                .iter()
                .position(|word| *word == format!("continuity_{name}"))
                .and_then(|at| words.get(at + 1))
                .and_then(|word| word.parse().ok())
        })
        .collect();
    let expected = vec![Some(1), Some(2), Some(3), Some(4), Some(5)];
    assert_eq!((counted, shown), (expected.clone(), expected));
    Ok(())
}

/// Section 11.2: each `attempts.boundary_*` count is summed over the day's polls under its own
/// label, in the JSON row and on the human line; distinct sums 11 to 44, so a swap shows.
#[test]
fn every_boundary_generation_is_summed_under_its_own_label_in_json_and_human() -> TestResult {
    let root = fixture()?;
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    for (attempt, scale) in [("a-0701-edgar-0", 1), ("a-0701-edgar-1", 10)] {
        db.execute(
            "UPDATE attempts SET boundary_same_generation = ?2, boundary_older_generation = ?3,
                    boundary_newer_generation = ?4, boundary_generation_unknown = ?5
              WHERE attempt_id = ?1",
            rusqlite::params![attempt, scale, 2 * scale, 3 * scale, 4 * scale],
        )?;
    }
    drop(db);
    let labels = [
        "same_generation",
        "older_generation",
        "newer_generation",
        "generation_unknown",
    ];
    let day = "2026-07-01";
    let json = over(&root, day, day)?.json()?;
    let counted: Vec<Option<i64>> = labels
        .iter()
        .map(|label| {
            per_source(&json, day, EDGAR)
                .and_then(|edgar| edgar.get("boundaries"))
                .and_then(|counts| counts.get(*label))
                .and_then(Value::as_i64)
        })
        .collect();
    let human = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("ledger")
        .args(["--store", &root.path().to_string_lossy()])
        .args(["--at", AT, "--format", "human"])
        .args(["--from", day, "--to", day])
        .output()?
        .stdout;
    let line = String::from_utf8(human)?
        .lines()
        .find(|line| line.trim_start().starts_with(EDGAR))
        .map(str::to_owned)
        .ok_or("no human line names the source")?;
    let words: Vec<&str> = line.split_whitespace().collect();
    let shown: Vec<Option<i64>> = labels
        .iter()
        .map(|label| {
            words
                .iter()
                .position(|word| *word == format!("boundary_{label}"))
                .and_then(|at| words.get(at + 1))
                .and_then(|word| word.parse().ok())
        })
        .collect();
    let expected = vec![Some(11), Some(22), Some(33), Some(44)];
    assert_eq!((counted, shown), (expected.clone(), expected));
    Ok(())
}

/// Section 10: `--check` does not subtract `robots_disallowed`, so a day whose every vendor row
/// robots refused still exits 1. Mutation 2026-11-17, the count subtracted: green to exit 0.
#[test]
fn check_still_flags_zero_rows_when_robots_disallowed_every_vendor_row() -> TestResult {
    let root = fixture()?;
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    db.execute(
        "UPDATE attempts SET robots_disallowed = 59 WHERE attempt_id = 'a-0704-edgar-0'",
        [],
    )?;
    drop(db);
    let answer = run(
        &root,
        &["--from", ZERO_ITEM_DAY, "--to", ZERO_ITEM_DAY, "--check"],
    )?;
    let report = answer.json()?;
    let counted = per_source(&report, ZERO_ITEM_DAY, EDGAR)
        .and_then(|edgar| edgar.get("robots_disallowed"))
        .and_then(Value::as_i64);
    assert_eq!(
        (answer.code, codes(&report), counted),
        (
            EXIT_NO,
            vec!["rows_committed_zero_against_vendor_rows".to_owned()],
            Some(59)
        )
    );
    Ok(())
}

/// P10 (section 3.9): a body stored cut at the limit is counted for its source and day, in the
/// JSON row and on the human line, and an attempt with no link or an uncut one counts nothing.
/// Mutations 2026-11-16, the `SUM` made constant 0: red; the field dropped from `render_human`: red.
#[test]
fn a_body_stored_cut_at_the_limit_is_counted_for_its_source_and_day() -> TestResult {
    let root = fixture()?;
    let json = over(&root, FIRST_DAY, FAILED_DAY)?.json()?;
    let counted = |day: &str, source: &str| {
        per_source(&json, day, source)
            .and_then(|row| row.get("bodies_truncated"))
            .and_then(Value::as_i64)
    };
    let human = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("ledger")
        .args(["--store", &root.path().to_string_lossy()])
        .args(["--at", AT, "--format", "human"])
        .args(["--from", FAILED_DAY, "--to", FAILED_DAY])
        .output()?
        .stdout;
    let line = String::from_utf8(human)?
        .lines()
        .find(|line| line.trim_start().starts_with(EDGAR))
        .map(str::to_owned)
        .ok_or("no human line names the source")?;
    let words: Vec<&str> = line.split_whitespace().collect();
    let shown = words
        .iter()
        .position(|word| *word == "bodies_truncated")
        .and_then(|at| words.get(at + 1))
        .map(|word| (*word).to_owned());
    assert_eq!(
        (
            counted(FAILED_DAY, EDGAR),
            counted(FAILED_DAY, POLYGON),
            counted(FIRST_DAY, EDGAR),
            shown
        ),
        (Some(1), Some(0), Some(0), Some("1".to_owned()))
    );
    Ok(())
}

/// Section 8 row R15, decision 10: a held story re-listed under another minute counts for the
/// source and day of the index attempt it was recorded on, in the JSON row and on the human line.
#[test]
fn a_relisted_story_is_counted_for_the_source_and_day_of_its_index_attempt() -> TestResult {
    let root = fixture()?;
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    db.execute(
        "INSERT INTO relisted VALUES ('a-0703-edgar-0', 'a story', 'listed', 'b', 'a')",
        [],
    )?;
    drop(db);
    let json = over(&root, FIRST_DAY, FAILED_DAY)?.json()?;
    let counted = |day: &str, source: &str| {
        per_source(&json, day, source)
            .and_then(|row| row.get("relisted_disputed"))
            .and_then(Value::as_i64)
    };
    let human = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("ledger")
        .args(["--store", &root.path().to_string_lossy()])
        .args(["--at", AT, "--format", "human"])
        .args(["--from", FAILED_DAY, "--to", FAILED_DAY])
        .output()?
        .stdout;
    let line = String::from_utf8(human)?
        .lines()
        .find(|line| line.trim_start().starts_with(EDGAR))
        .map(str::to_owned)
        .ok_or("no human line names the source")?;
    let words: Vec<&str> = line.split_whitespace().collect();
    let shown = words
        .iter()
        .position(|word| *word == "relisted_disputed")
        .and_then(|at| words.get(at + 1))
        .map(|word| (*word).to_owned());
    assert_eq!(
        (
            counted(FAILED_DAY, EDGAR),
            counted(FAILED_DAY, POLYGON),
            counted(FIRST_DAY, EDGAR),
            shown
        ),
        (Some(1), Some(0), Some(0), Some("1".to_owned()))
    );
    Ok(())
}

/// Decision x (2026-11-16): `relisted_compared` sums `attempts.relisted_compared` for the source
/// and day of its index attempt, in the JSON row and on the human line, so 0 disputed among 3
/// compared differs from nothing compared. Two attempts share one (run, source, outcome) group and
/// a third, interrupted, has its own. Mutations 2026-11-16, the `SUM` made 0: red; made `MAX`: red;
/// the per-source `+=` made a maximum: red; the field dropped from `render_human`: red.
#[test]
fn a_relisted_comparison_count_is_summed_for_the_source_and_day_of_its_index_attempt() -> TestResult
{
    let root = fixture()?;
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    db.execute_batch(
        "UPDATE attempts SET relisted_compared = 3 WHERE attempt_id = 'a-0703-edgar-0';
         UPDATE attempts SET relisted_compared = 4 WHERE attempt_id = 'a-0703-edgar-1';
         CREATE TEMP TABLE twin AS SELECT * FROM attempts WHERE attempt_id = 'a-0703-edgar-0';
         UPDATE twin SET attempt_id = 'a-0703-edgar-2', relisted_compared = 5;
         INSERT INTO attempts SELECT * FROM twin;",
    )?;
    drop(db);
    let json = over(&root, FIRST_DAY, FAILED_DAY)?.json()?;
    let counted = |day: &str, source: &str, field: &str| {
        per_source(&json, day, source)
            .and_then(|row| row.get(field))
            .and_then(Value::as_i64)
    };
    let human = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("ledger")
        .args(["--store", &root.path().to_string_lossy()])
        .args(["--at", AT, "--format", "human"])
        .args(["--from", FAILED_DAY, "--to", FAILED_DAY])
        .output()?
        .stdout;
    let line = String::from_utf8(human)?
        .lines()
        .find(|line| line.trim_start().starts_with(EDGAR))
        .map(str::to_owned)
        .ok_or("no human line names the source")?;
    let words: Vec<&str> = line.split_whitespace().collect();
    let shown = words
        .iter()
        .position(|word| *word == "relisted_compared")
        .and_then(|at| words.get(at + 1))
        .map(|word| (*word).to_owned());
    assert_eq!(
        (
            counted(FAILED_DAY, EDGAR, "relisted_compared"),
            counted(FAILED_DAY, EDGAR, "relisted_disputed"),
            counted(FAILED_DAY, POLYGON, "relisted_compared"),
            counted(FIRST_DAY, EDGAR, "relisted_compared"),
            shown
        ),
        (Some(12), Some(0), Some(0), Some(0), Some("12".to_owned()))
    );
    Ok(())
}

/// Section 3.2 `held`: only a row carrying `vendor_rows` adds its `held`, so an unclosed index
/// row holding 60 cannot outweigh 59 vendor rows. Mutation 2026-11-15, the ledger summing `held`
/// over every row again: red.
#[test]
fn check_still_flags_zero_rows_when_an_unclosed_row_carries_held() -> TestResult {
    let root = fixture()?;
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    db.execute(
        "INSERT INTO attempts (attempt_id, run_id, source, target_window, filter_params,
             opened_at, held, universe_version)
         VALUES ('a-0704-edgar-1', 'run-0704-a', 'edgar_press', '2026-07-04', 'fp-index',
             '2026-07-04T00:34:41.000000Z', 60, 'uv-roster-2026-05-29')",
        [],
    )?;
    drop(db);
    let answer = run(
        &root,
        &["--from", ZERO_ITEM_DAY, "--to", ZERO_ITEM_DAY, "--check"],
    )?;
    let report = answer.json()?;
    let held = per_source(&report, ZERO_ITEM_DAY, EDGAR).and_then(|edgar| edgar.get("held"));
    assert_eq!(
        (answer.code, codes(&report), held),
        (
            EXIT_NO,
            vec!["rows_committed_zero_against_vendor_rows".to_owned()],
            Some(&Value::from(0))
        )
    );
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

/// A read-only reader cannot migrate, so a store at the previous version or an unknown later one
/// is refused by name (exit 2, `input_schema_mismatch`) instead of failing on a missing column.
#[test]
fn a_reader_refuses_a_store_at_another_user_version() -> TestResult {
    let mut answers = Vec::new();
    for version in [SCHEMA_VERSION - 1, SCHEMA_VERSION + 1] {
        let root = fixture()?;
        let db = Connection::open(root.path().join(COMMIT_LOG))?;
        db.pragma_update(None, "user_version", version)?;
        drop(db);
        for command in ["ledger", "items"] {
            let output = Command::new(env!("CARGO_BIN_EXE_newsvault"))
                .arg(command)
                .args(["--store", &root.path().to_string_lossy()])
                .args(["--at", AT])
                .args(["--from", FIRST_DAY, "--to", FIRST_DAY])
                .output()?;
            let named = String::from_utf8_lossy(&output.stderr).contains("user_version");
            answers.push((
                command,
                version,
                output.status.code(),
                output.stdout.len(),
                named,
            ));
        }
    }
    let refused: Vec<_> = answers
        .iter()
        .map(|(command, version, ..)| (*command, *version, Some(EXIT_USAGE), 0, true))
        .collect();
    assert_eq!(answers, refused);
    Ok(())
}

/// S7-D4: `deferred` rows are counted per reason for the source and day of their run, in the JSON
/// row and on the human line. Distinct counts 5 (two runs), 7 and 11 so a swapped or merged source,
/// run or day shows; a `host_held` row and a row deferred after `--at` count nowhere.
#[test]
fn deferred_rows_are_counted_per_reason_for_the_source_and_day_of_their_run() -> TestResult {
    let root = fixture()?;
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    let rows = [
        (
            "run-0701-0034",
            EDGAR,
            2,
            "follow_up_cap",
            "2026-07-01T00:40:00.000000Z",
        ),
        (
            "run-0701-0630",
            EDGAR,
            3,
            "follow_up_cap",
            "2026-07-01T06:40:00.000000Z",
        ),
        (
            "run-0701-0034",
            POLYGON,
            7,
            "follow_up_cap",
            "2026-07-01T00:40:00.000000Z",
        ),
        (
            "run-0702-a",
            EDGAR,
            11,
            "follow_up_cap",
            "2026-07-02T09:40:00.000000Z",
        ),
        (
            "run-0701-1230",
            EDGAR,
            1,
            "host_held",
            "2026-07-01T12:40:00.000000Z",
        ),
        (
            "run-0701-1230",
            EDGAR,
            1,
            "follow_up_cap",
            "2026-08-01T00:00:00.000000Z",
        ),
    ];
    for (run, source, many, reason, at) in rows {
        for nth in 0..many {
            db.execute(
                "INSERT INTO deferred (run_id, source, target_window, host, url, phase, reason,
                                       deferred_at)
                 VALUES (?1, ?2, '2026-07-01', 'example.test', ?3, 'document', ?4, ?5)",
                rusqlite::params![
                    run,
                    source,
                    format!("https://example.test/{nth}"),
                    reason,
                    at
                ],
            )?;
        }
    }
    drop(db);
    let json = over(&root, FIRST_DAY, EMPTY_DAY)?.json()?;
    let counted = |day: &str, source: &str| {
        per_source(&json, day, source)
            .and_then(|row| row.get("deferred"))
            .and_then(|counts| counts.get("follow_up_cap"))
            .and_then(Value::as_i64)
    };
    let human = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("ledger")
        .args(["--store", &root.path().to_string_lossy()])
        .args(["--at", AT, "--format", "human"])
        .args(["--from", FIRST_DAY, "--to", FIRST_DAY])
        .output()?
        .stdout;
    let shown: Vec<Option<i64>> = String::from_utf8(human)?
        .lines()
        .filter(|line| line.starts_with("  "))
        .map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            words
                .iter()
                .position(|word| *word == "deferred_follow_up_cap")
                .and_then(|at| words.get(at + 1))
                .and_then(|word| word.parse().ok())
        })
        .collect();
    assert_eq!(
        (
            counted(FIRST_DAY, EDGAR),
            counted(FIRST_DAY, POLYGON),
            counted(EMPTY_DAY, EDGAR),
            counted(EMPTY_DAY, POLYGON),
            shown,
        ),
        (Some(5), Some(7), Some(11), Some(0), vec![Some(5), Some(7)])
    );
    let edgar = run(
        &root,
        &["--from", FIRST_DAY, "--to", FIRST_DAY, "--source", EDGAR],
    )?
    .json()?;
    let filtered = |source: &str| {
        per_source(&edgar, FIRST_DAY, source).map(|row| row["deferred"]["follow_up_cap"].as_i64())
    };
    assert_eq!((filtered(EDGAR), filtered(POLYGON)), (Some(Some(5)), None));
    Ok(())
}

// ---- S7-D9 and S7-D11 (architecture.md section 11.5 step 7 PR 4) ----

const BUSY_DAY: &str = "2026-07-10";
const OVER_DAY: &str = "2026-07-11";

/// Run files of `BUSY_DAY`: one observed run at exactly half a day of wall time and of one
/// host's busy time, with two capped holds, and one run the lease kept out; and one run of
/// `OVER_DAY` one second over half a day on both.
fn busy_runs(root: &TempDir) -> TestResult {
    let runs = root.path().join(RUNS_DIR);
    let hold = |host: &str| {
        serde_json::json!({"host": host, "retry_after": "315360000",
                           "held_until": "2026-07-11T11:00:00Z"})
    };
    let files = [
        (
            "run-0710-a.json",
            serde_json::json!({"schema": "newsvault.run/1", "run_id": "run-0710-a",
                "run_date": BUSY_DAY, "expected_runs": 2,
                "started_at": "2026-07-10T11:00:00Z", "finished_at": "2026-07-10T11:14:00Z",
                "wall_s": 43_200.0,
                "busy": [{"host": "a.test", "busy_s": 43_200.0}, {"host": "c.test", "busy_s": 7.5}],
                "holds_capped": [hold("a.test"), hold("c.test")]}),
        ),
        (
            "run-0710-b.json",
            serde_json::json!({"schema": "newsvault.run/1", "run_id": "run-0710-b",
                "run_date": BUSY_DAY, "expected_runs": 3,
                "started_at": "2026-07-10T11:00:01Z", "finished_at": "2026-07-10T11:00:01Z",
                "refused": "lease_held"}),
        ),
        (
            "run-0711-a.json",
            serde_json::json!({"schema": "newsvault.run/1", "run_id": "run-0711-a",
                "run_date": OVER_DAY, "expected_runs": 1,
                "started_at": "2026-07-11T11:00:00Z", "finished_at": "2026-07-11T11:14:00Z",
                "wall_s": 43_201.0, "busy": [{"host": "b.test", "busy_s": 43_201.0}]}),
        ),
    ];
    for (name, body) in files {
        fs::write(runs.join(name), serde_json::to_vec(&body)?)?;
    }
    Ok(())
}

/// The lease-held run is left out of the runs observed and still sets its day's runs expected
/// (3, above the observed run's 2); its day's wall time and busy hosts are the observed run's;
/// past half a day, never at it, each bound is a `--check` finding; the human lines agree.
#[test]
fn the_ledger_sums_busy_time_against_half_a_day_and_leaves_lease_held_runs_unobserved() -> TestResult
{
    let root = fixture()?;
    busy_runs(&root)?;
    let answer = run(&root, &["--from", BUSY_DAY, "--to", OVER_DAY, "--check"])?;
    let report = answer.json()?;
    let busy_day = day(&report, BUSY_DAY).ok_or("busy day")?;
    let over_day = day(&report, OVER_DAY).ok_or("over day")?;
    let field = |day: &Value, name: &str| day[name].clone();
    assert_eq!(
        (
            answer.code,
            codes(&report),
            [
                "runs_observed",
                "runs_expected",
                "runs_lease_held",
                "wall_s",
                "wall_share",
                "holds_capped"
            ]
            .map(|name| field(busy_day, name)),
            field(busy_day, "busy"),
            field(over_day, "wall_share"),
            report["busy_bound"].clone(),
        ),
        (
            EXIT_NO,
            vec![
                "runs_below_expected".to_owned(),
                "wall_time_over_bound".to_owned(),
                "host_busy_over_bound".to_owned()
            ],
            [
                Value::from(1),
                Value::from(3),
                Value::from(1),
                Value::from(43_200.0),
                Value::from(0.5),
                Value::from(2)
            ],
            serde_json::json!([
                {"host": "a.test", "busy_s": 43_200.0, "share": 0.5},
                {"host": "c.test", "busy_s": 7.5, "share": 7.5 / 86_400.0}
            ]),
            Value::from(43_201.0 / 86_400.0),
            Value::from(0.5),
        )
    );
    let human = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("ledger")
        .args(["--store", &root.path().to_string_lossy()])
        .args(["--at", AT, "--format", "human"])
        .args(["--from", BUSY_DAY, "--to", BUSY_DAY])
        .output()?
        .stdout;
    let lines: Vec<String> = String::from_utf8(human)?
        .lines()
        .filter(|line| line.starts_with("busy"))
        .map(str::to_owned)
        .collect();
    assert_eq!(
        lines,
        vec![
            "busy             lease_held 1 wall_s 43200 wall_share 0.5 bound 0.5 holds_capped 2"
                .to_owned(),
            "busy_host        a.test busy_s 43200 share 0.5".to_owned(),
            format!(
                "busy_host        c.test busy_s 7.5 share {}",
                7.5 / 86_400.0
            ),
        ]
    );
    Ok(())
}

// ---- S7-D5 and PR #52 answer 6 (architecture.md section 11.5 step 7 PR 5) ----

/// `many` `deferred` rows of `source`, each closed `closed_as` by `run` at `closed_at`.
fn closed(
    db: &Connection,
    run: &str,
    source: &str,
    (closed_as, many): (&str, usize),
    closed_at: &str,
) -> TestResult {
    for _ in 0..many {
        db.execute(
            "INSERT INTO deferred (run_id, source, target_window, url, phase, reason, deferred_at)
             VALUES ('an earlier run', ?1, '2026-06-30', 'https://example.test/', 'document',
                     'run_deadline', '2026-06-30T23:00:00.000000Z')",
            [source],
        )?;
        let attempt = (closed_as == "answered").then_some("an attempt");
        db.execute(
            "INSERT INTO deferred_closed (deferred_id, run_id, closed_as, attempt_id, closed_at)
             VALUES (last_insert_rowid(), ?1, ?2, ?3, ?4)",
            rusqlite::params![run, closed_as, attempt, closed_at],
        )?;
    }
    Ok(())
}

/// S7-D5, S7-D8: closed rows count per closing for the source and day of the closing run (1, 2, 3
/// for EDGAR, 5 expired for the other), not after `--at`; any expired count is a
/// `deferred_expired` finding, a day with none has none, and the human line agrees.
#[test]
fn closed_deferred_rows_are_counted_per_closing_and_an_expired_one_is_a_finding() -> TestResult {
    let root = fixture()?;
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    let first = "2026-07-01T00:40:00.000000Z";
    closed(&db, "run-0701-0034", EDGAR, ("answered", 1), first)?;
    closed(&db, "run-0701-0034", EDGAR, ("held", 2), first)?;
    closed(&db, "run-0701-0630", EDGAR, ("expired", 3), first)?;
    closed(&db, "run-0701-0034", POLYGON, ("expired", 5), first)?;
    closed(
        &db,
        "run-0702-a",
        EDGAR,
        ("answered", 7),
        "2026-07-02T09:40:00.000000Z",
    )?;
    closed(
        &db,
        "run-0701-1230",
        EDGAR,
        ("expired", 1),
        "2026-08-01T00:00:00.000000Z",
    )?;
    drop(db);
    let answer = run(&root, &["--from", FIRST_DAY, "--to", EMPTY_DAY, "--check"])?;
    let report = answer.json()?;
    let counted = |day: &str, source: &str| {
        per_source(&report, day, source).map(|row| row["carried"].clone())
    };
    let expired_of = |report: &Value| -> Vec<(String, String)> {
        report["check"]["findings"]
            .as_array()
            .map_or_else(Vec::new, |findings| {
                findings
                    .iter()
                    .filter(|finding| finding["code"] == "deferred_expired")
                    .map(|finding| {
                        (
                            finding["run_date"].as_str().unwrap_or_default().to_owned(),
                            finding["source"].as_str().unwrap_or_default().to_owned(),
                        )
                    })
                    .collect()
            })
    };
    let expired = expired_of(&report);
    let edgar_only = run(
        &root,
        &[
            "--from", FIRST_DAY, "--to", FIRST_DAY, "--source", EDGAR, "--check",
        ],
    )?
    .json()?;
    let human = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("ledger")
        .args(["--store", &root.path().to_string_lossy()])
        .args(["--at", AT, "--format", "human"])
        .args(["--from", FIRST_DAY, "--to", FIRST_DAY, "--source", EDGAR])
        .output()?
        .stdout;
    let shown = String::from_utf8(human)?
        .lines()
        .find_map(|line| line.split_once("carried_answered"))
        .and_then(|(_, rest)| rest.split_once(" items_new"))
        .map(|(counts, _)| counts.to_owned());
    assert_eq!(
        (
            counted(FIRST_DAY, EDGAR),
            counted(FIRST_DAY, POLYGON),
            counted(EMPTY_DAY, EDGAR),
            expired,
            shown,
            per_source(&edgar_only, FIRST_DAY, POLYGON).is_some(),
            expired_of(&edgar_only),
        ),
        (
            Some(serde_json::json!({"answered": 1, "held": 2, "expired": 3})),
            Some(serde_json::json!({"answered": 0, "held": 0, "expired": 5})),
            Some(serde_json::json!({"answered": 7, "held": 0, "expired": 0})),
            vec![
                (FIRST_DAY.to_owned(), EDGAR.to_owned()),
                (FIRST_DAY.to_owned(), POLYGON.to_owned())
            ],
            Some(" 1 carried_held 2 carried_expired 3".to_owned()),
            false,
            vec![(FIRST_DAY.to_owned(), EDGAR.to_owned())],
        )
    );
    Ok(())
}

const SOURCED_DAY: &str = "2026-07-12";

/// PR #52 answer 6: run files name their source, so `--source` sums only that source's runs'
/// wall and busy time and capped holds; a file naming none counts only with no `--source`.
#[test]
fn the_ledger_filters_busy_time_by_the_run_files_source() -> TestResult {
    let root = fixture()?;
    let runs = root.path().join(RUNS_DIR);
    let hold = serde_json::json!({"host": "b.test", "retry_after": "315360000",
                                  "held_until": "2026-07-13T11:00:00Z"});
    let files = [
        (
            "run-0712-a.json",
            serde_json::json!({"schema": "newsvault.run/1", "run_id": "run-0712-a",
                "run_date": SOURCED_DAY, "source": EDGAR, "expected_runs": 3,
                "started_at": "2026-07-12T11:00:00Z", "finished_at": "2026-07-12T11:14:00Z",
                "wall_s": 11.0, "busy": [{"host": "a.test", "busy_s": 3.0}]}),
        ),
        (
            "run-0712-b.json",
            serde_json::json!({"schema": "newsvault.run/1", "run_id": "run-0712-b",
                "run_date": SOURCED_DAY, "source": POLYGON, "expected_runs": 3,
                "started_at": "2026-07-12T12:00:00Z", "finished_at": "2026-07-12T12:14:00Z",
                "wall_s": 13.0, "busy": [{"host": "b.test", "busy_s": 5.0}],
                "holds_capped": [hold]}),
        ),
        (
            "run-0712-c.json",
            serde_json::json!({"schema": "newsvault.run/1", "run_id": "run-0712-c",
                "run_date": SOURCED_DAY, "expected_runs": 3,
                "started_at": "2026-07-12T13:00:00Z", "finished_at": "2026-07-12T13:14:00Z",
                "wall_s": 17.0, "busy": [{"host": "a.test", "busy_s": 7.0}]}),
        ),
    ];
    for (name, body) in files {
        fs::write(runs.join(name), serde_json::to_vec(&body)?)?;
    }
    let mut seen = Vec::new();
    for source in [None, Some(EDGAR), Some(POLYGON)] {
        let mut args = vec!["--from", SOURCED_DAY, "--to", SOURCED_DAY];
        args.extend(source.map(|one| ["--source", one]).into_iter().flatten());
        let report = run(&root, &args)?.json()?;
        let sourced = day(&report, SOURCED_DAY).ok_or("the day is missing")?;
        let lanes: Vec<(Value, Value)> =
            sourced["busy"].as_array().map_or_else(Vec::new, |lanes| {
                lanes
                    .iter()
                    .map(|lane| (lane["host"].clone(), lane["busy_s"].clone()))
                    .collect()
            });
        seen.push((
            sourced["wall_s"].clone(),
            lanes,
            sourced["holds_capped"].clone(),
            sourced["runs_observed"].clone(),
        ));
    }
    let lane = |host: &str, busy_s: f64| (Value::from(host), Value::from(busy_s));
    assert_eq!(
        seen,
        vec![
            (
                Value::from(41.0),
                vec![lane("a.test", 10.0), lane("b.test", 5.0)],
                Value::from(1),
                Value::from(3)
            ),
            (
                Value::from(11.0),
                vec![lane("a.test", 3.0)],
                Value::from(0),
                Value::from(3)
            ),
            (
                Value::from(13.0),
                vec![lane("b.test", 5.0)],
                Value::from(1),
                Value::from(3)
            ),
        ]
    );
    Ok(())
}

// ---- §8 row R22: listing-to-fetch latency ----

const WIRES: &str = "open_web_wires";
const LISTING_RUN: &str = "run-0701-0034";
const CARRYING_RUN: &str = "run-0701-0630";
const LISTED_AT: &str = "2026-07-01T00:35:00.000000Z";
/// `LISTED_AT` + 31 days, received after `AT`, so the reader's `--at` cutoff leaves it out.
const AFTER_AT_S: i64 = 31 * 86_400;

/// One answered attempt of `source` in `run`, received `after_s` seconds after `LISTED_AT`; a
/// Document attempt (a story) with its payload at `url` when `url` is set, else the index.
fn answered(
    db: &Connection,
    (attempt, run, source): (&str, &str, &str),
    after_s: i64,
    url: Option<&str>,
    listed_by: Option<&str>,
) -> TestResult {
    let listed: chrono::DateTime<chrono::Utc> = LISTED_AT.parse()?;
    let received = (listed + chrono::TimeDelta::seconds(after_s))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    db.execute(
        "INSERT INTO attempts (attempt_id, run_id, source, target_window, filter_params,
                               universe_version, opened_at, received_at, story_id, listed_by)
         VALUES (?1, ?2, ?3, '2026-07-01', 'fp', 'uv', ?4, ?4, ?5, ?6)",
        rusqlite::params![
            attempt,
            run,
            source,
            received,
            url.map(|_| format!("story of {attempt}")),
            listed_by
        ],
    )?;
    if let Some(url) = url {
        db.execute(
            "INSERT INTO payloads (attempt_id, url, phase, raw_blob, headers, truncated)
             VALUES (?1, ?2, 'document', 'b-0701-p', '[]', 0)",
            [attempt, url],
        )?;
    }
    Ok(())
}

/// R22: per run and host, fresh documents (listed by the run's own index) and carried ones
/// (listed by an earlier run's) apart, nearest-rank p50 and p95 over unsorted rows, max, the
/// unlinked count; `--source` keeps one source; a document received after `--at` is not counted;
/// the human line states the JSON row.
#[test]
fn listing_latency_is_reported_per_run_and_host_with_fresh_and_carried_apart() -> TestResult {
    let root = fixture()?;
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    let index = "l-index";
    answered(&db, (index, LISTING_RUN, WIRES), 0, None, None)?;
    for (nth, after_s) in [70, 10, 100, 40, 20, 90, 30, 60, 80, 50]
        .into_iter()
        .enumerate()
    {
        let attempt = format!("l-fresh-{nth}");
        let url = format!("https://a.test/{nth}");
        let at = (attempt.as_str(), LISTING_RUN, WIRES);
        answered(&db, at, after_s, Some(&url), Some(index))?;
    }
    let at = |attempt| (attempt, LISTING_RUN, WIRES);
    answered(
        &db,
        at("l-other-host"),
        5,
        Some("https://b.test/0"),
        Some(index),
    )?;
    answered(&db, at("l-walk-id"), 7, Some("https://a.test/w"), None)?;
    let late = Some("https://a.test/late");
    answered(&db, at("l-after-at"), AFTER_AT_S, late, Some(index))?;
    let polygon = ("l-polygon", LISTING_RUN, POLYGON);
    answered(&db, polygon, 1, Some("https://c.test/0"), Some(index))?;
    for (attempt, after_s) in [("l-carried-0", 7_200), ("l-carried-1", 3_600)] {
        let at = (attempt, CARRYING_RUN, WIRES);
        answered(&db, at, after_s, Some("https://a.test/c"), Some(index))?;
    }
    drop(db);
    let report = over(&root, FIRST_DAY, FIRST_DAY)?.json()?;
    let wires = run(
        &root,
        &["--from", FIRST_DAY, "--to", FIRST_DAY, "--source", WIRES],
    )?
    .json()?;
    let rows = |report: &Value| day(report, FIRST_DAY).map(|day| day["listing_latency"].clone());
    let none = serde_json::json!({"documents": 0, "p50_s": null, "p95_s": null, "max_s": null});
    let span = |documents: i64, p50: f64, p95: f64, max: f64| serde_json::json!({"documents": documents, "p50_s": p50, "p95_s": p95, "max_s": max});
    let row = |run: &str, source: &str, host: &str, fresh: &Value, carried: &Value, unlinked| {
        serde_json::json!({"run_id": run, "source": source, "host": host, "fresh": fresh,
                           "carried": carried, "unlinked": unlinked})
    };
    let a_fresh = row(
        LISTING_RUN,
        WIRES,
        "a.test",
        &span(10, 50.0, 100.0, 100.0),
        &none,
        1,
    );
    let b_fresh = row(
        LISTING_RUN,
        WIRES,
        "b.test",
        &span(1, 5.0, 5.0, 5.0),
        &none,
        0,
    );
    let c_fresh = row(
        LISTING_RUN,
        POLYGON,
        "c.test",
        &span(1, 1.0, 1.0, 1.0),
        &none,
        0,
    );
    let a_carried = row(
        CARRYING_RUN,
        WIRES,
        "a.test",
        &none,
        &span(2, 3_600.0, 7_200.0, 7_200.0),
        0,
    );
    let human = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("ledger")
        .args(["--store", &root.path().to_string_lossy()])
        .args(["--at", AT, "--format", "human"])
        .args(["--from", FIRST_DAY, "--to", FIRST_DAY, "--source", WIRES])
        .output()?
        .stdout;
    let lines: Vec<String> = String::from_utf8(human)?
        .lines()
        .filter(|line| line.starts_with("latency"))
        .map(str::to_owned)
        .collect();
    assert_eq!(
        (rows(&report), rows(&wires)),
        (
            Some(serde_json::json!([
                a_fresh.clone(),
                b_fresh.clone(),
                c_fresh,
                a_carried.clone()
            ])),
            Some(serde_json::json!([a_fresh, b_fresh, a_carried])),
        )
    );
    assert_eq!(
        lines,
        vec![
            format!(
                "latency          {LISTING_RUN} {WIRES} a.test fresh 10 p50_s 50 p95_s 100 \
                 max_s 100 carried 0 p50_s - p95_s - max_s - unlinked 1"
            ),
            format!(
                "latency          {LISTING_RUN} {WIRES} b.test fresh 1 p50_s 5 p95_s 5 max_s 5 \
                 carried 0 p50_s - p95_s - max_s - unlinked 0"
            ),
            format!(
                "latency          {CARRYING_RUN} {WIRES} a.test fresh 0 p50_s - p95_s - max_s - \
                 carried 2 p50_s 3600 p95_s 7200 max_s 7200 unlinked 0"
            ),
        ]
    );
    Ok(())
}

/// R22: a Document attempt answered with no body (a transport failure: `Payload::stored` keeps
/// none) is counted under a null host, printed `-`; one received exactly at `--at` is counted.
#[test]
fn listing_latency_counts_a_bodyless_answer_under_no_host_and_one_received_at_the_cutoff(
) -> TestResult {
    let root = fixture()?;
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    let index = "l-index";
    answered(&db, (index, LISTING_RUN, WIRES), 0, None, None)?;
    let at = |attempt| (attempt, LISTING_RUN, WIRES);
    let bodyless = "l-bodyless";
    answered(&db, at(bodyless), 30, Some("https://a.test/0"), Some(index))?;
    db.execute("DELETE FROM payloads WHERE attempt_id = ?1", [bodyless])?;
    let listed: chrono::DateTime<chrono::Utc> = LISTED_AT.parse()?;
    let at_s = (AT.parse::<chrono::DateTime<chrono::Utc>>()? - listed).num_seconds();
    answered(
        &db,
        at("l-at"),
        at_s,
        Some("https://d.test/at"),
        Some(index),
    )?;
    drop(db);
    let report = over(&root, FIRST_DAY, FIRST_DAY)?.json()?;
    let human = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("ledger")
        .args(["--store", &root.path().to_string_lossy()])
        .args(["--at", AT, "--format", "human"])
        .args(["--from", FIRST_DAY, "--to", FIRST_DAY])
        .output()?
        .stdout;
    let lines: Vec<String> = String::from_utf8(human)?
        .lines()
        .filter(|line| line.starts_with("latency"))
        .map(str::to_owned)
        .collect();
    let hosts: Option<Vec<(Value, Value, Value)>> = day(&report, FIRST_DAY)
        .and_then(|day| day["listing_latency"].as_array())
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    let fresh = &row["fresh"];
                    (
                        row["host"].clone(),
                        fresh["documents"].clone(),
                        fresh["max_s"].clone(),
                    )
                })
                .collect()
        });
    let none = "carried 0 p50_s - p95_s - max_s - unlinked 0";
    assert_eq!(
        (hosts, lines),
        (
            Some(vec![
                (Value::Null, Value::from(1), Value::from(30.0)),
                (
                    Value::from("d.test"),
                    Value::from(1),
                    Value::from(at_s as f64)
                ),
            ]),
            vec![
                format!(
                    "latency          {LISTING_RUN} {WIRES} - fresh 1 p50_s 30 p95_s 30 \
                     max_s 30 {none}"
                ),
                format!(
                    "latency          {LISTING_RUN} {WIRES} d.test fresh 1 p50_s {at_s} \
                     p95_s {at_s} max_s {at_s} {none}"
                ),
            ]
        )
    );
    Ok(())
}
