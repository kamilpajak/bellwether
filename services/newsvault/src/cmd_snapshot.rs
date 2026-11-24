//! `newsvault snapshot --out <file>`: one consistent copy of the commit log for the daily backup
//! (architecture.md section 10, N1). SQLite's online backup in ONE step, so the copy is a single
//! read transaction, taken under the store's lease and checked before it is renamed into place.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

use crate::store::{self, StoreError, COMMIT_LOG};

pub const SCHEMA: &str = "newsvault.snapshot/1";
/// Every page in one `sqlite3_backup_step`: a step count above 1 restarts on each foreign commit.
const ALL_PAGES: i32 = -1;
/// The wait for a commit by a connection outside the lease, which holds no `LOCK` (a raw writer).
const BUSY_WAIT: Duration = Duration::from_secs(30);
const QUICK_CHECK_OK: &str = "ok";

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

impl From<StoreError> for SnapshotError {
    fn from(failure: StoreError) -> Self {
        match failure {
            StoreError::LeaseHeld(path) => Self::LeaseHeld(path),
            StoreError::Io(failure) => Self::Io(failure),
            StoreError::Sqlite(failure) => Self::Sqlite(failure),
            other => Self::Io(std::io::Error::other(other.to_string())),
        }
    }
}

/// The partial copy beside `out`, renamed onto it only once checked.
fn partial_of(out: &Path) -> PathBuf {
    let name = out
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    out.with_file_name(format!(".{name}.partial"))
}

fn user_version(db: &Connection) -> Result<i32, rusqlite::Error> {
    db.query_row("PRAGMA user_version", [], |row| row.get(0))
}

/// Refuses before anything is opened for writing; a dry run stops after it with the source's
/// version and size.
pub fn run(ask: &Ask) -> Result<Snapshot, SnapshotError> {
    let log = ask.store.join(COMMIT_LOG);
    if !log.is_file() {
        return Err(SnapshotError::StoreAbsent(log));
    }
    let directory = match ask.out.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    if !directory.is_dir() {
        return Err(SnapshotError::OutputDirectoryAbsent(directory));
    }
    if fs::symlink_metadata(&ask.out).is_ok() {
        return Err(SnapshotError::OutputExists(ask.out.clone()));
    }
    let _lease = store::hold_lease_to_copy(&ask.store)?;
    let source = store::read_only(&log)?;
    source.busy_timeout(BUSY_WAIT)?;
    let shown = |path: &Path| path.display().to_string();
    if ask.dry_run {
        let size: i64 = source.query_row(
            "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
            [],
            |row| row.get(0),
        )?;
        return Ok(Snapshot {
            schema: SCHEMA,
            store: shown(&ask.store),
            out: shown(&ask.out),
            user_version: user_version(&source)?,
            bytes: u64::try_from(size).unwrap_or_default(),
            dry_run: true,
            changed: false,
        });
    }
    let partial = partial_of(&ask.out);
    match fs::remove_file(&partial) {
        Err(failure) if failure.kind() != std::io::ErrorKind::NotFound => {
            return Err(failure.into())
        }
        _ => {}
    }
    let copied = copy(&source, &partial);
    let version = match copied {
        Ok(version) => version,
        Err(failure) => {
            #[allow(
                clippy::let_underscore_must_use,
                reason = "the copy's own failure is the one reported; a partial left behind is \
                          removed by the next run"
            )]
            let _ = fs::remove_file(&partial);
            return Err(failure);
        }
    };
    File::open(&partial)?.sync_all()?;
    fs::rename(&partial, &ask.out)?;
    File::open(&directory)?.sync_all()?;
    Ok(Snapshot {
        schema: SCHEMA,
        store: shown(&ask.store),
        out: shown(&ask.out),
        user_version: version,
        bytes: fs::metadata(&ask.out)?.len(),
        dry_run: false,
        changed: true,
    })
}

/// The online backup into `partial`, then the copy's `quick_check` and `user_version`; a page
/// SQLite reads as corrupt on either side is `CopyMalformed`.
fn copy(source: &Connection, partial: &Path) -> Result<i32, SnapshotError> {
    checked_copy(source, partial).map_err(|failure| match failure {
        SnapshotError::Sqlite(rusqlite::Error::SqliteFailure(code, message))
            if code.code == rusqlite::ErrorCode::DatabaseCorrupt =>
        {
            SnapshotError::CopyMalformed(message.unwrap_or_else(|| code.to_string()))
        }
        other => other,
    })
}

fn checked_copy(source: &Connection, partial: &Path) -> Result<i32, SnapshotError> {
    let mut target = store::connect(partial, OpenFlags::default())?;
    {
        let backup = Backup::new(source, &mut target)?;
        loop {
            match backup.step(ALL_PAGES)? {
                StepResult::Done => break,
                StepResult::More => {}
                _ => return Err(SnapshotError::SourceBusy),
            }
        }
    }
    let verdicts: Vec<String> = target
        .prepare("PRAGMA quick_check")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    if verdicts != [QUICK_CHECK_OK] {
        return Err(SnapshotError::CopyMalformed(verdicts.join("; ")));
    }
    let version = user_version(&target)?;
    target.close().map_err(|(_, failure)| failure)?;
    Ok(version)
}
