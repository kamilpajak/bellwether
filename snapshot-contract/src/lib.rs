//! The `Snapshot` contract, `vault` to `derive` (`docs/design/architecture.md` section 3.0): one
//! immutable directory holding `manifest.json` and `snapshot.parquet`. The column list, the
//! manifest fields, the visibility predicate, the writer and the validator live here, so the
//! producer checks its own output before the rename and the consumer runs the same checks.
//! Why a crate of its own: `CONTRIBUTING.md`, "Everything else".

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, Int64Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use canon::digest::{snapshot_id, snapshot_row, SnapshotRowFields};
use canon::{CanonError, Digest, Stamp};
use chrono::{DateTime, NaiveDate, Utc};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::errors::ParquetError;
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub mod validate;
pub mod visibility;

pub use validate::{validate, Finding, ValidateError, FINDING_CODES};
pub use visibility::{visible_at, Rule, StoredTimes, TimeError};

pub const MANIFEST_SCHEMA: &str = "snapshot.manifest/1";
pub const MANIFEST_FILE: &str = "manifest.json";
pub const DATA_FILE: &str = "snapshot.parquet";
/// The data file's key-value metadata keys.
pub const SCHEMA_KEY: &str = "schema";
pub const SNAPSHOT_ID_KEY: &str = "snapshot_id";
/// `news_provider::Provenance::name` of each variant; newsvault's
/// `the_contract_names_every_provenance_the_store_can_hold` keeps the two lists equal.
pub const PROVENANCES: [&str; 3] = ["live_ingest", "vendor_archive", "bulk_dataset"];
/// Encoded bytes a row group may reach before the writer closes it. parquet 60.1.0's default is
/// no byte bound and 1,048,576 rows, all held in memory; a year of L1 text is 2.3-3.9 GB (§3.0).
pub const ROW_GROUP_BYTES: usize = 64 * 1024 * 1024;
/// Rows the writer gathers before it hands them to parquet as one batch.
const BATCH_ROWS: usize = 256;
/// The canonical form's instant rendering (canon's `INSTANT_FORMAT`), which `cutoff` must equal.
const CUTOFF_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.6fZ";

/// The column list of `snapshot.data/1`, in file order. One list: the writer builds from it and
/// the validator compares a file's schema with it.
#[must_use]
pub fn data_schema() -> SchemaRef {
    let text = |name: &str, nullable: bool| Field::new(name, DataType::Utf8, nullable);
    Arc::new(Schema::new(vec![
        Field::new("seq", DataType::Int64, false),
        text("item_id", false),
        text("story_id", false),
        text("doc_id", true),
        text("source", false),
        text("provenance", false),
        text("valid_at", true),
        text("valid_minute", true),
        text("valid_day", true),
        text("valid_tz", true),
        text("observed_at", false),
        text("body_blob", true),
        text("body", true),
    ]))
}

/// ZSTD on every column and a byte-bounded row group, never parquet's defaults; the validator's
/// `not_zstd` check reads the codec back.
#[must_use]
pub fn writer_properties(snapshot_id: &str, rule: Rule) -> WriterProperties {
    WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .set_max_row_group_bytes(Some(ROW_GROUP_BYTES))
        .set_key_value_metadata(Some(vec![
            KeyValue::new(SCHEMA_KEY.to_owned(), rule.data_schema().to_owned()),
            KeyValue::new(SNAPSHOT_ID_KEY.to_owned(), snapshot_id.to_owned()),
        ]))
        .build()
}

/// The cutoff as the manifest and the digest carry it: microseconds, `Z`.
#[must_use]
pub fn cutoff_text(cutoff: DateTime<Utc>) -> String {
    cutoff.format(CUTOFF_FORMAT).to_string()
}

/// One row of `snapshot.data/1`. Every time is the stored string, never re-rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRow {
    pub seq: i64,
    pub item_id: String,
    pub story_id: String,
    pub doc_id: Option<String>,
    pub source: String,
    pub provenance: String,
    pub valid_at: Option<String>,
    pub valid_minute: Option<String>,
    pub valid_day: Option<String>,
    pub valid_tz: Option<String>,
    pub observed_at: String,
    pub body_blob: Option<String>,
    /// Null iff `body_blob` is; the producer reads it from the blob store, never a stand-in.
    pub body: Option<String>,
}

/// Why a row has no [`SnapshotRow::digest`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RowError {
    #[error(transparent)]
    Time(#[from] TimeError),
    /// `CanonError` implements `Display` and not `Error`, so it is carried rather than wrapped.
    #[error("{0}")]
    Canon(CanonError),
}

impl From<CanonError> for RowError {
    fn from(failure: CanonError) -> Self {
        Self::Canon(failure)
    }
}

impl SnapshotRow {
    #[must_use]
    pub fn times(&self) -> StoredTimes<'_> {
        StoredTimes {
            valid_at: self.valid_at.as_deref(),
            valid_minute: self.valid_minute.as_deref(),
            valid_day: self.valid_day.as_deref(),
            valid_tz: self.valid_tz.as_deref(),
            observed_at: &self.observed_at,
        }
    }

    /// `canon::digest::snapshot_row` over this row: every column but `seq` and `body`.
    pub fn digest(&self) -> Result<Digest, RowError> {
        let observed_at = parsed_instant("observed_at", &self.observed_at)?;
        Ok(snapshot_row(&SnapshotRowFields {
            item_id: &self.item_id,
            story_id: &self.story_id,
            doc_id: self.doc_id.as_deref(),
            source: &self.source,
            provenance: &self.provenance,
            valid: self.stamp()?,
            valid_tz: self.valid_tz.as_deref(),
            observed_at,
            body_blob: self.body_blob.as_deref(),
        })?)
    }

    fn stamp(&self) -> Result<Stamp, TimeError> {
        match (&self.valid_at, &self.valid_minute, &self.valid_day) {
            (Some(at), None, None) => Ok(Stamp::Instant(parsed_instant("valid_at", at)?)),
            (None, Some(minute), None) => {
                Ok(Stamp::Minute(parsed_instant("valid_minute", minute)?))
            }
            (None, None, Some(day)) => {
                day.parse::<NaiveDate>()
                    .map(Stamp::Day)
                    .map_err(|_| TimeError::Unparseable {
                        column: "valid_day",
                        value: day.clone(),
                        expected: "a YYYY-MM-DD date",
                    })
            }
            (None, None, None) => Err(TimeError::Shape("no publication value")),
            _ => Err(TimeError::Shape("more than one publication value")),
        }
    }
}

fn parsed_instant(column: &'static str, text: &str) -> Result<DateTime<Utc>, TimeError> {
    text.parse().map_err(|_| TimeError::Unparseable {
        column,
        value: text.to_owned(),
        expected: "an RFC 3339 instant",
    })
}

/// `snapshot_id` over the cutoff and the rows' digests in ascending `seq` (§2).
pub fn snapshot_id_of(cutoff: DateTime<Utc>, row_digests: &[&str]) -> Result<Digest, CanonError> {
    snapshot_id(cutoff, row_digests)
}

/// `manifest.json`, `snapshot.manifest/1`. Field meanings: architecture.md section 3.0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: String,
    pub snapshot_id: String,
    pub cutoff: String,
    pub seq_high_water: i64,
    pub items: u64,
    /// Exactly the [`PROVENANCES`] keys.
    pub provenance_counts: BTreeMap<String, u64>,
    pub data_sha256: String,
}

/// The lower-case hex sha256 of a file and its length, read in fixed chunks so a data file of any
/// size is hashed in bounded memory.
pub fn file_sha256(path: &Path) -> std::io::Result<(String, u64)> {
    const CHUNK: usize = 1 << 20;
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; CHUNK];
    let mut length = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        length += u64::try_from(read).unwrap_or(u64::MAX);
    }
    Ok((const_hex::encode(hasher.finalize()), length))
}

/// Writes `snapshot.data/1` rows under [`writer_properties`]: at most [`BATCH_ROWS`] rows wait in
/// this struct and at most about [`ROW_GROUP_BYTES`] in parquet's open row group.
pub struct DataWriter {
    writer: ArrowWriter<File>,
    pending: Vec<SnapshotRow>,
    written: u64,
}

impl DataWriter {
    pub fn create(file: File, snapshot_id: &str) -> Result<Self, ParquetError> {
        Self::with_properties(file, writer_properties(snapshot_id, Rule::CURRENT))
    }

    /// A writer under other properties, for a test that needs a smaller row group or another
    /// codec. Production calls [`DataWriter::create`].
    pub fn with_properties(file: File, properties: WriterProperties) -> Result<Self, ParquetError> {
        Ok(Self {
            writer: ArrowWriter::try_new(file, data_schema(), Some(properties))?,
            pending: Vec::with_capacity(BATCH_ROWS),
            written: 0,
        })
    }

    pub fn push(&mut self, row: SnapshotRow) -> Result<(), ParquetError> {
        self.pending.push(row);
        if self.pending.len() >= BATCH_ROWS {
            self.write_pending()?;
        }
        Ok(())
    }

    /// Writes the footer. The file stays reachable through [`DataWriter::file`] for an fsync.
    pub fn finish(&mut self) -> Result<u64, ParquetError> {
        self.write_pending()?;
        self.writer.finish()?;
        Ok(self.written)
    }

    pub fn file(&mut self) -> &mut File {
        self.writer.inner_mut()
    }

    fn write_pending(&mut self) -> Result<(), ParquetError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let batch = batch(&self.pending)?;
        self.writer.write(&batch)?;
        self.written += u64::try_from(self.pending.len()).unwrap_or(u64::MAX);
        self.pending.clear();
        Ok(())
    }
}

fn batch(rows: &[SnapshotRow]) -> Result<RecordBatch, arrow::error::ArrowError> {
    let text = |pick: fn(&SnapshotRow) -> Option<&str>| -> ArrayRef {
        Arc::new(StringArray::from(
            rows.iter().map(pick).collect::<Vec<Option<&str>>>(),
        ))
    };
    let columns: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from(
            rows.iter().map(|row| row.seq).collect::<Vec<i64>>(),
        )),
        text(|row| Some(&row.item_id)),
        text(|row| Some(&row.story_id)),
        text(|row| row.doc_id.as_deref()),
        text(|row| Some(&row.source)),
        text(|row| Some(&row.provenance)),
        text(|row| row.valid_at.as_deref()),
        text(|row| row.valid_minute.as_deref()),
        text(|row| row.valid_day.as_deref()),
        text(|row| row.valid_tz.as_deref()),
        text(|row| Some(&row.observed_at)),
        text(|row| row.body_blob.as_deref()),
        text(|row| row.body.as_deref()),
    ];
    RecordBatch::try_new(data_schema(), columns)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::Value;

    use crate::SnapshotRow;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn text(value: &Value) -> Option<String> {
        value.as_str().map(str::to_owned)
    }

    /// canon's committed `snapshot_row` vectors, reached through the mapping the producer and the
    /// validator share. Mutations 2026-11-22, one at a time: `valid_tz` dropped, `body_blob`
    /// dropped, a minute digested as an instant: red each.
    #[test]
    fn every_snapshot_row_vector_reproduces_through_the_rows_own_digest() -> TestResult {
        let vectors = Path::new(env!("CARGO_MANIFEST_DIR")).join("../canon/vectors");
        let mut precisions = std::collections::BTreeSet::new();
        let (mut zoned, mut bodied) = (false, false);
        for entry in std::fs::read_dir(vectors)? {
            let path = entry?.path();
            let vector: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
            if vector["digest"] != "snapshot_row" {
                continue;
            }
            let input = &vector["input"];
            let valid = &input["valid"];
            let row = SnapshotRow {
                seq: 1,
                item_id: text(&input["item_id"]).unwrap_or_default(),
                story_id: text(&input["story_id"]).unwrap_or_default(),
                doc_id: text(&input["doc_id"]),
                source: text(&input["source"]).unwrap_or_default(),
                provenance: text(&input["provenance"]).unwrap_or_default(),
                valid_at: text(&valid["instant"]),
                valid_minute: text(&valid["minute"]),
                valid_day: text(&valid["day"]),
                valid_tz: text(&input["valid_tz"]),
                observed_at: text(&input["observed_at"]).unwrap_or_default(),
                body_blob: text(&input["body_blob"]),
                body: None,
            };
            precisions.extend(
                valid
                    .as_object()
                    .into_iter()
                    .flat_map(|keys| keys.keys().cloned()),
            );
            zoned |= row.valid_tz.is_some();
            bodied |= row.body_blob.is_some();
            assert_eq!(
                Some(row.digest()?.hex()),
                vector["sha256"].as_str(),
                "{}",
                path.display()
            );
        }
        assert_eq!(
            precisions.into_iter().collect::<Vec<_>>(),
            ["day", "instant", "minute"]
        );
        assert!(zoned && bodied);
        Ok(())
    }
}
