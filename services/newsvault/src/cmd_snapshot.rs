//! `newsvault snapshot --out <file>`: one consistent copy of the commit log for the daily backup
//! (architecture.md section 10, N1). The copy is `vault_core::log_copy`'s; this file holds the
//! news store's paths, the envelope and the section 5.1 codes.

use std::path::{Path, PathBuf};

use serde::Serialize;
use vault_core::log_copy::{LogCopy, BUSY_WAIT};
use vault_core::CopyError;

use crate::store::{COMMIT_LOG, LOCK_FILE};

pub const SCHEMA: &str = "newsvault.snapshot/1";

#[derive(Debug, Clone)]
pub struct Ask {
    pub store: PathBuf,
    pub out: PathBuf,
    pub dry_run: bool,
}

/// The envelope: the copy's own `user_version` and size, or, for a dry run, the source's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    pub schema: &'static str,
    pub store: String,
    pub out: String,
    pub user_version: i32,
    pub bytes: u64,
    pub dry_run: bool,
    pub changed: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("no commit log at {0}")]
    StoreAbsent(PathBuf),
    #[error("the directory {0} the copy is written into does not exist")]
    OutputDirectoryAbsent(PathBuf),
    #[error("{0} exists, and a snapshot never replaces a file")]
    OutputExists(PathBuf),
    #[error("another run holds the lease on {0}")]
    LeaseHeld(PathBuf),
    #[error("a writer outside the lease kept the commit log locked past {BUSY_WAIT:?}")]
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

impl SnapshotError {
    /// The section 5.1 registry code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::StoreAbsent(_) | Self::OutputDirectoryAbsent(_) => "input_absent",
            Self::OutputExists(_) => "invalid_argument",
            Self::LeaseHeld(_) | Self::SourceBusy => "lease_held",
            Self::CopyMalformed(_) => "input_unparseable",
            Self::Sqlite(_) | Self::Io(_) => "io_failed",
        }
    }

    #[must_use]
    pub const fn exit(&self) -> u8 {
        match self {
            Self::StoreAbsent(_) | Self::OutputDirectoryAbsent(_) => 4,
            Self::LeaseHeld(_) | Self::SourceBusy => 7,
            _ => 2,
        }
    }

    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(self, Self::LeaseHeld(_) | Self::SourceBusy)
    }
}

impl From<CopyError> for SnapshotError {
    fn from(failure: CopyError) -> Self {
        match failure {
            CopyError::StoreAbsent(log) => Self::StoreAbsent(log),
            CopyError::OutputDirectoryAbsent(directory) => Self::OutputDirectoryAbsent(directory),
            CopyError::OutputExists(out) => Self::OutputExists(out),
            CopyError::LeaseHeld(lock) => Self::LeaseHeld(lock),
            CopyError::SourceBusy => Self::SourceBusy,
            CopyError::CopyMalformed(why) => Self::CopyMalformed(why),
            CopyError::Sqlite(failure) => Self::Sqlite(failure),
            CopyError::Io(failure) => Self::Io(failure),
        }
    }
}

/// A dry run stops after the refusals with the source's version and size.
pub fn run(ask: &Ask) -> Result<Snapshot, SnapshotError> {
    let copy = LogCopy::open(
        &ask.store.join(LOCK_FILE),
        &ask.store.join(COMMIT_LOG),
        &ask.out,
    )?;
    let state = if ask.dry_run {
        copy.source_state()?
    } else {
        copy.write()?
    };
    let shown = |path: &Path| path.display().to_string();
    Ok(Snapshot {
        schema: SCHEMA,
        store: shown(&ask.store),
        out: shown(&ask.out),
        user_version: state.user_version,
        bytes: state.bytes,
        dry_run: ask.dry_run,
        changed: !ask.dry_run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seam of architecture.md section 12.8 step 4: each `vault_core::CopyError` becomes the
    /// `SnapshotError` of the same name, fields and text, which is the one this command prints.
    #[test]
    fn a_copy_failure_becomes_the_snapshot_failure_of_the_same_name_fields_and_text() {
        let path = || PathBuf::from("a/store/file");
        let failures = [
            CopyError::StoreAbsent(path()),
            CopyError::OutputDirectoryAbsent(path()),
            CopyError::OutputExists(path()),
            CopyError::LeaseHeld(path()),
            CopyError::SourceBusy,
            CopyError::CopyMalformed("row 3 missing from index".to_owned()),
            CopyError::Sqlite(rusqlite::Error::QueryReturnedNoRows),
            CopyError::Io(std::io::Error::other("the disk is full")),
        ];
        for failure in failures {
            let (shown, text) = (format!("{failure:?}"), failure.to_string());
            let kept = SnapshotError::from(failure);
            assert_eq!((format!("{kept:?}"), kept.to_string()), (shown, text));
        }
    }
}
