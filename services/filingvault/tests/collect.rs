//! `filings collect` over a stub `Exchange` and a moving clock: what each answer leaves in the
//! store, which requests a run makes and in what order (architecture.md sections 12.2, 12.3).

mod support;

use std::cell::RefCell;
use std::rc::Rc;

use filingvault::error::FilingsError;
use filingvault::store::{Writer, LOCK};
use news_provider::RunId;
use support::*;
use tempfile::TempDir;
use vault_core::lease::Lease;

const TRAFFIC_403: &[u8] = b"<html><body>Request Rate Threshold Exceeded</body></html>";
const ACCESS_DENIED: &[u8] =
    b"<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>";
const NOT_FOUND_PAGE: &[u8] = b"<html><body>Page Not Found</body></html>";

/// Section 8 row R4, the filings clause: at every send, a second connection already reads the
/// attempt's row, open. Mutation 2026-11-27, the `Opened` insert moved into `finalised`: red.
#[test]
fn no_vendor_call_leaves_without_an_open_attempt_row() -> TestResult {
    let temp = TempDir::new()?;
    let store = temp.path().to_path_buf();
    let seen: Rc<RefCell<Vec<String>>> = Rc::default();
    let record = Rc::clone(&seen);
    let mut stub = Stub::sec();
    stub.on_send = Some(Box::new(move |attempt| {
        let open = log(&store).and_then(|db| {
            Ok(db.query_row(
                "SELECT count(*) FROM attempts WHERE attempt_id = ?1 AND closed_at IS NULL",
                [attempt],
                |row| row.get::<_, i64>(0),
            )?)
        });
        record.borrow_mut().push(format!("{open:?}"));
    }));
    let report = run(temp.path(), &stub, RUN)??;
    let sends = stub.asked().len();
    assert_eq!(
        (report.exit, seen.borrow().clone()),
        (0, vec!["Ok(1)".to_owned(); sends])
    );
    Ok(())
}

/// Written whether or not anything came back: a transport error, a 404 and a 200 each close.
/// Mutation 2026-11-27, a transport error abandoning the attempt instead of closing it: red.
#[test]
fn an_attempt_is_closed_whatever_came_back() -> TestResult {
    let temp = TempDir::new()?;
    let stub = Stub::sec()
        .replacing(&index_url("2026-11-11"), Reply::Transport)
        .replacing(
            &index_url("2026-11-10"),
            Reply::Status(404, NOT_FOUND_PAGE.to_vec()),
        );
    let report = run(temp.path(), &stub, RUN)??;
    assert_eq!(
        (
            report.exit,
            rows(
                temp.path(),
                "SELECT target, outcome, verdict, status, raw_blob IS NOT NULL FROM attempts
                  WHERE kind = 'index' ORDER BY seq LIMIT 3"
            )?
        ),
        (
            8,
            vec![
                "2026-11-11|failed|transient_failure|null|0".to_owned(),
                "2026-11-10|failed|permanent_failure|404|1".to_owned(),
                "2026-11-09|ok|usable|200|1".to_owned(),
            ]
        )
    );
    Ok(())
}

/// Mutations 2026-11-27, each red here: the sweep skipped; the dead run's row left open.
#[test]
fn a_dead_runs_open_attempt_and_run_are_closed_interrupted_by_the_next_writer() -> TestResult {
    let temp = TempDir::new()?;
    {
        let writer = Writer::open(temp.path(), &RunId::new("filings-dead"))?;
        writer.start_run("2026-11-11T06:10:00Z".parse()?)?;
        writer.db().execute(
            "INSERT INTO attempts (attempt_id, run_id, kind, target, url, body_limit, opened_at)
             VALUES ('dead', 'filings-dead', 'robots', 'www.sec.gov', 'u', 1,
                     '2026-11-11T06:10:01.000000Z')",
            [],
        )?;
    }
    let report = run(temp.path(), &Stub::sec(), RUN)??;
    assert_eq!(
        (
            report.value["swept"].clone(),
            rows(
                temp.path(),
                "SELECT outcome, closed_at IS NOT NULL FROM attempts WHERE attempt_id = 'dead'"
            )?,
            rows(
                temp.path(),
                "SELECT run_id, stopped_by FROM runs ORDER BY started_at"
            )?,
        ),
        (
            serde_json::json!({"attempts": 1, "runs": 1}),
            vec!["interrupted|1".to_owned()],
            vec![
                "filings-dead|interrupted".to_owned(),
                format!(
                    "{}|null",
                    report.value["run_id"].as_str().unwrap_or_default()
                ),
            ]
        )
    );
    Ok(())
}

/// A start that cannot take the lease writes no row. Mutation 2026-11-27, the lease not taken: red.
#[test]
fn a_second_writer_is_refused_lease_held_and_writes_no_run() -> TestResult {
    let temp = TempDir::new()?;
    drop(Writer::open(temp.path(), &RunId::new("filings-first"))?);
    let held = Lease::take(&temp.path().join(LOCK))?;
    let stub = Stub::sec();
    let refused = run(temp.path(), &stub, RUN)?;
    drop(held);
    assert_eq!(
        (
            refused.err().map(|why| why.code().name()),
            stub.asked().len(),
            rows(temp.path(), "SELECT count(*) FROM runs")?
        ),
        (Some("lease_held"), 0, vec!["0".to_owned()])
    );
    Ok(())
}

/// One `listings` row per row of a listed form, the first attempt that listed it kept, and a
/// filing listed under two CIKs two rows of one accession. Mutations 2026-11-27, each red here:
/// every form stored; `first_seq` updated on conflict; the key without `cik`.
#[test]
fn an_index_day_lists_one_row_per_listed_form_row_and_keeps_the_first_attempt_that_listed_it(
) -> TestResult {
    let temp = TempDir::new()?;
    let co_filer =
        "10-K             CO-FILER HOLDINGS INC                                         \
                    7654321     20261111    edgar/data/7654321/0001493152-26-050839.txt\n";
    let combined = [index_of("2026-11-11"), co_filer.as_bytes().to_vec()].concat();
    let stub = Stub::sec().replacing(&index_url("2026-11-11"), Reply::Status(OK, combined));
    let first = run(temp.path(), &stub, RUN)??;
    run(temp.path(), &stub, NEXT_RUN)??;
    let day = "index_day = '2026-11-11'";
    assert_eq!(
        (
            rows(
                temp.path(),
                &format!("SELECT count(*), count(DISTINCT accession) FROM listings WHERE {day}")
            )?,
            rows(
                temp.path(),
                &format!("SELECT DISTINCT form FROM listings WHERE {day} ORDER BY form")
            )?,
            rows(
                temp.path(),
                &format!(
                    "SELECT count(DISTINCT l.first_seq), min(a.run_id), max(a.run_id)
                       FROM listings l JOIN attempts a ON a.seq = l.first_seq WHERE {day}"
                )
            )?,
            rows(
                temp.path(),
                &format!(
                    "SELECT cik FROM listings WHERE {day}
                        AND accession = '0001493152-26-050839' ORDER BY cik"
                )
            )?,
        ),
        (
            vec!["11|10".to_owned()],
            vec!["10-K".to_owned(), "10-K/A".to_owned()],
            vec![format!(
                "1|{0}|{0}",
                first.value["run_id"].as_str().unwrap_or_default()
            )],
            vec!["1889106".to_owned(), "7654321".to_owned()],
        )
    );
    Ok(())
}

/// "We asked and found nothing": an index with no row of a listed form is `empty`, its day asked.
/// Mutation 2026-11-27, zero listed rows read as usable: red.
#[test]
fn an_index_with_no_listed_form_row_is_empty_and_its_day_asked() -> TestResult {
    let temp = TempDir::new()?;
    let only_8k: String = INDEX
        .lines()
        .filter(|line| !line.starts_with("10-K"))
        .map(|line| format!("{line}\n"))
        .collect();
    let stub = Stub::sec().replacing(
        &index_url("2026-11-11"),
        Reply::Status(OK, only_8k.into_bytes()),
    );
    run(temp.path(), &stub, RUN)??;
    assert_eq!(
        rows(
            temp.path(),
            "SELECT outcome, verdict FROM attempts WHERE kind = 'index' AND target = '2026-11-11'"
        )?,
        vec!["empty|empty".to_owned()]
    );
    Ok(())
}

/// Only days a listing names are asked: a weekend between them is not. Mutation 2026-11-27, the
/// trailing days taken from the calendar: red.
#[test]
fn a_day_no_listing_names_is_never_asked() -> TestResult {
    let temp = TempDir::new()?;
    let stub = Stub::sec();
    run(temp.path(), &stub, RUN)??;
    assert_eq!(
        asked_days(&stub),
        vec![
            "2026-11-11",
            "2026-11-10",
            "2026-11-09",
            "2026-11-06",
            "2026-11-05"
        ]
    );
    Ok(())
}

/// A 404 on a listed day: permanent, its body kept, and the day asked again by the next run even
/// outside the trailing three. Mutation 2026-11-27, a failed day counted asked: red.
#[test]
fn a_listed_day_answering_404_is_a_permanent_failure_and_asked_again_next_run() -> TestResult {
    let temp = TempDir::new()?;
    let stub = Stub::sec().replacing(
        &index_url("2026-11-05"),
        Reply::Status(404, NOT_FOUND_PAGE.to_vec()),
    );
    run(temp.path(), &stub, RUN)??;
    let next = Stub::sec();
    run(temp.path(), &next, "2026-11-16T06:10:00Z")??;
    assert_eq!(
        (
            rows(
                temp.path(),
                "SELECT verdict, raw_blob IS NOT NULL FROM attempts
                  WHERE kind = 'index' AND target = '2026-11-05' ORDER BY seq"
            )?,
            asked_days(&next).contains(&"2026-11-05".to_owned()),
            asked_days(&next).contains(&"2026-11-06".to_owned()),
        ),
        (
            vec!["permanent_failure|1".to_owned(), "usable|1".to_owned()],
            true,
            false
        )
    );
    Ok(())
}

/// Section 12.3: SEC's traffic 403 is transient, its S3 `AccessDenied` permanent; both bodies kept.
/// Mutation 2026-11-27, `AccessDenied` read as transient in `sec_edgar::body_of_200`: red.
#[test]
fn a_traffic_403_is_transient_and_access_denied_permanent() -> TestResult {
    let temp = TempDir::new()?;
    let stub = Stub::sec()
        .replacing(
            &index_url("2026-11-11"),
            Reply::Status(403, TRAFFIC_403.to_vec()),
        )
        .replacing(
            &index_url("2026-11-10"),
            Reply::Status(403, ACCESS_DENIED.to_vec()),
        );
    let report = run(temp.path(), &stub, RUN)??;
    assert_eq!(
        (
            report.exit,
            rows(
                temp.path(),
                "SELECT target, verdict, raw_blob IS NOT NULL FROM attempts
                  WHERE kind = 'index' AND status = 403 ORDER BY seq"
            )?
        ),
        (
            8,
            vec![
                "2026-11-11|transient_failure|1".to_owned(),
                "2026-11-10|permanent_failure|1".to_owned(),
            ]
        )
    );
    Ok(())
}

/// A body over the limit is stored cut, `truncated` 1 with the limit on its row, and not asked
/// again by a run under the same limit. Mutation 2026-11-27, the truncated state dropped: red.
#[test]
fn a_body_over_the_limit_is_stored_cut_and_not_asked_again_under_the_same_limit() -> TestResult {
    let temp = TempDir::new()?;
    let stub = Stub::sec().replacing(&index_url("2026-11-05"), Reply::OverLimit(b"cut".to_vec()));
    let report = run(temp.path(), &stub, RUN)??;
    let reported = report.value["days"]
        .as_array()
        .and_then(|days| days.iter().find(|row| row["index_day"] == "2026-11-05"))
        .map(|row| row["truncated"].clone());
    let next = Stub::sec();
    run(temp.path(), &next, "2026-11-16T06:10:00Z")??;
    assert_eq!(
        (
            rows(
                temp.path(),
                "SELECT a.verdict, a.truncated, a.body_limit, b.bytes FROM attempts a
                   JOIN blobs b ON b.digest = a.raw_blob
                  WHERE a.kind = 'index' AND a.target = '2026-11-05'"
            )?,
            asked_days(&next).contains(&"2026-11-05".to_owned()),
            reported,
        ),
        (
            vec![format!("permanent_failure|1|{}|3", 64 * 1_024 * 1_024)],
            false,
            Some(serde_json::json!(true)),
        )
    );
    Ok(())
}

/// A 200 carrying the vendor's own text is no index: permanent, never a quiet day, body kept.
/// Mutation 2026-11-27, `index_rows` returning `None` read as empty: red.
#[test]
fn a_200_that_is_no_index_is_a_permanent_failure_with_its_body_kept() -> TestResult {
    let temp = TempDir::new()?;
    let stub = Stub::sec().replacing(
        &index_url("2026-11-11"),
        Reply::Status(OK, NOT_FOUND_PAGE.to_vec()),
    );
    run(temp.path(), &stub, RUN)??;
    assert_eq!(
        rows(
            temp.path(),
            "SELECT verdict, raw_blob IS NOT NULL FROM attempts
              WHERE kind = 'index' AND target = '2026-11-11'"
        )?,
        vec!["permanent_failure|1".to_owned()]
    );
    Ok(())
}

/// Section 12.8 step 9b's refusal in this context: the attempt closed `not_sent` with no blob,
/// the run ended `gate_refused`, exit 7. Mutation 2026-11-27, `GateHeld` closed as a failed
/// answer: red.
#[test]
fn a_send_the_gate_refuses_is_closed_not_sent_and_the_run_ends_gate_refused() -> TestResult {
    let temp = TempDir::new()?;
    let stub = Stub::sec().replacing(&index_url("2026-11-10"), Reply::GateHeld);
    let refused = run(temp.path(), &stub, RUN)?;
    let Err(FilingsError::GateRefused { report, .. }) = refused else {
        return Err(format!("not refused at the gate: {refused:?}").into());
    };
    assert_eq!(
        (
            report["exit"]["code"].clone(),
            rows(
                temp.path(),
                "SELECT outcome, verdict, requested_at, raw_blob FROM attempts
                  WHERE kind = 'index' AND target = '2026-11-10'"
            )?,
            rows(temp.path(), "SELECT exit, stopped_by FROM runs")?,
            asked_days(&stub).len(),
        ),
        (
            serde_json::json!(7),
            vec!["not_sent|null|null|null".to_owned()],
            vec!["7|gate_refused".to_owned()],
            2,
        )
    );
    Ok(())
}

/// A reservation past the deadline opens no row and stops the run `deadline`, exit 8.
/// Mutation 2026-11-27, the reservation's answer ignored: red.
#[test]
fn a_reservation_past_the_deadline_opens_no_attempt_and_stops_the_run_deadline() -> TestResult {
    let temp = TempDir::new()?;
    let stub = Stub::sec().past_deadline(&index_url("2026-11-10"));
    let report = run(temp.path(), &stub, RUN)??;
    assert_eq!(
        (
            report.exit,
            report.value["stopped_by"].clone(),
            rows(
                temp.path(),
                "SELECT count(*) FROM attempts WHERE target = '2026-11-10'"
            )?,
            asked_days(&stub),
        ),
        (
            8,
            serde_json::json!("deadline"),
            vec!["0".to_owned()],
            vec!["2026-11-11".to_owned()],
        )
    );
    Ok(())
}

/// Mutation 2026-11-27, `allowed` not consulted: red.
#[test]
fn robots_that_disallow_the_archive_ask_nothing_else_and_exit_7() -> TestResult {
    let temp = TempDir::new()?;
    let stub = Stub::sec().replacing(
        ROBOTS_URL,
        Reply::Status(OK, b"User-agent: *\nDisallow: /Archives/edgar/\n".to_vec()),
    );
    let refused = run(temp.path(), &stub, RUN)?;
    assert_eq!(
        (
            refused
                .as_ref()
                .err()
                .map(|why| (why.code().name(), why.code().exit())),
            refused
                .err()
                .map(|why| why.object(&[])["details"]["reason"].clone()),
            stub.asked(),
            rows(temp.path(), "SELECT exit, stopped_by FROM runs")?,
        ),
        (
            Some(("robots_stopped", 7)),
            Some(serde_json::json!("disallowed")),
            vec![ROBOTS_URL.to_owned()],
            vec!["7|robots".to_owned()],
        )
    );
    Ok(())
}

/// A Crawl-delay at most 500 ms is paced; over it, an unparseable body or a 500 asks nothing more.
/// Mutations 2026-11-27, each red here: the Crawl-delay compared with 110 ms, or by `>=`; an
/// unparseable body read as allowing every path; a 500 read as no rules.
#[test]
fn a_robots_crawl_delay_over_500_ms_or_a_body_that_does_not_parse_asks_nothing() -> TestResult {
    let mut reasons = Vec::new();
    for (status, body) in [
        (OK, &b"User-agent: *\nCrawl-delay: 0.3\n"[..]),
        (OK, &b"User-agent: *\nCrawl-delay: 0.5\n"[..]),
        (OK, &b"User-agent: *\nCrawl-delay: 0.6\n"[..]),
        (OK, &b"<html><body>robots</body></html>"[..]),
        (500, &b""[..]),
    ] {
        let temp = TempDir::new()?;
        let stub = Stub::sec().replacing(ROBOTS_URL, Reply::Status(status, body.to_vec()));
        let refused = run(temp.path(), &stub, RUN)?;
        reasons.push((
            refused
                .err()
                .map(|why| why.object(&[])["details"]["reason"].clone()),
            stub.asked().len(),
        ));
    }
    assert_eq!(
        reasons,
        vec![
            (None, 7),
            (None, 7),
            (Some(serde_json::json!("crawl_delay_over_interval")), 1),
            (Some(serde_json::json!("unparseable")), 1),
            (Some(serde_json::json!("not_answered")), 1),
        ]
    );
    Ok(())
}

/// A robots.txt SEC does not have holds no rule. Mutation 2026-11-27, a 404 read as a stop: red.
#[test]
fn a_robots_404_lets_the_run_go_on() -> TestResult {
    let temp = TempDir::new()?;
    let stub = Stub::sec().replacing(ROBOTS_URL, Reply::Status(404, NOT_FOUND_PAGE.to_vec()));
    let report = run(temp.path(), &stub, RUN)??;
    assert_eq!(
        (
            report.exit,
            report.value["robots"]["verdict"].clone(),
            asked_days(&stub).len()
        ),
        (0, serde_json::json!("empty"), 5)
    );
    Ok(())
}

/// A past quarter whose listing came after its last day is complete and not asked again; the
/// current one always is. Mutation 2026-11-27, every quarter of the window asked: red.
#[test]
fn a_past_quarters_listing_received_after_its_end_is_not_asked_again() -> TestResult {
    let temp = TempDir::new()?;
    let q3 = listing_url("2026-09-30");
    let stub = || {
        Stub::sec()
            .answering(
                &q3,
                Reply::Status(OK, listing(&["2026-09-29", "2026-09-30"])),
            )
            .answering(
                &index_url("2026-09-29"),
                Reply::Status(OK, index_of("2026-09-29")),
            )
            .answering(
                &index_url("2026-09-30"),
                Reply::Status(OK, index_of("2026-09-30")),
            )
    };
    let mut first = support::ask(temp.path());
    first.rules = rules(60);
    let one = stub();
    run_ask(&first, &one, RUN)??;
    let two = stub();
    run_ask(&first, &two, NEXT_RUN)??;
    let listings = |stub: &Stub| {
        stub.asked()
            .into_iter()
            .filter(|url| url.ends_with("index.json"))
            .collect::<Vec<String>>()
    };
    assert_eq!(
        (listings(&one), listings(&two)),
        (
            vec![listing_url(LISTED[0]), q3],
            vec![listing_url(LISTED[0])]
        )
    );
    Ok(())
}

/// A past quarter is complete only on a `usable` listing: one naming no day is asked again
/// (review EF2). Mutation 2026-11-27, an `empty` listing completing its quarter: red here.
#[test]
fn a_past_quarters_empty_listing_does_not_complete_it() -> TestResult {
    let temp = TempDir::new()?;
    let q3 = listing_url("2026-09-30");
    let stub = || Stub::sec().answering(&q3, Reply::Status(OK, listing(&[])));
    let mut first = support::ask(temp.path());
    first.rules = rules(60);
    run_ask(&first, &stub(), RUN)??;
    let two = stub();
    run_ask(&first, &two, NEXT_RUN)??;
    let listings: Vec<String> = two
        .asked()
        .into_iter()
        .filter(|url| url.ends_with("index.json"))
        .collect();
    assert_eq!(listings, vec![listing_url(LISTED[0]), q3]);
    Ok(())
}

/// The first days of a quarter: its trailing three reach into the previous one through that
/// quarter's listing. Mutation 2026-11-27, the previous quarter never asked: red.
#[test]
fn the_trailing_three_index_days_cross_a_quarter_through_both_listings() -> TestResult {
    let temp = TempDir::new()?;
    let stub = Stub::default()
        .answering(ROBOTS_URL, Reply::Status(OK, ROBOTS.to_vec()))
        .answering(
            &listing_url("2026-10-01"),
            Reply::Status(OK, listing(&["2026-10-01"])),
        )
        .answering(
            &listing_url("2026-09-30"),
            Reply::Status(OK, listing(&["2026-09-29", "2026-09-30"])),
        );
    let stub = ["2026-10-01", "2026-09-30", "2026-09-29"]
        .into_iter()
        .fold(stub, |stub, text| {
            stub.answering(&index_url(text), Reply::Status(OK, index_of(text)))
        });
    let mut asked = support::ask(temp.path());
    asked.rules = rules(3);
    run_ask(&asked, &stub, "2026-10-02T06:10:00Z")??;
    assert_eq!(
        asked_days(&stub),
        vec!["2026-10-01", "2026-09-30", "2026-09-29"]
    );
    Ok(())
}

/// Newest first, back to the fill's start: a day before it is never asked. Mutations 2026-11-27,
/// each red here: oldest first; `fill_days` ignored.
#[test]
fn unasked_days_are_asked_newest_first_back_to_the_fill_start() -> TestResult {
    let temp = TempDir::new()?;
    let stub = Stub::sec();
    let mut asked = support::ask(temp.path());
    asked.rules = rules(7);
    run_ask(&asked, &stub, RUN)??;
    assert_eq!(
        asked_days(&stub),
        vec![
            "2026-11-11",
            "2026-11-10",
            "2026-11-09",
            "2026-11-06",
            "2026-11-05"
        ]
    );
    let short = TempDir::new()?;
    let stub = Stub::sec();
    let mut asked = support::ask(short.path());
    asked.rules = rules(5);
    run_ask(&asked, &stub, RUN)??;
    assert_eq!(
        asked_days(&stub),
        vec!["2026-11-11", "2026-11-10", "2026-11-09"]
    );
    Ok(())
}

/// A day asked under a narrower list is asked again under the wider one. Mutation 2026-11-27,
/// `forms` not compared: red.
#[test]
fn a_day_asked_under_a_narrower_form_list_is_asked_again() -> TestResult {
    let temp = TempDir::new()?;
    let mut narrow = support::ask(temp.path());
    narrow.rules.forms = vec!["10-K".to_owned()];
    run_ask(&narrow, &Stub::sec(), RUN)??;
    let wide = Stub::sec();
    run_ask(&support::ask(temp.path()), &wide, "2026-11-16T06:10:00Z")??;
    assert!(asked_days(&wide).contains(&"2026-11-05".to_owned()));
    Ok(())
}

/// Section 12.2's `blobs` row: the size the write left on disk beside the payload's.
/// Mutation 2026-11-27, `stored_bytes` written from the payload's length: red.
#[test]
fn a_blob_row_records_the_size_the_write_left() -> TestResult {
    let temp = TempDir::new()?;
    run(temp.path(), &Stub::sec(), RUN)??;
    let writer = Writer::open(temp.path(), &RunId::new("filings-read"))?;
    let mut statement = writer
        .db()
        .prepare("SELECT digest, bytes, stored_bytes FROM blobs")?;
    let blobs = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut checked = 0;
    for (digest, bytes, stored) in &blobs {
        let Some(named) = writer.blobs().named(digest) else {
            return Err(format!("no file for {digest}").into());
        };
        let on_disk = std::fs::metadata(writer.blobs().compressed_path_of(&named))?.len();
        assert_eq!(
            (u64::try_from(*stored)?, *bytes > 0),
            (on_disk, true),
            "{digest}"
        );
        checked += 1;
    }
    assert_eq!(checked, blobs.len());
    assert!(checked >= 3);
    Ok(())
}

/// An error mid-run leaves no attempt and no run open, and an answer whose blob write failed keeps
/// its status (review of 2026-11-27, PIT-4 and PIT-5). Mutations 2026-11-27, each red here: the
/// attempt not abandoned after a budget error, a missing credential or a failed blob write.
#[test]
fn an_error_mid_run_closes_its_attempt_and_its_run() -> TestResult {
    let mut seen = Vec::new();
    for case in ["budget", "credential", "blob"] {
        let temp = TempDir::new()?;
        let stub = match case {
            "budget" => Stub::sec().replacing(ROBOTS_URL, Reply::Budget),
            "credential" => Stub::sec().replacing(ROBOTS_URL, Reply::NoCredential),
            _ => {
                std::fs::write(temp.path().join("blobs"), b"")?;
                Stub::sec()
            }
        };
        let code = run(temp.path(), &stub, RUN)?
            .err()
            .map(|why| why.code().name());
        seen.push((
            code,
            rows(temp.path(), "SELECT kind, outcome, status FROM attempts")?,
            rows(
                temp.path(),
                "SELECT exit, stopped_by, finished_at IS NOT NULL FROM runs",
            )?,
        ));
    }
    let closed = |code: &'static str, attempt: &str| {
        (
            Some(code),
            vec![attempt.to_owned()],
            vec!["2|error|1".to_owned()],
        )
    };
    assert_eq!(
        seen,
        vec![
            closed("io_failed", "robots|interrupted|null"),
            closed("credential_contact_missing", "robots|interrupted|null"),
            closed("io_failed", "robots|failed|200"),
        ]
    );
    Ok(())
}

/// Section 12.2's settled quarter, and the current one as of the day before the run: a past
/// quarter's listing is asked once more, on the first run over the QS-3 re-ask point of 1 day
/// (owner decision 4 of 2026-11-27). Mutations 2026-11-27, each red here: completeness from the
/// run's date; `>=`; `QUARTER_SETTLE_DAYS` 0 or 3; the current quarter from the run's own date.
#[test]
fn a_past_quarter_is_listed_again_until_a_listing_comes_a_day_after_its_end() -> TestResult {
    let temp = TempDir::new()?;
    let q4 = ["2026-12-29", "2026-12-30", "2026-12-31"];
    let q1 = ["2027-01-02"];
    let stub = || {
        let stub = Stub::default()
            .answering(ROBOTS_URL, Reply::Status(OK, ROBOTS.to_vec()))
            .answering(&listing_url(q4[0]), Reply::Status(OK, listing(&q4)))
            .answering(&listing_url(q1[0]), Reply::Status(OK, listing(&q1)));
        q4.iter().chain(&q1).fold(stub, |stub, text| {
            stub.answering(&index_url(text), Reply::Status(OK, index_of(text)))
        })
    };
    let mut listed = Vec::new();
    for at in [
        "2026-12-30",
        "2027-01-01",
        "2027-01-03",
        "2027-01-04",
        "2027-01-05",
    ] {
        let asked = stub();
        run(temp.path(), &asked, &format!("{at}T06:10:00Z"))??;
        listed.push(
            asked
                .asked()
                .into_iter()
                .filter(|url| url.ends_with("index.json"))
                .collect::<Vec<String>>(),
        );
    }
    let (q4, q1) = (listing_url(q4[0]), listing_url(q1[0]));
    assert_eq!(
        listed,
        vec![
            vec![q4.clone()],
            vec![q4.clone()],
            vec![q1.clone(), q4],
            vec![q1.clone()],
            vec![q1],
        ]
    );
    Ok(())
}

/// A day the listing shows written after the `Last-Modified` of its stored answer is asked
/// again, once, and the row SEC added is held (review QS-2): 11-12 rewritten on 11-18, after it
/// left the trailing three. Mutation 2026-11-27, the rewritten days not planned: red here.
#[test]
fn a_day_rewritten_after_its_stored_answer_is_asked_again_once() -> TestResult {
    let temp = TempDir::new()?;
    let first: Vec<&str> = LISTED.iter().copied().chain(["2026-11-12"]).collect();
    let dated = |stub: Stub, body: Vec<u8>, header: &'static str| {
        stub.replacing(&index_url("2026-11-12"), Reply::Dated(OK, body, header))
    };
    let one = dated(
        listing_of(&first),
        index_of("2026-11-12"),
        "Fri, 13 Nov 2026 03:00:00 GMT",
    );
    run(temp.path(), &one, NEXT_RUN)??;
    let later = [
        &first[..],
        &["2026-11-13", "2026-11-16", "2026-11-17", "2026-11-18"],
    ]
    .concat();
    let mut written: Vec<(&str, String)> = later
        .iter()
        .map(|text| {
            (
                *text,
                format!("{}/{}/2026 10:00:00 PM", &text[5..7], &text[8..]),
            )
        })
        .collect();
    written[5].1 = "11/18/2026 09:00:00 PM".to_owned();
    let added = "10-K             LATE ADDED CORP                                               \
                 1112223     20261112    edgar/data/1112223/0001112223-26-000001.txt\n";
    let rewritten = [index_of("2026-11-12"), added.as_bytes().to_vec()].concat();
    let stub = || {
        dated(
            listing_of(&later).replacing(
                &listing_url(LISTED[0]),
                Reply::Status(OK, listing_written(&written)),
            ),
            rewritten.clone(),
            "Thu, 19 Nov 2026 02:00:00 GMT",
        )
    };
    let (two, three) = (stub(), stub());
    run(temp.path(), &two, "2026-11-19T06:10:00Z")??;
    run(temp.path(), &three, "2026-11-20T06:10:00Z")??;
    assert_eq!(
        (
            asked_days(&two),
            asked_days(&three),
            rows(
                temp.path(),
                "SELECT count(*) FROM listings WHERE accession = '0001112223-26-000001'"
            )?,
        ),
        (
            [
                "2026-11-18",
                "2026-11-17",
                "2026-11-16",
                "2026-11-13",
                "2026-11-12"
            ]
            .map(str::to_owned)
            .to_vec(),
            ["2026-11-18", "2026-11-17", "2026-11-16"]
                .map(str::to_owned)
                .to_vec(),
            vec!["1".to_owned()],
        )
    );
    Ok(())
}

/// The planner keeps a day an older stored listing named when a newer stored one omits it and
/// this run's listing fails. Mutation 2026-11-27, `Window::read` replacing the held days: red.
#[test]
fn the_planner_keeps_a_day_only_an_older_stored_listing_names() -> TestResult {
    let temp = TempDir::new()?;
    run(temp.path(), &listing_of(&LISTED), RUN)??;
    let later = [
        "2026-11-05",
        "2026-11-06",
        "2026-11-09",
        "2026-11-10",
        "2026-11-11",
        "2026-11-12",
        "2026-11-13",
        "2026-11-16",
    ];
    let failing = listing_of(&later).replacing(&index_url("2026-11-13"), Reply::Transport);
    drop(run(temp.path(), &failing, "2026-11-17T06:10:00Z")?);
    let omits = Stub::default()
        .answering(ROBOTS_URL, Reply::Status(OK, ROBOTS.to_vec()))
        .answering(
            &listing_url(LISTED[0]),
            Reply::Status(OK, listing(&["2026-11-12", "2026-11-16", "2026-11-17"])),
        );
    drop(run(temp.path(), &omits, "2026-11-18T06:10:00Z")?);
    let blind = listing_of(&later).replacing(&listing_url(LISTED[0]), Reply::Transport);
    drop(run(temp.path(), &blind, "2026-11-19T06:10:00Z")?);
    assert!(asked_days(&blind).contains(&"2026-11-13".to_owned()));
    Ok(())
}

/// A past quarter's listing received the day after its end does not complete it, and the next
/// day's does: the QS-3 value 1. Mutation 2026-11-27, `QUARTER_SETTLE_DAYS` 2: red here.
#[test]
fn a_past_quarter_completes_on_a_listing_received_two_days_after_its_end() -> TestResult {
    let temp = TempDir::new()?;
    let q4 = ["2026-12-29", "2026-12-30", "2026-12-31"];
    let q1 = ["2027-01-02"];
    let stub = || {
        let stub = Stub::default()
            .answering(ROBOTS_URL, Reply::Status(OK, ROBOTS.to_vec()))
            .answering(&listing_url(q4[0]), Reply::Status(OK, listing(&q4)))
            .answering(&listing_url(q1[0]), Reply::Status(OK, listing(&q1)));
        q4.iter().chain(&q1).fold(stub, |stub, text| {
            stub.answering(&index_url(text), Reply::Status(OK, index_of(text)))
        })
    };
    let mut last = Vec::new();
    for at in ["2026-12-30", "2027-01-02", "2027-01-03"] {
        let asked = stub();
        run(temp.path(), &asked, &format!("{at}T06:10:00Z"))??;
        last = asked
            .asked()
            .into_iter()
            .filter(|url| url.ends_with("index.json"))
            .collect::<Vec<String>>();
    }
    assert_eq!(last, vec![listing_url(q1[0])]);
    Ok(())
}

/// A rewrite is found from the day's newest usable answer when a later re-ask of it failed.
/// Mutation 2026-11-27, the failed attempt's headers compared: red here.
#[test]
fn a_rewrite_after_a_failed_reask_is_asked() -> TestResult {
    let temp = TempDir::new()?;
    let dated = listing_of(&LISTED).replacing(
        &index_url("2026-11-10"),
        Reply::Dated(OK, index_of("2026-11-10"), "Wed, 11 Nov 2026 03:00:00 GMT"),
    );
    run(temp.path(), &dated, RUN)??;
    let named: Vec<&str> = LISTED.iter().copied().chain(["2026-11-12"]).collect();
    let failing = listing_of(&named).replacing(&index_url("2026-11-10"), Reply::Transport);
    drop(run(temp.path(), &failing, NEXT_RUN)?);
    let later = [
        "2026-11-05",
        "2026-11-06",
        "2026-11-09",
        "2026-11-10",
        "2026-11-11",
        "2026-11-12",
        "2026-11-13",
        "2026-11-16",
    ];
    let written: Vec<(&str, String)> = later
        .iter()
        .map(|text| match *text {
            "2026-11-10" => (*text, "11/16/2026 10:00:00 PM".to_owned()),
            _ => (
                *text,
                format!("{}/{}/2026 10:00:00 PM", &text[5..7], &text[8..]),
            ),
        })
        .collect();
    let rewritten = listing_of(&later).replacing(
        &listing_url(LISTED[0]),
        Reply::Status(OK, listing_written(&written)),
    );
    drop(run(temp.path(), &rewritten, "2026-11-17T06:10:00Z")?);
    assert!(asked_days(&rewritten).contains(&"2026-11-10".to_owned()));
    Ok(())
}

/// A listing naming a `form.*.idx` of a day outside its own quarter is refused, not read
/// (review 2026-11-27, N2: `form.20271231.idx` in 2026 Q4's listing hid its two missing last
/// days, exit 0). Mutation 2026-11-27, the quarter check dropped: red here.
#[test]
fn a_listing_naming_a_day_of_another_quarter_is_a_permanent_failure() -> TestResult {
    let temp = TempDir::new()?;
    let stray = [&LISTED[..], &["2027-12-31"]].concat();
    let stub =
        listing_of(&LISTED).replacing(&listing_url(LISTED[0]), Reply::Status(OK, listing(&stray)));
    drop(run(temp.path(), &stub, RUN)?);
    assert_eq!(
        (
            rows(
                temp.path(),
                "SELECT verdict FROM attempts WHERE kind = 'listing'"
            )?,
            asked_days(&stub),
        ),
        (vec!["permanent_failure".to_owned()], Vec::<String>::new())
    );
    Ok(())
}

/// The listings of `days` before each night's run, 11-12 listed written at `written_12`.
fn nights_listing_12(night: &str, written_12: &str) -> Stub {
    let days = [
        "2026-11-05",
        "2026-11-06",
        "2026-11-09",
        "2026-11-10",
        "2026-11-12",
        "2026-11-13",
        "2026-11-16",
        "2026-11-17",
        "2026-11-18",
        "2026-11-19",
    ];
    let named: Vec<(&str, String)> = days
        .iter()
        .filter(|text| **text < night)
        .map(|text| match *text {
            "2026-11-12" => (*text, written_12.to_owned()),
            _ => (
                *text,
                format!("{}/{}/2026 10:00:00 PM", &text[5..7], &text[8..]),
            ),
        })
        .collect();
    let names: Vec<&str> = named.iter().map(|(text, _)| *text).collect();
    listing_of(&names).replacing(
        &listing_url(LISTED[0]),
        Reply::Status(OK, listing_written(&named)),
    )
}

/// A listed write that stays 1 s after the stored `Last-Modified` asks the day again only until
/// an answer is received after the write (review 2026-11-27, N4: asked on all 13 nights).
/// Mutation 2026-11-27, the receive instant not compared: red here.
#[test]
fn a_listed_write_never_matched_by_the_header_is_not_asked_every_night() -> TestResult {
    let temp = TempDir::new()?;
    let mut nights = Vec::new();
    for night in day("2026-11-13")
        .iter_days()
        .take_while(|on| *on <= day("2026-11-20"))
    {
        let night = night.format("%Y-%m-%d").to_string();
        let stub = nights_listing_12(&night, "11/12/2026 10:00:01 PM").replacing(
            &index_url("2026-11-12"),
            Reply::Dated(OK, index_of("2026-11-12"), "Fri, 13 Nov 2026 03:00:00 GMT"),
        );
        drop(run(temp.path(), &stub, &format!("{night}T06:10:00Z"))?);
        if asked_days(&stub).contains(&"2026-11-12".to_owned()) {
            nights.push(night);
        }
    }
    assert_eq!(
        nights,
        [
            "2026-11-13",
            "2026-11-14",
            "2026-11-15",
            "2026-11-16",
            "2026-11-17"
        ]
        .map(str::to_owned)
    );
    Ok(())
}

/// A stored `Last-Modified` that does not parse does not hide a rewrite: the receive instant is
/// compared (review 2026-11-27, F2: never asked again). Mutation 2026-11-27, an unparseable
/// header read as "not rewritten": red here.
#[test]
fn a_rewrite_after_an_answer_whose_last_modified_does_not_parse_is_asked() -> TestResult {
    let temp = TempDir::new()?;
    let first = nights_listing_12("2026-11-13", "11/12/2026 10:00:00 PM").replacing(
        &index_url("2026-11-12"),
        Reply::Dated(OK, index_of("2026-11-12"), "yesterday"),
    );
    run(temp.path(), &first, NEXT_RUN)??;
    let rewritten = nights_listing_12("2026-11-20", "11/18/2026 09:00:00 PM");
    run(temp.path(), &rewritten, "2026-11-20T06:10:00Z")??;
    assert!(asked_days(&rewritten).contains(&"2026-11-12".to_owned()));
    Ok(())
}

/// One failed request is enough for exit 8. Mutation 2026-11-27, exit 8 from two failures: red.
#[test]
fn one_failed_request_makes_the_run_partial() -> TestResult {
    let temp = TempDir::new()?;
    let stub = Stub::sec().replacing(&index_url("2026-11-10"), Reply::Transport);
    assert_eq!(run(temp.path(), &stub, RUN)??.exit, 8);
    Ok(())
}

/// Robots is read for every path asked, the index files too. Mutation 2026-11-27, robots
/// consulted for the listing only: red.
#[test]
fn robots_disallowing_only_the_index_files_asks_no_index() -> TestResult {
    let temp = TempDir::new()?;
    let stub = Stub::sec().replacing(
        ROBOTS_URL,
        Reply::Status(
            OK,
            b"User-agent: *\nDisallow: /Archives/edgar/daily-index/2026/QTR4/form.\n".to_vec(),
        ),
    );
    let refused = run(temp.path(), &stub, RUN)?
        .err()
        .map(|why| why.code().name());
    assert_eq!(
        (refused, asked_days(&stub).len()),
        (Some("robots_stopped"), 0)
    );
    Ok(())
}

/// A 200 listing that is no listing is a permanent failure and never held, so the next run
/// plans as usual. Mutations 2026-11-27, each red here: it read `empty`; any verdict held.
#[test]
fn a_listing_that_does_not_parse_does_not_stop_the_next_run() -> TestResult {
    let temp = TempDir::new()?;
    let bad = Stub::sec().replacing(
        &listing_url(LISTED[0]),
        Reply::Status(OK, b"<html>".to_vec()),
    );
    drop(run(temp.path(), &bad, RUN)?);
    let next = Stub::sec();
    let report = run(temp.path(), &next, NEXT_RUN)??;
    assert_eq!((report.exit, asked_days(&next).len()), (0, 5));
    Ok(())
}

/// Each attempt's instants are in order, from the run's start: opened, requested, received,
/// closed. Mutations 2026-11-27, each red here: closed at the open instant; `requested_at` and
/// `received_at` swapped or made one; `opened_at` a day early.
#[test]
fn an_attempts_instants_follow_one_another_from_the_runs_start() -> TestResult {
    let temp = TempDir::new()?;
    run(temp.path(), &Stub::sec(), RUN)??;
    let out_of_order = rows(
        temp.path(),
        "SELECT a.target FROM attempts a JOIN runs r ON r.run_id = a.run_id
          WHERE NOT (r.started_at <= a.opened_at AND a.opened_at < a.requested_at
            AND a.requested_at < a.received_at AND a.received_at < a.closed_at)",
    )?;
    let all = rows(temp.path(), "SELECT count(*) FROM attempts")?;
    assert_eq!(
        (out_of_order, all),
        (Vec::<String>::new(), vec!["7".to_owned()])
    );
    Ok(())
}

/// First sighting wins: a row listed again with another form and filed day keeps the first.
/// Mutation 2026-11-27, the conflict overwriting `form` and `filed_day`: red.
#[test]
fn a_row_listed_again_keeps_its_first_form_and_filed_day() -> TestResult {
    let temp = TempDir::new()?;
    let day = "SELECT cik, accession, form, filed_day, first_seq FROM listings
                WHERE index_day = '2026-11-11' ORDER BY cik, accession";
    run(temp.path(), &Stub::sec(), RUN)??;
    let first = rows(temp.path(), day)?;
    let changed = String::from_utf8(index_of("2026-11-11"))?
        .replace("10-K/A ", "10-K   ")
        .replace("20261111", "20261109");
    let again = Stub::sec().replacing(
        &index_url("2026-11-11"),
        Reply::Status(OK, changed.into_bytes()),
    );
    run(temp.path(), &again, NEXT_RUN)??;
    assert_eq!((rows(temp.path(), day)?, first.len()), (first, 10));
    Ok(())
}

/// A gate refusal owed past the deadline ends the run `deadline`, exit 8, its attempt not sent.
/// Mutation 2026-11-27, it read as `gate_refused`: red.
#[test]
fn a_gate_owing_past_the_deadline_stops_the_run_deadline() -> TestResult {
    let temp = TempDir::new()?;
    let stub = Stub::sec().replacing(&index_url("2026-11-10"), Reply::GateOwed);
    let report = run(temp.path(), &stub, RUN)??;
    assert_eq!(
        (
            report.exit,
            report.value["stopped_by"].clone(),
            rows(
                temp.path(),
                "SELECT outcome FROM attempts WHERE target = '2026-11-10'"
            )?,
        ),
        (
            8,
            serde_json::json!("deadline"),
            vec!["not_sent".to_owned()]
        )
    );
    Ok(())
}

/// A dry run over a store plans the listings and the days a run would ask: the trailing three.
/// Mutations 2026-11-27, each red here: the dry run planning no day; `TRAILING_DAYS` 2 or 4.
#[test]
fn a_dry_run_over_a_store_plans_the_trailing_three_days() -> TestResult {
    let temp = TempDir::new()?;
    run(temp.path(), &Stub::sec(), RUN)??;
    let mut dry = support::ask(temp.path());
    dry.dry_run = true;
    let stub = Stub::sec();
    let report = run_ask(&dry, &stub, NEXT_RUN)??.value;
    assert_eq!(
        (
            report["store"].clone(),
            report["planned"].clone(),
            stub.asked().len()
        ),
        (
            serde_json::json!("held"),
            serde_json::json!({
                "listings": ["2026/QTR4"],
                "days": ["2026-11-11", "2026-11-10", "2026-11-09"],
            }),
            0
        )
    );
    Ok(())
}
