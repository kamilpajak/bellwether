//! `validate(dir)`: whether one snapshot directory is admissible. Every check that can still be
//! decided is decided, so one run names every problem; a check a broken input makes undecidable is
//! skipped rather than guessed. The checks and their codes: architecture.md section 3.0.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};

use arrow::array::{Array, Int64Array, RecordBatch, StringArray};
use canon::FileDigest;
use chrono::{DateTime, Utc};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::Compression;
use serde_json::{Map, Value};

use crate::{
    cutoff_text, data_schema, file_sha256, snapshot_id_of, visible_at, Rule, SnapshotRow,
    DATA_FILE, MANIFEST_FILE, MANIFEST_SCHEMA, PROVENANCES, SCHEMA_KEY, SNAPSHOT_ID_KEY,
};

/// Rows a batch of the reader holds.
const READ_BATCH_ROWS: usize = 1024;

/// Every code [`validate`] can emit, in check order; `snapshot-contract schema` prints it and
/// `every_finding_code_is_reached_by_a_named_case` reaches each one.
pub const FINDING_CODES: [&str; 20] = [
    "manifest_absent",
    "manifest_unparseable",
    "unknown_schema",
    "missing_field",
    "wrong_type",
    "data_absent",
    "data_digest_mismatch",
    "data_unreadable",
    "unknown_data_schema",
    "stray_pairing",
    "column_mismatch",
    "not_zstd",
    "seq_order",
    "precision_shape",
    "not_visible",
    "time_unparseable",
    "body_digest_mismatch",
    "count_mismatch",
    "provenance_counts",
    "identity_mismatch",
];

/// One reason a snapshot is not admissible.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Finding {
    /// One of [`FINDING_CODES`], never renamed.
    pub code: &'static str,
    pub message: String,
}

impl Finding {
    fn new(code: &'static str, message: String) -> Self {
        Self { code, message }
    }
}

/// The validator could not look at the directory at all, which is not a verdict.
#[derive(Debug, thiserror::Error)]
pub enum ValidateError {
    #[error("no snapshot directory at {0}")]
    Absent(PathBuf),
    #[error("{0} is not a directory")]
    NotADirectory(PathBuf),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// The manifest fields every later check reads, once each is known to be well-formed.
struct Stated {
    snapshot_id: String,
    cutoff: DateTime<Utc>,
    seq_high_water: i64,
    items: u64,
    provenance_counts: BTreeMap<String, u64>,
    data_sha256: String,
}

pub fn validate(dir: &Path) -> Result<Vec<Finding>, ValidateError> {
    match std::fs::metadata(dir) {
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => {
            return Err(ValidateError::Absent(dir.to_path_buf()))
        }
        Err(source) => {
            return Err(ValidateError::Io {
                path: dir.to_path_buf(),
                source,
            })
        }
        Ok(meta) if !meta.is_dir() => return Err(ValidateError::NotADirectory(dir.to_path_buf())),
        Ok(_) => {}
    }
    let mut findings = Vec::new();
    let Some(manifest) = read_manifest(&dir.join(MANIFEST_FILE), &mut findings)? else {
        return Ok(findings);
    };
    let stated = check_manifest(&manifest, &mut findings);
    let data = dir.join(DATA_FILE);
    if !data.is_file() {
        findings.push(Finding::new(
            "data_absent",
            format!("{} is not beside the manifest", data.display()),
        ));
        return Ok(findings);
    }
    let (written, _) = file_sha256(&data).map_err(|source| ValidateError::Io {
        path: data.clone(),
        source,
    })?;
    if let Some(stated) = &stated {
        if written != stated.data_sha256 {
            findings.push(Finding::new(
                "data_digest_mismatch",
                format!(
                    "{DATA_FILE} hashes to {written}; the manifest states {}",
                    stated.data_sha256
                ),
            ));
        }
    }
    check_data(&data, stated.as_ref(), &mut findings)?;
    Ok(findings)
}

fn read_manifest(path: &Path, findings: &mut Vec<Finding>) -> Result<Option<Value>, ValidateError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => {
            findings.push(Finding::new(
                "manifest_absent",
                format!("no {MANIFEST_FILE} at {}", path.display()),
            ));
            return Ok(None);
        }
        Err(source) => {
            return Err(ValidateError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(value) if value.is_object() => Ok(Some(value)),
        Ok(_) | Err(_) => {
            findings.push(Finding::new(
                "manifest_unparseable",
                format!("{} is not one JSON object", path.display()),
            ));
            Ok(None)
        }
    }
}

/// `None` when any field a later check reads is missing or malformed; each such field is its own
/// finding.
fn check_manifest(manifest: &Value, findings: &mut Vec<Finding>) -> Option<Stated> {
    let table = manifest.as_object()?;
    match table.get("schema").and_then(Value::as_str) {
        Some(MANIFEST_SCHEMA) => {}
        Some(other) => findings.push(Finding::new(
            "unknown_schema",
            format!("schema is {other:?}; this validator knows {MANIFEST_SCHEMA:?} only"),
        )),
        None => missing_or_wrong(table, "schema", "a string", findings),
    }
    let snapshot_id = digest_field(table, "snapshot_id", findings);
    let data_sha256 = digest_field(table, "data_sha256", findings);
    let cutoff = cutoff_field(table, findings);
    let seq_high_water =
        positive(table, "seq_high_water", findings).and_then(|value| i64::try_from(value).ok());
    let items = positive(table, "items", findings);
    let provenance_counts = counts_field(table, items, findings);
    Some(Stated {
        snapshot_id: snapshot_id?,
        cutoff: cutoff?,
        seq_high_water: seq_high_water?,
        items: items?,
        provenance_counts: provenance_counts?,
        data_sha256: data_sha256?,
    })
}

fn missing_or_wrong(
    table: &Map<String, Value>,
    name: &str,
    shape: &str,
    findings: &mut Vec<Finding>,
) {
    findings.push(match table.get(name) {
        None => Finding::new("missing_field", format!("`{name}` is missing")),
        Some(value) => Finding::new(
            "wrong_type",
            format!("`{name}` must be {shape}, and is {value}"),
        ),
    });
}

/// canon's digest text form, which panel-contract also requires of a panel's `snapshot_id`.
fn digest_field(
    table: &Map<String, Value>,
    name: &str,
    findings: &mut Vec<Finding>,
) -> Option<String> {
    match table.get(name).and_then(Value::as_str) {
        Some(text) if canon::is_digest_hex(text) => Some(text.to_owned()),
        _ => {
            missing_or_wrong(table, name, "64 lower-case hex characters", findings);
            None
        }
    }
}

fn cutoff_field(table: &Map<String, Value>, findings: &mut Vec<Finding>) -> Option<DateTime<Utc>> {
    let parsed = table
        .get("cutoff")
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<DateTime<Utc>>().ok().map(|at| (text, at)));
    match parsed {
        Some((text, at)) if cutoff_text(at) == text => Some(at),
        _ => {
            missing_or_wrong(
                table,
                "cutoff",
                "an RFC 3339 UTC instant with exactly six fractional digits",
                findings,
            );
            None
        }
    }
}

fn positive(table: &Map<String, Value>, name: &str, findings: &mut Vec<Finding>) -> Option<u64> {
    match table.get(name).and_then(Value::as_u64) {
        Some(value) if value >= 1 => Some(value),
        _ => {
            missing_or_wrong(table, name, "an integer of at least 1", findings);
            None
        }
    }
}

fn counts_field(
    table: &Map<String, Value>,
    items: Option<u64>,
    findings: &mut Vec<Finding>,
) -> Option<BTreeMap<String, u64>> {
    let Some(object) = table.get("provenance_counts").and_then(Value::as_object) else {
        missing_or_wrong(table, "provenance_counts", "an object", findings);
        return None;
    };
    let keys: BTreeSet<&str> = object.keys().map(String::as_str).collect();
    let expected: BTreeSet<&str> = PROVENANCES.into_iter().collect();
    let counts: Option<BTreeMap<String, u64>> = object
        .iter()
        .map(|(key, value)| value.as_u64().map(|count| (key.clone(), count)))
        .collect();
    match counts {
        Some(counts) if keys == expected => {
            let total: u64 = counts.values().sum();
            if items.is_some_and(|items| items != total) {
                findings.push(Finding::new(
                    "provenance_counts",
                    format!("provenance_counts sum to {total} and items is {items:?}"),
                ));
            }
            Some(counts)
        }
        _ => {
            findings.push(Finding::new(
                "provenance_counts",
                format!("provenance_counts must hold exactly {PROVENANCES:?}, each an integer, and holds {}", Value::Object(object.clone())),
            ));
            None
        }
    }
}

/// One check failed on some rows: counted, and the first `seq` named.
#[derive(Default)]
struct Tally {
    rows: u64,
    first: Option<(i64, String)>,
}

impl Tally {
    fn note(&mut self, seq: i64, why: impl FnOnce() -> String) {
        self.rows += 1;
        if self.first.is_none() {
            self.first = Some((seq, why()));
        }
    }
}

/// What the row pass accumulates.
#[derive(Default)]
struct RowPass {
    last_seq: Option<i64>,
    rows: u64,
    provenance: BTreeMap<String, u64>,
    digests: Vec<String>,
    digest_failed: bool,
    tallies: BTreeMap<&'static str, Tally>,
}

fn check_data(
    data: &Path,
    stated: Option<&Stated>,
    findings: &mut Vec<Finding>,
) -> Result<(), ValidateError> {
    let file = File::open(data).map_err(|source| ValidateError::Io {
        path: data.to_path_buf(),
        source,
    })?;
    let unreadable = |why: String| Finding::new("data_unreadable", format!("{DATA_FILE}: {why}"));
    let builder = match ParquetRecordBatchReaderBuilder::try_new(file) {
        Ok(builder) => builder.with_batch_size(READ_BATCH_ROWS),
        Err(why) => {
            findings.push(unreadable(why.to_string()));
            return Ok(());
        }
    };
    let metadata = builder.metadata().clone();
    let pairs: BTreeMap<String, Option<String>> = metadata
        .file_metadata()
        .key_value_metadata()
        .map(|pairs| {
            pairs
                .iter()
                .map(|pair| (pair.key.clone(), pair.value.clone()))
                .collect()
        })
        .unwrap_or_default();
    let named = |key: &str| pairs.get(key).cloned().flatten();
    let Some(rule) = named(SCHEMA_KEY).as_deref().and_then(Rule::of_data_schema) else {
        findings.push(Finding::new(
            "unknown_data_schema",
            format!(
                "{DATA_FILE} metadata `{SCHEMA_KEY}` is {:?}; known: {:?}",
                named(SCHEMA_KEY),
                Rule::ALL.map(Rule::data_schema)
            ),
        ));
        return Ok(());
    };
    if let Some(stated) = stated {
        if named(SNAPSHOT_ID_KEY).as_deref() != Some(stated.snapshot_id.as_str()) {
            findings.push(Finding::new(
                "stray_pairing",
                format!(
                    "{DATA_FILE} names snapshot {:?} and the manifest {}",
                    named(SNAPSHOT_ID_KEY),
                    stated.snapshot_id
                ),
            ));
        }
    }
    let expected = data_schema();
    if builder.schema().fields() != expected.fields() {
        findings.push(Finding::new(
            "column_mismatch",
            format!(
                "{DATA_FILE} columns are {:?}; {} names {:?}",
                builder.schema().fields(),
                rule.data_schema(),
                expected.fields()
            ),
        ));
        return Ok(());
    }
    let uncompressed: Vec<String> = metadata
        .row_groups()
        .iter()
        .enumerate()
        .flat_map(|(group, row_group)| {
            row_group
                .columns()
                .iter()
                .filter(|column| !matches!(column.compression(), Compression::ZSTD(_)))
                .map(move |column| {
                    format!(
                        "row group {group} {} {:?}",
                        column.column_path(),
                        column.compression()
                    )
                })
        })
        .collect();
    if let Some(first) = uncompressed.first() {
        findings.push(Finding::new(
            "not_zstd",
            format!(
                "{} column chunks are not ZSTD, first {first}",
                uncompressed.len()
            ),
        ));
    }
    let reader = match builder.build() {
        Ok(reader) => reader,
        Err(why) => {
            findings.push(unreadable(why.to_string()));
            return Ok(());
        }
    };
    let mut pass = RowPass::default();
    for batch in reader {
        match batch
            .map_err(|why| why.to_string())
            .and_then(|batch| rows_of(&batch))
        {
            Ok(rows) => rows
                .iter()
                .for_each(|row| check_row(row, rule, stated, &mut pass)),
            Err(why) => {
                findings.push(unreadable(why));
                return Ok(());
            }
        }
    }
    report_rows(pass, stated, findings);
    Ok(())
}

fn check_row(row: &SnapshotRow, rule: Rule, stated: Option<&Stated>, pass: &mut RowPass) {
    let seq = row.seq;
    let last = pass.last_seq;
    let high_water = stated.map(|stated| stated.seq_high_water);
    pass.rows += 1;
    if last.is_some_and(|last| seq <= last) || high_water.is_some_and(|high| seq > high) {
        tally(pass, "seq_order", seq, || {
            format!("seq {seq} after {last:?}, high water {high_water:?}")
        });
    }
    pass.last_seq = Some(seq);
    *pass.provenance.entry(row.provenance.clone()).or_default() += 1;
    let set = [&row.valid_at, &row.valid_minute, &row.valid_day]
        .iter()
        .filter(|value| value.is_some())
        .count();
    let shaped = set == 1 && row.valid_day.is_some() == row.valid_tz.is_some();
    if !shaped {
        tally(pass, "precision_shape", seq, || {
            format!("{set} publication values, valid_tz {:?}", row.valid_tz)
        });
    }
    let body_sound = match (&row.body_blob, &row.body) {
        (None, None) => true,
        (Some(blob), Some(body)) => FileDigest::of(body.as_bytes()).hex() == blob,
        _ => false,
    };
    if !body_sound {
        tally(pass, "body_digest_mismatch", seq, || {
            format!(
                "body_blob {:?} and a body of {:?} bytes",
                row.body_blob,
                row.body.as_ref().map(String::len)
            )
        });
    }
    let mut time_failed = !shaped;
    if let (true, Some(stated)) = (shaped, stated) {
        match visible_at(rule, &row.times(), pit::Cutoff::new(stated.cutoff)) {
            Ok(true) => {}
            Ok(false) => tally(pass, "not_visible", seq, || {
                format!(
                    "not visible at {} under {}",
                    cutoff_text(stated.cutoff),
                    rule.data_schema()
                )
            }),
            Err(why) => {
                time_failed = true;
                tally(pass, "time_unparseable", seq, || why.to_string());
            }
        }
    }
    match row.digest() {
        Ok(digest) => pass.digests.push(digest.hex().to_owned()),
        Err(why) => {
            pass.digest_failed = true;
            if !time_failed {
                tally(pass, "time_unparseable", seq, || why.to_string());
            }
        }
    }
}

fn tally(pass: &mut RowPass, code: &'static str, seq: i64, why: impl FnOnce() -> String) {
    pass.tallies.entry(code).or_default().note(seq, why);
}

fn report_rows(pass: RowPass, stated: Option<&Stated>, findings: &mut Vec<Finding>) {
    for (code, tally) in &pass.tallies {
        if let Some((seq, why)) = &tally.first {
            findings.push(Finding::new(
                code,
                format!("{} rows, first seq {seq}: {why}", tally.rows),
            ));
        }
    }
    let Some(stated) = stated else {
        return;
    };
    if pass.rows != stated.items {
        findings.push(Finding::new(
            "count_mismatch",
            format!(
                "{DATA_FILE} holds {} rows; the manifest states {}",
                pass.rows, stated.items
            ),
        ));
    }
    let mut recounted: BTreeMap<String, u64> = PROVENANCES
        .iter()
        .map(|name| ((*name).to_owned(), 0))
        .collect();
    for (name, count) in &pass.provenance {
        *recounted.entry(name.clone()).or_default() += count;
    }
    if recounted != stated.provenance_counts {
        findings.push(Finding::new(
            "provenance_counts",
            format!(
                "the rows count {recounted:?}; the manifest states {:?}",
                stated.provenance_counts
            ),
        ));
    }
    if pass.digest_failed {
        return;
    }
    let hexes: Vec<&str> = pass.digests.iter().map(String::as_str).collect();
    match snapshot_id_of(stated.cutoff, &hexes) {
        Ok(id) if id.hex() == stated.snapshot_id => {}
        Ok(id) => findings.push(Finding::new(
            "identity_mismatch",
            format!(
                "the rows digest to {id}; the manifest states {}",
                stated.snapshot_id
            ),
        )),
        Err(why) => findings.push(Finding::new(
            "identity_mismatch",
            format!("the rows have no snapshot_id: {why}"),
        )),
    }
}

/// The batch's rows; the columns are already known to equal [`data_schema`].
fn rows_of(batch: &RecordBatch) -> Result<Vec<SnapshotRow>, String> {
    let seq = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or("seq is not Int64")?;
    let text: Vec<&StringArray> = (1..batch.num_columns())
        .map(|index| {
            batch
                .column(index)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or(format!("column {index} is not Utf8"))
        })
        .collect::<Result<_, _>>()?;
    let [item_id, story_id, doc_id, source, provenance, valid_at, valid_minute, valid_day, valid_tz, observed_at, body_blob, body] =
        text.as_slice()
    else {
        return Err(format!("{} text columns", text.len()));
    };
    let optional = |column: &StringArray, row: usize| {
        (!column.is_null(row)).then(|| column.value(row).to_owned())
    };
    Ok((0..batch.num_rows())
        .map(|row| SnapshotRow {
            seq: seq.value(row),
            item_id: item_id.value(row).to_owned(),
            story_id: story_id.value(row).to_owned(),
            doc_id: optional(doc_id, row),
            source: source.value(row).to_owned(),
            provenance: provenance.value(row).to_owned(),
            valid_at: optional(valid_at, row),
            valid_minute: optional(valid_minute, row),
            valid_day: optional(valid_day, row),
            valid_tz: optional(valid_tz, row),
            observed_at: observed_at.value(row).to_owned(),
            body_blob: optional(body_blob, row),
            body: optional(body, row),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs::File;
    use std::path::Path;
    use std::sync::Arc;

    use arrow::array::{ArrayRef, Int64Array, RecordBatch};
    use arrow::datatypes::{DataType, Field, Schema};
    use chrono::{DateTime, Utc};
    use parquet::arrow::ArrowWriter;
    use parquet::basic::Compression;
    use parquet::file::metadata::KeyValue;
    use parquet::file::properties::WriterProperties;
    use parquet::schema::types::ColumnPath;
    use serde_json::Value;
    use tempfile::TempDir;

    use super::{validate, FINDING_CODES};
    use crate::{
        cutoff_text, file_sha256, snapshot_id_of, writer_properties, DataWriter, Manifest, Rule,
        SnapshotRow, DATA_FILE, MANIFEST_FILE, MANIFEST_SCHEMA, PROVENANCES, ROW_GROUP_BYTES,
        SCHEMA_KEY, SNAPSHOT_ID_KEY,
    };
    use canon::FileDigest;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    const CUTOFF: &str = "2026-07-05T00:00:00.000000Z";
    const NEW_YORK: &str = "America/New_York";
    /// Any digest-shaped text that no row set here digests to.
    const OTHER_HEX: &str = "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb";

    fn body(text: &str) -> (Option<String>, Option<String>) {
        (
            Some(FileDigest::of(text.as_bytes()).hex().to_owned()),
            Some(text.to_owned()),
        )
    }

    /// Three synthetic rows, one per precision, two provenances, one with no body.
    fn sound() -> Vec<SnapshotRow> {
        let (first_blob, first_body) = body("synthetic body one");
        let (third_blob, third_body) = body("synthetic body three");
        let base = SnapshotRow {
            seq: 1,
            item_id: "item-1".to_owned(),
            story_id: "story-1".to_owned(),
            doc_id: Some("doc-1".to_owned()),
            source: "open_web_wires".to_owned(),
            provenance: PROVENANCES[0].to_owned(),
            valid_at: Some("2026-07-01T13:30:00.000000Z".to_owned()),
            valid_minute: None,
            valid_day: None,
            valid_tz: None,
            observed_at: "2026-07-01T13:31:00.000000Z".to_owned(),
            body_blob: first_blob,
            body: first_body,
        };
        vec![
            base.clone(),
            SnapshotRow {
                seq: 2,
                item_id: "item-2".to_owned(),
                story_id: "story-2".to_owned(),
                doc_id: None,
                valid_at: None,
                valid_minute: Some("2026-07-02T19:00:00.000000Z".to_owned()),
                observed_at: "2026-07-02T19:00:30.000000Z".to_owned(),
                body_blob: None,
                body: None,
                ..base.clone()
            },
            SnapshotRow {
                seq: 4,
                item_id: "item-3".to_owned(),
                story_id: "story-3".to_owned(),
                source: "edgar_press".to_owned(),
                provenance: PROVENANCES[1].to_owned(),
                valid_at: None,
                valid_day: Some("2026-07-02".to_owned()),
                valid_tz: Some(NEW_YORK.to_owned()),
                observed_at: "2026-07-03T06:00:00.000000Z".to_owned(),
                body_blob: third_blob,
                body: third_body,
                ..base
            },
        ]
    }

    fn cutoff() -> TestResult<DateTime<Utc>> {
        Ok(CUTOFF.parse()?)
    }

    /// The id the rows digest to; a row with no digest leaves the id to the other rows.
    fn id_of(rows: &[SnapshotRow]) -> TestResult<String> {
        let digests: Vec<String> = rows
            .iter()
            .filter_map(|row| row.digest().ok())
            .map(|digest| digest.hex().to_owned())
            .collect();
        let hexes: Vec<&str> = digests.iter().map(String::as_str).collect();
        Ok(snapshot_id_of(cutoff()?, &hexes)
            .map_err(|why| why.to_string())?
            .hex()
            .to_owned())
    }

    fn write_data(path: &Path, rows: &[SnapshotRow], properties: WriterProperties) -> TestResult {
        let mut writer = DataWriter::with_properties(File::create_new(path)?, properties)?;
        for row in rows {
            writer.push(row.clone())?;
        }
        writer.finish()?;
        Ok(())
    }

    /// The manifest a sound producer writes over `rows` and the data file already at `dir`.
    fn manifest_for(dir: &Path, rows: &[SnapshotRow], id: &str) -> TestResult<Value> {
        let mut counts: std::collections::BTreeMap<String, u64> = PROVENANCES
            .iter()
            .map(|name| ((*name).to_owned(), 0))
            .collect();
        for row in rows {
            if let Some(count) = counts.get_mut(&row.provenance) {
                *count += 1;
            }
        }
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA.to_owned(),
            snapshot_id: id.to_owned(),
            cutoff: cutoff_text(cutoff()?),
            seq_high_water: rows.iter().map(|row| row.seq).max().unwrap_or(1),
            items: u64::try_from(rows.len())?,
            provenance_counts: counts,
            data_sha256: file_sha256(&dir.join(DATA_FILE))?.0,
        };
        Ok(serde_json::to_value(manifest)?)
    }

    fn write_manifest(dir: &Path, manifest: &Value) -> TestResult {
        std::fs::write(
            dir.join(MANIFEST_FILE),
            serde_json::to_vec_pretty(manifest)?,
        )?;
        Ok(())
    }

    /// A pair written the way `newsvault cut` writes one, from `rows`.
    fn pair(rows: &[SnapshotRow]) -> TestResult<TempDir> {
        let dir = TempDir::new()?;
        let id = id_of(rows)?;
        write_data(
            &dir.path().join(DATA_FILE),
            rows,
            writer_properties(&id, Rule::CURRENT),
        )?;
        write_manifest(dir.path(), &manifest_for(dir.path(), rows, &id)?)?;
        Ok(dir)
    }

    fn codes(dir: &Path) -> TestResult<Vec<&'static str>> {
        Ok(validate(dir)?.iter().map(|finding| finding.code).collect())
    }

    fn edited(rows: &[SnapshotRow], edit: impl FnOnce(&mut Value)) -> TestResult<TempDir> {
        let dir = pair(rows)?;
        let mut manifest: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(MANIFEST_FILE))?)?;
        edit(&mut manifest);
        write_manifest(dir.path(), &manifest)?;
        Ok(dir)
    }

    fn with_row(edit: impl FnOnce(&mut SnapshotRow)) -> TestResult<TempDir> {
        let mut rows = sound();
        if let Some(row) = rows.get_mut(1) {
            edit(row);
        }
        pair(&rows)
    }

    /// The data file rewritten under other properties, the manifest's digest of it kept true.
    fn rewritten(properties: WriterProperties) -> TestResult<TempDir> {
        let dir = pair(&sound())?;
        let data = dir.path().join(DATA_FILE);
        std::fs::remove_file(&data)?;
        write_data(&data, &sound(), properties)?;
        refresh_data_digest(dir.path())?;
        Ok(dir)
    }

    fn refresh_data_digest(dir: &Path) -> TestResult {
        let mut manifest: Value = serde_json::from_slice(&std::fs::read(dir.join(MANIFEST_FILE))?)?;
        manifest["data_sha256"] = Value::String(file_sha256(&dir.join(DATA_FILE))?.0);
        write_manifest(dir, &manifest)
    }

    fn properties(schema: &str, id: &str, compression: Compression) -> WriterProperties {
        WriterProperties::builder()
            .set_compression(compression)
            .set_key_value_metadata(Some(vec![
                KeyValue::new(SCHEMA_KEY.to_owned(), schema.to_owned()),
                KeyValue::new(SNAPSHOT_ID_KEY.to_owned(), id.to_owned()),
            ]))
            .build()
    }

    #[test]
    fn a_pair_written_as_the_producer_writes_it_has_no_finding() -> TestResult {
        let dir = pair(&sound())?;
        assert_eq!(codes(dir.path())?, Vec::<&str>::new());
        Ok(())
    }

    /// One case per check: each corrupts one thing in a sound pair and names the codes it must
    /// raise; a code no case reaches is red too. Mutations 2026-11-22, eleven checks disabled one
    /// at a time: red each (architecture.md section 8 row S1 lists them).
    #[test]
    fn each_check_names_exactly_its_own_defect() -> TestResult {
        let id = id_of(&sound())?;
        let cases: Vec<(&str, TempDir, Vec<&str>)> = vec![
            (
                "manifest_absent",
                {
                    let dir = pair(&sound())?;
                    std::fs::remove_file(dir.path().join(MANIFEST_FILE))?;
                    dir
                },
                vec!["manifest_absent"],
            ),
            (
                "manifest_unparseable",
                {
                    let dir = pair(&sound())?;
                    std::fs::write(dir.path().join(MANIFEST_FILE), b"[]")?;
                    dir
                },
                vec!["manifest_unparseable"],
            ),
            (
                "unknown_schema",
                edited(&sound(), |m| {
                    m["schema"] = Value::from("snapshot.manifest/2");
                })?,
                vec!["unknown_schema"],
            ),
            (
                "missing_field",
                edited(&sound(), |m| {
                    if let Some(table) = m.as_object_mut() {
                        table.remove("seq_high_water");
                    }
                })?,
                vec!["missing_field"],
            ),
            (
                "wrong_type: a prefixed snapshot_id",
                edited(&sound(), |m| {
                    m["snapshot_id"] = Value::from(format!("sha256:{id}"));
                })?,
                vec!["wrong_type"],
            ),
            (
                "wrong_type: a cutoff without its six digits",
                edited(&sound(), |m| {
                    m["cutoff"] = Value::from("2026-07-05T00:00:00Z");
                })?,
                vec!["wrong_type"],
            ),
            (
                "data_absent",
                {
                    let dir = pair(&sound())?;
                    std::fs::remove_file(dir.path().join(DATA_FILE))?;
                    dir
                },
                vec!["data_absent"],
            ),
            (
                "data_digest_mismatch",
                edited(&sound(), |m| {
                    m["data_sha256"] = Value::from(OTHER_HEX);
                })?,
                vec!["data_digest_mismatch"],
            ),
            (
                "data_unreadable",
                {
                    let dir = pair(&sound())?;
                    std::fs::write(dir.path().join(DATA_FILE), b"not parquet")?;
                    refresh_data_digest(dir.path())?;
                    dir
                },
                vec!["data_unreadable"],
            ),
            (
                "unknown_data_schema",
                rewritten(properties(
                    "snapshot.data/0",
                    &id,
                    Compression::ZSTD(parquet::basic::ZstdLevel::default()),
                ))?,
                vec!["unknown_data_schema"],
            ),
            (
                "stray_pairing",
                rewritten(properties(
                    Rule::CURRENT.data_schema(),
                    OTHER_HEX,
                    Compression::ZSTD(parquet::basic::ZstdLevel::default()),
                ))?,
                vec!["stray_pairing"],
            ),
            (
                "column_mismatch",
                {
                    let dir = pair(&sound())?;
                    write_one_column(&dir.path().join(DATA_FILE), &id)?;
                    refresh_data_digest(dir.path())?;
                    dir
                },
                vec!["column_mismatch"],
            ),
            (
                "not_zstd",
                rewritten(properties(
                    Rule::CURRENT.data_schema(),
                    &id,
                    Compression::UNCOMPRESSED,
                ))?,
                vec!["not_zstd"],
            ),
            (
                "seq_order: descending",
                with_row(|row| row.seq = 9)?,
                vec!["seq_order"],
            ),
            (
                "seq_order: above the high water",
                edited(&sound(), |m| {
                    m["seq_high_water"] = Value::from(3);
                })?,
                vec!["seq_order"],
            ),
            (
                "precision_shape",
                with_row(|row| {
                    row.valid_at = Some("2026-07-02T19:00:00.000000Z".to_owned());
                })?,
                vec!["precision_shape"],
            ),
            (
                "not_visible",
                with_row(|row| {
                    row.observed_at = "2026-07-06T00:00:00.000000Z".to_owned();
                })?,
                vec!["not_visible"],
            ),
            (
                "time_unparseable",
                with_row(|row| {
                    row.observed_at = "the next morning".to_owned();
                })?,
                vec!["time_unparseable"],
            ),
            (
                "body_digest_mismatch",
                with_row(|row| {
                    row.body = Some("a body with no blob".to_owned());
                })?,
                vec!["body_digest_mismatch"],
            ),
            (
                "count_mismatch",
                edited(&sound(), |m| {
                    m["items"] = Value::from(4);
                    m["provenance_counts"]["bulk_dataset"] = Value::from(1);
                })?,
                vec!["count_mismatch", "provenance_counts"],
            ),
            (
                "provenance_counts",
                with_row(|row| {
                    row.provenance = "rumour".to_owned();
                })?,
                vec!["provenance_counts", "provenance_counts"],
            ),
            (
                "identity_mismatch",
                {
                    let dir = TempDir::new()?;
                    write_data(
                        &dir.path().join(DATA_FILE),
                        &sound(),
                        writer_properties(OTHER_HEX, Rule::CURRENT),
                    )?;
                    write_manifest(dir.path(), &manifest_for(dir.path(), &sound(), OTHER_HEX)?)?;
                    dir
                },
                vec!["identity_mismatch"],
            ),
        ];
        let mut reached = BTreeSet::new();
        for (case, dir, expected) in &cases {
            assert_eq!(&codes(dir.path())?, expected, "{case}");
            reached.extend(expected.iter().copied());
        }
        assert_eq!(reached, FINDING_CODES.into_iter().collect::<BTreeSet<_>>());
        Ok(())
    }

    fn write_one_column(path: &Path, id: &str) -> TestResult {
        std::fs::remove_file(path)?;
        let schema = Arc::new(Schema::new(vec![Field::new("seq", DataType::Int64, false)]));
        let column: ArrayRef = Arc::new(Int64Array::from(vec![1_i64]));
        let mut writer = ArrowWriter::try_new(
            File::create_new(path)?,
            schema.clone(),
            Some(writer_properties(id, Rule::CURRENT)),
        )?;
        writer.write(&RecordBatch::try_new(schema, vec![column])?)?;
        writer.close()?;
        Ok(())
    }

    /// The writer stamps the rule it selected under, and that stamp is what the validator reads
    /// (`Rule::of_data_schema`); an unknown stamp is `unknown_data_schema`, never the current rule.
    #[test]
    fn a_file_is_judged_by_the_rule_its_own_data_schema_names() -> TestResult {
        let dir = pair(&sound())?;
        let file = File::open(dir.path().join(DATA_FILE))?;
        let builder = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)?;
        let named = builder
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .and_then(|pairs| pairs.iter().find(|pair| pair.key == SCHEMA_KEY))
            .and_then(|pair| pair.value.clone());
        assert_eq!(named.as_deref(), Some(Rule::V1.data_schema()));
        assert_eq!(codes(dir.path())?, Vec::<&str>::new());
        Ok(())
    }

    /// C2: parquet 60.1.0's defaults are UNCOMPRESSED and an unbounded byte size; the production
    /// properties name ZSTD and `ROW_GROUP_BYTES`. Mutation 2026-11-22, the byte bound removed: red.
    #[test]
    fn the_production_properties_name_zstd_and_a_byte_bounded_row_group() {
        let properties = writer_properties(OTHER_HEX, Rule::CURRENT);
        assert!(matches!(
            properties.compression(&ColumnPath::from("body")),
            Compression::ZSTD(_)
        ));
        assert_eq!(properties.max_row_group_bytes(), Some(ROW_GROUP_BYTES));
    }

    /// The bound holds in a written file: 600 rows of 2,000-byte bodies under a 64 KiB bound
    /// close more than one row group. Mutation 2026-11-22, `set_max_row_group_bytes` dropped from
    /// this test's properties: one row group, red.
    #[test]
    fn a_byte_bound_closes_row_groups_before_the_whole_file_is_held() -> TestResult {
        const ROWS: i64 = 600;
        const BODY_BYTES: usize = 2_000;
        const BOUND: usize = 64 * 1024;
        let template = sound().remove(0);
        let rows: Vec<SnapshotRow> = (1..=ROWS)
            .map(|seq| {
                let text = format!("{seq:06}{}", "x".repeat(BODY_BYTES));
                let (blob, body) = body(&text);
                SnapshotRow {
                    seq,
                    body_blob: blob,
                    body,
                    ..template.clone()
                }
            })
            .collect();
        let dir = TempDir::new()?;
        let path = dir.path().join(DATA_FILE);
        let properties = WriterProperties::builder()
            .set_compression(Compression::ZSTD(parquet::basic::ZstdLevel::default()))
            .set_max_row_group_bytes(Some(BOUND))
            .build();
        write_data(&path, &rows, properties)?;
        let builder = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
            File::open(&path)?,
        )?;
        let groups = builder.metadata().row_groups().len();
        assert!(groups > 1, "{groups} row group(s)");
        Ok(())
    }
}
