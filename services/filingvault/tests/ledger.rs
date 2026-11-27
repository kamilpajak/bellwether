//! `ledger check --at` over stores the collector wrote through a stub (architecture.md section
//! 12.2, "What complete means"): exit 1 is the answer no, and the answer is as of `--at`.

mod support;

use std::path::Path;

use chrono::{DateTime, Utc};
use filingvault::ledger::{check, Asked, Checked};
use support::*;
use tempfile::TempDir;

const NOW: &str = "2026-12-01T00:00:00Z";

/// robots.txt, QTR4 listing `days`, and each day's index.
fn listing_of(days: &[&str]) -> Stub {
    let stub = Stub::default()
        .answering(ROBOTS_URL, Reply::Status(OK, ROBOTS.to_vec()))
        .answering(&listing_url(days[0]), Reply::Status(OK, listing(days)));
    days.iter().fold(stub, |stub, text| {
        stub.answering(&index_url(text), Reply::Status(OK, index_of(text)))
    })
}

fn checked(store: &Path, at: &str) -> Result<Checked, Box<dyn std::error::Error>> {
    let asked = Asked {
        at: at.parse()?,
        store: store.to_path_buf(),
        rules: rules(10),
    };
    Ok(check(&asked, NOW.parse::<DateTime<Utc>>()?)?)
}

const FIRST: [&str; 5] = LISTED;
const LATER: [&str; 8] = [
    "2026-11-05",
    "2026-11-06",
    "2026-11-09",
    "2026-11-10",
    "2026-11-11",
    "2026-11-12",
    "2026-11-13",
    "2026-11-16",
];

/// The store of three runs: 11-12 the backfill, 11-17 with 11-12 failing, 11-18 asking it again.
fn three_runs(store: &Path) -> Result<(), Box<dyn std::error::Error>> {
    run(store, &listing_of(&FIRST), RUN)??;
    let failing = listing_of(&LATER).replacing(&index_url("2026-11-12"), Reply::Transport);
    run(store, &failing, "2026-11-17T06:10:00Z")??;
    Ok(())
}

/// Mutation 2026-11-27, the forward window started from the newest collect run: red.
#[test]
fn ledger_check_exits_1_while_a_forward_day_is_unasked_and_0_when_none_is() -> TestResult {
    let temp = TempDir::new()?;
    three_runs(temp.path())?;
    let open = checked(temp.path(), "2026-11-17T12:00:00Z")?;
    let later = [&LATER[..], &["2026-11-17"]].concat();
    run(temp.path(), &listing_of(&later), "2026-11-18T06:10:00Z")??;
    let closed = checked(temp.path(), "2026-11-18T12:00:00Z")?;
    assert_eq!(
        (
            open.exit,
            open.report["forward_from"].clone(),
            open.report["forward_unasked"].clone(),
            closed.exit,
            closed.report["forward_unasked"].clone(),
        ),
        (
            1,
            serde_json::json!("2026-11-12"),
            serde_json::json!(1),
            0,
            serde_json::json!(0)
        )
    );
    Ok(())
}

/// "We looked and failed" is not "we did not look". Mutation 2026-11-27, `Failed` folded into
/// `Unasked`: red.
#[test]
fn a_day_whose_attempts_all_failed_is_failed_and_not_unasked() -> TestResult {
    let temp = TempDir::new()?;
    three_runs(temp.path())?;
    let read = checked(temp.path(), "2026-11-17T12:00:00Z")?;
    let state = |day: &str| {
        read.report["per_day"]
            .as_array()
            .and_then(|days| days.iter().find(|row| row["index_day"] == day))
            .map(|row| row["state"].clone())
    };
    assert_eq!(
        (
            state("2026-11-12"),
            state("2026-11-13"),
            read.report["days"].clone()
        ),
        (
            Some(serde_json::json!("failed")),
            Some(serde_json::json!("asked")),
            serde_json::json!({"asked": 7, "failed": 1, "narrower": 0, "truncated": 0, "unasked": 0})
        )
    );
    Ok(())
}

/// The same `--at` gives the same answer after a later run, and a run still going at `--at`
/// shows no close. Mutations 2026-11-27, each red here: the `closed_at <= at` bound dropped from
/// the index read, or from the listing read; the last run's close shown whenever it came.
#[test]
fn ledger_check_answers_as_of_at_and_a_later_attempt_changes_nothing() -> TestResult {
    let temp = TempDir::new()?;
    run(temp.path(), &listing_of(&FIRST), RUN)??;
    let before = checked(temp.path(), "2026-11-16T12:00:00Z")?;
    run(temp.path(), &listing_of(&LATER), "2026-11-17T06:10:00Z")??;
    let after = checked(temp.path(), "2026-11-16T12:00:00Z")?;
    let during = checked(temp.path(), "2026-11-17T06:10:01Z")?.report["last_run"].clone();
    assert_eq!(
        (
            after.exit,
            after.report,
            during["started_at"].clone(),
            during["finished_at"].clone()
        ),
        (
            before.exit,
            before.report,
            serde_json::json!("2026-11-17T06:10:00.000000Z"),
            serde_json::Value::Null
        )
    );
    Ok(())
}

/// No collect run by `--at`: no forward window, and "we did not look" is never "nothing open".
/// Mutation 2026-11-27, exit 0 with no run: red.
#[test]
fn a_store_with_no_collect_run_by_at_has_no_forward_window_and_answers_no() -> TestResult {
    let temp = TempDir::new()?;
    run(temp.path(), &listing_of(&FIRST), RUN)??;
    let before = checked(temp.path(), "2026-11-12T06:00:00Z")?;
    assert_eq!(
        (
            before.exit,
            before.report["forward_from"].clone(),
            before.report["last_run"].clone()
        ),
        (1, serde_json::Value::Null, serde_json::Value::Null)
    );
    Ok(())
}

/// Mutation 2026-11-27, the read-only open replaced by the writer's create: red.
#[test]
fn ledger_check_without_a_store_exits_4_and_creates_nothing() -> TestResult {
    let temp = TempDir::new()?;
    let refused = checked(temp.path(), "2026-11-17T12:00:00Z")
        .err()
        .map(|why| why.to_string());
    assert_eq!(
        (refused.is_some(), std::fs::read_dir(temp.path())?.count()),
        (true, 0)
    );
    let asked = Asked {
        at: "2026-11-17T12:00:00Z".parse()?,
        store: temp.path().to_path_buf(),
        rules: rules(10),
    };
    let code = check(&asked, NOW.parse()?).err().map(|why| why.code());
    assert_eq!(
        code.map(|code| (code.name(), code.exit())),
        Some(("input_absent", 4))
    );
    Ok(())
}

/// Section 12.3's "rows added after day D": a row first listed by a later attempt of its day,
/// counted from the day's first usable attempt. Mutations 2026-11-27, each red here: `first_seq`
/// not compared; counted from the first attempt of any verdict; `last_attempt` the oldest.
#[test]
fn a_row_listed_by_a_later_attempt_is_counted_listed_later() -> TestResult {
    let temp = TempDir::new()?;
    let failing = listing_of(&FIRST).replacing(&index_url("2026-11-10"), Reply::Transport);
    run(temp.path(), &failing, RUN)??;
    let added = "10-K             LATE FILER INC                                                \
                 7654321     20261111    edgar/data/7654321/0009999999-26-000001.txt\n";
    let relisted = listing_of(&FIRST).replacing(
        &index_url("2026-11-11"),
        Reply::Status(
            OK,
            [index_of("2026-11-11"), added.as_bytes().to_vec()].concat(),
        ),
    );
    run(temp.path(), &relisted, NEXT_RUN)??;
    let read = checked(temp.path(), "2026-11-14T00:00:00Z")?;
    let of = |text: &str| {
        read.report["per_day"]
            .as_array()
            .and_then(|days| days.iter().find(|row| row["index_day"] == text).cloned())
            .unwrap_or_default()
    };
    let (day, retried) = (of("2026-11-11"), of("2026-11-10"));
    assert_eq!(
        (
            day["listed_rows"].clone(),
            day["listed_later"].clone(),
            day["late"].clone(),
            retried["listed_later"].clone(),
            retried["last_attempt"]["verdict"].clone(),
        ),
        (
            serde_json::json!(11),
            serde_json::json!(1),
            serde_json::json!(0),
            serde_json::json!(0),
            serde_json::json!("usable"),
        )
    );
    Ok(())
}

/// A forward day no held listing could have named yet is owed as well: a listing that fails night
/// after night leaves the newest days unlisted, never "nothing owed", and the held days are still
/// asked (review of 2026-11-27, PIT-1). Mutation 2026-11-27, a failed listing erasing the days the
/// store holds: red here.
#[test]
fn forward_days_after_the_newest_listing_are_unlisted_and_answer_no() -> TestResult {
    let temp = TempDir::new()?;
    run(temp.path(), &listing_of(&FIRST), RUN)??;
    let failing = listing_of(&FIRST).replacing(&listing_url(FIRST[0]), Reply::Transport);
    drop(run(temp.path(), &failing, "2026-11-17T06:10:00Z")?);
    let read = checked(temp.path(), "2026-11-21T12:00:00Z")?;
    assert_eq!(
        (
            asked_days(&failing),
            read.exit,
            read.report["forward_unasked"].clone(),
            read.report["forward_unlisted"].clone(),
        ),
        (
            vec![
                "2026-11-11".to_owned(),
                "2026-11-10".to_owned(),
                "2026-11-09".to_owned()
            ],
            1,
            serde_json::json!(0),
            serde_json::json!(6),
        )
    );
    Ok(())
}

fn per_day(read: &Checked, day: &str) -> serde_json::Value {
    read.report["per_day"]
        .as_array()
        .and_then(|days| days.iter().find(|row| row["index_day"] == day).cloned())
        .unwrap_or_default()
}

/// "We did not look" is never "we looked": a listed day no attempt names is `unasked`, and a day
/// on or after `--at`'s date is not counted. Mutations 2026-11-27, each red here: a day with no
/// attempt read asked; the listed days not cut at `--at`'s date.
#[test]
fn a_listed_day_no_attempt_names_is_unasked() -> TestResult {
    let temp = TempDir::new()?;
    let with_today = [&LISTED[..], &["2026-11-12"]].concat();
    let stub = listing_of(&with_today).past_deadline(&index_url("2026-11-11"));
    run(temp.path(), &stub, RUN)??;
    let read = checked(temp.path(), "2026-11-12T12:00:00Z")?;
    assert_eq!(
        (
            per_day(&read, "2026-11-11")["state"].clone(),
            read.report["per_day"].as_array().map(Vec::len),
        ),
        (serde_json::json!("unasked"), Some(5))
    );
    Ok(())
}

/// A held listing whose blob is gone is an error, never a quarter of no day. Mutation
/// 2026-11-27, a missing blob skipped: red.
#[test]
fn a_held_listing_whose_blob_is_gone_is_an_error() -> TestResult {
    let temp = TempDir::new()?;
    run(temp.path(), &listing_of(&FIRST), RUN)??;
    let blobs = vault_core::blobs::BlobStore::new(
        temp.path().join(filingvault::store::BLOBS),
        filingvault::store::WRITE_POLICY,
    );
    for hex in rows(
        temp.path(),
        "SELECT raw_blob FROM attempts WHERE kind = 'listing'",
    )? {
        if let Some(digest) = blobs.named(&hex) {
            std::fs::remove_file(blobs.compressed_path_of(&digest))?;
        }
    }
    let refused = checked(temp.path(), "2026-11-12T12:00:00Z")
        .err()
        .map(|why| why.to_string());
    assert!(refused.is_some_and(|why| why.contains("does not exist")));
    Ok(())
}

/// Each listings row lives under the index day that listed it and keeps its own filed day, a
/// row filed earlier counts `late`, and the index's `Last-Modified` reaches the ledger.
/// Mutations 2026-11-27, each red here: rows filed under their filed day; `filed_day` dropped;
/// `late` never counted; the headers not stored; `last_modified` not read.
#[test]
fn an_index_days_rows_keep_their_filed_day_and_its_last_modified() -> TestResult {
    let temp = TempDir::new()?;
    let modified = "Wed, 11 Nov 2026 22:00:00 GMT";
    let stub = listing_of(&FIRST).replacing(
        &index_url("2026-11-11"),
        Reply::Dated(OK, index_of("2026-11-10"), modified),
    );
    run(temp.path(), &stub, RUN)??;
    let read = checked(temp.path(), "2026-11-12T12:00:00Z")?;
    let day = per_day(&read, "2026-11-11");
    assert_eq!(
        (
            rows(
                temp.path(),
                "SELECT DISTINCT index_day FROM listings WHERE filed_day = '2026-11-10' ORDER BY 1"
            )?,
            day["late"].clone(),
            day["last_modified"].clone(),
        ),
        (
            vec!["2026-11-10".to_owned(), "2026-11-11".to_owned()],
            serde_json::json!(10),
            serde_json::json!(modified),
        )
    );
    Ok(())
}

/// A forward day is owed once more than three days have passed after it, and a day asked under
/// a narrower form list is not asked. Mutations 2026-11-27, each red here: the grace `<` made
/// `<=`; the grace 0; a narrower day counted asked.
#[test]
fn a_forward_day_is_owed_after_three_days_and_a_narrower_one_is_unasked() -> TestResult {
    let temp = TempDir::new()?;
    run(temp.path(), &listing_of(&FIRST), RUN)??;
    let mut narrow = support::ask(temp.path());
    narrow.rules.forms = vec!["10-K".to_owned()];
    let with_12 = [&FIRST[..], &["2026-11-12"]].concat();
    run_ask(&narrow, &listing_of(&with_12), NEXT_RUN)??;
    let answer = |at: &str| -> Result<(u8, serde_json::Value), Box<dyn std::error::Error>> {
        let read = checked(temp.path(), at)?;
        Ok((read.exit, read.report["forward_unasked"].clone()))
    };
    assert_eq!(
        (
            answer("2026-11-15T12:00:00Z")?,
            answer("2026-11-16T12:00:00Z")?
        ),
        ((0, serde_json::json!(0)), (1, serde_json::json!(1)))
    );
    Ok(())
}

/// Mutation 2026-11-27, an `--at` after now accepted: red.
#[test]
fn ledger_check_refuses_an_at_after_now() -> TestResult {
    let temp = TempDir::new()?;
    run(temp.path(), &listing_of(&FIRST), RUN)??;
    let asked = Asked {
        at: "2026-12-02T00:00:00Z".parse()?,
        store: temp.path().to_path_buf(),
        rules: rules(10),
    };
    let code = check(&asked, NOW.parse()?)
        .err()
        .map(|why| why.code().name());
    assert_eq!(code, Some("invalid_argument"));
    Ok(())
}
