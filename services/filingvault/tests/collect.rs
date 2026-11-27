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
            vec![format!("1|{0}|{0}", first.value["run_id"].as_str().unwrap_or_default())],
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
    run(temp.path(), &stub, RUN)??;
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
        ),
        (
            vec![format!("permanent_failure|1|{}|3", 64 * 1_024 * 1_024)],
            false
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

/// A Crawl-delay at most our 500 ms is honoured by pace; above it, and a body that does not
/// parse, the run asks nothing more. Mutations 2026-11-27, each red here: the Crawl-delay
/// compared with 110 ms; a body that does not parse read as allowing every path.
#[test]
fn a_robots_crawl_delay_over_500_ms_or_a_body_that_does_not_parse_asks_nothing() -> TestResult {
    let mut reasons = Vec::new();
    for body in [
        &b"User-agent: *\nCrawl-delay: 0.3\n"[..],
        &b"User-agent: *\nCrawl-delay: 0.6\n"[..],
        &b"<html><body>robots</body></html>"[..],
    ] {
        let temp = TempDir::new()?;
        let stub = Stub::sec().replacing(ROBOTS_URL, Reply::Status(OK, body.to_vec()));
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
            (Some(serde_json::json!("crawl_delay_over_interval")), 1),
            (Some(serde_json::json!("unparseable")), 1),
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
