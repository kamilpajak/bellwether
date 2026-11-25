//! The one SQLite opener of every store that links this crate: `docs/design/architecture.md`
//! section 8 row T1.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

/// The one place this crate opens SQLite: §8 row T1. `temp_store = MEMORY` keeps a sort, a
/// transient index and a materialised subquery out of a temporary file, which a `--read-only`
/// container has no directory for (SQLITE_IOERR_GETTEMPPATH, 6410, production 2026-11-23).
pub fn connect(path: &Path, flags: OpenFlags) -> rusqlite::Result<Connection> {
    let db = Connection::open_with_flags(path, flags)?;
    db.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(db)
}

/// A reader's connection to the commit log at `log`.
pub fn read_only(log: &Path) -> rusqlite::Result<Connection> {
    connect(log, OpenFlags::SQLITE_OPEN_READ_ONLY)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// `PRAGMA temp_store` reads 2 for MEMORY, 0 for the build's default and 1 for FILE.
    const TEMP_STORE_MEMORY: i64 = 2;

    fn temp_store(db: &Connection) -> rusqlite::Result<i64> {
        db.query_row("PRAGMA temp_store", [], |row| row.get(0))
    }

    /// Section 8 row T1 in the crate that holds the opener: a created file, a reader of it and a
    /// database held in memory.
    #[test]
    fn a_connection_opened_here_keeps_its_temporary_data_in_memory() -> TestResult {
        let temp = TempDir::new()?;
        let log = temp.path().join("any.sqlite3");
        let writer = temp_store(&connect(&log, OpenFlags::default())?)?;
        let reader = temp_store(&read_only(&log)?)?;
        let in_memory = temp_store(&connect(Path::new(":memory:"), OpenFlags::default())?)?;
        assert_eq!(
            (writer, reader, in_memory),
            (TEMP_STORE_MEMORY, TEMP_STORE_MEMORY, TEMP_STORE_MEMORY)
        );
        Ok(())
    }
}
