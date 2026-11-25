//! Store mechanics shared by every archive of this repository. What may live here and what may
//! not: `docs/design/architecture.md` section 12.1.

use std::path::PathBuf;

pub mod blobs;
pub mod lease;
pub mod log_copy;
pub mod sqlite;

/// Every failure [`blobs::BlobStore`] returns.
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("no blob is stored under {digest}")]
    BlobAbsent { digest: String },
    /// `BlobStore::put` keeps a file that exists, so a second `put` cannot repair this one.
    #[error("the blob stored under {digest} now hashes to {found}")]
    BlobAltered { digest: String, found: String },
    /// A `.zst` copy whose header or frame zstd cannot read back to one payload.
    #[error("the compressed blob {digest} cannot be decoded: {why}")]
    BlobUndecodable { digest: String, why: String },
    /// A file in the blob tree named neither `<digest>` nor `<digest>.zst`.
    #[error("{} is in the blob tree and names no digest", .path.display())]
    BlobStray { path: PathBuf },
    /// A dictionary a `.zst` header or a caller names is not on disk; never read as "none".
    #[error("the zstd dictionary {dictionary} is not in the store")]
    DictionaryAbsent { dictionary: String },
    #[error("the zstd dictionary stored under {dictionary} now hashes to {found}")]
    DictionaryAltered { dictionary: String, found: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Every failure of taking a [`lease::Lease`].
#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error("another run holds the lease on {0}")]
    LeaseHeld(PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Every failure [`log_copy::LogCopy`] returns.
#[derive(Debug, thiserror::Error)]
pub enum CopyError {
    #[error("no commit log at {0}")]
    StoreAbsent(PathBuf),
    #[error("the directory {0} the copy is written into does not exist")]
    OutputDirectoryAbsent(PathBuf),
    #[error("{0} exists, and a snapshot never replaces a file")]
    OutputExists(PathBuf),
    #[error("another run holds the lease on {0}")]
    LeaseHeld(PathBuf),
    #[error(
        "a writer outside the lease kept the commit log locked past {:?}",
        log_copy::BUSY_WAIT
    )]
    SourceBusy,
    /// The online backup copies pages as they are, so a damaged page reaches the copy and is
    /// named here, by SQLite's `SQLITE_CORRUPT` or by `quick_check`, instead of being uploaded.
    #[error("the copy is not a sound database: {0}")]
    CopyMalformed(String),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<LeaseError> for CopyError {
    fn from(failure: LeaseError) -> Self {
        match failure {
            LeaseError::LeaseHeld(lock) => Self::LeaseHeld(lock),
            LeaseError::Io(failure) => Self::Io(failure),
        }
    }
}
