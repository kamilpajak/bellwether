//! `newsvault blobs verify | compress | pack | dictionary train` through the binary, over a store
//! laid out as every store was before compression: raw files named by their sha256
//! (architecture.md section 3.9, row "compression"). Training reads repo fixtures only; nothing
//! trained is kept. The pack tests run `deploy/systemd/newsvault-backup` under dash against a
//! stub `docker` whose `run` is this build's binary (section 10, N1, "Packs").

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use news_provider::RunId;
use newsvault::store::{self, Committer, StoreError, COMMIT_LOG};
use rusqlite::Connection;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use vault_core::blobs::Encoding;

type TestResult = Result<(), Box<dyn Error>>;
/// A store and the payloads it holds.
type Store = (TempDir, Vec<Vec<u8>>);
/// A store, its payloads and the dictionary they are compressed under.
type Compressed = (TempDir, Vec<Vec<u8>>, String);

const SAMPLES: &str = "40";
const HOLDOUT: &str = "10";

fn newsvault(store: &Path, args: &[&str]) -> Result<Output, Box<dyn Error>> {
    Ok(Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .args(args)
        .arg("--store")
        .arg(store)
        .args(["--format", "json"])
        .stdin(Stdio::null())
        .output()?)
}

fn answered(output: &Output) -> Result<Value, Box<dyn Error>> {
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn train(store: &Path, extra: &[&str]) -> Result<Output, Box<dyn Error>> {
    newsvault(
        store,
        &[
            &[
                "blobs",
                "dictionary",
                "train",
                "--samples",
                SAMPLES,
                "--holdout",
                HOLDOUT,
            ],
            extra,
        ]
        .concat(),
    )
}

/// Every file of five `open-web-feed` fixture directories: 57 payloads on 2026-11-21.
fn pages() -> Result<Vec<Vec<u8>>, Box<dyn Error>> {
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../providers/open-web-feed/tests/fixtures");
    let mut paths = Vec::new();
    for directory in ["cnw", "fed", "gnw", "prn", "yahoo"] {
        for entry in std::fs::read_dir(root.join(directory))? {
            paths.push(entry?.path());
        }
    }
    paths.sort();
    Ok(paths.iter().map(std::fs::read).collect::<Result<_, _>>()?)
}

fn sha256(bytes: &[u8]) -> String {
    const_hex::encode(Sha256::digest(bytes))
}

fn raw_path(store: &Path, hex: &str) -> PathBuf {
    store
        .join("blobs/sha256")
        .join(&hex[..2])
        .join(&hex[2..4])
        .join(hex)
}

/// A store at this build's version holding every fixture page as a raw file with its `blobs`
/// row, as `collect` left every payload before compression.
fn legacy_store() -> Result<Store, Box<dyn Error>> {
    let root = TempDir::new()?;
    drop(Committer::open(root.path(), &RunId::new("the-store"))?);
    let db = Connection::open(root.path().join(COMMIT_LOG))?;
    let pages = pages()?;
    for page in &pages {
        index_raw(root.path(), &db, page)?;
    }
    Ok((root, pages))
}

/// One payload as a raw file with its `blobs` row.
fn index_raw(store: &Path, db: &Connection, page: &[u8]) -> TestResult {
    let hex = sha256(page);
    let path = raw_path(store, &hex);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, page)?;
    db.execute(
        "INSERT INTO blobs (digest, bytes, first_indexed_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(digest) DO NOTHING",
        (
            hex,
            i64::try_from(page.len())?,
            "2026-11-21T00:00:00.000000Z",
        ),
    )?;
    Ok(())
}

/// The legacy store after `train` and `compress`, with the dictionary's digest.
fn compressed_store() -> Result<Compressed, Box<dyn Error>> {
    let (root, pages) = legacy_store()?;
    let trained = answered(&train(root.path(), &[])?)?;
    let compressed = newsvault(root.path(), &["blobs", "compress"])?;
    assert_eq!(compressed.status.code(), Some(0));
    let dictionary = trained["dictionary"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    Ok((root, pages, dictionary))
}

fn counted(store: &Path, table: &str) -> Result<i64, Box<dyn Error>> {
    let db = Connection::open(store.join(COMMIT_LOG))?;
    Ok(
        db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })?,
    )
}

/// Rewriting before the first dictionary would pin the weaker ratio on every file, so it is
/// refused by name with stdout empty, and allowed when asked for.
#[test]
fn compress_without_an_active_dictionary_is_refused_unless_allowed() -> TestResult {
    let (root, pages) = legacy_store()?;
    let refused = newsvault(root.path(), &["blobs", "compress"])?;
    let error: Value = serde_json::from_slice(&refused.stderr)?;
    let allowed = newsvault(root.path(), &["blobs", "compress", "--allow-no-dictionary"])?;
    let report = answered(&allowed)?;
    assert_eq!(
        (
            refused.status.code(),
            refused.stdout.is_empty(),
            error["code"].as_str(),
            allowed.status.code(),
            report["compressed"].as_u64(),
            report["dictionary"].is_null(),
        ),
        (
            Some(2),
            true,
            Some("invalid_argument"),
            Some(0),
            Some(u64::try_from(pages.len())?),
            true
        )
    );
    Ok(())
}

#[test]
fn a_dry_run_trains_and_measures_and_stores_and_activates_nothing() -> TestResult {
    let (root, _) = legacy_store()?;
    let output = train(root.path(), &["--dry-run"])?;
    let report = answered(&output)?;
    assert_eq!(
        (
            output.status.code(),
            report["changed"].as_bool(),
            report["active"].is_null(),
            report["holdout"]["blobs"].as_u64(),
            report["holdout"]["with_dictionary"]
                .as_u64()
                .is_some_and(|bytes| bytes > 0),
            root.path().join("blobs/dictionaries").exists(),
            counted(root.path(), "dictionaries")?,
        ),
        (Some(0), Some(false), true, Some(10), true, false, 0)
    );
    Ok(())
}

/// The dictionary lives in the store under its own sha256, which the backup's first pack carries, and
/// the log records it and its activation; the same samples train the same dictionary, so a
/// second run changes nothing.
#[test]
fn training_stores_the_dictionary_by_its_digest_and_activates_it_once() -> TestResult {
    let (root, _) = legacy_store()?;
    let first = answered(&train(root.path(), &[])?)?;
    let again = answered(&train(root.path(), &[])?)?;
    let hex = first["dictionary"].as_str().unwrap_or_default();
    let file = std::fs::read(root.path().join("blobs/dictionaries").join(hex))?;
    assert_eq!(
        (
            sha256(&file),
            first["active"].as_str(),
            first["changed"].as_bool(),
            first["samples"].as_u64(),
            again["dictionary"].as_str(),
            again["changed"].as_bool(),
            counted(root.path(), "dictionaries")?,
            counted(root.path(), "dictionary_activations")?,
        ),
        (
            hex.to_owned(),
            Some(hex),
            Some(true),
            Some(40),
            Some(hex),
            Some(false),
            1,
            1
        )
    );
    Ok(())
}

/// Every raw file is rewritten under the active dictionary, a second run finds none, and no
/// raw file is left.
#[test]
fn compress_rewrites_every_raw_blob_once_under_the_active_dictionary() -> TestResult {
    let (root, pages) = legacy_store()?;
    let trained = answered(&train(root.path(), &[])?)?;
    let planned = answered(&newsvault(
        root.path(),
        &["blobs", "compress", "--dry-run"],
    )?)?;
    let first = answered(&newsvault(root.path(), &["blobs", "compress"])?)?;
    let second = answered(&newsvault(root.path(), &["blobs", "compress"])?)?;
    let raw_left = pages
        .iter()
        .filter(|page| raw_path(root.path(), &sha256(page)).exists())
        .count();
    let count = u64::try_from(pages.len())?;
    assert_eq!(
        (
            (
                planned["raw_found"].as_u64(),
                planned["compressed"].as_u64()
            ),
            (first["raw_found"].as_u64(), first["compressed"].as_u64()),
            first["failed"].as_u64(),
            first["dictionary"] == trained["dictionary"],
            (second["raw_found"].as_u64(), second["changed"].as_bool()),
            raw_left,
        ),
        (
            (Some(count), Some(0)),
            (Some(count), Some(count)),
            Some(0),
            true,
            (Some(0), Some(false)),
            0
        )
    );
    Ok(())
}

/// The real-path reader of the bulk read: every blob decodes to its digest, and `blobs.bytes`
/// is still each payload's own length after the rewrite (invariant 3, §11.1's held read).
#[test]
fn verify_reads_every_compressed_blob_back_and_matches_every_row() -> TestResult {
    let (root, pages, dictionary) = compressed_store()?;
    let output = newsvault(root.path(), &["blobs", "verify"])?;
    let report = answered(&output)?;
    let logical: usize = pages.iter().map(Vec::len).sum();
    let count = u64::try_from(pages.len())?;
    assert_eq!(
        (
            output.status.code(),
            report["verified"].as_u64(),
            report["by_encoding"]["zstd"]["logical_bytes"].as_u64(),
            report["by_encoding"]["raw"].is_null(),
            report["by_dictionary"][0]["dictionary"].as_str(),
            report["by_dictionary"][0]["recorded"]["samples"].as_u64(),
            report["indexed"].clone(),
        ),
        (
            Some(0),
            Some(count),
            Some(u64::try_from(logical)?),
            true,
            Some(dictionary.as_str()),
            Some(40),
            serde_json::json!({"rows": count, "absent": 0, "length_differs": 0}),
        )
    );
    Ok(())
}

/// A frame behind a header naming no dictionary is a sound blob, counted under `zstd` and
/// under the `null` dictionary. Mutation 2026-11-26, `split_header` refusing a header that names
/// none: red.
#[test]
fn verify_reads_every_frame_written_without_a_dictionary() -> TestResult {
    let (root, pages) = legacy_store()?;
    let compressed = newsvault(root.path(), &["blobs", "compress", "--allow-no-dictionary"])?;
    let output = newsvault(root.path(), &["blobs", "verify"])?;
    let report = answered(&output)?;
    let count = u64::try_from(pages.len())?;
    assert_eq!(
        (
            compressed.status.code(),
            output.status.code(),
            report["verified"].as_u64(),
            report["by_encoding"]["zstd"]["blobs"].as_u64(),
            report["by_encoding"]["raw"].is_null(),
            report["by_dictionary"]
                .as_array()
                .map(|rows| (rows.len(), rows[0]["dictionary"].is_null())),
            report["by_dictionary"][0]["blobs"].as_u64(),
        ),
        (
            Some(0),
            Some(0),
            Some(count),
            Some(count),
            true,
            Some((1, true)),
            Some(count),
        )
    );
    Ok(())
}

/// Exit 1 is the answer no: a blob that no longer decodes, or a row whose length is not its
/// payload's. Mutation 2026-11-21, `is_sound` ignoring `length_differs`: red.
#[test]
fn verify_answers_no_for_an_undecodable_blob_and_for_a_row_of_another_length() -> TestResult {
    let (root, pages, _) = compressed_store()?;
    let hex = sha256(&pages[0]);
    let mut damaged = raw_path(root.path(), &hex).into_os_string();
    damaged.push(".zst");
    std::fs::write(&damaged, b"not a frame")?;
    let undecodable = newsvault(root.path(), &["blobs", "verify"])?;
    let (root, pages, _) = compressed_store()?;
    Connection::open(root.path().join(COMMIT_LOG))?.execute(
        "UPDATE blobs SET bytes = bytes + 1 WHERE digest = ?1",
        [sha256(&pages[0])],
    )?;
    let longer = newsvault(root.path(), &["blobs", "verify"])?;
    let (undecodable_report, longer_report) = (answered(&undecodable)?, answered(&longer)?);
    assert_eq!(
        (
            undecodable.status.code(),
            undecodable_report["failures"][0]["code"].as_str(),
            undecodable_report["failures"][0]["blob"].as_str(),
            longer.status.code(),
            longer_report["indexed"]["length_differs"].as_u64(),
        ),
        (
            Some(1),
            Some("blob_undecodable"),
            Some(hex.as_str()),
            Some(1),
            Some(1)
        )
    );
    Ok(())
}

/// `collect` writes through `Committer::put_blob`, which compresses under the newest
/// activation from the moment the store is opened. Mutation 2026-11-21, `Committer::open`
/// activating nothing: red.
#[test]
fn a_writer_opened_after_training_compresses_under_the_active_dictionary() -> TestResult {
    let (root, _) = legacy_store()?;
    let trained = answered(&train(root.path(), &[])?)?;
    let committer = Committer::open(root.path(), &RunId::new("a-collect"))?;
    let digest = committer.put_blob(b"<html><p>a later page</p></html>")?;
    let read = committer.blobs().read_copy(&digest, Encoding::Zstd)?;
    assert_eq!(read.dictionary.as_deref(), trained["dictionary"].as_str());
    Ok(())
}

/// A dictionary the log activated and the store no longer holds stops the writer by name
/// rather than letting it write without one.
#[test]
fn a_writer_whose_active_dictionary_file_is_gone_is_refused_by_name() -> TestResult {
    let (root, _) = legacy_store()?;
    let trained = answered(&train(root.path(), &[])?)?;
    let hex = trained["dictionary"].as_str().unwrap_or_default();
    std::fs::remove_file(root.path().join("blobs/dictionaries").join(hex))?;
    let opened = Committer::open(root.path(), &RunId::new("a-collect"));
    assert!(
        matches!(&opened, Err(StoreError::DictionaryAbsent { dictionary }) if dictionary == hex),
        "{opened:?}"
    );
    Ok(())
}

/// A reader of the payload a digest names gets the same bytes before and after the rewrite.
#[test]
fn the_bytes_a_reader_gets_are_the_same_before_and_after_the_rewrite() -> TestResult {
    let (root, pages, _) = compressed_store()?;
    let mut read = Vec::new();
    for page in &pages {
        read.push(store::blob_bytes(root.path(), &sha256(page))?);
    }
    assert_eq!(read, pages.into_iter().map(Some).collect::<Vec<_>>());
    Ok(())
}

fn zst_path(store: &Path, hex: &str) -> PathBuf {
    let mut path = raw_path(store, hex).into_os_string();
    path.push(".zst");
    PathBuf::from(path)
}

/// A valid `.zst` file holding `payload` with no dictionary: what `compress_raw` writes for it.
fn frame_of(payload: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    let scratch = TempDir::new()?;
    let blobs = store::blob_store(scratch.path());
    let digest = blobs.put("scratch", payload)?;
    blobs.compress_raw("scratch", &digest)?;
    Ok(std::fs::read(blobs.compressed_path_of(&digest))?)
}

fn user_version(store: &Path) -> Result<i64, Box<dyn Error>> {
    let db = Connection::open(store.join(COMMIT_LOG))?;
    Ok(db.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

/// A walk that could not read part of the tree has not verified it. Mutation 2026-11-21,
/// `is_sound` ignoring `listing_failed`: red.
#[test]
fn verify_answers_no_for_a_file_in_the_tree_that_names_no_digest() -> TestResult {
    let (root, pages, _) = compressed_store()?;
    let leaf = raw_path(root.path(), &sha256(&pages[0]));
    let stray = leaf.with_file_name("notes.txt");
    std::fs::write(&stray, b"x")?;
    let output = newsvault(root.path(), &["blobs", "verify"])?;
    let report = answered(&output)?;
    assert_eq!(
        (
            output.status.code(),
            report["failed"].as_u64(),
            report["listing_failed"].as_u64(),
            report["failures"][0]["code"].as_str(),
        ),
        (Some(1), Some(0), Some(1), Some("blob_stray"))
    );
    Ok(())
}

/// A `blobs` row whose file is gone in both forms is the answer no, in both renderings.
/// Mutation 2026-11-21, `is_sound` ignoring `indexed.absent`: red.
#[test]
fn verify_answers_no_for_a_row_whose_file_is_gone() -> TestResult {
    let (root, pages, _) = compressed_store()?;
    std::fs::remove_file(zst_path(root.path(), &sha256(&pages[0])))?;
    let output = newsvault(root.path(), &["blobs", "verify"])?;
    let report = answered(&output)?;
    let human = Command::new(env!("CARGO_BIN_EXE_newsvault"))
        .args(["blobs", "verify", "--store"])
        .arg(root.path())
        .stdin(Stdio::null())
        .output()?;
    assert_eq!(
        (
            output.status.code(),
            report["indexed"]["absent"].as_u64(),
            report["failed"].as_u64(),
            human.status.code(),
            String::from_utf8_lossy(&human.stdout).contains(" absent 1 "),
        ),
        (Some(1), Some(1), Some(0), Some(1), true)
    );
    Ok(())
}

/// The bulk read re-hashes: a valid frame of other bytes, and a bad `.zst` left beside a good
/// raw file, are each `blob_altered`. Mutations 2026-11-21, `stream` decoding without
/// re-hashing, and verify skipping the second copy of a digest held twice: red, each.
#[test]
fn verify_answers_no_for_a_frame_of_other_bytes_in_either_place() -> TestResult {
    let (root, pages, _) = compressed_store()?;
    let altered = sha256(&pages[0]);
    std::fs::write(zst_path(root.path(), &altered), frame_of(b"other bytes")?)?;
    let alone = newsvault(root.path(), &["blobs", "verify"])?;
    let (root, pages) = legacy_store()?;
    let beside = sha256(&pages[0]);
    std::fs::write(zst_path(root.path(), &beside), frame_of(b"other bytes")?)?;
    let twice = newsvault(root.path(), &["blobs", "verify"])?;
    let (alone_report, twice_report) = (answered(&alone)?, answered(&twice)?);
    assert_eq!(
        (
            (alone.status.code(), twice.status.code()),
            alone_report["failures"][0]["code"].as_str(),
            alone_report["failures"][0]["blob"].as_str(),
            twice_report["failures"][0]["code"].as_str(),
            twice_report["failures"][0]["blob"].as_str(),
            twice_report["both_copies"].as_u64(),
        ),
        (
            (Some(1), Some(1)),
            Some("blob_altered"),
            Some(altered.as_str()),
            Some("blob_altered"),
            Some(beside.as_str()),
            Some(1)
        )
    );
    Ok(())
}

/// A retrain activates the new dictionary: the newest activation is the one a writer uses.
/// Mutation 2026-11-21, `active_dictionary` ordering by `activation ASC`: red.
#[test]
fn a_second_dictionary_trained_is_the_one_every_later_write_names() -> TestResult {
    let (root, _) = legacy_store()?;
    let first = answered(&train(root.path(), &[])?)?;
    let second = answered(&newsvault(
        root.path(),
        &[
            "blobs",
            "dictionary",
            "train",
            "--samples",
            "30",
            "--holdout",
            HOLDOUT,
        ],
    )?)?;
    let committer = Committer::open(root.path(), &RunId::new("a-collect"))?;
    let digest = committer.put_blob(b"<html><p>a page after the retrain</p></html>")?;
    let read = committer.blobs().read_copy(&digest, Encoding::Zstd)?;
    assert_eq!(
        (
            first["dictionary"] == second["dictionary"],
            second["active"] == second["dictionary"],
            read.dictionary.as_deref(),
            counted(root.path(), "dictionary_activations")?,
        ),
        (false, true, second["dictionary"].as_str(), 2)
    );
    Ok(())
}

/// The read-only commands work on a store from before user_version 16 while a writer holds the
/// lease, and leave its version alone. Mutations 2026-11-21, the `holds_table` guards removed;
/// the compress dry run and the train dry run opening a `Committer`: red, each.
#[test]
fn verify_and_both_dry_runs_read_a_v15_store_under_a_held_lease_without_migrating_it() -> TestResult
{
    let (root, _) = legacy_store()?;
    Connection::open(root.path().join(COMMIT_LOG))?.execute_batch(
        "DROP TRIGGER attempts_provenance_set_once; ALTER TABLE attempts DROP COLUMN provenance;
         DROP TABLE dictionary_activations; DROP TABLE dictionaries; PRAGMA user_version = 15;",
    )?;
    let lease = std::fs::File::open(root.path().join("LOCK"))?;
    lease.try_lock()?;
    let runs = [
        newsvault(root.path(), &["blobs", "verify"])?,
        train(root.path(), &["--dry-run"])?,
        newsvault(
            root.path(),
            &["blobs", "compress", "--dry-run", "--allow-no-dictionary"],
        )?,
    ];
    drop(lease);
    assert_eq!(
        (
            runs.map(|run| run.status.code()),
            user_version(root.path())?
        ),
        ([Some(0); 3], 15)
    );
    Ok(())
}

/// A blob that fails its rewrite keeps its raw file and the run says so with exit 8. Here the
/// raw file no longer hashes to its name. Mutation 2026-11-21, `is_complete` always true: red.
#[test]
fn compress_exits_eight_when_a_blob_fails_and_keeps_its_raw_file() -> TestResult {
    let (root, pages) = legacy_store()?;
    train(root.path(), &[])?;
    let damaged = raw_path(root.path(), &sha256(&pages[0]));
    std::fs::write(&damaged, b"other bytes")?;
    let output = newsvault(root.path(), &["blobs", "compress"])?;
    let report = answered(&output)?;
    assert_eq!(
        (
            output.status.code(),
            report["failed"].as_u64(),
            report["failures"][0]["code"].as_str(),
            std::fs::read(&damaged)?,
        ),
        (
            Some(8),
            Some(1),
            Some("blob_altered"),
            b"other bytes".to_vec()
        )
    );
    Ok(())
}

/// Under its lease compress removes the staging files a crash left and replaces a leftover
/// `.zst` that does not decode. Mutation
/// 2026-11-21, compress not calling `remove_staging_leftovers`: red.
#[test]
fn compress_removes_staging_leftovers_and_replaces_a_bad_leftover_copy() -> TestResult {
    let (root, pages) = legacy_store()?;
    train(root.path(), &[])?;
    let staging = root.path().join("blobs/sha256/.tmp-crashed-run-0");
    std::fs::write(&staging, b"part")?;
    let hex = sha256(&pages[0]);
    std::fs::write(zst_path(root.path(), &hex), &pages[0])?;
    let output = newsvault(root.path(), &["blobs", "compress"])?;
    let report = answered(&output)?;
    assert_eq!(
        (
            output.status.code(),
            report["staging_removed"].as_u64(),
            report["replaced"].as_u64(),
            staging.exists(),
            store::blob_bytes(root.path(), &hex)?,
        ),
        (Some(0), Some(1), Some(1), false, Some(pages[0].clone()))
    );
    Ok(())
}

/// No activation row names a dictionary file the store does not hold: a crash between the two
/// would stop every later `Committer::open` with `DictionaryAbsent`. Mutation 2026-11-21,
/// `record_dictionary` without its load of the file: red.
#[test]
fn a_dictionary_is_recorded_only_once_its_file_is_in_the_store() -> TestResult {
    let (root, _) = legacy_store()?;
    let mut committer = Committer::open(root.path(), &RunId::new("a-train"))?;
    let stored = committer
        .blobs()
        .store_dictionary("a-train", b"a dictionary's bytes")?;
    std::fs::remove_file(committer.blobs().dictionary_path(stored.hex()))?;
    let trained = store::Trained {
        samples: 1,
        sample_bytes: 1,
    };
    let at = "2026-11-21T00:00:00Z".parse::<chrono::DateTime<chrono::Utc>>()?;
    let recorded = committer.record_dictionary(&stored, &trained, at);
    drop(committer);
    assert!(
        matches!(&recorded, Err(StoreError::DictionaryAbsent { .. })),
        "{recorded:?}"
    );
    assert_eq!(counted(root.path(), "dictionary_activations")?, 0);
    Ok(())
}

/// A count of 0 is a usage error before the store is read, not "too few payloads".
#[test]
fn training_on_zero_samples_or_zero_held_out_is_a_usage_error() -> TestResult {
    let (root, _) = legacy_store()?;
    let mut seen = Vec::new();
    for flag in ["--samples", "--holdout"] {
        let output = newsvault(
            root.path(),
            &["blobs", "dictionary", "train", flag, "0", "--dry-run"],
        )?;
        let error: Value = serde_json::from_slice(&output.stderr)?;
        seen.push((
            output.status.code(),
            error["code"].as_str().map(str::to_owned),
        ));
    }
    let usage = (Some(2), Some("invalid_argument".to_owned()));
    assert_eq!(seen, [usage.clone(), usage]);
    Ok(())
}

const FOLDER: &str = "stub/ncdata/ncuser/files/Backups/bellwether";
const NIGHTS: [&str; 3] = ["20261124T225500Z", "20261125T225500Z", "20261126T225500Z"];
const PACK_1: &str = "bellwether-vault-blobs-20261124T225500Z.tar";
const PACK_2: &str = "bellwether-vault-blobs-20261125T225500Z.tar";
/// Stands in for docker under the backup script. `run`: this build's binary on the words after
/// the image, with `/vault` and `/out` mapped to their mounts. `exec`: the Nextcloud container
/// as a shell over `$STUB_DIR/ncdata`; `occ` exits 0.
const BACKUP_DOCKER: &str = r#"#!/bin/sh
if [ "$1" = run ]; then
    for arg; do
        case $arg in
            type=bind,src=*,dst=/vault,readonly) store=${arg#type=bind,src=}; store=${store%%,dst=*} ;;
            type=bind,src=*,dst=/out) out=${arg#type=bind,src=}; out=${out%%,dst=*} ;;
        esac
    done
    while [ "$1" != newsvault:test ]; do shift; done
    shift
    n=$#
    for arg; do
        case $arg in /out/*) arg="$out/${arg#/out/}" ;; esac
        set -- "$@" "$arg"
    done
    shift "$n"
    BELLWETHER_VAULT=$store exec "$STUB_NEWSVAULT" "$@"
fi
shift
while :; do
    case $1 in -i) shift ;; -u) shift 2 ;; *) break ;; esac
done
shift
n=$#
for arg; do
    case $arg in /mnt/ncdata/*) arg="$STUB_DIR/ncdata/${arg#/mnt/ncdata/}" ;; esac
    set -- "$@" "$arg"
done
shift "$n"
case $1 in
    php) exit 0 ;;
    sh | mkdir | rm | mv | ls) cmd=$1; shift; exec "/bin/$cmd" "$@" ;;
esac
exit 127
"#;
/// `date -u +%s` and the name's stamp: each night is the one the test names.
const DATE_STUB: &str = "#!/bin/sh\ncase $2 in +%s) echo 0 ;; *) echo \"$STUB_STAMP\" ;; esac\n";

/// A home whose vault is `store`, an empty Nextcloud data directory and the two stubs.
fn backup_bench(store: &Path) -> Result<TempDir, Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;
    let work = TempDir::new()?;
    for dir in [
        "home/.bellwether",
        "stub/bin",
        "stub/ncdata/ncuser/files",
        "textfile",
    ] {
        std::fs::create_dir_all(work.path().join(dir))?;
    }
    std::os::unix::fs::symlink(store, work.path().join("home/.bellwether/vault"))?;
    for (name, text) in [("docker", BACKUP_DOCKER), ("date", DATE_STUB)] {
        let path = work.path().join("stub/bin").join(name);
        std::fs::write(&path, text)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(work)
}

/// One run of `deploy/systemd/newsvault-backup` under the unit's shell; any exit but `exit` is
/// an error carrying the run's stderr.
fn night(work: &Path, stamp: &str, exit: i32) -> Result<Output, Box<dyn Error>> {
    let run = Command::new("/bin/dash")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/systemd/newsvault-backup"))
        .current_dir(work)
        .env_clear()
        .env("HOME", work.join("home"))
        // macOS keeps `sha256sum` in /sbin.
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin:/sbin", work.join("stub/bin").display()),
        )
        .env("NEWSVAULT_IMAGE", "newsvault:test")
        .env("NEWSVAULT_TEXTFILE_DIR", work.join("textfile"))
        .env("NEXTCLOUD_CONTAINER", "nextcloud-aio-nextcloud")
        .env("NEXTCLOUD_DATA", "/mnt/ncdata")
        .env("NEXTCLOUD_USER", "ncuser")
        .env("NEXTCLOUD_FOLDER", "Backups/bellwether")
        .env("STUB_DIR", work.join("stub"))
        .env("STUB_NEWSVAULT", env!("CARGO_BIN_EXE_newsvault"))
        .env("STUB_STAMP", stamp)
        .stdin(Stdio::null())
        .output()?;
    match run.status.code() == Some(exit) {
        true => Ok(run),
        false => Err(format!("{}: {}", run.status, String::from_utf8_lossy(&run.stderr)).into()),
    }
}

fn names(dir: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

/// What the script reads from the folder for `blobs pack`, as `[have, manifests]` in `into`.
fn folder_facts(folder: &Path, into: &Path) -> Result<[PathBuf; 2], Box<dyn Error>> {
    let facts = [into.join("have"), into.join("manifests")];
    if facts[1].exists() {
        std::fs::remove_dir_all(&facts[1])?;
    }
    std::fs::create_dir(&facts[1])?;
    let mut have = String::new();
    for name in names(folder)? {
        if name.ends_with(".tar") {
            let bytes = std::fs::read(folder.join(&name))?;
            have += &format!("{name} {} {}\n", bytes.len(), sha256(&bytes));
        } else if name.ends_with(".tar.manifest") {
            std::fs::copy(folder.join(&name), facts[1].join(&name))?;
        }
    }
    std::fs::write(&facts[0], have)?;
    Ok(facts)
}

fn pack(
    store: &Path,
    log: &Path,
    facts: &[PathBuf; 2],
    extra: &[&str],
) -> Result<Output, Box<dyn Error>> {
    let shown = |path: &Path| path.display().to_string();
    let (log, have, manifests) = (shown(log), shown(&facts[0]), shown(&facts[1]));
    let asked = ["blobs", "pack", "--log", &log, "--have", &have];
    newsvault(
        store,
        &[&asked, &["--manifests", &manifests][..], extra].concat(),
    )
}

/// The restore runbook's unpacking: the newest log archive, then every pack but those in
/// `skip`, into one directory. `.1` is whether the archive alone left a `blobs` entry.
fn restore(folder: &Path, skip: &[String]) -> Result<(TempDir, bool), Box<dyn Error>> {
    let into = TempDir::new()?;
    let unpack = |flag: &str, name: &String| -> TestResult {
        let done = Command::new("tar")
            .arg(flag)
            .arg(folder.join(name))
            .arg("-C")
            .arg(into.path())
            .status()?;
        if done.success() {
            Ok(())
        } else {
            Err(format!("tar {flag} {name}: {done}").into())
        }
    };
    let all = names(folder)?;
    let newest = all.iter().rfind(|name| name.ends_with(".tar.gz"));
    unpack("-xzf", newest.ok_or("no log archive")?)?;
    let archive_holds_blobs = into.path().join("blobs").exists();
    for name in all.iter().filter(|name| name.ends_with(".tar")) {
        if !skip.contains(name) {
            unpack("-xf", name)?;
        }
    }
    Ok((into, archive_holds_blobs))
}

fn strings(value: &Value) -> Vec<String> {
    let listed = value.as_array().into_iter().flatten();
    listed
        .filter_map(|name| name.as_str().map(str::to_owned))
        .collect()
}

/// Section 8 row B3. The first night packs the dictionary and every blob, the second only the
/// two blobs indexed and compressed since, without their dictionary again, the third none; no
/// log archive holds a blob. The newest archive and
/// every pack, unpacked, are a store `blobs verify` accepts, and a dry run over the restored log
/// finds nothing the packs lack.
#[test]
fn each_night_packs_only_what_no_pack_holds_and_the_restore_verifies() -> TestResult {
    let (root, pages, dictionary) = compressed_store()?;
    let work = backup_bench(root.path())?;
    let folder = work.path().join(FOLDER);
    for (index, stamp) in NIGHTS.iter().enumerate() {
        if index == 1 {
            let db = Connection::open(root.path().join(COMMIT_LOG))?;
            index_raw(root.path(), &db, b"a page indexed after the first night")?;
            index_raw(root.path(), &db, b"and another one")?;
            newsvault(root.path(), &["blobs", "compress"])?;
        }
        night(work.path(), stamp, 0)?;
    }
    let members = |pack: &str| -> Result<[usize; 2], Box<dyn Error>> {
        let text = std::fs::read_to_string(folder.join(format!("{pack}.manifest")))?;
        Ok(["dictionary ", "blob "]
            .map(|kind| text.lines().filter(|l| l.starts_with(kind)).count()))
    };
    let (restored, archive_holds_blobs) = restore(&folder, &[])?;
    let verified = newsvault(restored.path(), &["blobs", "verify"])?;
    let report = answered(&verified)?;
    let facts = folder_facts(&folder, work.path())?;
    let log = restored.path().join(COMMIT_LOG);
    let planned = pack(restored.path(), &log, &facts, &["--dry-run"])?;
    let plan = answered(&planned)?;
    let count = u64::try_from(pages.len())?;
    let mut held: Vec<String> = NIGHTS
        .iter()
        .map(|stamp| format!("bellwether-vault-log-{stamp}.tar.gz"))
        .collect();
    for pack in [PACK_1, PACK_2] {
        held.extend([pack.to_owned(), format!("{pack}.manifest")]);
    }
    held.sort();
    assert_eq!(
        (
            names(&folder)?,
            members(PACK_1)?,
            members(PACK_2)?,
            std::fs::read_to_string(folder.join(format!("{PACK_1}.manifest")))?
                .contains(&format!("dictionary {dictionary}\n")),
            archive_holds_blobs,
            (verified.status.code(), report["verified"].as_u64()),
            report["indexed"].clone(),
            (
                planned.status.code(),
                plan["named"].as_u64(),
                plan["held"].as_u64(),
                plan["packed"].as_u64(),
                plan["dry_run"].as_bool(),
            ),
        ),
        (
            held,
            [1, pages.len()],
            [0, 2],
            true,
            false,
            (Some(0), Some(count + 2)),
            serde_json::json!({"rows": count + 2, "absent": 0, "length_differs": 0}),
            (
                Some(0),
                Some(count + 3),
                Some(count + 3),
                Some(0),
                Some(true)
            ),
        )
    );
    Ok(())
}

/// Section 8 row B3. A pack that is gone, of another length or of other bytes at the same
/// length, a manifest that is not whole (no `end` line, cut mid-line, a second blank in a line,
/// another count, a line after `end`, another pack's name) and a pack with no manifest: a dry run names each, in
/// both renderings, and answers no with every member unpacked; the next night exits 8 naming it,
/// deletes no pack or manifest and packs the members again, after which the packs that count
/// restore to a store `blobs verify` accepts.
#[test]
fn a_lost_short_or_unrecorded_pack_is_named_and_its_blobs_are_packed_again() -> TestResult {
    const DAMAGES: [&str; 10] = [
        "gone",
        "short",
        "altered",
        "cut",
        "torn",
        "spaced",
        "miscounted",
        "trailing",
        "misnamed",
        "unrecorded",
    ];
    let mut got = Vec::new();
    for damage in DAMAGES {
        let (root, _, _) = compressed_store()?;
        let work = backup_bench(root.path())?;
        let folder = work.path().join(FOLDER);
        night(work.path(), NIGHTS[0], 0)?;
        let (tar, manifest) = (
            folder.join(PACK_1),
            folder.join(format!("{PACK_1}.manifest")),
        );
        let text = std::fs::read_to_string(&manifest)?;
        let ended = text.rsplit_once("end ").map_or("", |(kept, _)| kept);
        match damage {
            "gone" => std::fs::remove_file(&tar)?,
            "short" => std::fs::write(&tar, "cut")?,
            "altered" => {
                let mut bytes = std::fs::read(&tar)?;
                if let Some(byte) = bytes.last_mut() {
                    *byte ^= 1;
                }
                std::fs::write(&tar, bytes)?;
            }
            "cut" => std::fs::write(&manifest, ended)?,
            "torn" => std::fs::write(&manifest, &text[..text.len() / 2])?,
            "spaced" => std::fs::write(&manifest, text.replacen("blob ", "blob  ", 1))?,
            "miscounted" => std::fs::write(&manifest, format!("{ended}end 0\n"))?,
            "trailing" => std::fs::write(&manifest, format!("{text}blob abcd\n"))?,
            "misnamed" => std::fs::write(&manifest, text.replacen(PACK_1, PACK_2, 1))?,
            _ => std::fs::remove_file(&manifest)?,
        }
        let before = names(&folder)?;
        let log = root.path().join(COMMIT_LOG);
        let facts = folder_facts(&folder, work.path())?;
        let planned = pack(root.path(), &log, &facts, &["--dry-run"])?;
        let plan = answered(&planned)?;
        let human = Command::new(env!("CARGO_BIN_EXE_newsvault"))
            .args(["blobs", "pack", "--dry-run", "--store"])
            .arg(root.path())
            .arg("--log")
            .arg(&log)
            .arg("--have")
            .arg(&facts[0])
            .arg("--manifests")
            .arg(&facts[1])
            .output()?;
        let second = night(work.path(), NIGHTS[1], 8)?;
        let after = names(&folder)?;
        let healed = pack(
            root.path(),
            &log,
            &folder_facts(&folder, work.path())?,
            &["--dry-run"],
        )?;
        let lost = strings(&answered(&healed)?["lost"]);
        let (restored, _) = restore(&folder, &lost)?;
        got.push((
            damage,
            planned.status.code(),
            (
                strings(&plan["lost"]),
                strings(&plan["unrecorded"]),
                plan["packed"].as_u64(),
            ),
            String::from_utf8_lossy(&human.stdout).contains(PACK_1),
            String::from_utf8_lossy(&second.stdout).contains(&format!("\"{PACK_1}\"")),
            before.iter().all(|name| after.contains(name)) && after.len() == before.len() + 3,
            (healed.status.code(), answered(&healed)?["packed"].as_u64()),
            newsvault(restored.path(), &["blobs", "verify"])?
                .status
                .code(),
        ));
    }
    let all = Some(u64::try_from(pages()?.len())? + 1);
    let (named, none) = (vec![PACK_1.to_owned()], Vec::<String>::new());
    let expected: Vec<_> = DAMAGES
        .iter()
        .map(|damage| {
            let (lost, unrecorded) = match *damage {
                "unrecorded" => (none.clone(), named.clone()),
                _ => (named.clone(), none.clone()),
            };
            (
                *damage,
                Some(1),
                (lost, unrecorded, all),
                true,
                true,
                true,
                (Some(8), Some(0)),
                Some(0),
            )
        })
        .collect();
    assert_eq!(got, expected);
    Ok(())
}

/// Section 8 row B3. A blob the log names that is gone, that no longer reads back to its digest
/// or that is no zstd frame, before any pack held it: the night packs every other member, writes
/// the log archive and the metric and exits 8, the blob named in the report and by one stderr
/// line. A dry run over the restored log names it and answers no. The next night names it
/// again and writes no pack; once the file is back a night packs it alone and exits 0, and a
/// pack that holds it is the copy a later loss restores from.
#[test]
fn a_named_blob_that_is_gone_or_altered_is_named_and_left_out_until_the_store_gives_it(
) -> TestResult {
    let reported = |output: &Output| {
        serde_json::Deserializer::from_slice(&output.stdout)
            .into_iter::<Value>()
            .map_while(Result::ok)
            .find(|value| value["schema"] == "newsvault.blobs.pack/1")
            .unwrap_or_default()
    };
    let mut got = Vec::new();
    let mut expected = Vec::new();
    for (damage, reason, code) in [
        (None, "absent", "input_absent"),
        (
            Some(frame_of(b"other bytes")?),
            "digest_mismatch",
            "input_unparseable",
        ),
        (
            Some(b"no frame".to_vec()),
            "digest_mismatch",
            "input_unparseable",
        ),
    ] {
        let (root, pages, _) = compressed_store()?;
        let work = backup_bench(root.path())?;
        let folder = work.path().join(FOLDER);
        let hex = sha256(&pages[0]);
        let file = zst_path(root.path(), &hex);
        let good = std::fs::read(&file)?;
        match damage {
            Some(bytes) => std::fs::write(&file, bytes)?,
            None => std::fs::remove_file(&file)?,
        }
        let left = |why: &str| serde_json::json!([{"kind": "blob", "digest": hex, "reason": why}]);
        let first = night(work.path(), NIGHTS[0], 8)?;
        let report = reported(&first);
        let said = String::from_utf8_lossy(&first.stderr).to_string();
        let lines: Vec<_> = said
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .map(|error| {
                (
                    error["code"].as_str().map(str::to_owned),
                    error["details"]["digest"].as_str() == Some(&hex),
                    error["message"].as_str().is_some_and(|m| m.contains(&hex)),
                )
            })
            .collect();
        let after_first = names(&folder)?;
        let manifest = std::fs::read_to_string(folder.join(format!("{PACK_1}.manifest")))?;
        let metric = std::fs::read_to_string(work.path().join("textfile/newsvault-backup.prom"))?;
        let (restored, _) = restore(&folder, &[])?;
        let facts = folder_facts(&folder, work.path())?;
        let log = restored.path().join(COMMIT_LOG);
        let planned = pack(restored.path(), &log, &facts, &["--dry-run"])?;
        let verified = newsvault(restored.path(), &["blobs", "verify"])?;
        let second = night(work.path(), NIGHTS[1], 8)?;
        let after_second = names(&folder)?;
        std::fs::write(&file, &good)?;
        night(work.path(), NIGHTS[2], 0)?;
        let third = format!("bellwether-vault-blobs-{}.tar.manifest", NIGHTS[2]);
        let repaired = std::fs::read_to_string(folder.join(third))?;
        std::fs::remove_file(&file)?;
        let out = TempDir::new()?;
        let skipped = pack(
            root.path(),
            &root.path().join(COMMIT_LOG),
            &folder_facts(&folder, work.path())?,
            &["--out", &out.path().join(PACK_2).display().to_string()],
        )?;
        let (whole, _) = restore(&folder, &[])?;
        got.push((
            (
                report["pack"].as_str().map(str::to_owned),
                report["packed"].as_u64(),
                report["not_packed"].clone(),
            ),
            lines,
            said.contains("is tried again every night"),
            (after_first.len(), manifest.contains(&hex)),
            metric.contains("newsvault_backup_last_success_timestamp_seconds 0\n"),
            (
                planned.status.code(),
                planned.stderr.is_empty(),
                answered(&planned)?["not_packed"].clone(),
            ),
            (
                verified.status.code(),
                answered(&verified)?["indexed"]["absent"].as_u64(),
            ),
            (
                reported(&second)["pack"].is_null(),
                reported(&second)["changed"].as_bool(),
                reported(&second)["not_packed"].clone(),
                after_second.len(),
            ),
            repaired.lines().skip(1).map(str::to_owned).collect(),
            (skipped.status.code(), answered(&skipped)?["pack"].is_null()),
            newsvault(whole.path(), &["blobs", "verify"])?.status.code(),
        ));
        expected.push((
            (
                Some(PACK_1.to_owned()),
                Some(u64::try_from(pages.len())?),
                left(reason),
            ),
            vec![(Some(code.to_owned()), true, true)],
            true,
            (3, false),
            true,
            (Some(1), true, left("dry_run")),
            (Some(1), Some(1)),
            (true, Some(false), left(reason), 4),
            vec![format!("blob {hex}"), "end 1".to_owned()],
            (Some(0), true),
            Some(0),
        ));
    }
    assert_eq!(got, expected);
    Ok(())
}

/// A `.zst` file names its dictionary in its header, so the dictionary goes into the pack that
/// first holds such a blob even when the log given has no row for it.
#[test]
fn a_dictionary_a_packed_blob_names_is_packed_with_it_when_the_log_has_no_row() -> TestResult {
    let (root, pages, dictionary) = compressed_store()?;
    let out = TempDir::new()?;
    let log = out.path().join(COMMIT_LOG);
    std::fs::copy(root.path().join(COMMIT_LOG), &log)?;
    Connection::open(&log)?
        .execute_batch("DELETE FROM dictionary_activations; DELETE FROM dictionaries;")?;
    let tar = out.path().join(PACK_1);
    let facts = folder_facts(out.path(), out.path())?;
    let packed = pack(
        root.path(),
        &log,
        &facts,
        &["--out", &tar.display().to_string()],
    )?;
    let report = answered(&packed)?;
    let manifest = std::fs::read_to_string(out.path().join(format!("{PACK_1}.manifest")))?;
    let count = u64::try_from(pages.len())?;
    assert_eq!(
        (
            packed.status.code(),
            (report["pack"].as_str(), report["changed"].as_bool()),
            report["named"].as_u64(),
            report["packed"].as_u64(),
            report["pack_bytes"].as_u64(),
            manifest
                .matches(&format!("dictionary {dictionary}\n"))
                .count(),
            manifest.lines().next_back(),
        ),
        (
            Some(0),
            (Some(PACK_1), Some(true)),
            Some(count),
            Some(count + 1),
            Some(std::fs::metadata(&tar)?.len()),
            1,
            Some(format!("end {}", count + 1).as_str()),
        )
    );
    Ok(())
}

/// A writer needs its active dictionary's file before any blob names it, so a dictionary the
/// log records is packed from its row: here every blob is still raw. A dry run given `--out`
/// writes nothing and answers no; a dictionary of other bytes, or one that is gone, is named
/// and left out of a pack of every raw blob, exit 8.
#[test]
fn a_dictionary_no_blob_names_yet_is_packed_from_the_logs_row() -> TestResult {
    let (root, pages) = legacy_store()?;
    let trained = answered(&train(root.path(), &[])?)?;
    let out = TempDir::new()?;
    let facts = folder_facts(out.path(), out.path())?;
    let tar = out.path().join(PACK_1).display().to_string();
    let log = root.path().join(COMMIT_LOG);
    let planned = pack(root.path(), &log, &facts, &["--dry-run", "--out", &tar])?;
    let plan = answered(&planned)?;
    let planned_only = !Path::new(&tar).exists() && plan["pack"].is_null();
    let packed = pack(root.path(), &log, &facts, &["--out", &tar])?;
    let restored = TempDir::new()?;
    let unpacked = Command::new("tar")
        .args(["-xf", &tar, "-C"])
        .arg(restored.path())
        .status()?;
    let dictionary = trained["dictionary"].as_str().unwrap_or_default();
    let stored = root.path().join("blobs/dictionaries").join(dictionary);
    let kept = std::fs::read(&stored).ok();
    let mut refusals = Vec::new();
    for (again, altered) in [(PACK_2, true), ("gone.tar", false)] {
        if altered {
            std::fs::write(&stored, "other bytes")?;
        } else {
            std::fs::remove_file(&stored)?;
        }
        let tar = out.path().join(again).display().to_string();
        let partial = pack(root.path(), &log, &facts, &["--out", &tar])?;
        let error: Value = serde_json::from_slice(&partial.stderr)?;
        let report = answered(&partial)?;
        refusals.push((
            partial.status.code(),
            error["code"].as_str().map(str::to_owned),
            report["not_packed"].clone(),
            report["packed"].as_u64(),
            std::fs::read_to_string(format!("{tar}.manifest"))?.contains(dictionary),
        ));
    }
    let refusal = |code: &str, reason: &str| {
        (
            Some(8),
            Some(code.to_owned()),
            serde_json::json!([{"kind": "dictionary", "digest": dictionary, "reason": reason}]),
            u64::try_from(pages.len()).ok(),
            false,
        )
    };
    assert_eq!(
        (
            (
                planned.status.code(),
                planned_only,
                plan["dry_run"].as_bool()
            ),
            packed.status.code(),
            answered(&packed)?["packed"].as_u64(),
            unpacked.success(),
            std::fs::read(restored.path().join("blobs/dictionaries").join(dictionary)).ok(),
            refusals,
        ),
        (
            (Some(1), true, Some(true)),
            Some(0),
            Some(u64::try_from(pages.len())? + 1),
            true,
            kept,
            vec![
                refusal("input_unparseable", "digest_mismatch"),
                refusal("input_absent", "absent"),
            ],
        )
    );
    Ok(())
}

/// Each refusal exits by its code with stdout empty: no `--out` outside a dry run, an input that
/// is absent, a listing line that is not three words, and an `--out` or its manifest that
/// exists, which is left as it was.
#[test]
fn each_pack_refusal_exits_by_its_code_with_stdout_empty() -> TestResult {
    const KEPT: &str = "a manifest that was there";
    let (root, _) = legacy_store()?;
    let out = TempDir::new()?;
    let at = |name: &str| out.path().join(name);
    let log = root.path().join(COMMIT_LOG);
    let good = folder_facts(out.path(), out.path())?;
    for (name, text) in [
        ("one-word", "one-word\n"),
        ("two-words", "p.tar 1\n"),
        ("four-words", "p.tar 1 ab cd\n"),
        ("taken", ""),
        ("fresh.tar.manifest", KEPT),
    ] {
        std::fs::write(at(name), text)?;
    }
    let shown = |name: &str| at(name).display().to_string();
    let (tar, taken, fresh) = (shown(PACK_1), shown("taken"), shown("fresh.tar"));
    let listed = |name: &str| ([at(name), good[1].clone()], vec!["--dry-run"]);
    let mut asks = vec![
        (log.clone(), good.clone(), vec![]),
        (at("no-log"), good.clone(), vec!["--out", tar.as_str()]),
        (
            log.clone(),
            [good[0].clone(), at("none")],
            vec!["--dry-run"],
        ),
        (log.clone(), good.clone(), vec!["--out", taken.as_str()]),
        (log.clone(), good.clone(), vec!["--out", fresh.as_str()]),
    ];
    for name in ["no-have", "one-word", "two-words", "four-words"] {
        let (facts, extra) = listed(name);
        asks.push((log.clone(), facts, extra));
    }
    let mut got = Vec::new();
    for (log, facts, extra) in asks {
        let refused = pack(root.path(), &log, &facts, &extra)?;
        let code = serde_json::from_slice::<Value>(&refused.stderr)
            .ok()
            .and_then(|error| error["code"].as_str().map(str::to_owned));
        got.push((refused.status.code(), refused.stdout.is_empty(), code));
    }
    let coded = |exit, code: &str| (Some(exit), true, Some(code.to_owned()));
    assert_eq!(
        (
            got,
            std::fs::read_to_string(at("fresh.tar.manifest"))?,
            at("fresh.tar").exists(),
        ),
        (
            vec![
                (Some(2), true, None),
                coded(4, "input_absent"),
                coded(4, "input_absent"),
                coded(2, "invalid_argument"),
                coded(2, "invalid_argument"),
                coded(4, "input_absent"),
                coded(2, "input_unparseable"),
                coded(2, "input_unparseable"),
                coded(2, "input_unparseable"),
            ],
            KEPT.to_owned(),
            false,
        )
    );
    Ok(())
}

/// The manifest is hand-written and the parser derived, so each flag `--help` lists is named.
#[test]
fn every_option_the_blob_parsers_take_is_in_the_machine_readable_schema() -> TestResult {
    let binary = env!("CARGO_BIN_EXE_newsvault");
    let schema: Value = serde_json::from_slice(
        &Command::new(binary)
            .args(["schema", "--format", "json"])
            .output()?
            .stdout,
    )?;
    let mut missing = Vec::new();
    for path in [
        vec!["blobs", "verify"],
        vec!["blobs", "compress"],
        vec!["blobs", "pack"],
        vec!["blobs", "dictionary", "train"],
    ] {
        let help = Command::new(binary).args(&path).arg("--help").output()?;
        let named: Vec<String> = schema["commands"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|command| command["path"] == serde_json::json!(path))
            .and_then(|command| command["options"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|option| option["name"].as_str().map(|name| name.replace('_', "-")))
            .collect();
        let help = String::from_utf8_lossy(&help.stdout).to_string();
        missing.extend(
            help.split_whitespace()
                .filter_map(|word| {
                    word.trim_end_matches(|c: char| !c.is_alphanumeric())
                        .strip_prefix("--")
                })
                .filter(|long| {
                    !long.is_empty() && *long != "help" && !named.contains(&(*long).to_owned())
                })
                .map(|long| format!("{} --{long}", path.join(" "))),
        );
        if named.is_empty() {
            missing.push(path.join(" "));
        }
    }
    assert_eq!(missing, Vec::<String>::new());
    Ok(())
}
