//! Store mechanics shared by every archive of this repository. What may live here and what may
//! not: `docs/design/architecture.md` section 12.1.

use std::path::PathBuf;

pub mod blobs;

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
