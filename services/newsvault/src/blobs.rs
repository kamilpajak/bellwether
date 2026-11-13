//! The content-addressed payload store: architecture.md section 2's "Content-addressed blobs"
//! and section 3.9's L2 layer. A blob is written, fsynced and renamed BEFORE any transaction
//! opens, so a crash leaves an unreferenced file and never an index row about a missing one.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::store::StoreError;

/// A sha256 over a payload's bytes, lower-case hex. The field is private and [`BlobStore::put`]
/// is the only producer, so a value of this type names bytes this process hashed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlobDigest(String);

impl BlobDigest {
    #[must_use]
    pub fn hex(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BlobDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Hex characters per directory level, two levels deep, which is the layout design.md section 5
/// states.
const FAN_OUT: usize = 2;

#[derive(Debug)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn path_of(&self, digest: &BlobDigest) -> PathBuf {
        let hex = digest.hex();
        self.root
            .join(&hex[..FAN_OUT])
            .join(&hex[FAN_OUT..FAN_OUT * 2])
            .join(hex)
    }

    /// Where the bytes land before the rename. Named by the run as well as the digest: a crash
    /// leaves this path behind and [`BlobStore::put`] refuses to reopen it.
    pub(crate) fn staging_path(&self, run: &str, digest: &BlobDigest) -> PathBuf {
        self.root.join(format!(".tmp-{run}-{}", digest.hex()))
    }

    /// Stores a payload under its own digest and returns it.
    pub fn put(&self, run: &str, payload: &[u8]) -> Result<BlobDigest, StoreError> {
        let digest = BlobDigest(const_hex::encode(Sha256::digest(payload)));
        let stored = self.path_of(&digest);
        if stored.exists() {
            return Ok(digest);
        }
        if let Some(parent) = stored.parent() {
            fs::create_dir_all(parent)?;
        }
        let staging = self.staging_path(run, &digest);
        // `File::create` TRUNCATES an existing path: measured 2026-11-13, a 10-byte file rewritten
        // through it is 2 bytes afterwards, while `create_new` answers `AlreadyExists`.
        let mut file = File::create_new(&staging)?;
        file.write_all(payload)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&staging, &stored)?;
        sync_directory(stored.parent())?;
        Ok(digest)
    }

    /// The stored length, and the check that no index row is written about a missing file.
    pub fn len_of(&self, digest: &BlobDigest) -> Result<u64, StoreError> {
        fs::metadata(self.path_of(digest))
            .map(|stored| stored.len())
            .map_err(|_| StoreError::BlobAbsent {
                digest: digest.hex().to_owned(),
            })
    }
}

/// A rename is durable only once the directory holding the new name is fsynced.
fn sync_directory(directory: Option<&Path>) -> Result<(), StoreError> {
    if let Some(directory) = directory {
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}
