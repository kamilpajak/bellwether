//! The store lease: an exclusive `flock` on a lock file the caller names, held while the value
//! lives. Which file, and in which directory, is the store's own layout.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use crate::LeaseError;

/// A held lease. Dropping it closes the lock file, which releases the `flock`.
#[derive(Debug)]
pub struct Lease {
    _file: File,
}

impl Lease {
    /// The single writer's: creates the lock file at `lock` when it is absent, never waits.
    pub fn take(lock: &Path) -> Result<Self, LeaseError> {
        let lease = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock)?;
        Self::locked(lease, lock)
    }

    /// The writer's lease, taken by a reader that must see no commit while it copies; `None` when
    /// no writer ever made `lock`. Opened read-only: `flock` needs no write access, so a read-only
    /// mount serves, and a writer started meanwhile is `LeaseHeld`.
    pub fn hold_to_copy(lock: &Path) -> Result<Option<Self>, LeaseError> {
        let lease = match File::open(lock) {
            Ok(lease) => lease,
            Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(failure) => return Err(LeaseError::Io(failure)),
        };
        Self::locked(lease, lock).map(Some)
    }

    fn locked(lease: File, lock: &Path) -> Result<Self, LeaseError> {
        match lease.try_lock() {
            Ok(()) => Ok(Self { _file: lease }),
            Err(TryLockError::WouldBlock) => Err(LeaseError::LeaseHeld(lock.to_path_buf())),
            Err(TryLockError::Error(failure)) => Err(LeaseError::Io(failure)),
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// No name is the crate's: the lock file is whatever path the store passes.
    const LOCK: &str = "any-name.lock";

    #[test]
    fn a_second_taker_and_a_copying_reader_are_each_refused_while_the_lease_is_held() -> TestResult
    {
        let temp = TempDir::new()?;
        let lock = temp.path().join(LOCK);
        let held = Lease::take(&lock)?;
        let writer = Lease::take(&lock);
        let reader = Lease::hold_to_copy(&lock);
        let named =
            |refused: &LeaseError| matches!(refused, LeaseError::LeaseHeld(path) if *path == lock);
        assert!(
            writer.as_ref().is_err_and(named) && reader.as_ref().is_err_and(named),
            "{writer:?} {reader:?}"
        );
        drop(held);
        assert!(Lease::take(&lock).is_ok());
        Ok(())
    }

    #[test]
    fn a_copying_reader_makes_no_lock_file_and_holds_the_one_a_writer_made() -> TestResult {
        let temp = TempDir::new()?;
        let lock = temp.path().join(LOCK);
        let before_a_writer = Lease::hold_to_copy(&lock)?.is_some();
        let made = lock.exists();
        drop(Lease::take(&lock)?);
        let reader = Lease::hold_to_copy(&lock)?;
        let writer_kept_out = matches!(Lease::take(&lock), Err(LeaseError::LeaseHeld(_)));
        assert_eq!(
            (before_a_writer, made, reader.is_some(), writer_kept_out),
            (false, false, true, true)
        );
        Ok(())
    }
}
