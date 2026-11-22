//! `newsvault cut`, the §3.0 Snapshot: which rows a cut holds, how a cut is rebuilt, and each
//! refusal by its §5.1 code. Every row is synthetic: the repository is public.

use std::cell::RefCell;
use std::error::Error;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use canon::digest::{item_id, story_id, ItemFields};
use canon::{FileDigest, Stamp};
use chrono::{DateTime, NaiveDate, Utc};
use news_provider::{
    AttemptSlot, Baseline, Counts, Outcome, Provenance, RawResponse, Request, RequestPhase, RunId,
    SourceId, TargetWindow, ValidSource, Verdict,
};
use newsvault::cmd_cut::{self, Ask, CutError, Report};
use newsvault::cmd_items::{self, PerStory};
use newsvault::publish::{Durability, Fsync};
use newsvault::store::{blob_store, Committer, ItemRow, COMMIT_LOG};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use pit::datedness::{Minute, Published};
use pit::{ObservationTime, ValidTime};
use rusqlite::Connection;
use serde_json::Value;
use snapshot_contract::{validate, DATA_FILE, MANIFEST_FILE};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const RUN: &str = "2026-07-06T00:00:00Z";
const SOURCE: &str = "open_web_wires";
const WINDOW: &str = "2026-07-01";
const URL: &str = "https://example.test/release";
const TITLE: &str = "A synthetic release";
const PAYLOAD: &[u8] = b"<rss>synthetic</rss>";
const ROSTER: &[u8] = b"{}";
const STATUS_OK: u16 = 200;
const NEW_YORK: &str = "America/New_York";
/// The cutoff every test below cuts at, and the clock's now in `ask`.
const CUTOFF: &str = "2026-07-05T00:00:00Z";
const NOW: &str = "2026-07-06T00:00:00Z";

/// One synthetic item to commit: its story, publication value and observation.
struct Seed {
    story: &'static str,
    published: Published,
    observed: &'static str,
    body: Option<&'static str>,
    provenance: Provenance,
}

fn at(text: &str) -> TestResult<DateTime<Utc>> {
    Ok(text.parse()?)
}

fn instant(
    published: &'static str,
    observed: &'static str,
    story: &'static str,
) -> TestResult<Seed> {
    Ok(Seed {
        story,
        published: Published::At(ValidTime::new(at(published)?)),
        observed,
        body: Some("synthetic body"),
        provenance: Provenance::LiveIngest,
    })
}

fn stamp_of(published: Published) -> Stamp {
    match published {
        Published::At(valid) => Stamp::Instant(valid.inner()),
        Published::AtMinute(minute) => Stamp::Minute(minute.start()),
        Published::On { day, .. } => Stamp::Day(day),
        Published::Unknown => Stamp::Day(NaiveDate::MIN),
    }
}

/// One item under one closed attempt, the only door into the archive.
fn commit(store: &mut Committer, ordinal: u32, seed: &Seed) -> TestResult {
    let run = RunId::new(store.run().as_str());
    let observed = ObservationTime::new(at(seed.observed)?);
    let request = Request {
        source: SourceId::new(SOURCE),
        target_window: TargetWindow::new(WINDOW),
        phase: RequestPhase::Index,
        provenance: seed.provenance,
        url: URL.to_owned(),
        headers: Vec::new(),
        params: Vec::new(),
        universe_version: FileDigest::of(ROSTER),
        listed_date: None,
        names_ticker: false,
        hosts_version: None,
        extract_version: None,
    };
    let slot = AttemptSlot::open(store, &request, None, &run, ordinal, observed)?;
    let answer = RawResponse::new(
        slot.attempt_id().clone(),
        STATUS_OK,
        PAYLOAD.to_vec(),
        observed,
        observed,
    );
    let outcome = Outcome::decide(
        &Verdict::Usable,
        Counts {
            vendor_rows: 1,
            listing_unusable: 0,
            robots_disallowed: 0,
        },
        &Baseline::from_trailing_ok(&[1, 1, 1]),
    );
    let closed = slot.close(store, outcome, &Ok(answer), observed)?;
    let body_blob = match seed.body {
        Some(text) => Some(store.put_blob(text.as_bytes())?),
        None => None,
    };
    let row = ItemRow {
        item_id: item_id(&ItemFields {
            source: SOURCE,
            source_item_id: seed.story,
            url: URL,
            valid: stamp_of(seed.published),
            title: TITLE,
            body_ref: body_blob.as_ref().map(|blob| blob.hex()),
        })
        .map_err(|why| why.to_string())?,
        story_id: story_id(SOURCE, seed.story).map_err(|why| why.to_string())?,
        doc_id: None,
        valid: seed.published,
        valid_raw: "synthetic".to_owned(),
        valid_field: "pubDate".to_owned(),
        valid_source: ValidSource::VendorPublished,
        observed,
        raw_blob: store.put_blob(PAYLOAD)?,
        body_blob,
    };
    store.record(closed, None, &[row], observed)?;
    Ok(())
}

/// Commits `seeds` under `store`, which the caller keeps open: a second `Committer::open` in one
/// test answered `LeaseHeld` once in a full `cargo test` run (macOS, 2026-11-22), cause unknown.
fn seeded(store: &mut Committer, root: &Path, seeds: &[Seed]) -> TestResult {
    let base = ordinals(root)?;
    for (ordinal, seed) in (base..).zip(seeds) {
        commit(store, ordinal, seed)?;
    }
    Ok(())
}

/// Ordinals already used in this store's one run, so a second seeding opens new attempts.
fn ordinals(root: &Path) -> TestResult<u32> {
    let log = root.join(COMMIT_LOG);
    if !log.exists() {
        return Ok(0);
    }
    let count: i64 =
        Connection::open(log)?.query_row("SELECT count(*) FROM attempts", [], |row| row.get(0))?;
    Ok(u32::try_from(count)?)
}

/// Five rows: visible as an instant, a minute and a New York day; a refused one; one observed
/// after the cutoff. The store stays open for a test that commits more.
fn five(root: &Path) -> TestResult<Committer> {
    let mut store = Committer::open(root, &RunId::new(RUN))?;
    let minute = Minute::new(at("2026-07-02T19:00:00Z")?)?;
    seeded(
        &mut store,
        root,
        &[
            instant(
                "2026-07-01T13:30:00Z",
                "2026-07-01T13:31:00Z",
                "visible-instant",
            )?,
            Seed {
                story: "visible-minute",
                published: Published::AtMinute(minute),
                observed: "2026-07-02T19:00:30Z",
                body: None,
                provenance: Provenance::LiveIngest,
            },
            Seed {
                story: "visible-day",
                published: Published::On {
                    day: "2026-07-02".parse()?,
                    tz: NEW_YORK,
                },
                observed: "2026-07-02T15:00:00Z",
                body: Some("synthetic day body"),
                provenance: Provenance::VendorArchive,
            },
            instant("2026-07-03T12:00:00Z", "2026-07-03T11:00:00Z", "refused")?,
            instant(
                "2026-07-04T12:00:00Z",
                "2026-07-05T00:00:01Z",
                "observed-after",
            )?,
        ],
    )?;
    Ok(store)
}

fn ask(root: &Path, out: &Path) -> TestResult<Ask> {
    Ok(Ask {
        store: root.to_path_buf(),
        out: Some(out.to_path_buf()),
        at: at(CUTOFF)?,
        now: at(NOW)?,
        through_seq: None,
        dry_run: false,
    })
}

fn cut(ask: &Ask) -> Result<Report, CutError> {
    cmd_cut::run(ask, &Fsync)
}

fn data_seqs(dir: &Path) -> TestResult<Vec<i64>> {
    let reader =
        ParquetRecordBatchReaderBuilder::try_new(File::open(dir.join(DATA_FILE))?)?.build()?;
    let mut seqs = Vec::new();
    for batch in reader {
        let batch = batch?;
        let Some(column) = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
        else {
            return Err("seq is not Int64".into());
        };
        seqs.extend(column.values().iter().copied());
    }
    Ok(seqs)
}

fn items_seqs(root: &Path, cutoff: &str) -> TestResult<Vec<i64>> {
    let report = cmd_items::read(
        root,
        &cmd_items::Ask {
            at: at(cutoff)?,
            from: None,
            to: None,
            source: None,
            limit: None,
            fields: None,
            per_story: PerStory::All,
        },
    )?;
    Ok(report
        .items
        .iter()
        .filter_map(|item| item["seq"].as_i64())
        .collect())
}

fn the_path(report: &Report) -> TestResult<PathBuf> {
    Ok(PathBuf::from(report.path.as_deref().ok_or("no path")?))
}

fn binary(root: &Path, args: &[&str]) -> TestResult<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .arg("cut")
        .arg("--store")
        .arg(root)
        .args(args)
        .args(["--format", "json"])
        .stdin(Stdio::null())
        .output()?)
}

/// The refusal's code and exit, and that stdout stayed empty.
fn refusal(output: &Output) -> TestResult<(Option<i32>, String, bool, Value)> {
    let error: Value = serde_json::from_slice(&output.stderr)?;
    Ok((
        output.status.code(),
        error["code"].as_str().unwrap_or_default().to_owned(),
        output.stdout.is_empty(),
        error["details"].clone(),
    ))
}

/// Mutations 2026-11-22, each red: the `seq` bound dropped from the chunk query; the day arm
/// closing at the day's last microsecond, the `>=` R13 named, which admits the day row at a
/// cutoff of its last microsecond (the second half below).
#[test]
fn a_cut_holds_exactly_the_rows_items_returns_at_the_same_at_through_its_high_water() -> TestResult
{
    let root = TempDir::new()?;
    let mut store = five(root.path())?;
    seeded(
        &mut store,
        root.path(),
        &[instant(
            "2026-07-01T09:00:00Z",
            "2026-07-01T09:01:00Z",
            "above-the-bound",
        )?],
    )?;
    drop(store);
    let out = TempDir::new()?;
    let report = cut(&Ask {
        through_seq: Some(5),
        ..ask(root.path(), out.path())?
    })?;
    let through_five: Vec<i64> = items_seqs(root.path(), CUTOFF)?
        .into_iter()
        .filter(|seq| *seq <= 5)
        .collect();
    assert_eq!(data_seqs(&the_path(&report)?)?, through_five);
    assert_eq!(through_five, vec![1, 2, 3]);
    assert_eq!(
        (
            report.items,
            report.stories,
            report.seq_high_water,
            report.changed
        ),
        (3, 3, 5, true)
    );
    let mut at_each = Vec::new();
    let mut items_each = Vec::new();
    for cutoff in ["2026-07-03T03:59:59.999999Z", "2026-07-03T04:00:00Z"] {
        let dry = cut(&Ask {
            at: at(cutoff)?,
            through_seq: Some(5),
            dry_run: true,
            out: None,
            ..ask(root.path(), out.path())?
        })?;
        at_each.push(dry.items);
        items_each.push(
            items_seqs(root.path(), cutoff)?
                .into_iter()
                .filter(|seq| *seq <= 5)
                .count(),
        );
    }
    assert_eq!((at_each, items_each), (vec![2, 3], vec![2, 3]));
    Ok(())
}

/// Mutation 2026-11-22, `--through-seq` ignored in favour of the store's highest seq: red.
#[test]
fn a_row_committed_after_the_cut_with_an_observation_before_it_is_absent_from_a_rebuild_at_the_recorded_high_water(
) -> TestResult {
    let root = TempDir::new()?;
    let mut store = five(root.path())?;
    let out = TempDir::new()?;
    let first = cut(&ask(root.path(), out.path())?)?;
    seeded(
        &mut store,
        root.path(),
        &[instant(
            "2026-07-04T10:00:00Z",
            "2026-07-04T10:05:00Z",
            "late-commit",
        )?],
    )?;
    drop(store);
    let rebuilt = cut(&Ask {
        through_seq: Some(first.seq_high_water),
        dry_run: true,
        ..ask(root.path(), out.path())?
    })?;
    let grown = cut(&Ask {
        dry_run: true,
        ..ask(root.path(), out.path())?
    })?;
    assert_eq!(rebuilt.snapshot_id, first.snapshot_id);
    assert_ne!(grown.snapshot_id, first.snapshot_id);
    assert_eq!((first.items, grown.items), (3, 4));
    Ok(())
}

#[test]
fn a_through_seq_above_the_store_or_below_one_is_refused() -> TestResult {
    let root = TempDir::new()?;
    drop(five(root.path())?);
    let out = TempDir::new()?;
    let out_text = out.path().display().to_string();
    let above = binary(
        root.path(),
        &["--at", CUTOFF, "--out", &out_text, "--through-seq", "6"],
    )?;
    let below = binary(
        root.path(),
        &["--at", CUTOFF, "--out", &out_text, "--through-seq", "0"],
    )?;
    let (exit, code, empty, details) = refusal(&above)?;
    assert_eq!(
        (exit, code.as_str(), empty, details["highest_seq"].as_i64()),
        (Some(2), "invalid_argument", true, Some(5))
    );
    assert_eq!(refusal(&below)?.1, "invalid_argument");
    Ok(())
}

#[test]
fn a_cutoff_in_the_future_or_below_the_microsecond_is_refused() -> TestResult {
    let root = TempDir::new()?;
    drop(five(root.path())?);
    let out = TempDir::new()?;
    let future = cut(&Ask {
        at: at("2026-07-06T00:00:01Z")?,
        ..ask(root.path(), out.path())?
    });
    let sub_micro = cut(&Ask {
        at: at("2026-07-05T00:00:00.000000500Z")?,
        ..ask(root.path(), out.path())?
    });
    assert!(
        matches!(future, Err(CutError::AtInTheFuture { .. })),
        "{future:?}"
    );
    assert!(
        matches!(sub_micro, Err(CutError::AtBelowMicrosecond(_))),
        "{sub_micro:?}"
    );
    Ok(())
}

/// A pre-v17 `edgar_press` attempt is NULL (§3.2). The trigger forbids the change, so the test
/// drops it first. Mutation 2026-11-22, NULL counted as `live_ingest`: red.
#[test]
fn a_selection_holding_an_unrecorded_provenance_exits_four_by_name() -> TestResult {
    let root = TempDir::new()?;
    drop(five(root.path())?);
    Connection::open(root.path().join(COMMIT_LOG))?.execute_batch(
        "DROP TRIGGER attempts_provenance_set_once;
         UPDATE attempts SET provenance = NULL
         WHERE attempt_id IN (SELECT attempt_id FROM items WHERE seq IN (2, 3));",
    )?;
    let out = TempDir::new()?;
    let answered = binary(
        root.path(),
        &["--at", CUTOFF, "--out", &out.path().display().to_string()],
    )?;
    let (exit, code, empty, details) = refusal(&answered)?;
    assert_eq!(
        (exit, code.as_str(), empty, details),
        (
            Some(4),
            "provenance_unrecorded",
            true,
            serde_json::json!({"first_seq": 2, "last_seq": 3, "rows": 2})
        )
    );
    assert_eq!(std::fs::read_dir(out.path())?.count(), 0);
    Ok(())
}

fn body_path(root: &Path, seq: i64) -> TestResult<PathBuf> {
    let hex: String = Connection::open(root.join(COMMIT_LOG))?.query_row(
        "SELECT body_blob FROM items WHERE seq = ?1",
        [seq],
        |row| row.get(0),
    )?;
    let blobs = blob_store(root);
    let digest = blobs.named(&hex).ok_or("the body is not stored")?;
    Ok(blobs.path_of(&digest))
}

/// Mutation 2026-11-22, the presence check in the first pass removed: the cut writes a staging
/// directory before it refuses, red.
#[test]
fn a_selected_body_blob_absent_on_disk_exits_four_and_writes_nothing() -> TestResult {
    let root = TempDir::new()?;
    drop(five(root.path())?);
    std::fs::remove_file(body_path(root.path(), 1)?)?;
    let out = TempDir::new()?;
    let answered = binary(
        root.path(),
        &["--at", CUTOFF, "--out", &out.path().display().to_string()],
    )?;
    let (exit, code, empty, details) = refusal(&answered)?;
    assert_eq!(
        (exit, code.as_str(), empty, details["seq"].as_i64()),
        (Some(4), "input_absent", true, Some(1))
    );
    assert_eq!(std::fs::read_dir(out.path())?.count(), 0);
    Ok(())
}

#[test]
fn a_body_whose_bytes_differ_from_its_digest_refuses_the_cut() -> TestResult {
    let root = TempDir::new()?;
    drop(five(root.path())?);
    let path = body_path(root.path(), 3)?;
    let mut permissions = std::fs::metadata(&path)?.permissions();
    #[allow(
        clippy::permissions_set_readonly_false,
        reason = "the test rewrites a stored blob so that it no longer hashes to its digest"
    )]
    permissions.set_readonly(false);
    std::fs::set_permissions(&path, permissions)?;
    std::fs::write(&path, b"altered on disk")?;
    let out = TempDir::new()?;
    let refused = cut(&ask(root.path(), out.path())?);
    assert!(
        matches!(&refused, Err(error @ CutError::BodyUnreadable { seq: 3, .. }) if error.code() == "input_unparseable"),
        "{refused:?}"
    );
    Ok(())
}

#[test]
fn an_empty_selection_exits_four_and_writes_nothing() -> TestResult {
    let root = TempDir::new()?;
    drop(five(root.path())?);
    let out = TempDir::new()?;
    let answered = binary(
        root.path(),
        &[
            "--at",
            "2026-07-01T00:00:00Z",
            "--out",
            &out.path().display().to_string(),
        ],
    )?;
    let (exit, code, empty, _) = refusal(&answered)?;
    assert_eq!(
        (exit, code.as_str(), empty),
        (Some(4), "nothing_visible", true)
    );
    assert_eq!(std::fs::read_dir(out.path())?.count(), 0);
    Ok(())
}

/// Mutation 2026-11-22, the standing-directory branch removed: red.
#[test]
fn a_second_cut_of_the_same_set_writes_nothing_and_reports_unchanged() -> TestResult {
    let root = TempDir::new()?;
    drop(five(root.path())?);
    let out = TempDir::new()?;
    let first = cut(&ask(root.path(), out.path())?)?;
    let before = std::fs::read(the_path(&first)?.join(DATA_FILE))?;
    let modified = std::fs::metadata(the_path(&first)?.join(MANIFEST_FILE))?.modified()?;
    let second = cut(&ask(root.path(), out.path())?)?;
    assert_eq!(
        (
            second.changed,
            &second.snapshot_id,
            &second.data_sha256,
            second.bytes
        ),
        (false, &first.snapshot_id, &first.data_sha256, first.bytes)
    );
    assert_eq!(std::fs::read(the_path(&second)?.join(DATA_FILE))?, before);
    assert_eq!(
        std::fs::metadata(the_path(&second)?.join(MANIFEST_FILE))?.modified()?,
        modified
    );
    Ok(())
}

#[test]
fn a_standing_directory_that_does_not_validate_is_refused_and_left_as_found() -> TestResult {
    let root = TempDir::new()?;
    drop(five(root.path())?);
    let out = TempDir::new()?;
    let first = cut(&ask(root.path(), out.path())?)?;
    let manifest = the_path(&first)?.join(MANIFEST_FILE);
    std::fs::write(&manifest, b"{}")?;
    let refused = cut(&ask(root.path(), out.path())?);
    assert!(
        matches!(&refused, Err(error @ CutError::StandingInvalid { .. }) if error.code() == "input_unparseable"),
        "{refused:?}"
    );
    assert_eq!(std::fs::read(&manifest)?, b"{}");
    Ok(())
}

#[test]
fn a_staging_directory_left_by_an_earlier_cut_is_refused_and_never_removed() -> TestResult {
    let root = TempDir::new()?;
    drop(five(root.path())?);
    let out = TempDir::new()?;
    let id = cut(&Ask {
        dry_run: true,
        ..ask(root.path(), out.path())?
    })?
    .snapshot_id;
    let leftover = out.path().join(format!(".tmp-{id}"));
    std::fs::create_dir(&leftover)?;
    std::fs::write(leftover.join("sentinel"), b"left by a crash")?;
    let refused = cut(&ask(root.path(), out.path())?);
    assert!(
        matches!(&refused, Err(error @ CutError::StagingExists(_)) if error.code() == "io_failed"),
        "{refused:?}"
    );
    assert_eq!(
        std::fs::read(leftover.join("sentinel"))?,
        b"left by a crash"
    );
    Ok(())
}

/// The durability acts in order, each performed, and a fault that appends to the first file it
/// syncs (the data file) when `corrupt` is set.
#[derive(Default)]
struct Journal {
    acts: RefCell<Vec<&'static str>>,
    corrupt: bool,
}

impl Durability for Journal {
    fn sync_file(&self, file: &File) -> std::io::Result<()> {
        if self.corrupt && self.acts.borrow().is_empty() {
            let mut appended = file;
            appended.write_all(b"bytes after the footer")?;
        }
        self.acts.borrow_mut().push("sync_file");
        Fsync.sync_file(file)
    }

    fn sync_dir(&self, directory: &Path) -> std::io::Result<()> {
        self.acts.borrow_mut().push("sync_dir");
        Fsync.sync_dir(directory)
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.acts.borrow_mut().push("rename");
        Fsync.rename(from, to)
    }
}

/// Mutation 2026-11-22, the `validate` call before the rename removed: the damaged cut reaches
/// its final name, red.
#[test]
fn the_staged_cut_is_validated_before_its_rename() -> TestResult {
    let root = TempDir::new()?;
    drop(five(root.path())?);
    let out = TempDir::new()?;
    let journal = Journal {
        corrupt: true,
        ..Journal::default()
    };
    let refused = cmd_cut::run(&ask(root.path(), out.path())?, &journal);
    let Err(CutError::StagedInvalid { path, findings }) = &refused else {
        return Err(format!("expected StagedInvalid, got {refused:?}").into());
    };
    assert!(path.is_dir());
    assert!(
        findings
            .iter()
            .any(|finding| finding.code == "data_unreadable"),
        "{findings:?}"
    );
    assert_eq!(std::fs::read_dir(out.path())?.count(), 1);
    assert!(!journal.acts.borrow().contains(&"rename"));
    Ok(())
}

/// Mutation 2026-11-22, the staging directory's `sync_dir` moved after the rename: red.
#[test]
fn a_cut_renames_only_after_both_files_and_the_directory_are_synced() -> TestResult {
    let root = TempDir::new()?;
    drop(five(root.path())?);
    let out = TempDir::new()?;
    let journal = Journal::default();
    let report = cmd_cut::run(&ask(root.path(), out.path())?, &journal)?;
    assert_eq!(
        journal.acts.borrow().as_slice(),
        ["sync_file", "sync_file", "sync_dir", "rename", "sync_dir"]
    );
    assert_eq!(validate(&the_path(&report)?)?, Vec::new());
    Ok(())
}

/// C4: a stored time the predicate cannot answer for stops both readers by name. Before
/// 2026-11-22 `items --at` dropped such a row and exited 0.
#[test]
fn a_stored_time_nobody_can_parse_stops_items_and_cut_by_its_seq() -> TestResult {
    let root = TempDir::new()?;
    drop(five(root.path())?);
    Connection::open(root.path().join(COMMIT_LOG))?.execute(
        "UPDATE items SET observed_at = 'the next morning' WHERE seq = 2",
        [],
    )?;
    let items = cmd_items::read(
        root.path(),
        &cmd_items::Ask {
            at: at(CUTOFF)?,
            from: None,
            to: None,
            source: None,
            limit: None,
            fields: None,
            per_story: PerStory::All,
        },
    );
    assert!(
        matches!(items, Err(cmd_items::ItemsError::StoredTime { seq: 2, .. })),
        "{items:?}"
    );
    let out = TempDir::new()?;
    let answered = binary(
        root.path(),
        &["--at", CUTOFF, "--out", &out.path().display().to_string()],
    )?;
    let (exit, code, empty, details) = refusal(&answered)?;
    assert_eq!(
        (exit, code.as_str(), empty, details["seq"].as_i64()),
        (Some(2), "input_unparseable", true, Some(2))
    );
    Ok(())
}

#[test]
fn a_written_cut_reports_its_envelope_on_stdout_and_validates() -> TestResult {
    let root = TempDir::new()?;
    drop(five(root.path())?);
    let out = TempDir::new()?;
    let answered = binary(
        root.path(),
        &["--at", CUTOFF, "--out", &out.path().display().to_string()],
    )?;
    let envelope: Value = serde_json::from_slice(&answered.stdout)?;
    assert_eq!(
        (
            answered.status.code(),
            envelope["schema"].as_str(),
            envelope["items"].as_u64(),
            envelope["provenance_counts"].clone(),
            answered.stderr.is_empty(),
        ),
        (
            Some(0),
            Some(cmd_cut::SCHEMA),
            Some(3),
            serde_json::json!({"bulk_dataset": 0, "live_ingest": 2, "vendor_archive": 1}),
            true,
        )
    );
    let path = PathBuf::from(envelope["path"].as_str().ok_or("no path")?);
    assert_eq!(validate(&path)?, Vec::new());
    Ok(())
}

#[test]
fn the_cut_command_and_its_codes_are_in_schema() -> TestResult {
    let output = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .args(["schema", "--format", "json"])
        .output()?;
    let schema: Value = serde_json::from_slice(&output.stdout)?;
    let cut = schema["commands"]
        .as_array()
        .and_then(|commands| {
            commands
                .iter()
                .find(|command| command["path"] == serde_json::json!(["cut"]))
        })
        .ok_or("no cut command")?;
    let codes: Vec<(&str, i64)> = schema["error_codes"]
        .as_array()
        .ok_or("no error codes")?
        .iter()
        .filter_map(|code| Some((code["code"].as_str()?, code["exit"].as_i64()?)))
        .collect();
    assert_eq!(cut["output"].as_str(), Some(cmd_cut::SCHEMA));
    assert!(codes.contains(&("provenance_unrecorded", 4)), "{codes:?}");
    assert!(codes.contains(&("nothing_visible", 4)), "{codes:?}");
    Ok(())
}
