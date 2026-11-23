//! The consumer's read of a snapshot directory: its manifest and its rows in file order, one
//! batch in memory at a time. Call [`crate::validate`] first; this reader checks nothing.

use std::fs::File;
use std::path::Path;

use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};

use crate::validate::rows_of;
use crate::{Manifest, SnapshotRow, DATA_FILE, MANIFEST_FILE};

/// Rows a batch holds, bodies included.
const READ_BATCH_ROWS: usize = 256;

/// Why a directory [`crate::validate`] would refuse could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("{path}: {why}")]
    Io { path: String, why: String },
    #[error("{MANIFEST_FILE} is not a snapshot.manifest/1 object: {0}")]
    Manifest(String),
    #[error("{DATA_FILE}: {0}")]
    Data(String),
}

pub fn manifest(dir: &Path) -> Result<Manifest, ReadError> {
    let path = dir.join(MANIFEST_FILE);
    let bytes = std::fs::read(&path).map_err(|why| ReadError::Io {
        path: path.display().to_string(),
        why: why.to_string(),
    })?;
    serde_json::from_slice(&bytes).map_err(|why| ReadError::Manifest(why.to_string()))
}

/// The rows of a data file, ascending `seq` as written.
pub struct Rows {
    reader: ParquetRecordBatchReader,
    batch: std::vec::IntoIter<SnapshotRow>,
}

pub fn rows(dir: &Path) -> Result<Rows, ReadError> {
    let path = dir.join(DATA_FILE);
    let file = File::open(&path).map_err(|why| ReadError::Io {
        path: path.display().to_string(),
        why: why.to_string(),
    })?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .and_then(|builder| builder.with_batch_size(READ_BATCH_ROWS).build())
        .map_err(|why| ReadError::Data(why.to_string()))?;
    Ok(Rows {
        reader,
        batch: Vec::new().into_iter(),
    })
}

impl Iterator for Rows {
    type Item = Result<SnapshotRow, ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(row) = self.batch.next() {
                return Some(Ok(row));
            }
            let read = self
                .reader
                .next()?
                .map_err(|why| why.to_string())
                .and_then(|batch| rows_of(&batch));
            match read {
                Ok(rows) => self.batch = rows.into_iter(),
                Err(why) => return Some(Err(ReadError::Data(why))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{manifest, rows};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Mutation 2026-11-23, `rows_of` dropping its `body` column to `None`: red.
    #[test]
    fn the_committed_fixture_reads_back_as_its_three_rows_and_its_manifest() -> TestResult {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/snapshot_v1");
        let read: Vec<_> = rows(&fixture)?.collect::<Result<_, _>>()?;
        let stated = manifest(&fixture)?;
        assert_eq!(
            read.iter().map(|row| row.seq).collect::<Vec<_>>(),
            [1, 2, 4]
        );
        assert_eq!(
            read.iter()
                .map(|row| row.body.is_some())
                .collect::<Vec<_>>(),
            [true, false, true]
        );
        assert_eq!((stated.items, stated.seq_high_water), (3, 4));
        Ok(())
    }
}
