//! One consistent copy of a store's log, for a backup. SQLite's online backup in ONE step, so the
//! copy is a single read transaction, taken under the store's lease and checked before it is
//! renamed into place. The lock file and the log are the paths the store passes.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags};

use crate::lease::Lease;
use crate::{sqlite, CopyError};

/// Every page in one `sqlite3_backup_step`: a step count above 1 restarts on each foreign commit.
const ALL_PAGES: i32 = -1;
/// The wait for a commit by a connection outside the lease, which holds no lock file (a raw
/// writer).
pub const BUSY_WAIT: Duration = Duration::from_secs(30);
const QUICK_CHECK_OK: &str = "ok";

/// A log's `user_version` and size in bytes: the copy's own, or, before a copy, the source's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogState {
    pub user_version: i32,
    pub bytes: u64,
}

/// A log opened to be copied to `out`: the lease is held until this is dropped.
#[derive(Debug)]
pub struct LogCopy {
    out: PathBuf,
    directory: PathBuf,
    source: Connection,
    _lease: Option<Lease>,
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

impl LogCopy {
    /// Refuses before anything is opened for writing, then takes the lease at `lock` and opens
    /// `log` read-only.
    pub fn open(lock: &Path, log: &Path, out: &Path) -> Result<Self, CopyError> {
        if !log.is_file() {
            return Err(CopyError::StoreAbsent(log.to_path_buf()));
        }
        let directory = match out.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        if !directory.is_dir() {
            return Err(CopyError::OutputDirectoryAbsent(directory));
        }
        if fs::symlink_metadata(out).is_ok() {
            return Err(CopyError::OutputExists(out.to_path_buf()));
        }
        let lease = Lease::hold_to_copy(lock)?;
        let source = sqlite::read_only(log)?;
        source.busy_timeout(BUSY_WAIT)?;
        Ok(Self {
            out: out.to_path_buf(),
            directory,
            source,
            _lease: lease,
        })
    }

    /// What a dry run reports: the source's version and size, with nothing written.
    pub fn source_state(&self) -> Result<LogState, CopyError> {
        let size: i64 = self.source.query_row(
            "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
            [],
            |row| row.get(0),
        )?;
        Ok(LogState {
            user_version: user_version(&self.source)?,
            bytes: u64::try_from(size).unwrap_or_default(),
        })
    }

    /// Copies into a partial file beside `out`, checks it, and renames it into place after its
    /// bytes are fsynced.
    pub fn write(self) -> Result<LogState, CopyError> {
        let partial = partial_of(&self.out);
        match fs::remove_file(&partial) {
            Err(failure) if failure.kind() != std::io::ErrorKind::NotFound => {
                return Err(failure.into())
            }
            _ => {}
        }
        let copied = copy(&self.source, &partial);
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
        fs::rename(&partial, &self.out)?;
        File::open(&self.directory)?.sync_all()?;
        Ok(LogState {
            user_version: version,
            bytes: fs::metadata(&self.out)?.len(),
        })
    }
}

/// The online backup into `partial`, then the copy's `quick_check` and `user_version`; a page
/// SQLite reads as corrupt on either side is `CopyMalformed`.
fn copy(source: &Connection, partial: &Path) -> Result<i32, CopyError> {
    checked_copy(source, partial).map_err(|failure| match failure {
        CopyError::Sqlite(rusqlite::Error::SqliteFailure(code, message))
            if code.code == rusqlite::ErrorCode::DatabaseCorrupt =>
        {
            CopyError::CopyMalformed(message.unwrap_or_else(|| code.to_string()))
        }
        other => other,
    })
}

fn checked_copy(source: &Connection, partial: &Path) -> Result<i32, CopyError> {
    let mut target = sqlite::connect(partial, OpenFlags::default())?;
    {
        let backup = Backup::new(source, &mut target)?;
        loop {
            match backup.step(ALL_PAGES)? {
                StepResult::Done => break,
                StepResult::More => {}
                _ => return Err(CopyError::SourceBusy),
            }
        }
    }
    let verdicts: Vec<String> = target
        .prepare("PRAGMA quick_check")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    if verdicts != [QUICK_CHECK_OK] {
        return Err(CopyError::CopyMalformed(verdicts.join("; ")));
    }
    let version = user_version(&target)?;
    target.close().map_err(|(_, failure)| failure)?;
    Ok(version)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::LeaseError;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Names no store of this repository uses: the crate fixes neither.
    const LOG: &str = "any.sqlite3";
    const LOCK: &str = "any.lock";
    const VERSION: i32 = 7;

    fn log_at(root: &Path) -> Result<PathBuf, rusqlite::Error> {
        let log = root.join(LOG);
        let db = sqlite::connect(&log, OpenFlags::default())?;
        db.execute_batch("CREATE TABLE kept (n INTEGER NOT NULL); INSERT INTO kept VALUES (3)")?;
        db.pragma_update(None, "user_version", VERSION)?;
        Ok(log)
    }

    #[test]
    fn a_log_under_any_file_name_is_copied_whole_and_a_held_lease_refuses_the_copy() -> TestResult {
        let temp = TempDir::new()?;
        let (lock, out) = (temp.path().join(LOCK), temp.path().join("copy.sqlite3"));
        let log = log_at(temp.path())?;
        let held = Lease::take(&lock)?;
        let refused = LogCopy::open(&lock, &log, &out);
        drop(held);
        let opened = LogCopy::open(&lock, &log, &out)?;
        let held_while_open = matches!(Lease::take(&lock), Err(LeaseError::LeaseHeld(_)));
        let planned = opened.source_state()?;
        let written_by_the_dry_read = out.exists();
        let copied = opened.write()?;
        let free_once_written = Lease::take(&lock).is_ok();
        let kept: i64 =
            sqlite::read_only(&out)?.query_row("SELECT n FROM kept", [], |row| row.get(0))?;
        let again = LogCopy::open(&lock, &log, &out);
        assert!(
            matches!(&refused, Err(CopyError::LeaseHeld(path)) if *path == lock)
                && matches!(&again, Err(CopyError::OutputExists(path)) if *path == out),
            "{refused:?} {again:?}"
        );
        assert_eq!(
            (planned.user_version, written_by_the_dry_read, copied, kept),
            (
                VERSION,
                false,
                LogState {
                    user_version: VERSION,
                    bytes: fs::metadata(&out)?.len()
                },
                3
            )
        );
        assert!(
            held_while_open && free_once_written,
            "the copy holds the lease from `open` to the end of `write`: \
             {held_while_open} {free_once_written}"
        );
        Ok(())
    }
}
