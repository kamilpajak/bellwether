//! The append-only segment export. One parquet segment per (run, publication day), written to a
//! staging name, fsynced with its directory, renamed, and only then does its `Published` row
//! commit. The grain, the file counts behind it and the partition key are design.md section 5;
//! none of that is restated here.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, BooleanArray, Int64Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use sha2::{Digest as _, Sha256};

use pit::ObservationTime;

use crate::store::{Committer, ExportRow, PublishedSegment, StoreError, ITEMS_DIR};

/// Digits a `seq` is padded to in a segment's name, so the lexical order of the names in a
/// partition is their numeric order. 12 covers 999,999,999,999 rows, past any horizon the
/// measured ~450 attempts a day reaches.
pub const SEQ_PAD: usize = 12;

const PARTITION_PREFIX: &str = "valid_date=";
const SEGMENT_PREFIX: &str = "seq=";
const SEGMENT_SUFFIX: &str = ".parquet";
const STAGING_PREFIX: &str = ".tmp-";

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("a segment is already published at {0}")]
    SegmentExists(PathBuf),
    #[error("a staged segment is already at {0}, left by a run that did not finish")]
    StagingExists(PathBuf),
    #[error("column {0} missing from segment")]
    MissingColumn(&'static str),
    #[error("column {column} has unusable type {actual}")]
    BadColumnType {
        column: &'static str,
        actual: String,
    },
    #[error("column {column} is null in row {row}")]
    NullColumn { column: &'static str, row: usize },
    #[error("{path} was written with {wrote} rows and reads back as {read}")]
    RowCountDisagrees {
        path: PathBuf,
        wrote: u32,
        read: u32,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Arrow(#[from] arrow::error::ArrowError),
    #[error(transparent)]
    Parquet(#[from] parquet::errors::ParquetError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The three filesystem acts that decide whether a half-written segment can reach a final name.
/// A trait rather than three direct calls because the order is the invariant: before it existed,
/// removing either fsync from the blob path left the whole suite at exit 0 - review 2026-11-13.
pub trait Durability {
    fn sync_file(&self, file: &File) -> std::io::Result<()>;
    fn sync_dir(&self, directory: &Path) -> std::io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()>;
}

/// The real one.
#[derive(Debug, Clone, Copy, Default)]
pub struct Fsync;

impl Durability for Fsync {
    fn sync_file(&self, file: &File) -> std::io::Result<()> {
        file.sync_all()
    }

    fn sync_dir(&self, directory: &Path) -> std::io::Result<()> {
        File::open(directory)?.sync_all()
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::rename(from, to)
    }
}

/// One segment being written. Its only constructor is [`Segment::create_new`], and it holds the
/// staging path rather than the final one until [`Segment::commit`].
#[derive(Debug)]
#[must_use = "a dropped segment leaves a staged file and no Published row"]
pub struct Segment {
    staging: PathBuf,
    final_path: PathBuf,
    writer: ArrowWriter<File>,
    seq_lo: i64,
    seq_hi: i64,
    rows: u32,
}

/// A segment at its final name, with the digest its `Published` row carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentFile {
    pub path: PathBuf,
    pub digest: String,
    pub seq_lo: i64,
    pub seq_hi: i64,
    pub rows: u32,
}

impl Segment {
    /// The ONLY constructor, and both paths are refused when they already exist.
    /// `std::fs::File::create` TRUNCATES: measured 2026-11-13, a 10-byte file rewritten through
    /// it is 2 bytes afterwards, while `create_new` answers `AlreadyExists`.
    pub fn create_new(
        partition: &Path,
        run: &str,
        seq_lo: i64,
        seq_hi: i64,
    ) -> Result<Self, PublishError> {
        let final_path = partition.join(segment_name(seq_lo, seq_hi));
        if final_path.exists() {
            return Err(PublishError::SegmentExists(final_path));
        }
        std::fs::create_dir_all(partition)?;
        let staging = partition.join(format!(
            "{STAGING_PREFIX}{run}-{}",
            segment_name(seq_lo, seq_hi)
        ));
        let file = match File::create_new(&staging) {
            Ok(file) => file,
            Err(failure) if failure.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(PublishError::StagingExists(staging))
            }
            Err(failure) => return Err(PublishError::Io(failure)),
        };
        Ok(Self {
            writer: ArrowWriter::try_new(file, segment_schema(), None)?,
            staging,
            final_path,
            seq_lo,
            seq_hi,
            rows: 0,
        })
    }

    pub fn append(&mut self, rows: &[ExportRow]) -> Result<(), PublishError> {
        self.writer.write(&batch(rows)?)?;
        self.rows += u32::try_from(rows.len()).unwrap_or(u32::MAX);
        Ok(())
    }

    /// Bytes fsynced, then the directory holding the staged name, THEN the rename, then the
    /// directory again so the final name itself is durable.
    pub fn commit(mut self, durability: &dyn Durability) -> Result<SegmentFile, PublishError> {
        // `finish` writes the footer and `into_inner` then refuses with "SerializedFileWriter
        // already finished" (measured 2026-11-13 against parquet 60.1.0), so the file is reached
        // through `inner_mut`, which is sound for an fsync and not for a write.
        self.writer.finish()?;
        durability.sync_file(self.writer.inner_mut())?;
        let Self {
            staging,
            final_path,
            writer,
            seq_lo,
            seq_hi,
            rows,
        } = self;
        drop(writer);
        let partition = final_path.parent().unwrap_or(&final_path);
        durability.sync_dir(partition)?;
        if final_path.exists() {
            return Err(PublishError::SegmentExists(final_path));
        }
        durability.rename(&staging, &final_path)?;
        durability.sync_dir(partition)?;
        // Read back through this module's own reader before the caller may file a `Published`
        // row. It is what keeps the writer and the reader from drifting apart: the predecessor's
        // lake had no reader in code and stored a row set production did not use.
        let mut read_back = Vec::new();
        read_segment(&final_path, &mut read_back)?;
        let read = u32::try_from(read_back.len()).unwrap_or(u32::MAX);
        if read != rows {
            return Err(PublishError::RowCountDisagrees {
                path: final_path,
                wrote: rows,
                read,
            });
        }
        Ok(SegmentFile {
            digest: const_hex::encode(Sha256::digest(std::fs::read(&final_path)?)),
            path: final_path,
            seq_lo,
            seq_hi,
            rows,
        })
    }
}

/// `seq=<lo>-<hi>.parquet`, both ends padded to [`SEQ_PAD`].
#[must_use]
pub fn segment_name(seq_lo: i64, seq_hi: i64) -> String {
    format!("{SEGMENT_PREFIX}{seq_lo:0SEQ_PAD$}-{seq_hi:0SEQ_PAD$}{SEGMENT_SUFFIX}")
}

#[must_use]
pub fn partition_of(root: &Path, valid_date: &str) -> PathBuf {
    root.join(ITEMS_DIR)
        .join(format!("{PARTITION_PREFIX}{valid_date}"))
}

/// Exports every committed row no segment carries, one segment per publication day, and files
/// each `Published` row only after its file is at its final name. The restore drill
/// (architecture.md section 10) is this path's first user.
pub fn publish_run(
    store: &mut Committer,
    durability: &dyn Durability,
    at: ObservationTime,
) -> Result<Vec<PublishedSegment>, PublishError> {
    let mut by_day: BTreeMap<String, Vec<ExportRow>> = BTreeMap::new();
    for row in store.unpublished_rows()? {
        by_day.entry(row.valid_date.clone()).or_default().push(row);
    }
    let run = store.run().as_str().to_owned();
    let root = store.root().to_path_buf();
    let mut published = Vec::new();
    for (valid_date, rows) in by_day {
        let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
            continue;
        };
        let partition = partition_of(&root, &valid_date);
        let mut segment = Segment::create_new(&partition, &run, first.seq, last.seq)?;
        segment.append(&rows)?;
        let file = segment.commit(durability)?;
        let segment = PublishedSegment {
            segment_digest: file.digest,
            run_id: run.clone(),
            valid_date,
            path: file.path.to_string_lossy().into_owned(),
            seq_lo: file.seq_lo,
            seq_hi: file.seq_hi,
            rows: file.rows,
        };
        store.record_published(&segment, at)?;
        published.push(segment);
    }
    Ok(published)
}

/// One exported row as the reader returns it. The instants are the stored strings, not re-parsed
/// values, so the reader cannot move a value the `item_id` digests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentRow {
    pub seq: i64,
    pub item_id: String,
    pub story_id: String,
    pub doc_id: Option<String>,
    pub attempt_id: String,
    pub source: String,
    pub valid_at: Option<String>,
    /// A publication DATE and the calendar it is a day in, which no reader may fold into an
    /// instant: architecture.md section 3.1's `valid_precision` row.
    pub valid_day: Option<String>,
    pub valid_tz: Option<String>,
    pub valid_refused: Option<String>,
    pub observed_at: String,
    pub valid_disputed: bool,
    pub earliest_valid_at: Option<String>,
    pub raw_blob: String,
    pub body_blob: Option<String>,
    pub committed_at: String,
}

/// Every row of one publication day, in `seq` order across every segment of its partition. A
/// backfill appends a second segment into a months-old partition, so the day is the files
/// together and never the newest one.
pub fn read_day(root: &Path, valid_date: &str) -> Result<Vec<SegmentRow>, PublishError> {
    let partition = partition_of(root, valid_date);
    let mut segments: Vec<PathBuf> = match std::fs::read_dir(&partition) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| is_segment(path))
            .collect(),
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(failure) => return Err(PublishError::Io(failure)),
    };
    segments.sort();
    let mut rows = Vec::new();
    for segment in &segments {
        read_segment(segment, &mut rows)?;
    }
    in_seq_order(&mut rows);
    Ok(rows)
}

/// A segment and never the staging file beside it. A torn file at a readable name takes out the
/// whole publication day: measured 2026-11-12, one run file truncated to 60% beside a valid one
/// gave exit 1, `Corrupt footer` and an empty stdout.
fn is_segment(path: &Path) -> bool {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .is_some_and(|name| name.starts_with(SEGMENT_PREFIX) && name.ends_with(SEGMENT_SUFFIX))
}

/// The one ordering helper, keyed on `seq` and on nothing else: architecture.md section 2 makes
/// `seq` the only ordering authority and a wall clock evidence.
fn in_seq_order(rows: &mut [SegmentRow]) {
    rows.sort_by_key(|row| row.seq);
}

fn read_segment(path: &Path, into: &mut Vec<SegmentRow>) -> Result<(), PublishError> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?.build()?;
    for batch in reader {
        let batch = batch?;
        let seq = int64(&batch, "seq")?;
        let disputed = boolean(&batch, "valid_disputed")?;
        let text = [
            "item_id",
            "story_id",
            "doc_id",
            "attempt_id",
            "source",
            "valid_at",
            "valid_day",
            "valid_tz",
            "valid_refused",
            "observed_at",
            "earliest_valid_at",
            "raw_blob",
            "body_blob",
            "committed_at",
        ]
        .map(|column| utf8(&batch, column));
        let [item_id, story_id, doc_id, attempt_id, source, valid_at, valid_day, valid_tz, valid_refused, observed_at, earliest_valid_at, raw_blob, body_blob, committed_at] =
            text;
        let (item_id, story_id, doc_id) = (item_id?, story_id?, doc_id?);
        let (attempt_id, source, valid_at) = (attempt_id?, source?, valid_at?);
        let (valid_day, valid_tz) = (valid_day?, valid_tz?);
        let (valid_refused, observed_at) = (valid_refused?, observed_at?);
        let (earliest_valid_at, raw_blob) = (earliest_valid_at?, raw_blob?);
        let (body_blob, committed_at) = (body_blob?, committed_at?);
        for row in 0..batch.num_rows() {
            into.push(SegmentRow {
                seq: required_seq(&seq, row)?,
                item_id: required(&item_id, "item_id", row)?,
                story_id: required(&story_id, "story_id", row)?,
                doc_id: optional(&doc_id, row),
                attempt_id: required(&attempt_id, "attempt_id", row)?,
                source: required(&source, "source", row)?,
                valid_at: optional(&valid_at, row),
                valid_day: optional(&valid_day, row),
                valid_tz: optional(&valid_tz, row),
                valid_refused: optional(&valid_refused, row),
                observed_at: required(&observed_at, "observed_at", row)?,
                valid_disputed: disputed.value(row),
                earliest_valid_at: optional(&earliest_valid_at, row),
                raw_blob: required(&raw_blob, "raw_blob", row)?,
                body_blob: optional(&body_blob, row),
                committed_at: required(&committed_at, "committed_at", row)?,
            });
        }
    }
    Ok(())
}

/// The columns, in the order the file carries them. Every instant is a string: see [`ExportRow`].
fn segment_schema() -> SchemaRef {
    let text = |name: &str, nullable: bool| Field::new(name, DataType::Utf8, nullable);
    Arc::new(Schema::new(vec![
        Field::new("seq", DataType::Int64, false),
        text("item_id", false),
        text("story_id", false),
        text("doc_id", true),
        text("attempt_id", false),
        text("source", false),
        text("valid_at", true),
        text("valid_day", true),
        text("valid_tz", true),
        text("valid_refused", true),
        text("observed_at", false),
        Field::new("valid_disputed", DataType::Boolean, false),
        text("earliest_valid_at", true),
        text("raw_blob", false),
        text("body_blob", true),
        text("committed_at", false),
    ]))
}

fn batch(rows: &[ExportRow]) -> Result<RecordBatch, arrow::error::ArrowError> {
    let text = |pick: fn(&ExportRow) -> Option<String>| -> ArrayRef {
        Arc::new(StringArray::from(
            rows.iter().map(pick).collect::<Vec<Option<String>>>(),
        ))
    };
    let columns: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from(
            rows.iter().map(|row| row.seq).collect::<Vec<i64>>(),
        )),
        text(|row| Some(row.item_id.clone())),
        text(|row| Some(row.story_id.clone())),
        text(|row| row.doc_id.clone()),
        text(|row| Some(row.attempt_id.clone())),
        text(|row| Some(row.source.clone())),
        text(|row| row.valid_at.clone()),
        text(|row| row.valid_day.clone()),
        text(|row| row.valid_tz.clone()),
        text(|row| row.valid_refused.clone()),
        text(|row| Some(row.observed_at.clone())),
        Arc::new(BooleanArray::from(
            rows.iter()
                .map(|row| row.valid_disputed)
                .collect::<Vec<bool>>(),
        )),
        text(|row| row.earliest_valid_at.clone()),
        text(|row| Some(row.raw_blob.clone())),
        text(|row| row.body_blob.clone()),
        text(|row| Some(row.committed_at.clone())),
    ];
    RecordBatch::try_new(segment_schema(), columns)
}

fn utf8(batch: &RecordBatch, name: &'static str) -> Result<StringArray, PublishError> {
    let column = batch
        .column_by_name(name)
        .ok_or(PublishError::MissingColumn(name))?;
    column
        .as_any()
        .downcast_ref::<StringArray>()
        .cloned()
        .ok_or_else(|| PublishError::BadColumnType {
            column: name,
            actual: column.data_type().to_string(),
        })
}

fn int64(batch: &RecordBatch, name: &'static str) -> Result<Int64Array, PublishError> {
    let column = batch
        .column_by_name(name)
        .ok_or(PublishError::MissingColumn(name))?;
    column
        .as_any()
        .downcast_ref::<Int64Array>()
        .cloned()
        .ok_or_else(|| PublishError::BadColumnType {
            column: name,
            actual: column.data_type().to_string(),
        })
}

fn boolean(batch: &RecordBatch, name: &'static str) -> Result<BooleanArray, PublishError> {
    let column = batch
        .column_by_name(name)
        .ok_or(PublishError::MissingColumn(name))?;
    column
        .as_any()
        .downcast_ref::<BooleanArray>()
        .cloned()
        .ok_or_else(|| PublishError::BadColumnType {
            column: name,
            actual: column.data_type().to_string(),
        })
}

fn required(column: &StringArray, name: &'static str, row: usize) -> Result<String, PublishError> {
    if column.is_null(row) {
        return Err(PublishError::NullColumn { column: name, row });
    }
    Ok(column.value(row).to_owned())
}

fn required_seq(column: &Int64Array, row: usize) -> Result<i64, PublishError> {
    if column.is_null(row) {
        return Err(PublishError::NullColumn { column: "seq", row });
    }
    Ok(column.value(row))
}

fn optional(column: &StringArray, row: usize) -> Option<String> {
    (!column.is_null(row)).then(|| column.value(row).to_owned())
}

#[cfg(test)]
mod tests {
    use canon::digest::{item_id, story_id, ItemFields};
    use canon::{FileDigest, Stamp};
    use chrono::DateTime;
    use news_provider::{
        AttemptSlot, Baseline, Outcome, Provenance, RawResponse, Request, RequestPhase, RunId,
        SourceId, TargetWindow, Verdict,
    };
    use pit::datedness::Published;
    use pit::{ObservationTime, ValidTime};
    use tempfile::TempDir;

    use super::{
        in_seq_order, partition_of, publish_run, read_day, segment_name, Durability, Fsync,
        PublishError, Segment, SegmentRow, SEQ_PAD,
    };
    use crate::store::{Committer, ItemRow};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const FIRST_RUN: &str = "2026-07-02T00:30:00Z";
    const SECOND_RUN: &str = "2026-07-03T00:30:00Z";
    const SOURCE: &str = "edgar_press";
    const WINDOW: &str = "2026-07-01";
    const URL: &str = "https://example.test/filing";
    const TITLE: &str = "A filing";
    const PAYLOAD: &[u8] = b"{\"filings\": 1}";
    /// One roster value: these tests are about the segment, and `universe_version` only has to be
    /// present and stable.
    const ROSTER: &[u8] = b"{\"320193\": \"AAPL\"}";
    const STATUS_OK: u16 = 200;
    const VENDOR_ROWS: u32 = 59;
    const TRAILING_OK: [u32; 3] = [181, 45, 11];
    /// A filing published on 2026-01-10 and read back six months later: the backfill case that
    /// appends into a closed-looking partition.
    const SIX_MONTHS_EARLIER: &str = "2026-01-10T14:00:00Z";
    const PARTITION_SIX_MONTHS_EARLIER: &str = "2026-01-10";
    const OBSERVED_FIRST: &str = "2026-07-02T00:31:00Z";
    const OBSERVED_SECOND: &str = "2026-07-03T00:31:00Z";
    const SENTINEL: &[u8] = b"a staged segment a crash left behind";
    const NEW_YORK: &str = "America/New_York";
    /// Two seqs whose decimal lengths differ, which is what an unpadded name sorts wrongly.
    const SINGLE_DIGIT: i64 = 9;
    const TWO_DIGITS: i64 = 10;

    fn at(text: &str) -> ObservationTime {
        ObservationTime::new(text.parse().unwrap_or(DateTime::UNIX_EPOCH))
    }

    fn valid(text: &str) -> ValidTime {
        ValidTime::new(text.parse().unwrap_or(DateTime::UNIX_EPOCH))
    }

    /// Records one item under one closed attempt, which is the only door into the archive.
    fn commit_one(
        store: &mut Committer,
        source_item_id: &str,
        published: &str,
        observed: &str,
    ) -> TestResult {
        commit_valued(
            store,
            source_item_id,
            Published::At(valid(published)),
            Stamp::Instant(valid(published).inner()),
            observed,
        )
    }

    /// The same door, with the publication value stated: an instant and a bare day are two
    /// different stored shapes and the segment carries both.
    fn commit_valued(
        store: &mut Committer,
        source_item_id: &str,
        published: Published,
        stamp: Stamp,
        observed: &str,
    ) -> TestResult {
        let run = RunId::new(store.run().as_str());
        let request = Request {
            source: SourceId::new(SOURCE),
            target_window: TargetWindow::new(WINDOW),
            phase: RequestPhase::Index,
            provenance: Provenance::LiveIngest,
            url: URL.to_owned(),
            headers: Vec::new(),
            params: Vec::new(),
            universe_version: FileDigest::of(ROSTER),
        };
        let slot = AttemptSlot::open(store, &request, &run, 0, at(observed))?;
        let answer = RawResponse::new(
            slot.attempt_id().clone(),
            STATUS_OK,
            PAYLOAD.to_vec(),
            at(observed),
            at(observed),
        );
        let outcome = Outcome::decide(
            &Verdict::Usable,
            VENDOR_ROWS,
            &Baseline::from_trailing_ok(&TRAILING_OK),
        );
        let closed = slot.close(store, outcome, &Ok(answer), at(observed))?;
        let row = ItemRow {
            item_id: item_id(&ItemFields {
                source: SOURCE,
                source_item_id,
                url: URL,
                valid: stamp,
                title: TITLE,
                body_ref: None,
            })
            .map_err(|failure| failure.to_string())?,
            story_id: story_id(SOURCE, source_item_id).map_err(|failure| failure.to_string())?,
            doc_id: None,
            valid: published,
            observed: ObservationTime::new(at(observed).inner()),
            raw_blob: store.put_blob(PAYLOAD)?,
            body_blob: None,
        };
        let committed = store.record(closed, &[row], at(observed))?;
        assert_eq!(committed.items_committed, 1);
        Ok(())
    }

    fn open(root: &TempDir, run: &str) -> Result<Committer, crate::store::StoreError> {
        Committer::open(root.path(), &RunId::new(run))
    }

    /// Records the durability acts in the order they are asked for, and performs them, so the
    /// rest of the export is real. Without a journal, removing a single fsync changes nothing
    /// observable - which is how the blob path's three mutations all passed (review 2026-11-13).
    #[derive(Default)]
    struct Journal {
        acts: std::cell::RefCell<Vec<&'static str>>,
    }

    impl Journal {
        fn acts(&self) -> Vec<&'static str> {
            self.acts.borrow().clone()
        }
    }

    impl Durability for Journal {
        fn sync_file(&self, file: &std::fs::File) -> std::io::Result<()> {
            self.acts.borrow_mut().push("sync_file");
            Fsync.sync_file(file)
        }

        fn sync_dir(&self, directory: &std::path::Path) -> std::io::Result<()> {
            self.acts.borrow_mut().push("sync_dir");
            Fsync.sync_dir(directory)
        }

        fn rename(&self, from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
            self.acts.borrow_mut().push("rename");
            Fsync.rename(from, to)
        }
    }

    /// One run holding one item published on `SIX_MONTHS_EARLIER`, exported.
    fn one_published_item(root: &TempDir) -> TestResult {
        let mut store = open(root, FIRST_RUN)?;
        commit_one(
            &mut store,
            "0000320193-26-000071",
            SIX_MONTHS_EARLIER,
            OBSERVED_FIRST,
        )?;
        let published = publish_run(&mut store, &Fsync, at(OBSERVED_FIRST))?;
        assert_eq!(published.len(), 1);
        Ok(())
    }

    fn segments_in(root: &TempDir) -> Vec<String> {
        let partition = partition_of(root.path(), PARTITION_SIX_MONTHS_EARLIER);
        let mut names: Vec<String> = match std::fs::read_dir(partition) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .filter(|entry| super::is_segment(&entry.path()))
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(),
        };
        names.sort();
        names
    }

    #[test]
    fn a_second_write_to_an_existing_published_path_fails_with_a_named_error() -> TestResult {
        let root = TempDir::new()?;
        one_published_item(&root)?;
        let partition = partition_of(root.path(), PARTITION_SIX_MONTHS_EARLIER);
        let again = Segment::create_new(&partition, SECOND_RUN, 1, 1);
        match again {
            Err(PublishError::SegmentExists(path)) => {
                assert_eq!(path, partition.join(segment_name(1, 1)));
            }
            other => return Err(format!("expected SegmentExists, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn a_segment_is_renamed_into_place_only_after_its_bytes_and_its_directory_are_fsynced(
    ) -> TestResult {
        let root = TempDir::new()?;
        let mut store = open(&root, FIRST_RUN)?;
        commit_one(
            &mut store,
            "0000320193-26-000071",
            SIX_MONTHS_EARLIER,
            OBSERVED_FIRST,
        )?;
        let journal = Journal::default();
        let published = publish_run(&mut store, &journal, at(OBSERVED_FIRST))?;
        assert_eq!(published.len(), 1);
        assert_eq!(
            journal.acts(),
            vec!["sync_file", "sync_dir", "rename", "sync_dir"]
        );
        Ok(())
    }

    #[test]
    fn a_crash_before_the_published_row_leaves_a_tmp_file_and_no_claim_about_it() -> TestResult {
        let root = TempDir::new()?;
        let mut store = open(&root, FIRST_RUN)?;
        commit_one(
            &mut store,
            "0000320193-26-000071",
            SIX_MONTHS_EARLIER,
            OBSERVED_FIRST,
        )?;
        let rows = store.unpublished_rows()?;
        let partition = partition_of(root.path(), PARTITION_SIX_MONTHS_EARLIER);
        let mut segment = Segment::create_new(&partition, FIRST_RUN, 1, 1)?;
        segment.append(&rows)?;
        drop(segment);
        let staged: Vec<String> = std::fs::read_dir(&partition)?
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(staged.len(), 1, "{staged:?}");
        assert!(
            staged.iter().all(|name| name.starts_with(".tmp-")),
            "{staged:?}"
        );
        assert_eq!(segments_in(&root), Vec::<String>::new());
        assert_eq!(
            read_day(root.path(), PARTITION_SIX_MONTHS_EARLIER)?,
            Vec::new()
        );
        assert_eq!(
            store.unpublished_seq_ranges()?,
            vec![crate::store::SeqRange {
                seq_lo: 1,
                seq_hi: 1
            }]
        );
        Ok(())
    }

    #[test]
    fn a_late_item_appends_a_second_segment_into_a_six_month_old_partition() -> TestResult {
        let root = TempDir::new()?;
        one_published_item(&root)?;
        let mut later = open(&root, SECOND_RUN)?;
        commit_one(
            &mut later,
            "0000320193-26-000072",
            SIX_MONTHS_EARLIER,
            OBSERVED_SECOND,
        )?;
        let published = publish_run(&mut later, &Fsync, at(OBSERVED_SECOND))?;
        assert_eq!(published.len(), 1);
        assert_eq!(
            segments_in(&root),
            vec![segment_name(1, 1), segment_name(2, 2)]
        );
        Ok(())
    }

    #[test]
    fn the_reader_returns_both_segments_in_seq_order() -> TestResult {
        let root = TempDir::new()?;
        one_published_item(&root)?;
        let mut later = open(&root, SECOND_RUN)?;
        commit_one(
            &mut later,
            "0000320193-26-000072",
            SIX_MONTHS_EARLIER,
            OBSERVED_SECOND,
        )?;
        let published = publish_run(&mut later, &Fsync, at(OBSERVED_SECOND))?;
        assert_eq!(published.len(), 1);
        let day = read_day(root.path(), PARTITION_SIX_MONTHS_EARLIER)?;
        assert_eq!(
            day.iter().map(|row| row.seq).collect::<Vec<i64>>(),
            vec![1, 2]
        );
        Ok(())
    }

    #[test]
    fn the_padding_width_is_pinned_so_the_lexical_sort_is_the_numeric_sort() {
        let single = segment_name(SINGLE_DIGIT, SINGLE_DIGIT);
        let two = segment_name(TWO_DIGITS, TWO_DIGITS);
        assert!(single < two, "{single} should sort before {two}");
        assert_eq!(
            single,
            format!("seq={:0SEQ_PAD$}-{:0SEQ_PAD$}.parquet", 9, 9)
        );
    }

    #[test]
    fn a_published_day_round_trips_through_the_strict_column_reader() -> TestResult {
        let root = TempDir::new()?;
        let mut store = open(&root, FIRST_RUN)?;
        commit_one(
            &mut store,
            "0000320193-26-000071",
            SIX_MONTHS_EARLIER,
            OBSERVED_FIRST,
        )?;
        let stored = store.unpublished_rows()?;
        let published = publish_run(&mut store, &Fsync, at(OBSERVED_FIRST))?;
        assert_eq!(published.len(), 1);
        let read = read_day(root.path(), PARTITION_SIX_MONTHS_EARLIER)?;
        let expected: Vec<SegmentRow> = stored
            .iter()
            .map(|row| SegmentRow {
                seq: row.seq,
                item_id: row.item_id.clone(),
                story_id: row.story_id.clone(),
                doc_id: row.doc_id.clone(),
                attempt_id: row.attempt_id.clone(),
                source: row.source.clone(),
                valid_at: row.valid_at.clone(),
                valid_day: row.valid_day.clone(),
                valid_tz: row.valid_tz.clone(),
                valid_refused: row.valid_refused.clone(),
                observed_at: row.observed_at.clone(),
                valid_disputed: row.valid_disputed,
                earliest_valid_at: row.earliest_valid_at.clone(),
                raw_blob: row.raw_blob.clone(),
                body_blob: row.body_blob.clone(),
                committed_at: row.committed_at.clone(),
            })
            .collect();
        assert_eq!(read, expected);
        Ok(())
    }

    /// The two columns the day-precision fix added. Without them in the segment schema the
    /// exported row is an item whose publication value the archive silently dropped.
    #[test]
    fn a_day_dated_row_reaches_the_segment_with_its_day_and_its_calendar() -> TestResult {
        let root = TempDir::new()?;
        let mut store = open(&root, FIRST_RUN)?;
        let day = PARTITION_SIX_MONTHS_EARLIER.parse()?;
        commit_valued(
            &mut store,
            "0000320193-26-000071",
            Published::On { day, tz: NEW_YORK },
            Stamp::Day(day),
            OBSERVED_FIRST,
        )?;
        publish_run(&mut store, &Fsync, at(OBSERVED_FIRST))?;
        let read = read_day(root.path(), PARTITION_SIX_MONTHS_EARLIER)?;
        let dated: Vec<(Option<String>, Option<String>, Option<String>)> = read
            .iter()
            .map(|row| {
                (
                    row.valid_at.clone(),
                    row.valid_day.clone(),
                    row.valid_tz.clone(),
                )
            })
            .collect();
        assert_eq!(
            dated,
            vec![(
                None,
                Some(PARTITION_SIX_MONTHS_EARLIER.to_owned()),
                Some(NEW_YORK.to_owned())
            )]
        );
        Ok(())
    }

    #[test]
    fn changing_the_one_segment_constructor_to_file_create_turns_the_write_gate_red() -> TestResult
    {
        let root = TempDir::new()?;
        let partition = partition_of(root.path(), PARTITION_SIX_MONTHS_EARLIER);
        std::fs::create_dir_all(&partition)?;
        let staged = partition.join(format!(".tmp-{FIRST_RUN}-{}", segment_name(1, 1)));
        std::fs::write(&staged, SENTINEL)?;
        let refused = Segment::create_new(&partition, FIRST_RUN, 1, 1);
        // The bytes first: `File::create` truncates them away and answers `Ok`, so this is the
        // assertion the mutation fails on even if a later one is never reached.
        assert_eq!(std::fs::read(&staged)?, SENTINEL);
        match refused {
            Err(PublishError::StagingExists(path)) => assert_eq!(path, staged),
            other => return Err(format!("expected StagingExists, got {other:?}").into()),
        }
        Ok(())
    }

    /// The helper the reader's claim rests on, over the one shape the writer cannot produce: a
    /// partition whose files are read in an order that is not seq order.
    #[test]
    fn the_ordering_helper_sorts_on_seq_and_leaves_no_row_behind() {
        let row = |seq: i64| SegmentRow {
            seq,
            item_id: String::new(),
            story_id: String::new(),
            doc_id: None,
            attempt_id: String::new(),
            source: String::new(),
            valid_at: None,
            valid_day: None,
            valid_tz: None,
            valid_refused: None,
            observed_at: String::new(),
            valid_disputed: false,
            earliest_valid_at: None,
            raw_blob: String::new(),
            body_blob: None,
            committed_at: String::new(),
        };
        let mut rows = vec![row(3), row(1), row(2)];
        in_seq_order(&mut rows);
        assert_eq!(
            rows.iter().map(|row| row.seq).collect::<Vec<i64>>(),
            vec![1, 2, 3]
        );
    }
}
