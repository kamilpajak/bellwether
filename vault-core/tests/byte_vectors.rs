//! Every file below a blob root after one put of a fixed payload, under each write policy,
//! against `vectors/blob_tree.json`. What may change that file: architecture.md section 8 row B4.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;

use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use vault_core::blobs::{BlobStore, WritePolicy};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const VECTOR: &str = include_str!("vectors/blob_tree.json");
const RUN: &str = "2026-07-06T00:00:00Z";
const RAW: &str = "raw_until_dictionary_before_a_dictionary";
const ZSTD: &str = "raw_until_dictionary_under_the_dictionary";
const TRAINED: &str = "raw_until_dictionary_under_a_trained_dictionary";
const PLAIN: &str = "plain_zstd";

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

/// The vector's payload put into an empty blob root under the policy and the dictionary `case`
/// names: the files the case records, the files on disk, and whether a handle of the OTHER
/// policy read the payload back.
fn put(case: &str) -> TestResult<(Value, Value, bool)> {
    let vector: Value = serde_json::from_str(VECTOR)?;
    let text = |key: &str| vector["input"][key].as_str().ok_or(format!("no {key}"));
    let payload = text("payload")?.as_bytes();
    let root = TempDir::new()?;
    let (policy, other) = match case {
        PLAIN => (WritePolicy::PlainZstd, WritePolicy::RawUntilDictionary),
        _ => (WritePolicy::RawUntilDictionary, WritePolicy::PlainZstd),
    };
    let writer = BlobStore::new(root.path(), policy);
    let dictionary = match case {
        ZSTD => Some(text("dictionary")?.as_bytes().to_vec()),
        TRAINED => Some(const_hex::decode(text("trained_dictionary_hex")?)?),
        _ => None,
    };
    if let Some(dictionary) = dictionary {
        writer.activate(&writer.store_dictionary(RUN, &dictionary)?)?;
    }
    let digest = writer.put(RUN, payload)?.digest;
    let mut found = BTreeMap::new();
    files(root.path(), root.path(), &mut found)?;
    let read = BlobStore::new(root.path(), other).read_verified(&digest)?;
    Ok((vector[case].clone(), json!(found), read == payload))
}

#[test]
fn raw_until_dictionary_before_a_dictionary_leaves_the_recorded_raw_file() -> TestResult {
    let (recorded, written, read_back) = put(RAW)?;
    assert_eq!((written, read_back), (recorded, true));
    Ok(())
}

#[test]
fn raw_until_dictionary_under_a_dictionary_leaves_the_recorded_zst_file_beside_it() -> TestResult {
    let (recorded, written, read_back) = put(ZSTD)?;
    assert_eq!((written, read_back), (recorded, true));
    Ok(())
}

#[test]
fn raw_until_dictionary_under_a_trained_dictionary_leaves_the_recorded_zst_file() -> TestResult {
    let (recorded, written, read_back) = put(TRAINED)?;
    assert_eq!((written, read_back), (recorded, true));
    Ok(())
}

#[test]
fn plain_zstd_leaves_the_recorded_zst_file_and_no_dictionary() -> TestResult {
    let (recorded, written, read_back) = put(PLAIN)?;
    assert_eq!((written, read_back), (recorded, true));
    Ok(())
}
