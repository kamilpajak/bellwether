//! `ledger check --at` over stores the collector wrote through a stub (architecture.md section
//! 12.2, "What complete means"): exit 1 is the answer no, and the answer is as of `--at`.

mod support;

use std::path::Path;

use chrono::{DateTime, Utc};
use filingvault::ledger::{check, Asked, Checked};
use support::*;
use tempfile::TempDir;

const NOW: &str = "2026-12-01T00:00:00Z";

fn checked(store: &Path, at: &str) -> Result<Checked, Box<dyn std::error::Error>> {
    let asked = Asked {
        at: at.parse()?,
        store: store.to_path_buf(),
        rules: rules(10),
        holidays: holidays()?,
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

/// An index body whose `Last Data Received` is not its own day is counted, not failed: 11-16's
/// url answering 11-13's body (review LBD-5; owner decision 5 of 2026-11-27, a count and not yet
/// a gate). Mutations 2026-11-27, each red here: the header not stored; the count not compared.
#[test]
fn an_index_body_of_another_day_is_counted_by_its_last_data_received() -> TestResult {
    let temp = TempDir::new()?;
    run(temp.path(), &listing_of(&FIRST), RUN)??;
    let stale = listing_of(&LATER).replacing(
        &index_url("2026-11-16"),
        Reply::Status(OK, index_of("2026-11-13")),
    );
    run(temp.path(), &stale, "2026-11-17T06:10:00Z")??;
    let read = checked(temp.path(), "2026-11-17T12:00:00Z")?;
    assert_eq!(
        (
            read.exit,
            read.report["last_data_received_mismatch"].clone(),
            per_day(&read, "2026-11-16")["last_data_received"].clone(),
            per_day(&read, "2026-11-13")["last_data_received"].clone(),
        ),
        (
            0,
            serde_json::json!(1),
            serde_json::json!("2026-11-13"),
            serde_json::json!("2026-11-13"),
        )
    );
    Ok(())
}

/// A store whose only run received no listing answers no, even before a forward day is owed
/// (review LBD-6): robots.txt answered 503. Mutation 2026-11-27, `usable_listings` left out of
/// the exit: red here.
#[test]
fn a_store_holding_no_usable_listing_answers_no() -> TestResult {
    let temp = TempDir::new()?;
    let refused = Stub::default().answering(ROBOTS_URL, Reply::Status(503, Vec::new()));
    drop(run(temp.path(), &refused, RUN)?);
    let read = checked(temp.path(), "2026-11-15T12:00:00Z")?;
    assert_eq!(
        (read.exit, read.report["usable_listings"].clone()),
        (1, serde_json::json!(0))
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
        holidays: holidays()?,
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

/// Section 12.2's unlisted forward days, weekdays only, while the held days are still asked.
/// Mutations 2026-11-27, each red here: a failed listing erasing the held days;
/// `forward_unlisted` left out of the exit; a Saturday or a Sunday counted.
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
            serde_json::json!(4),
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

/// A row lives under its index day with its own filed day, and `late` and `Last-Modified` reach
/// the ledger. Mutations 2026-11-27, each red here: index and filed day swapped or made one;
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
        holidays: holidays()?,
    };
    let code = check(&asked, NOW.parse()?)
        .err()
        .map(|why| why.code().name());
    assert_eq!(code, Some("invalid_argument"));
    Ok(())
}

/// A weekday of a quarter whose listing the store never held is unlisted too, but not a listed
/// holiday: 01-04 and 01-05, not 2027-01-01. Mutations 2026-11-27, each red here: a quarter with
/// no held listing read as covering its days; the holiday list not read.
#[test]
fn the_days_of_a_quarter_never_listed_are_unlisted() -> TestResult {
    let temp = TempDir::new()?;
    run(
        temp.path(),
        &listing_of(&["2026-12-29"]),
        "2026-12-30T06:10:00Z",
    )??;
    let q1_failing = listing_of(&["2026-12-29", "2026-12-30", "2026-12-31"])
        .answering(&listing_url("2027-01-04"), Reply::Transport);
    drop(run(temp.path(), &q1_failing, "2027-01-05T06:10:00Z")?);
    let asked = Asked {
        at: "2027-01-09T12:00:00Z".parse()?,
        store: temp.path().to_path_buf(),
        rules: rules(10),
        holidays: holidays()?,
    };
    let read = check(&asked, "2027-02-01T00:00:00Z".parse()?)?;
    assert_eq!(
        (read.exit, read.report["forward_unlisted"].clone()),
        (1, serde_json::json!(2))
    );
    Ok(())
}

/// robots.txt, one listing body for the quarter of `quarter_day`, and each of `days` answered.
fn stub_with(quarter_day: &str, body: Vec<u8>, days: &[&str]) -> Stub {
    let stub = Stub::default()
        .answering(ROBOTS_URL, Reply::Status(OK, ROBOTS.to_vec()))
        .answering(&listing_url(quarter_day), Reply::Status(OK, body));
    days.iter().fold(stub, |stub, text| {
        stub.answering(&index_url(text), Reply::Status(OK, index_of(text)))
    })
}

/// The store of [`three_runs`] up to 11-17: 11-13 failed and 11-16 listed by the 11-17 run.
fn failed_13(store: &Path) -> Result<(), Box<dyn std::error::Error>> {
    run(store, &listing_of(&FIRST), RUN)??;
    let failing = listing_of(&LATER).replacing(&index_url("2026-11-13"), Reply::Transport);
    drop(run(store, &failing, "2026-11-17T06:10:00Z")?);
    Ok(())
}

/// A later listing naming no day keeps every day an earlier one named: the ledger still counts
/// 11-13 failed and the planner asks it again (review LBD-1). Mutations 2026-11-27, each red
/// here: the ledger reading the newest listing only; the planner replacing the held days.
#[test]
fn an_empty_listing_keeps_the_days_an_earlier_listing_named() -> TestResult {
    let temp = TempDir::new()?;
    failed_13(temp.path())?;
    let empty = stub_with(FIRST[0], listing(&[]), &[]);
    drop(run(temp.path(), &empty, "2026-11-18T06:10:00Z")?);
    let read = checked(temp.path(), "2026-11-18T12:00:00Z")?;
    assert_eq!(
        (
            asked_days(&empty),
            read.exit,
            per_day(&read, "2026-11-13")["state"].clone()
        ),
        (
            vec![
                "2026-11-16".to_owned(),
                "2026-11-13".to_owned(),
                "2026-11-12".to_owned()
            ],
            1,
            serde_json::json!("failed")
        )
    );
    Ok(())
}

/// A newer listing that omits a day an older one named keeps that day (review LBD-2). Mutation
/// 2026-11-27, the ledger reading the newest listing only: red here.
#[test]
fn a_newer_listing_that_omits_a_day_keeps_it() -> TestResult {
    let temp = TempDir::new()?;
    failed_13(temp.path())?;
    let without = ["2026-11-12", "2026-11-16", "2026-11-17"];
    drop(run(
        temp.path(),
        &stub_with(FIRST[0], listing(&without), &without),
        "2026-11-18T06:10:00Z",
    )?);
    let read = checked(temp.path(), "2026-11-19T12:00:00Z")?;
    assert_eq!(
        (read.exit, read.report["forward_unasked"].clone()),
        (1, serde_json::json!(1))
    );
    Ok(())
}

/// A weekday after the newest day any held listing of its quarter names is unlisted, whenever
/// the listing came: 11-16 written after the 11-17T01:00Z run's listing, and the next listing
/// failing (review LBD-3). Mutation 2026-11-27, a listing received on day R read as naming every
/// day before R: red here.
#[test]
fn a_day_written_after_the_listing_was_read_is_unlisted() -> TestResult {
    let temp = TempDir::new()?;
    run(temp.path(), &listing_of(&FIRST), RUN)??;
    let early = &LATER[..7];
    drop(run(
        temp.path(),
        &stub_with(FIRST[0], listing(early), early),
        "2026-11-17T01:00:00Z",
    )?);
    let failing = listing_of(&LATER).replacing(&listing_url(FIRST[0]), Reply::Transport);
    drop(run(temp.path(), &failing, "2026-11-18T06:10:00Z")?);
    let read = checked(temp.path(), "2026-11-20T12:00:00Z")?;
    assert_eq!(
        (read.exit, read.report["forward_unlisted"].clone()),
        (1, serde_json::json!(1))
    );
    Ok(())
}

/// A listing whose items name no form index, held night after night, never covers a weekday
/// (review EF1). Mutation 2026-11-27, a quarter with an `empty` listing read as covering every
/// day before its receipt: red here.
#[test]
fn a_listing_naming_no_form_index_covers_no_day() -> TestResult {
    let temp = TempDir::new()?;
    let named = br#"{"directory": {"item": [{"name": "company.20261112.idx"}]}}"#;
    for at in [
        RUN,
        NEXT_RUN,
        "2026-11-16T06:10:00Z",
        "2026-11-19T06:10:00Z",
    ] {
        run(temp.path(), &stub_with(FIRST[0], named.to_vec(), &[]), at)??;
    }
    let read = checked(temp.path(), "2026-11-20T12:00:00Z")?;
    assert_eq!(
        (read.exit, read.report["forward_unlisted"].clone()),
        (1, serde_json::json!(3))
    );
    Ok(())
}

/// SEC as it answers a run at `at`: each quarter's listing names every one of `days` before the
/// run's date, and each day's index answers.
fn sec_on(at: &str, days: &[String]) -> Stub {
    let run_day = day(&at[..10]);
    let mut by_quarter: std::collections::BTreeMap<String, Vec<&str>> = Default::default();
    for text in days {
        let named = by_quarter.entry(listing_url(text)).or_default();
        if day(text) < run_day {
            named.push(text);
        }
    }
    let robots = Stub::default().answering(ROBOTS_URL, Reply::Status(OK, ROBOTS.to_vec()));
    let stub = by_quarter.into_iter().fold(robots, |stub, (url, named)| {
        stub.answering(&url, Reply::Status(OK, listing(&named)))
    });
    days.iter().fold(stub, |stub, text| {
        stub.answering(&index_url(text), Reply::Status(OK, index_of(text)))
    })
}

/// A quarter ending on a federal holiday observed on a Friday, 2027-12-31 for 2028-01-01, in a
/// steady state where SEC writes every other weekday: nothing is unlisted (owner decision 1 of
/// 2026-11-27). Mutation 2026-11-27, the holiday list not read: red here, exit 1.
#[test]
fn a_quarter_ending_on_an_observed_holiday_leaves_no_day_unlisted() -> TestResult {
    let temp = TempDir::new()?;
    let written: Vec<String> = day("2027-12-13")
        .iter_days()
        .take_while(|on| *on <= day("2028-01-14"))
        .filter(|on| {
            !matches!(
                chrono::Datelike::weekday(on),
                chrono::Weekday::Sat | chrono::Weekday::Sun
            )
        })
        .map(|on| on.format("%Y-%m-%d").to_string())
        .filter(|text| !["2027-12-24", "2027-12-31"].contains(&text.as_str()))
        .collect();
    for night in day("2027-12-27")
        .iter_days()
        .take_while(|on| *on <= day("2028-01-12"))
    {
        let at = format!("{}T06:10:00Z", night.format("%Y-%m-%d"));
        run(temp.path(), &sec_on(&at, &written), &at)??;
    }
    let asked = Asked {
        at: "2028-01-12T12:00:00Z".parse()?,
        store: temp.path().to_path_buf(),
        rules: rules(10),
        holidays: holidays()?,
    };
    let read = check(&asked, "2029-01-01T00:00:00Z".parse()?)?;
    assert_eq!(
        (read.exit, read.report["forward_unlisted"].clone()),
        (0, serde_json::json!(0))
    );
    Ok(())
}

/// A weekday no listing names, before the newest day a held listing named, is not unlisted, and
/// a newer listing omitting that newest day does not move the bound back. Mutations 2026-11-27,
/// each red here: the bound dropped; the newest named day taken from the last body read.
#[test]
fn a_gap_before_the_newest_named_day_is_not_unlisted() -> TestResult {
    let temp = TempDir::new()?;
    let gapped = [
        "2026-11-05",
        "2026-11-06",
        "2026-11-09",
        "2026-11-10",
        "2026-11-11",
        "2026-11-12",
        "2026-11-16",
    ];
    run(temp.path(), &listing_of(&FIRST), RUN)??;
    drop(run(
        temp.path(),
        &listing_of(&gapped),
        "2026-11-17T06:10:00Z",
    )?);
    drop(run(
        temp.path(),
        &listing_of(&gapped[..6]),
        "2026-11-18T06:10:00Z",
    )?);
    let read = checked(temp.path(), "2026-11-20T12:00:00Z")?;
    assert_eq!(read.report["forward_unlisted"], serde_json::json!(0));
    Ok(())
}

/// A store whose only listing is `empty` holds no usable listing and answers no. Mutation
/// 2026-11-27, every held listing counted usable: red here.
#[test]
fn a_store_holding_only_an_empty_listing_answers_no() -> TestResult {
    let temp = TempDir::new()?;
    let stub = Stub::default()
        .answering(ROBOTS_URL, Reply::Status(OK, ROBOTS.to_vec()))
        .answering(&listing_url(LISTED[0]), Reply::Status(OK, listing(&[])));
    drop(run(temp.path(), &stub, RUN)?);
    let read = checked(temp.path(), "2026-11-12T12:00:00Z")?;
    assert_eq!(
        (read.exit, read.report["usable_listings"].clone()),
        (1, serde_json::json!(0))
    );
    Ok(())
}

/// `Last Data Received` is read from the day's newest usable answer: 11-12 answered stale, then
/// right, then failed. Mutations 2026-11-27, each red here: the oldest answer read; a failed
/// answer read; the last attempt shown.
#[test]
fn the_newest_usable_answer_names_the_days_last_data_received() -> TestResult {
    let temp = TempDir::new()?;
    let named: Vec<&str> = LISTED.iter().copied().chain(["2026-11-12"]).collect();
    run(temp.path(), &listing_of(&LISTED), RUN)??;
    let stale = listing_of(&named).replacing(
        &index_url("2026-11-12"),
        Reply::Status(OK, index_of("2026-11-11")),
    );
    run(temp.path(), &stale, NEXT_RUN)??;
    run(temp.path(), &listing_of(&named), "2026-11-14T06:10:00Z")??;
    let failing = listing_of(&named).replacing(&index_url("2026-11-12"), Reply::Transport);
    drop(run(temp.path(), &failing, "2026-11-16T06:10:00Z")?);
    let read = checked(temp.path(), "2026-11-16T12:00:00Z")?;
    assert_eq!(
        (
            read.report["last_data_received_mismatch"].clone(),
            per_day(&read, "2026-11-12")["last_data_received"].clone()
        ),
        (serde_json::json!(0), serde_json::json!("2026-11-12"))
    );
    Ok(())
}
