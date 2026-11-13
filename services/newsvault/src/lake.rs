//! Reader for the append-only news lake. Layout written by the Python ingest:
//! `thematic_news_lake/session_date=<YYYY-MM-DD>/run=<YYYYMMDDTHHMMSSsssZ>.parquet`, one file per
//! ingest run, holding that run's RAW per-source union before dedup and before the 200-item cap.
//!
//! Two differences from the Python current view. **No cap** - what the cap costs is measured in
//! `README.md`, point 1. **First sighting wins**: when several runs carry the same item the view
//! keeps the LAST run's `ingested_at` and this reader keeps the FIRST, because observation time is
//! the earliest instant we could have known the item.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};

use arrow::array::{Array, AsArray, RecordBatch};
use arrow::compute::cast;
use arrow::datatypes::{DataType, TimeUnit};
use chrono::{DateTime, NaiveDate, NaiveTime, SecondsFormat, TimeZone, Utc};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use sha2::{Digest as _, Sha256};

use pit::datedness::{Datedness, Published};
use pit::{ObservationTime, ValidTime, Visibility};

/// The lake has no precision column, so a publication value stamped exactly at midnight is a
/// DATE the predecessor rendered as an instant. Measured 2026-11-13 over all 779 run files and
/// 243,226 rows of `~/thematic_news_lake`: 7,767 rows (3.19%) are stamped `00:00:00.000000`, and
/// for source `perplexity` it is 7,141 of 7,141 - every row. Reading those as instants admitted
/// them up to a day before they existed.
const LAKE_CALENDAR: &str = "UTC";

/// One item as the archive holds it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct NewsItem {
    pub id: String,
    pub source: String,
    pub visibility: Datedness,
    pub tickers: Vec<String>,
    pub title: String,
    pub body: String,
    pub url: String,
}

/// Everything the archive holds for one UTC day, plus what it took to get there.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Day {
    pub date: NaiveDate,
    pub items: Vec<NewsItem>,
    /// Run files read, in filename order.
    pub runs: Vec<String>,
    /// Rows read before collapsing repeats across runs.
    pub raw_rows: usize,
    /// ROW events: one per row whose publication time differs from the one kept for that id, so an
    /// item re-reported in five later runs of this day contributes five. The first sighting's time
    /// is the one kept. Measured lake-wide beside the item count in memo section 5.1.
    pub valid_time_conflict_events: usize,
    /// ITEMS: distinct ids of this partition carrying more than one publication time, counted once
    /// each. This is the unit R15's handling has to size itself in, and it is the smaller of the
    /// two numbers - memo section 5.1 states both with their construction.
    pub items_with_conflicting_valid_time: usize,
    /// Rows dropped because a required field was null or, for the id, empty.
    pub unusable_rows: usize,
}

impl Day {
    /// The number `probes/parity_baseline.py` has computed since before this crate existed:
    /// sha256 over the newline-joined sorted distinct ids of the items returned. It says the id
    /// SET came back and nothing else - it matches with every title, body and url different,
    /// which is why [`Day::content_digest`] exists beside it.
    #[must_use]
    pub fn id_digest(&self) -> String {
        let mut ids: Vec<&str> = self.items.iter().map(|item| item.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        hex(Sha256::digest(ids.join("\n").as_bytes()).as_slice())
    }

    /// The whole item set as one number: every field of every item, in id order, each
    /// length-prefixed so no value can forge a field boundary. The second reader's own
    /// construction and what each possible disagreement means:
    /// `probes/newsvault_parity/PREREGISTRATION.md`.
    #[must_use]
    pub fn content_digest(&self) -> String {
        let mut items: Vec<&NewsItem> = self.items.iter().collect();
        items.sort_by(|left, right| left.id.cmp(&right.id));
        let mut hasher = Sha256::new();
        for item in items {
            for field in [
                item.id.as_str(),
                item.source.as_str(),
                &published_text(item.visibility.published()),
                &item
                    .visibility
                    .observed()
                    .inner()
                    .to_rfc3339_opts(SecondsFormat::Micros, true),
                item.title.as_str(),
                item.body.as_str(),
                item.url.as_str(),
            ] {
                field_into(&mut hasher, field.as_bytes());
            }
            hasher.update(format!("{}#", item.tickers.len()).as_bytes());
            for ticker in &item.tickers {
                field_into(&mut hasher, ticker.as_bytes());
            }
        }
        hex(hasher.finalize().as_slice())
    }
}

/// One field as `<byte length>:<bytes>`. A value holding the separator would otherwise forge a
/// field boundary, which is the defect `canon` was reviewed for on 2026-11-13; it closed that by
/// refusing three bytes, and a digest over archived values cannot refuse one.
fn field_into(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update(format!("{}:", bytes.len()).as_bytes());
    hasher.update(bytes);
}

/// A publication value as the two readers both render it: an instant at whole microseconds, or a
/// day with the calendar it is a day in.
fn published_text(published: Published) -> String {
    match published {
        Published::At(valid) => valid
            .inner()
            .to_rfc3339_opts(SecondsFormat::Micros, true)
            .to_string(),
        Published::On { day, tz } => format!("day {day} {tz}"),
        Published::Unknown => "unknown".to_owned(),
    }
}

fn hex(bytes: &[u8]) -> String {
    const_hex::encode(bytes)
}

/// What the reader keeps for one id inside one partition.
struct Sighting {
    /// The earliest observation seen for this id, which is the sighting that wins.
    observed: ObservationTime,
    item: NewsItem,
    /// Set the first time any other row of this partition carries a different publication time for
    /// this id, on either path through the dedup.
    valid_time_disputed: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum LakeError {
    #[error("lake root {0} does not exist")]
    MissingRoot(PathBuf),
    #[error("no partition for {date} under {root}")]
    MissingDay { root: PathBuf, date: NaiveDate },
    #[error("column {0} missing from run file")]
    MissingColumn(&'static str),
    #[error("column {column} has unusable type {actual}")]
    BadColumnType {
        column: &'static str,
        actual: String,
    },
    #[error(transparent)]
    Arrow(#[from] arrow::error::ArrowError),
    #[error(transparent)]
    Parquet(#[from] parquet::errors::ParquetError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Pit(#[from] pit::PitError),
}

pub struct LakeReader {
    root: PathBuf,
}

impl LakeReader {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Default location beside the Python current view.
    #[must_use]
    pub fn default_root() -> PathBuf {
        let home = std::env::var_os("HOME").map_or_else(PathBuf::new, PathBuf::from);
        home.join(".alphalens").join("thematic_news_lake")
    }

    pub fn day(&self, date: NaiveDate) -> Result<Day, LakeError> {
        if !self.root.exists() {
            return Err(LakeError::MissingRoot(self.root.clone()));
        }
        let dir = self.root.join(format!("session_date={date}"));
        if !dir.is_dir() {
            return Err(LakeError::MissingDay {
                root: self.root.clone(),
                date,
            });
        }
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "parquet"))
            .collect();
        files.sort();

        let mut first: HashMap<String, Sighting> = HashMap::new();
        let mut day = Day {
            date,
            items: Vec::new(),
            runs: Vec::new(),
            raw_rows: 0,
            valid_time_conflict_events: 0,
            items_with_conflicting_valid_time: 0,
            unusable_rows: 0,
        };
        for path in &files {
            day.runs.push(
                path.file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
            );
            read_run(path, &mut first, &mut day)?;
        }
        if files.is_empty() {
            return Err(LakeError::MissingDay {
                root: self.root.clone(),
                date,
            });
        }
        day.items_with_conflicting_valid_time = first
            .values()
            .filter(|seen| seen.valid_time_disputed)
            .count();
        let mut items: Vec<NewsItem> = first.into_values().map(|seen| seen.item).collect();
        items.sort_by(|a, b| {
            a.visibility
                .published()
                .lower_bound()
                .cmp(&b.visibility.published().lower_bound())
                .then_with(|| a.id.cmp(&b.id))
        });
        day.items = items;
        Ok(day)
    }
}

/// A publication value stamped exactly at midnight is read as a DATE, per [`LAKE_CALENDAR`].
///
/// The ordering test is here and not only inside `Visibility::from_live`, because the day branch
/// below never reaches that constructor: found 2026-11-13 by the first run of
/// `probes/newsvault_parity.py`, which drops a row whose ingest precedes its publication value
/// whatever the value's precision, over a shape the archive holds zero of.
fn datedness(valid_at: DateTime<Utc>, observed: ObservationTime) -> Option<Datedness> {
    if observed.inner() < valid_at {
        return None;
    }
    if valid_at == valid_at.date_naive().and_time(NaiveTime::MIN).and_utc() {
        return Some(Datedness::DayDated {
            day: valid_at.date_naive(),
            tz: LAKE_CALENDAR,
            observed,
        });
    }
    Visibility::from_live(ValidTime::new(valid_at), observed)
        .ok()
        .map(Datedness::Dated)
}

fn utf8_column<'a>(
    batch: &'a RecordBatch,
    name: &'static str,
    held: &'a mut Vec<arrow::array::ArrayRef>,
) -> Result<&'a arrow::array::StringArray, LakeError> {
    let col = batch
        .column_by_name(name)
        .ok_or(LakeError::MissingColumn(name))?;
    // pandas may hand us Utf8, LargeUtf8 or Utf8View depending on its version, so
    // normalise rather than branch on whatever this build happened to write.
    let casted = cast(col, &DataType::Utf8)?;
    held.push(casted);
    held.last()
        .and_then(|a| a.as_any().downcast_ref::<arrow::array::StringArray>())
        .ok_or_else(|| LakeError::BadColumnType {
            column: name,
            actual: col.data_type().to_string(),
        })
}

fn micros_column(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<arrow::array::ArrayRef, LakeError> {
    let col = batch
        .column_by_name(name)
        .ok_or(LakeError::MissingColumn(name))?;
    Ok(cast(
        col,
        &DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
    )?)
}

fn to_utc(micros: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_micros(micros).single()
}

/// The one list column, read like every other column: absent is [`LakeError::MissingColumn`] and a
/// type that cannot be read as a list of strings is [`LakeError::BadColumnType`], both once per
/// batch rather than silently per row. Before this existed `tickers` was fetched as an `Option` and
/// every failure returned an empty list with `unusable_rows` at 0 - measured 2026-11-12, a run file
/// with no `tickers` column and one whose `tickers` was a struct both exited 0 with every tag gone,
/// which is the empty fallback section 8 row R6 forbids.
fn string_list_column<'a>(
    batch: &RecordBatch,
    name: &'static str,
    held: &'a mut Vec<arrow::array::ArrayRef>,
) -> Result<&'a arrow::array::ListArray, LakeError> {
    let col = batch
        .column_by_name(name)
        .ok_or(LakeError::MissingColumn(name))?;
    // Refuse before casting, not after. `cast` succeeds from a numeric list, so a
    // `list<int64>` of [7] arrives as the ticker "7": plausible data rather than an error, which
    // is worse than the empty list R6 forbids. Measured 2026-11-12 on a one-row lake, exit 0 with
    // `unusable_rows` 0. Only the three string layouts pandas emits may be cast.
    let item_type = match col.data_type() {
        DataType::List(f) | DataType::LargeList(f) | DataType::ListView(f) => f.data_type(),
        other => {
            return Err(LakeError::BadColumnType {
                column: name,
                actual: other.to_string(),
            })
        }
    };
    if !matches!(
        item_type,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
    ) {
        return Err(LakeError::BadColumnType {
            column: name,
            actual: col.data_type().to_string(),
        });
    }
    let casted = cast(col, &DataType::new_list(DataType::Utf8, true)).map_err(|_| {
        LakeError::BadColumnType {
            column: name,
            actual: col.data_type().to_string(),
        }
    })?;
    held.push(casted);
    held.last()
        .and_then(|a| a.as_any().downcast_ref::<arrow::array::ListArray>())
        .ok_or_else(|| LakeError::BadColumnType {
            column: name,
            actual: col.data_type().to_string(),
        })
}

fn read_run(
    path: &Path,
    first: &mut HashMap<String, Sighting>,
    day: &mut Day,
) -> Result<(), LakeError> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?.build()?;
    for batch in reader {
        let batch = batch?;
        day.raw_rows += batch.num_rows();
        let mut held = Vec::new();
        let ids = utf8_column(&batch, "id", &mut held)?.clone();
        let sources = utf8_column(&batch, "source", &mut held)?.clone();
        let titles = utf8_column(&batch, "title", &mut held)?.clone();
        let bodies = utf8_column(&batch, "body", &mut held)?.clone();
        let urls = utf8_column(&batch, "url", &mut held)?.clone();
        let ts = micros_column(&batch, "timestamp")?;
        let ing = micros_column(&batch, "ingested_at")?;
        let ts = ts.as_primitive::<arrow::datatypes::TimestampMicrosecondType>();
        let ing = ing.as_primitive::<arrow::datatypes::TimestampMicrosecondType>();
        let mut held_list = Vec::new();
        let tickers = string_list_column(&batch, "tickers", &mut held_list)?;

        for row in 0..batch.num_rows() {
            // An id that is present and blank is not an id: it would become the dedup key and
            // collapse every blank-id row of the day into one item. `probes/parity_baseline.py`
            // rejects the same row, so the two readers of the archive stay one rule.
            if ids.is_null(row) || ids.value(row).trim().is_empty() {
                day.unusable_rows += 1;
                continue;
            }
            if ts.is_null(row) || ing.is_null(row) {
                day.unusable_rows += 1;
                continue;
            }
            let (Some(valid_at), Some(seen_at)) = (to_utc(ts.value(row)), to_utc(ing.value(row)))
            else {
                day.unusable_rows += 1;
                continue;
            };
            let observed = ObservationTime::new(seen_at);
            let visibility = match datedness(valid_at, observed) {
                Some(v) => v,
                None => {
                    day.unusable_rows += 1;
                    continue;
                }
            };
            let published = visibility.published();
            let id = ids.value(row).to_owned();
            // The publication-time comparison runs BEFORE the first-sighting test, so it runs on
            // both paths: the repeat that is discarded and the earlier-observed row that replaces
            // the kept one are each a disagreement. The replacement path counted nothing until
            // 2026-11-12; it is taken 0 times in the 243,226 archived rows and becomes reachable
            // with a second ingest schedule or a backfill (section 3.1 `provenance`).
            let mut disputed = false;
            if let Some(seen) = first.get_mut(&id) {
                if seen.item.visibility.published() != published {
                    day.valid_time_conflict_events += 1;
                    seen.valid_time_disputed = true;
                }
                disputed = seen.valid_time_disputed;
                if seen.observed <= observed {
                    continue;
                }
            }
            let item = NewsItem {
                id: id.clone(),
                source: sources.value(row).to_owned(),
                visibility,
                tickers: string_list_at(tickers, row)?,
                title: titles.value(row).to_owned(),
                body: bodies.value(row).to_owned(),
                url: urls.value(row).to_owned(),
            };
            first.insert(
                id,
                Sighting {
                    observed,
                    item,
                    valid_time_disputed: disputed,
                },
            );
        }
    }
    Ok(())
}

/// One row's tag list. **One** empty return is left and it is the only legitimate one: a null cell,
/// meaning the vendor tagged nothing. Every other way of failing to read the list is an error, so an
/// empty list in the output means the row carried none rather than that the column could not be
/// decoded.
fn string_list_at(list: &arrow::array::ListArray, row: usize) -> Result<Vec<String>, LakeError> {
    if list.is_null(row) {
        return Ok(Vec::new());
    }
    let values = list.value(row);
    // One arm, not three: `string_list_column` has already cast the whole column to
    // `list<utf8>`, so the values of any row it accepted are a `StringArray`. The arm stays
    // because the alternative is an `expect`, which the lint floor denies.
    let strings = values
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .ok_or_else(|| LakeError::BadColumnType {
            column: "tickers",
            actual: values.data_type().to_string(),
        })?;
    Ok((0..strings.len())
        .filter(|i| !strings.is_null(*i))
        .map(|i| strings.value(i).to_owned())
        .collect())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use arrow::array::{ListArray, StringArray, TimestampMicrosecondArray};
    use arrow::datatypes::{Field, Schema};
    use parquet::arrow::ArrowWriter;

    use super::*;
    use pit::Cutoff;

    /// One row of the lake schema, as the Python ingest writes it.
    struct Row {
        /// `None` writes an Arrow null. `Some("")` writes a real empty string, which is a
        /// different row: it is present, it is not an id, and it used to become the dedup key.
        id: Option<&'static str>,
        source: &'static str,
        title: &'static str,
        published_micros: i64,
        ingested_micros: i64,
    }

    /// The common case: an id that is a real id.
    fn row(id: &'static str, published_micros: i64, ingested_micros: i64) -> Row {
        Row {
            id: Some(id),
            source: "rss",
            title: id,
            published_micros,
            ingested_micros,
        }
    }

    fn schema() -> Arc<Schema> {
        let tz: Arc<str> = Arc::from("UTC");
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, true),
            Field::new("source", DataType::Utf8, true),
            Field::new(
                "timestamp",
                DataType::Timestamp(TimeUnit::Microsecond, Some(tz.clone())),
                true,
            ),
            Field::new("tickers", DataType::new_list(DataType::Utf8, true), true),
            Field::new("title", DataType::Utf8, true),
            Field::new("body", DataType::Utf8, true),
            Field::new("url", DataType::Utf8, true),
            Field::new(
                "ingested_at",
                DataType::Timestamp(TimeUnit::Microsecond, Some(tz)),
                true,
            ),
        ]))
    }

    fn write_run(dir: &Path, name: &str, rows: &[Row]) {
        let tz: Arc<str> = Arc::from("UTC");
        let ids: Vec<Option<&str>> = rows.iter().map(|r| r.id).collect();
        let tickers = ListArray::from_iter_primitive::<arrow::datatypes::Int32Type, _, _>(
            rows.iter().map(|_| Some(Vec::<Option<i32>>::new())),
        );
        // Cast the empty int list to a string list so the fixture matches the real schema.
        let tickers = cast(&tickers, &DataType::new_list(DataType::Utf8, true)).unwrap();
        let batch = RecordBatch::try_new(
            schema(),
            vec![
                Arc::new(StringArray::from(ids)),
                Arc::new(StringArray::from(
                    rows.iter().map(|r| r.source).collect::<Vec<_>>(),
                )),
                Arc::new(
                    TimestampMicrosecondArray::from(
                        rows.iter().map(|r| r.published_micros).collect::<Vec<_>>(),
                    )
                    .with_timezone(tz.clone()),
                ),
                tickers,
                Arc::new(StringArray::from(
                    rows.iter().map(|r| r.title).collect::<Vec<_>>(),
                )),
                Arc::new(StringArray::from(vec!["body"; rows.len()])),
                Arc::new(StringArray::from(vec!["https://example.test"; rows.len()])),
                Arc::new(
                    TimestampMicrosecondArray::from(
                        rows.iter().map(|r| r.ingested_micros).collect::<Vec<_>>(),
                    )
                    .with_timezone(tz),
                ),
            ],
        )
        .unwrap();
        let file = File::create(dir.join(name)).unwrap();
        let mut w = ArrowWriter::try_new(file, batch.schema(), None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
    }

    const SEC: i64 = 1_000_000;

    fn lake_with_two_runs() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("session_date=2026-07-01");
        std::fs::create_dir_all(&part).unwrap();
        write_run(
            &part,
            "run=20260702T003000000Z.parquet",
            &[
                row("a", 100 * SEC, 1_000 * SEC),
                row("b", 200 * SEC, 1_000 * SEC),
            ],
        );
        write_run(
            &part,
            "run=20260702T043000000Z.parquet",
            &[
                // same item, later run: the first sighting must win
                row("a", 100 * SEC, 9_000 * SEC),
                // only the later run has it
                row("c", 300 * SEC, 9_000 * SEC),
            ],
        );
        root
    }

    fn day_of(root: &Path) -> Day {
        LakeReader::new(root)
            .day(NaiveDate::from_ymd_opt(2026, 7, 1).unwrap())
            .unwrap()
    }

    /// Epoch microseconds of 2026-07-01T00:00:00Z, the partition's own midnight.
    const PARTITION_MIDNIGHT: i64 = 1_782_864_000 * SEC;
    /// 23:59:59.999999Z on the same day: the last instant [`LAKE_CALENDAR`] admits.
    const PARTITION_END_OF_DAY: i64 = PARTITION_MIDNIGHT + 86_400 * SEC - 1;

    fn lake_with_a_midnight_stamped_row() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("session_date=2026-07-01");
        std::fs::create_dir_all(&part).unwrap();
        write_run(
            &part,
            "run=20260701T000500000Z.parquet",
            &[row(
                "m",
                PARTITION_MIDNIGHT,
                PARTITION_MIDNIGHT + 5 * 60 * SEC,
            )],
        );
        root
    }

    fn cutoff_at(micros: i64) -> Cutoff {
        Cutoff::new(DateTime::UNIX_EPOCH + chrono::TimeDelta::microseconds(micros))
    }

    /// The lookahead this crate was moved to its own member to close: read as an instant, a
    /// midnight-stamped row was visible from 00:00 for a document published some time that day.
    #[test]
    fn a_midnight_stamped_row_is_not_visible_until_the_end_of_its_own_day() {
        let root = lake_with_a_midnight_stamped_row();
        let day = day_of(root.path());
        let item = day.items.iter().find(|i| i.id == "m").unwrap();
        assert!(
            !item
                .visibility
                .visible_at(cutoff_at(PARTITION_END_OF_DAY - 1)),
            "a date has no hour, so no cutoff inside its day admits it"
        );
    }

    #[test]
    fn a_midnight_stamped_row_is_visible_once_its_day_has_ended() {
        let root = lake_with_a_midnight_stamped_row();
        let day = day_of(root.path());
        let item = day.items.iter().find(|i| i.id == "m").unwrap();
        assert!(item.visibility.visible_at(cutoff_at(PARTITION_END_OF_DAY)));
    }

    /// The publication value a midnight-stamped row reports is a DATE, not an instant, so nothing
    /// downstream can print or compare a fabricated 00:00.
    #[test]
    fn a_midnight_stamped_rows_publication_value_prints_as_a_date_and_not_as_an_instant() {
        let root = lake_with_a_midnight_stamped_row();
        let day = day_of(root.path());
        let item = day.items.iter().find(|i| i.id == "m").unwrap();
        assert_eq!(
            item.visibility.published().to_string(),
            "2026-07-01 (UTC day)"
        );
    }

    #[test]
    fn collapses_repeats_across_runs_and_counts_the_raw_rows() {
        let root = lake_with_two_runs();
        let day = day_of(root.path());
        assert_eq!(day.raw_rows, 4, "every row of every run is counted");
        assert_eq!(day.items.len(), 3, "three distinct ids");
        assert_eq!(day.runs.len(), 2);
        assert_eq!(day.unusable_rows, 0);
    }

    #[test]
    fn first_sighting_wins_so_our_knowledge_is_not_postdated() {
        let root = lake_with_two_runs();
        let day = day_of(root.path());
        let a = day.items.iter().find(|i| i.id == "a").unwrap();
        assert_eq!(
            a.visibility.observed().inner().timestamp(),
            1_000,
            "the earlier run's instant, not the later one"
        );
    }

    #[test]
    fn an_item_is_invisible_before_the_run_that_fetched_it() {
        let root = lake_with_two_runs();
        let day = day_of(root.path());
        let c = day.items.iter().find(|i| i.id == "c").unwrap();
        let after_publication = Cutoff::new(Utc.timestamp_opt(400, 0).single().unwrap());
        assert!(
            !c.visibility.visible_at(after_publication),
            "published at 300 but not fetched until 9000"
        );
        let after_fetch = Cutoff::new(Utc.timestamp_opt(9_000, 0).single().unwrap());
        assert!(c.visibility.visible_at(after_fetch));
    }

    #[test]
    fn rows_without_an_id_or_a_timestamp_are_counted_not_dropped_silently() {
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("session_date=2026-07-01");
        std::fs::create_dir_all(&part).unwrap();
        write_run(
            &part,
            "run=20260702T003000000Z.parquet",
            &[
                Row {
                    id: None,
                    source: "rss",
                    title: "null id",
                    published_micros: 100 * SEC,
                    ingested_micros: 1_000 * SEC,
                },
                row("ok", 100 * SEC, 1_000 * SEC),
            ],
        );
        let day = day_of(root.path());
        assert_eq!(day.raw_rows, 2);
        assert_eq!(day.items.len(), 1);
        assert_eq!(day.unusable_rows, 1, "the null id is reported, not hidden");
    }

    /// A blank id is present, so nothing above counted it: it became the dedup key and every row
    /// carrying it collapsed into the first one. The fixture writes a real empty string rather
    /// than an Arrow null, which is why `Row::id` is an `Option`.
    #[test]
    fn a_blank_id_is_unusable_and_does_not_collapse_distinct_stories() {
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("session_date=2026-07-01");
        std::fs::create_dir_all(&part).unwrap();
        write_run(
            &part,
            "run=20260702T003000000Z.parquet",
            &[
                Row {
                    id: Some(""),
                    source: "rss",
                    title: "story 0",
                    published_micros: 100 * SEC,
                    ingested_micros: 1_000 * SEC,
                },
                Row {
                    id: Some("   "),
                    source: "rss",
                    title: "story 1",
                    published_micros: 200 * SEC,
                    ingested_micros: 1_000 * SEC,
                },
                row("real", 300 * SEC, 1_000 * SEC),
            ],
        );
        let day = day_of(root.path());
        assert_eq!(day.raw_rows, 3);
        assert_eq!(day.unusable_rows, 2, "both blanks are reported");
        assert_eq!(day.items.len(), 1);
        assert_eq!(day.items[0].id, "real");
        assert_eq!(
            day.valid_time_conflict_events, 0,
            "a lost story must not surface as a publication-time disagreement"
        );
    }

    #[test]
    fn a_row_fetched_before_its_own_publication_stamp_is_unusable() {
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("session_date=2026-07-01");
        std::fs::create_dir_all(&part).unwrap();
        write_run(
            &part,
            "run=20260702T003000000Z.parquet",
            &[row("backwards", 5_000 * SEC, 1_000 * SEC)],
        );
        let day = day_of(root.path());
        assert!(day.items.is_empty());
        assert_eq!(day.unusable_rows, 1);
    }

    /// The disagreement the parity comparator found on 2026-11-13, on a shape the archive does
    /// not hold: the inversion test used to sit inside `Visibility::from_live`, which the
    /// midnight branch never reached, so this row was an item here and unusable in the probe.
    #[test]
    fn a_row_fetched_before_its_own_day_stamp_is_unusable_and_not_day_dated() {
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("session_date=2026-07-01");
        std::fs::create_dir_all(&part).unwrap();
        write_run(
            &part,
            "run=20260702T003000000Z.parquet",
            &[row(
                "backwards",
                PARTITION_MIDNIGHT,
                PARTITION_MIDNIGHT - SEC,
            )],
        );
        let day = day_of(root.path());
        assert!(day.items.is_empty());
        assert_eq!(day.unusable_rows, 1);
    }

    /// Two counters, two units (R15). One id re-reported twice with a different publication time is
    /// two row events and one item; the memo's lake-wide pair is 301 against 79.
    #[test]
    fn a_publication_time_disagreement_is_counted_as_rows_and_as_items() {
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("session_date=2026-07-01");
        std::fs::create_dir_all(&part).unwrap();
        write_run(
            &part,
            "run=20260702T003000000Z.parquet",
            &[row("a", 100 * SEC, 1_000 * SEC)],
        );
        write_run(
            &part,
            "run=20260702T013000000Z.parquet",
            &[row("a", 500 * SEC, 2_000 * SEC)],
        );
        write_run(
            &part,
            "run=20260702T023000000Z.parquet",
            &[row("a", 700 * SEC, 3_000 * SEC)],
        );
        let day = day_of(root.path());
        assert_eq!(day.items.len(), 1);
        assert_eq!(
            day.valid_time_conflict_events, 2,
            "one event per re-reporting row"
        );
        assert_eq!(
            day.items_with_conflicting_valid_time, 1,
            "one item, however many times it was re-reported"
        );
        assert_eq!(
            day.items[0]
                .visibility
                .published()
                .lower_bound()
                .map(|b| b.timestamp()),
            Some(100),
            "the first sighting's publication time is the one kept"
        );
    }

    /// The replacement path: a run file sorted later carries an EARLIER observation, so its row
    /// replaces the kept one. The publication-time disagreement used to be dropped from the count
    /// on this path. Unreachable on the archive as it stands and reachable with a second ingest
    /// schedule or a backfill.
    #[test]
    fn a_replacing_row_counts_its_disagreement_too() {
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("session_date=2026-07-01");
        std::fs::create_dir_all(&part).unwrap();
        write_run(
            &part,
            "run=20260702T003000000Z.parquet",
            &[row("a", 100 * SEC, 9_000 * SEC)],
        );
        write_run(
            &part,
            "run=20260702T043000000Z.parquet",
            &[row("a", 500 * SEC, 1_000 * SEC)],
        );
        let day = day_of(root.path());
        assert_eq!(day.items.len(), 1);
        assert_eq!(
            day.items[0].visibility.observed().inner().timestamp(),
            1_000,
            "the earlier observation replaces the kept sighting"
        );
        assert_eq!(
            day.valid_time_conflict_events, 1,
            "the disagreement is counted on the replacement path"
        );
        assert_eq!(day.items_with_conflicting_valid_time, 1);
    }

    /// R6 over the one column that used to be optional. A run file with no `tickers` column, and
    /// one whose `tickers` cannot be read as a list of strings, are each a named error - not an
    /// empty tag list on every row with `unusable_rows` at 0.
    #[test]
    fn a_missing_or_undecodable_tickers_column_is_an_error_not_an_empty_list() {
        for (name, columns) in [("absent", false), ("wrong type", true)] {
            let root = tempfile::tempdir().unwrap();
            let part = root.path().join("session_date=2026-07-01");
            std::fs::create_dir_all(&part).unwrap();
            let tz: Arc<str> = Arc::from("UTC");
            let mut fields: Vec<Field> = schema()
                .fields()
                .iter()
                .filter(|f| columns || f.name() != "tickers")
                .map(|f| (**f).clone())
                .collect();
            let mut arrays: Vec<arrow::array::ArrayRef> = Vec::new();
            for field in &mut fields {
                arrays.push(match field.name().as_str() {
                    "timestamp" | "ingested_at" => Arc::new(
                        TimestampMicrosecondArray::from(vec![100 * SEC]).with_timezone(tz.clone()),
                    ),
                    // A struct where a list of strings belongs: the shape the reader used to
                    // swallow.
                    "tickers" => {
                        *field = Field::new(
                            "tickers",
                            DataType::Struct(vec![Field::new("sym", DataType::Utf8, true)].into()),
                            true,
                        );
                        Arc::new(arrow::array::StructArray::from(vec![(
                            Arc::new(Field::new("sym", DataType::Utf8, true)),
                            Arc::new(StringArray::from(vec!["V"])) as arrow::array::ArrayRef,
                        )]))
                    }
                    _ => Arc::new(StringArray::from(vec!["x"])),
                });
            }
            let batch =
                RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).expect("fixture batch");
            let file = File::create(part.join("run=20260702T003000000Z.parquet")).unwrap();
            let mut writer = ArrowWriter::try_new(file, batch.schema(), None).unwrap();
            writer.write(&batch).unwrap();
            writer.close().unwrap();

            let err = LakeReader::new(root.path())
                .day(NaiveDate::from_ymd_opt(2026, 7, 1).unwrap())
                .unwrap_err();
            match name {
                "absent" => assert!(
                    matches!(err, LakeError::MissingColumn("tickers")),
                    "{name}: got {err:?}"
                ),
                _ => assert!(
                    matches!(
                        err,
                        LakeError::BadColumnType {
                            column: "tickers",
                            ..
                        }
                    ),
                    "{name}: got {err:?}"
                ),
            }
        }
    }

    /// A `tickers` column that IS a list but of the wrong item type is refused, not cast.
    /// `cast` succeeds from a numeric list, so before this check a `list<int64>` of `[7]` arrived
    /// as the ticker `"7"` with `unusable_rows` at 0 - plausible data rather than an error, which
    /// is worse than the empty list R6 forbids. Measured 2026-11-12.
    #[test]
    fn a_numeric_tickers_list_is_refused_rather_than_cast_to_strings() {
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("session_date=2026-07-01");
        std::fs::create_dir_all(&part).unwrap();
        let tz: Arc<str> = Arc::from("UTC");
        let mut fields: Vec<Field> = schema().fields().iter().map(|f| (**f).clone()).collect();
        let mut arrays: Vec<arrow::array::ArrayRef> = Vec::new();
        for field in &mut fields {
            arrays.push(match field.name().as_str() {
                "timestamp" | "ingested_at" => Arc::new(
                    TimestampMicrosecondArray::from(vec![100 * SEC]).with_timezone(tz.clone()),
                ),
                "tickers" => {
                    *field = Field::new("tickers", DataType::new_list(DataType::Int64, true), true);
                    Arc::new(ListArray::from_iter_primitive::<
                        arrow::datatypes::Int64Type,
                        _,
                        _,
                    >(vec![Some(vec![Some(7)])]))
                }
                _ => Arc::new(StringArray::from(vec!["x"])),
            });
        }
        let batch =
            RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).expect("fixture batch");
        let file = File::create(part.join("run=20260702T003000000Z.parquet")).unwrap();
        let mut writer = ArrowWriter::try_new(file, batch.schema(), None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();

        let err = LakeReader::new(root.path())
            .day(NaiveDate::from_ymd_opt(2026, 7, 1).unwrap())
            .unwrap_err();
        assert!(
            matches!(&err, LakeError::BadColumnType { column, actual }
                if *column == "tickers" && actual.contains("Int64")),
            "{err:?}"
        );
    }

    #[test]
    fn a_missing_day_is_an_error_not_an_empty_result() {
        let root = lake_with_two_runs();
        let err = LakeReader::new(root.path())
            .day(NaiveDate::from_ymd_opt(2026, 7, 2).unwrap())
            .unwrap_err();
        assert!(matches!(err, LakeError::MissingDay { .. }), "got {err:?}");
    }

    #[test]
    fn a_missing_root_is_an_error() {
        let err = LakeReader::new("/nonexistent/newsvault/lake")
            .day(NaiveDate::from_ymd_opt(2026, 7, 1).unwrap())
            .unwrap_err();
        assert!(matches!(err, LakeError::MissingRoot(_)), "got {err:?}");
    }

    /// Opt-in check against the production lake. Set `NEWSVAULT_TEST_LAKE` to a copy
    /// of `thematic_news_lake`. Skipped when unset so CI needs no data.
    #[test]
    fn matches_the_python_count_on_the_real_lake() {
        let Some(root) = std::env::var_os("NEWSVAULT_TEST_LAKE") else {
            return;
        };
        let day = LakeReader::new(root)
            .day(NaiveDate::from_ymd_opt(2026, 7, 1).unwrap())
            .unwrap();
        // Counted independently in pandas on 2026-11-11.
        assert_eq!(day.raw_rows, 1_649);
        assert_eq!(day.items.len(), 287);
    }
}
