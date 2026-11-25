//! Architecture.md section 12.8 step 2: every file the blob tree holds after one put of a fixed
//! payload, and the names at a store's root, against `vectors/blob_tree.json`. What may change
//! that file: section 8 row B4. The store is reached through `store::blob_store` and
//! `Committer::open` alone, so nothing here names an item that section 12.8 moves.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;

use news_provider::RunId;
use newsvault::store::{blob_store, Committer};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const VECTOR: &str = include_str!("vectors/blob_tree.json");
const RUN: &str = "2026-07-06T00:00:00Z";
const RAW: &str = "before_a_dictionary";
const ZSTD: &str = "under_the_dictionary";
const TRAINED: &str = "under_a_trained_dictionary";
const ROOT: &str = "store_root";

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

/// The vector's payload put into an empty store, under the dictionary `case` names, if any:
/// the files the case records, the files on disk, and whether a second handle read it back.
fn put(case: &str) -> TestResult<(Value, Value, bool)> {
    let vector: Value = serde_json::from_str(VECTOR)?;
    let text = |key: &str| vector["input"][key].as_str().ok_or(format!("no {key}"));
    let payload = text("payload")?.as_bytes();
    let root = TempDir::new()?;
    let writer = blob_store(root.path());
    let dictionary = match case {
        ZSTD => Some(text("dictionary")?.as_bytes().to_vec()),
        TRAINED => Some(const_hex::decode(text("trained_dictionary_hex")?)?),
        _ => None,
    };
    if let Some(dictionary) = dictionary {
        writer.activate(&writer.store_dictionary(RUN, &dictionary)?)?;
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

#[test]
fn a_payload_put_under_a_trained_dictionary_is_the_recorded_zst_file() -> TestResult {
    let (recorded, written, read_back) = put(TRAINED)?;
    assert_eq!((written, read_back), (recorded, true));
    Ok(())
}

#[test]
fn a_store_opened_for_commits_has_the_recorded_names_at_its_root() -> TestResult {
    let vector: Value = serde_json::from_str(VECTOR)?;
    let root = TempDir::new()?;
    let _store = Committer::open(root.path(), &RunId::new(ROOT))?;
    let mut names = Vec::new();
    for entry in std::fs::read_dir(root.path())? {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    assert_eq!(json!(names), vector[ROOT]);
    Ok(())
}
