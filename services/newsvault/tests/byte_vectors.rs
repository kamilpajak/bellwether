//! Architecture.md section 12.8 step 2: every file the blob tree holds after one put of a fixed
//! payload, against `vectors/blob_tree.json`. What may change that file: section 8 row V1.
//! The tree is reached through `store::blob_store` alone, so nothing here names an item that
//! section 12.8 moves.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;

use newsvault::store::blob_store;
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const VECTOR: &str = include_str!("vectors/blob_tree.json");
const RUN: &str = "2026-07-06T00:00:00Z";
const RAW: &str = "before_a_dictionary";
const ZSTD: &str = "under_the_dictionary";

/// Every file under `directory`, by its `/`-joined path below `root`, with its bytes' sha256.
fn files(root: &Path, directory: &Path, found: &mut BTreeMap<String, String>) -> TestResult {
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            files(root, &path, found)?;
            continue;
        }
        let below: Vec<_> = path.strip_prefix(root)?.iter().collect();
        let name: Vec<_> = below.iter().map(|part| part.to_string_lossy()).collect();
        let digest = const_hex::encode(Sha256::digest(std::fs::read(&path)?));
        found.insert(name.join("/"), digest);
    }
    Ok(())
}

/// The vector's payload put into an empty store, under its dictionary when `case` is [`ZSTD`]:
/// the files the case records, the files on disk, and whether a second handle read it back.
fn put(case: &str) -> TestResult<(Value, Value, bool)> {
    let vector: Value = serde_json::from_str(VECTOR)?;
    let text = |key: &str| vector["input"][key].as_str().ok_or(format!("no {key}"));
    let payload = text("payload")?.as_bytes();
    let root = TempDir::new()?;
    let writer = blob_store(root.path());
    if case == ZSTD {
        writer.activate(&writer.store_dictionary(RUN, text("dictionary")?.as_bytes())?)?;
    }
    let digest = writer.put(RUN, payload)?;
    let mut found = BTreeMap::new();
    files(root.path(), root.path(), &mut found)?;
    let read = blob_store(root.path()).read_verified(&digest)?;
    Ok((vector[case].clone(), json!(found), read == payload))
}

#[test]
fn a_payload_put_before_any_dictionary_is_the_recorded_raw_file() -> TestResult {
    let (recorded, written, read_back) = put(RAW)?;
    assert_eq!((written, read_back), (recorded, true));
    Ok(())
}

#[test]
fn a_payload_put_under_a_dictionary_is_the_recorded_zst_file_beside_its_dictionary() -> TestResult {
    let (recorded, written, read_back) = put(ZSTD)?;
    assert_eq!((written, read_back), (recorded, true));
    Ok(())
}
