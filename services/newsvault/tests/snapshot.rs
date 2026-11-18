//! `newsvault snapshot`, the commit-log copy the daily backup uploads (architecture.md section 10,
//! N1): a copy taken while another connection commits is one whole commit, and each refusal exits
//! by its section 5.1 code with stdout empty.

use std::error::Error;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use news_provider::RunId;
use newsvault::store::{Committer, COMMIT_LOG, SCHEMA_VERSION};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

/// Rows each concurrent commit adds, so a copy holding a part of one commit is not a multiple.
const BATCH: i64 = 50;
const WRITER_WAIT: Duration = Duration::from_secs(30);
const COPY: &str = "copy.sqlite3";

fn snapshot(store: &Path, out: &Path, extra: &[&str]) -> Result<Output, Box<dyn Error>> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_newsvault"));
    command
        .args(["snapshot", "--store"])
        .arg(store)
        .arg("--out")
        .arg(out)
        .args(["--format", "json"])
        .args(extra)
        .stdin(Stdio::null());
    Ok(command.output()?)
}

/// A store at this build's version, its lease released.
fn store_at(root: &Path) -> TestResult {
    drop(Committer::open(root, &RunId::new("the-store"))?);
    Ok(())
}

fn counted(db: &Connection, table: &str) -> Result<i64, rusqlite::Error> {
    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
        row.get(0)
    })
}

/// Commits `BATCH` rows at a time into `grown` until `stop`, from its own connection.
fn write_until(log: &Path, stop: &AtomicBool) -> Result<i64, rusqlite::Error> {
    let mut db = Connection::open(log)?;
    db.busy_timeout(WRITER_WAIT)?;
    let mut commits = 0;
    while !stop.load(Ordering::SeqCst) {
        let transaction = db.transaction()?;
        for row in 0..BATCH {
            transaction.execute("INSERT INTO grown (n) VALUES (?1)", [row])?;
        }
        transaction.commit()?;
        commits += 1;
    }
    Ok(commits * BATCH)
}

#[test]
fn a_copy_taken_while_another_connection_commits_is_one_whole_commit_at_the_stores_version(
) -> TestResult {
    let root = tempfile::TempDir::new()?;
    store_at(root.path())?;
    let log = root.path().join(COMMIT_LOG);
    Connection::open(&log)?.execute_batch("CREATE TABLE grown (n INTEGER NOT NULL)")?;
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (log, stop) = (log.clone(), Arc::clone(&stop));
        std::thread::spawn(move || write_until(&log, &stop))
    };
    while counted(&Connection::open(&log)?, "grown")? == 0 {
        std::thread::yield_now();
    }
    let out = root.path().join(COPY);
    let answered = snapshot(root.path(), &out, &[])?;
    stop.store(true, Ordering::SeqCst);
    let written = match writer.join() {
        Ok(written) => written?,
        Err(_) => return Err("the writer thread panicked".into()),
    };
    let copy = Connection::open_with_flags(&out, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let rows = counted(&copy, "grown")?;
    let check: String = copy.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    let version: i32 = copy.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let envelope: Value = serde_json::from_slice(&answered.stdout)?;
    assert_eq!(
        (
            answered.status.code(),
            check,
            version,
            rows > 0 && rows <= written,
            rows % BATCH,
            counted(&copy, "attempts")?,
            envelope["schema"].as_str(),
            envelope["user_version"].as_i64(),
            envelope["bytes"].as_u64(),
            envelope["changed"].as_bool(),
        ),
        (
            Some(0),
            "ok".to_owned(),
            SCHEMA_VERSION,
            true,
            0,
            counted(&Connection::open(&log)?, "attempts")?,
            Some("newsvault.snapshot/1"),
            Some(i64::from(SCHEMA_VERSION)),
            Some(std::fs::metadata(&out)?.len()),
            Some(true),
        ),
        "{rows} of {written}"
    );
    Ok(())
}

/// Rows of `SPILL_PAD` bytes the open transaction inserts: about 2 MB against a cache and spill
/// threshold of `SPILL_CACHE_PAGES`. macOS's `/usr/lib` SQLite 3.54.0 defaults `cache_spill` to
/// 20,000 pages and spilled nothing until it was set (2026-11-18).
const SPILL_ROWS: i64 = 2_000;
const SPILL_PAD: i64 = 1_000;
const SPILL_CACHE_PAGES: i64 = 10;
const SPILL_HOLD: Duration = Duration::from_secs(2);

/// Inserts `SPILL_ROWS` in one transaction from a small cache, reports the spill on `spilled`,
/// holds the transaction open for `SPILL_HOLD`, then commits.
fn spill_then_commit(
    log: &Path,
    spilled: &std::sync::mpsc::Sender<()>,
) -> Result<i64, rusqlite::Error> {
    let db = Connection::open(log)?;
    db.pragma_update(None, "cache_size", SPILL_CACHE_PAGES)?;
    db.pragma_update(None, "cache_spill", SPILL_CACHE_PAGES)?;
    db.execute_batch("BEGIN IMMEDIATE")?;
    for row in 0..SPILL_ROWS {
        db.execute(
            "INSERT INTO grown (n, pad) VALUES (?1, zeroblob(?2))",
            [row, SPILL_PAD],
        )?;
    }
    if spilled.send(()).is_err() {
        return db.execute_batch("ROLLBACK").map(|()| 0);
    }
    std::thread::sleep(SPILL_HOLD);
    db.execute_batch("COMMIT")?;
    Ok(SPILL_ROWS)
}

/// A file copy taken now would hold the uncommitted pages without their journal; the online
/// backup waits on the writer's lock and copies the whole commit.
#[test]
fn a_copy_started_while_a_transaction_has_spilled_into_the_file_holds_that_whole_commit(
) -> TestResult {
    let root = tempfile::TempDir::new()?;
    store_at(root.path())?;
    let log = root.path().join(COMMIT_LOG);
    Connection::open(&log)?
        .execute_batch("CREATE TABLE grown (n INTEGER NOT NULL, pad BLOB NOT NULL)")?;
    let before = std::fs::metadata(&log)?.len();
    let (spilled, on_spill) = std::sync::mpsc::channel();
    let writer = {
        let log = log.clone();
        std::thread::spawn(move || spill_then_commit(&log, &spilled))
    };
    on_spill.recv()?;
    let grown_before_commit = std::fs::metadata(&log)?.len() > before;
    let out = root.path().join(COPY);
    let answered = snapshot(root.path(), &out, &[])?;
    let written = match writer.join() {
        Ok(written) => written?,
        Err(_) => return Err("the writer thread panicked".into()),
    };
    let copy = Connection::open_with_flags(&out, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let check: String = copy.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    assert_eq!(
        (
            grown_before_commit,
            answered.status.code(),
            counted(&copy, "grown")?,
            check
        ),
        (true, Some(0), written, "ok".to_owned()),
        "{}",
        String::from_utf8_lossy(&answered.stderr)
    );
    Ok(())
}

#[test]
fn a_dry_run_reports_the_stores_version_and_writes_nothing() -> TestResult {
    let root = tempfile::TempDir::new()?;
    store_at(root.path())?;
    let out = root.path().join(COPY);
    let answered = snapshot(root.path(), &out, &["--dry-run"])?;
    let envelope: Value = serde_json::from_slice(&answered.stdout)?;
    let mut left: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(root.path())? {
        left.push(entry?.file_name().to_string_lossy().into_owned());
    }
    left.sort();
    assert_eq!(
        (
            answered.status.code(),
            envelope["user_version"].as_i64(),
            envelope["dry_run"].as_bool(),
            envelope["changed"].as_bool(),
            envelope["bytes"].as_u64().is_some_and(|bytes| bytes > 0),
            left,
        ),
        (
            Some(0),
            Some(i64::from(SCHEMA_VERSION)),
            Some(true),
            Some(false),
            true,
            vec!["LOCK".to_owned(), COMMIT_LOG.to_owned()],
        )
    );
    Ok(())
}

/// `(exit, code, retryable, stdout empty)` of one refused snapshot.
type Refusal = (Option<i32>, Option<String>, Option<bool>, bool);

fn refusal(answered: &Output) -> Refusal {
    let object: Value = serde_json::from_slice(&answered.stderr).unwrap_or(Value::Null);
    (
        answered.status.code(),
        object["code"].as_str().map(str::to_owned),
        object["retryable"].as_bool(),
        answered.stdout.is_empty(),
    )
}

#[test]
fn each_refusal_exits_by_its_code_with_stdout_empty() -> TestResult {
    let root = tempfile::TempDir::new()?;
    let empty = tempfile::TempDir::new()?;
    store_at(root.path())?;
    let existing = root.path().join("existing");
    std::fs::write(&existing, b"kept")?;
    let no_store = snapshot(empty.path(), &root.path().join(COPY), &[])?;
    let exists = snapshot(root.path(), &existing, &[])?;
    let no_directory = snapshot(root.path(), &root.path().join("absent").join(COPY), &[])?;
    let held = {
        let _writer = Committer::open(root.path(), &RunId::new("a-collect"))?;
        snapshot(root.path(), &root.path().join(COPY), &[])?
    };
    let code = |text: &str| Some(text.to_owned());
    assert_eq!(
        (
            refusal(&no_store),
            refusal(&exists),
            std::fs::read(&existing)?,
            refusal(&no_directory),
            refusal(&held),
            root.path().join(COPY).exists(),
        ),
        (
            (Some(4), code("input_absent"), Some(false), true),
            (Some(2), code("invalid_argument"), Some(false), true),
            b"kept".to_vec(),
            (Some(4), code("input_absent"), Some(false), true),
            (Some(7), code("lease_held"), Some(true), true),
            false,
        )
    );
    Ok(())
}

/// One snapshot of a store `damage` changed, given the page size: its refusal, the message, and
/// what is left beside the store.
fn damaged(
    damage: impl Fn(&mut [u8], usize),
) -> Result<(Refusal, String, Vec<String>), Box<dyn Error>> {
    let root = tempfile::TempDir::new()?;
    store_at(root.path())?;
    let log = root.path().join(COMMIT_LOG);
    let mut bytes = std::fs::read(&log)?;
    let page: u16 = Connection::open(&log)?.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    damage(&mut bytes, usize::from(page));
    std::fs::write(&log, bytes)?;
    let answered = snapshot(root.path(), &root.path().join(COPY), &[])?;
    let mut left: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(root.path())? {
        left.push(entry?.file_name().to_string_lossy().into_owned());
    }
    left.sort();
    let object: Value = serde_json::from_slice(&answered.stderr).unwrap_or(Value::Null);
    let message = object["message"].as_str().unwrap_or_default().to_owned();
    Ok((refusal(&answered), message, left))
}

/// The online backup copies a page as it is: overwritten pages fail as `SQLITE_CORRUPT`, and
/// neither the copy nor its partial is left.
#[test]
fn a_damaged_store_is_refused_as_malformed_and_nothing_is_left() -> TestResult {
    let (refused, message, left) = damaged(|bytes, page| {
        let end = bytes.len().min(3 * page);
        for byte in bytes.iter_mut().take(end).skip(page) {
            *byte = 0xA5;
        }
    })?;
    assert_eq!(
        (refused, left),
        (
            (
                Some(2),
                Some("input_unparseable".to_owned()),
                Some(false),
                true
            ),
            vec!["LOCK".to_owned(), COMMIT_LOG.to_owned()],
        ),
        "{message}"
    );
    Ok(())
}

/// A header whose freelist count is 3 too high opens and copies cleanly; only the copy's
/// `quick_check` names it.
#[test]
fn a_freelist_count_the_pages_disagree_with_is_named_by_the_copys_quick_check() -> TestResult {
    const FREELIST_COUNT: std::ops::Range<usize> = 36..40;
    let (refused, message, left) = damaged(|bytes, _| {
        if let Some(count) = bytes.get_mut(FREELIST_COUNT) {
            count.copy_from_slice(&3_u32.to_be_bytes());
        }
    })?;
    assert_eq!(
        (refused, message.contains("Freelist"), left),
        (
            (
                Some(2),
                Some("input_unparseable".to_owned()),
                Some(false),
                true
            ),
            true,
            vec!["LOCK".to_owned(), COMMIT_LOG.to_owned()],
        ),
        "{message}"
    );
    Ok(())
}

/// The manifest is hand-written and the parser derived, so each flag `--help` lists is named.
#[test]
fn every_option_the_snapshot_parser_takes_is_in_the_machine_readable_schema() -> TestResult {
    let binary = env!("CARGO_BIN_EXE_newsvault");
    let help = Command::new(binary).args(["snapshot", "--help"]).output()?;
    let schema: Value = serde_json::from_slice(
        &Command::new(binary)
            .args(["schema", "--format", "json"])
            .output()?
            .stdout,
    )?;
    let named: Vec<String> = schema["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|command| command["path"][0].as_str() == Some("snapshot"))
        .and_then(|command| command["options"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|option| option["name"].as_str().map(|name| name.replace('_', "-")))
        .collect();
    let help = String::from_utf8_lossy(&help.stdout).to_string();
    let missing: Vec<String> = help
        .split_whitespace()
        .filter_map(|word| {
            word.trim_end_matches(|c: char| !c.is_alphanumeric())
                .strip_prefix("--")
        })
        .filter(|long| !long.is_empty() && *long != "help" && !named.contains(&(*long).to_owned()))
        .map(str::to_owned)
        .collect();
    assert_eq!((named.len(), missing), (5, Vec::<String>::new()));
    Ok(())
}
