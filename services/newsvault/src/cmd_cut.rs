//! `newsvault cut --at <t> --out <dir>`: the §3.0 Snapshot, one immutable directory
//! `<out>/<snapshot_id>/` holding `manifest.json` and `snapshot.parquet`. `snapshot` is the backup
//! copy of the commit log; this is the file `derive` reads (architecture.md section 5.1).
//!
//! Two passes over one `seq` prefix, each a run of short chunked reads so no read transaction
//! holds the store across the body reads: the first selects and digests, the second writes. A row
//! with `seq <= seq_high_water` is assumed never to change between them; the validator, run on
//! the staged directory before its rename, refuses the cut if one did (`identity_mismatch`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Timelike, Utc};
use pit::Cutoff;
use rusqlite::{params, Connection, OpenFlags, Row};
use serde::Serialize;
use serde_json::{json, Value};
use snapshot_contract::{
    cutoff_text, file_sha256, snapshot_id_of, validate, visible_at, DataWriter, Finding, Manifest,
    Rule, SnapshotRow, TimeError, ValidateError, DATA_FILE, MANIFEST_FILE, MANIFEST_SCHEMA,
    PROVENANCES,
};

use crate::publish::Durability;
use crate::store::{blob_bytes, blob_store, require_current_version, StoreError, COMMIT_LOG};

pub const SCHEMA: &str = "newsvault.cut/1";
/// Rows one chunked read returns.
const CHUNK_ROWS: i64 = 512;
const STAGING_PREFIX: &str = ".tmp-";
const NANOS_PER_MICRO: u32 = 1_000;
/// How long a read waits on a writer's lock before `lease_held`; `cmd_snapshot`'s wait.
pub const BUSY_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct Ask {
    pub store: PathBuf,
    /// Required unless `dry_run`.
    pub out: Option<PathBuf>,
    pub at: DateTime<Utc>,
    /// The clock's now, injected so a test can stand at any instant.
    pub now: DateTime<Utc>,
    pub through_seq: Option<i64>,
    pub dry_run: bool,
    /// [`BUSY_WAIT`] in the binary; a test waits less.
    pub busy_wait: Duration,
}

/// The `newsvault.cut/1` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub dry_run: bool,
    pub changed: bool,
    pub snapshot_id: String,
    pub cutoff: String,
    pub seq_high_water: i64,
    pub items: u64,
    /// Distinct `story_id` among the rows; envelope only.
    pub stories: u64,
    pub provenance_counts: BTreeMap<String, u64>,
    /// Attempts opened at or before the cutoff and neither closed nor abandoned at the read: each
    /// can still commit a row observed before the cutoff, which a later cut at the same `--at`
    /// and no `--through-seq` would hold.
    pub open_attempts: u64,
    /// Sum of `blobs.bytes` over the rows' bodies, uncompressed.
    pub body_bytes: u64,
    /// Null on a dry run.
    pub path: Option<String>,
    pub data_sha256: Option<String>,
    pub bytes: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum CutError {
    #[error("--out is required unless --dry-run")]
    OutMissing,
    #[error("--out {0} is not an existing directory")]
    OutNotADirectory(PathBuf),
    #[error("--out {} is inside the store {}, which its backup would copy", .out.display(), .store.display())]
    OutInsideStore { out: PathBuf, store: PathBuf },
    #[error(
        "--at {at} is after the clock's now {now}: rows not yet collected would be outside the cut"
    )]
    AtInTheFuture { at: String, now: String },
    #[error(
        "--at {0} carries digits below the microsecond, which the cutoff's canonical form drops"
    )]
    AtBelowMicrosecond(String),
    #[error("--through-seq {0} is below 1")]
    ThroughSeqBelowOne(i64),
    #[error("--through-seq {asked} is above the store's highest seq {highest}")]
    ThroughSeqAboveStore { asked: i64, highest: i64 },
    #[error("no commit log at {0}")]
    StoreAbsent(PathBuf),
    #[error("seq {seq}: {error}")]
    StoredTime { seq: i64, error: TimeError },
    #[error("attempt {attempt_id}: opened_at {value:?} is not an RFC 3339 instant")]
    AttemptTime { attempt_id: String, value: String },
    #[error("seq {seq}: no digest over the row: {why}")]
    RowDigest { seq: i64, why: String },
    #[error("seq {seq}: its body blob {digest} is not in the store")]
    BodyAbsent { seq: i64, digest: String },
    #[error("seq {seq}: {why}")]
    BodyUnreadable { seq: i64, why: String },
    #[error("seq {seq}: its body blob {digest} is not UTF-8")]
    BodyNotText { seq: i64, digest: String },
    #[error("seq {seq}: {why}")]
    DictionaryAbsent { seq: i64, why: String },
    #[error("{rows} selected rows, seq {first_seq} to {last_seq}, sit under attempts with no recorded provenance")]
    ProvenanceUnrecorded {
        first_seq: i64,
        last_seq: i64,
        rows: u64,
    },
    #[error("no row is visible at {cutoff} through seq {seq_high_water}")]
    NothingVisible { cutoff: String, seq_high_water: i64 },
    #[error("the staged cut at {} is not admissible; it is left there", .path.display())]
    StagedInvalid {
        path: PathBuf,
        findings: Vec<Finding>,
    },
    #[error("{} already stands and is not admissible; it is left as found", .path.display())]
    StandingInvalid {
        path: PathBuf,
        findings: Vec<Finding>,
    },
    /// `after`: the failure of this run that could not remove its own staging directory.
    #[error("a staging directory {} is left by a cut that did not finish{}; remove it once no cut of this set runs", .path.display(), .after.as_ref().map(|why| format!(" ({why})")).unwrap_or_default())]
    StagingLeft {
        path: PathBuf,
        after: Option<String>,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Parquet(#[from] parquet::errors::ParquetError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl CutError {
    /// The section 5.1 registry code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Sqlite(failure) | Self::Store(StoreError::Sqlite(failure)) if busy(failure) => {
                "lease_held"
            }
            Self::OutMissing
            | Self::OutNotADirectory(_)
            | Self::OutInsideStore { .. }
            | Self::AtInTheFuture { .. }
            | Self::AtBelowMicrosecond(_)
            | Self::ThroughSeqBelowOne(_)
            | Self::ThroughSeqAboveStore { .. } => "invalid_argument",
            Self::StoreAbsent(_) | Self::BodyAbsent { .. } | Self::DictionaryAbsent { .. } => {
                "input_absent"
            }
            Self::StoredTime { .. }
            | Self::AttemptTime { .. }
            | Self::RowDigest { .. }
            | Self::BodyUnreadable { .. }
            | Self::BodyNotText { .. }
            | Self::StagedInvalid { .. }
            | Self::StandingInvalid { .. } => "input_unparseable",
            Self::ProvenanceUnrecorded { .. } => "provenance_unrecorded",
            Self::NothingVisible { .. } => "nothing_visible",
            Self::Store(StoreError::SchemaVersion { .. }) => "input_schema_mismatch",
            Self::StagingLeft { .. } => "staging_left",
            Self::Store(_) | Self::Sqlite(_) | Self::Parquet(_) | Self::Json(_) | Self::Io(_) => {
                "io_failed"
            }
        }
    }

    #[must_use]
    pub fn exit(&self) -> u8 {
        match self.code() {
            "input_absent" | "provenance_unrecorded" | "nothing_visible" => 4,
            "lease_held" => 7,
            _ => 2,
        }
    }

    /// Only a writer's lock passes by itself; every other refusal returns on the same argv.
    #[must_use]
    pub fn retryable(&self) -> bool {
        self.code() == "lease_held"
    }

    /// The facts a caller branches on, beside the free-text message.
    #[must_use]
    pub fn details(&self) -> Value {
        let shown = |path: &Path| path.display().to_string();
        match self {
            Self::ProvenanceUnrecorded {
                first_seq,
                last_seq,
                rows,
            } => json!({"first_seq": first_seq, "last_seq": last_seq, "rows": rows}),
            Self::StagedInvalid { path, findings } | Self::StandingInvalid { path, findings } => {
                json!({"path": shown(path), "findings": findings})
            }
            Self::StagingLeft { path, .. }
            | Self::OutNotADirectory(path)
            | Self::StoreAbsent(path) => {
                json!({"path": shown(path)})
            }
            Self::OutInsideStore { out, store } => {
                json!({"out": shown(out), "store": shown(store)})
            }
            Self::AttemptTime { attempt_id, .. } => json!({"attempt_id": attempt_id}),
            Self::StoredTime { seq, .. }
            | Self::RowDigest { seq, .. }
            | Self::BodyAbsent { seq, .. }
            | Self::BodyUnreadable { seq, .. }
            | Self::BodyNotText { seq, .. }
            | Self::DictionaryAbsent { seq, .. } => json!({"seq": seq}),
            Self::NothingVisible {
                cutoff,
                seq_high_water,
            } => json!({"cutoff": cutoff, "seq_high_water": seq_high_water}),
            Self::ThroughSeqAboveStore { asked, highest } => {
                json!({"through_seq": asked, "highest_seq": highest})
            }
            _ => json!({}),
        }
    }
}

fn busy(failure: &rusqlite::Error) -> bool {
    matches!(
        failure.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

/// One stored row as both passes read it: the row, its attempt's provenance (`None`: not
/// recorded) and its body's `blobs.bytes`.
type Stored = (SnapshotRow, Option<String>, Option<u64>);

/// What the first pass keeps per selected row: its `seq`, for the second pass, and its digest.
struct Selection {
    seq_high_water: i64,
    seqs: Vec<i64>,
    digests: Vec<String>,
    stories: u64,
    provenance_counts: BTreeMap<String, u64>,
    body_bytes: u64,
}

pub fn run(ask: &Ask, durability: &dyn Durability) -> Result<Report, CutError> {
    refuse_arguments(ask)?;
    let log = ask.store.join(COMMIT_LOG);
    if !log.is_file() {
        return Err(CutError::StoreAbsent(log));
    }
    if let Some(out) = ask.out.as_deref().filter(|_| !ask.dry_run) {
        refuse_out_inside(out, &ask.store)?;
    }
    let db = Connection::open_with_flags(&log, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(ask.busy_wait)?;
    require_current_version(&db)?;
    let cutoff = Cutoff::new(ask.at);
    let highest: i64 = db.query_row("SELECT COALESCE(MAX(seq), 0) FROM items", [], |row| {
        row.get(0)
    })?;
    let seq_high_water = match ask.through_seq {
        Some(asked) if asked > highest => {
            return Err(CutError::ThroughSeqAboveStore { asked, highest })
        }
        Some(asked) => asked,
        None => highest,
    };
    let selection = select(&db, &ask.store, seq_high_water, cutoff)?;
    let hexes: Vec<&str> = selection.digests.iter().map(String::as_str).collect();
    let snapshot_id = snapshot_id_of(ask.at, &hexes)
        .map_err(|why| CutError::RowDigest {
            seq: seq_high_water,
            why: why.to_string(),
        })?
        .hex()
        .to_owned();
    let mut report = Report {
        schema: SCHEMA,
        dry_run: ask.dry_run,
        changed: false,
        snapshot_id: snapshot_id.clone(),
        cutoff: cutoff_text(ask.at),
        seq_high_water,
        items: u64::try_from(selection.seqs.len()).unwrap_or(u64::MAX),
        stories: selection.stories,
        provenance_counts: selection.provenance_counts.clone(),
        open_attempts: open_attempts(&db, ask.at)?,
        body_bytes: selection.body_bytes,
        path: None,
        data_sha256: None,
        bytes: None,
    };
    let Some(out) = ask.out.as_deref().filter(|_| !ask.dry_run) else {
        return Ok(report);
    };
    let standing = out.join(&snapshot_id);
    if fs::symlink_metadata(&standing).is_ok() {
        let findings = validated(&standing)?;
        if !findings.is_empty() {
            return Err(CutError::StandingInvalid {
                path: standing,
                findings,
            });
        }
        let manifest: Manifest = serde_json::from_slice(&fs::read(standing.join(MANIFEST_FILE))?)?;
        report.seq_high_water = manifest.seq_high_water;
        report.items = manifest.items;
        report.provenance_counts = manifest.provenance_counts;
        report.data_sha256 = Some(manifest.data_sha256);
        report.bytes = Some(fs::metadata(standing.join(DATA_FILE))?.len());
        report.path = Some(standing.display().to_string());
        return Ok(report);
    }
    let staging = out.join(format!("{STAGING_PREFIX}{snapshot_id}"));
    match fs::create_dir(&staging) {
        Ok(()) => {}
        Err(failure) if failure.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(CutError::StagingLeft {
                path: staging,
                after: None,
            })
        }
        Err(failure) => return Err(failure.into()),
    }
    let (data_sha256, bytes) = match staged(&db, ask, &selection, &staging, &report, durability) {
        Ok(written) => written,
        Err(failure @ CutError::StagedInvalid { .. }) => return Err(failure),
        Err(failure) => return Err(abandoned(staging, failure)),
    };
    durability.rename(&staging, &standing)?;
    durability.sync_dir(out)?;
    report.changed = true;
    report.path = Some(standing.display().to_string());
    report.data_sha256 = Some(data_sha256);
    report.bytes = Some(bytes);
    Ok(report)
}

/// Steps 3 and 4 of section 3.0's write sequence and the staging directory's sync: the data file,
/// the manifest, the validator. Answers the data file's sha256 and length.
fn staged(
    db: &Connection,
    ask: &Ask,
    selection: &Selection,
    staging: &Path,
    report: &Report,
    durability: &dyn Durability,
) -> Result<(String, u64), CutError> {
    write_data(
        db,
        ask,
        selection,
        &staging.join(DATA_FILE),
        &report.snapshot_id,
        durability,
    )?;
    let (data_sha256, bytes) = file_sha256(&staging.join(DATA_FILE))?;
    let manifest = Manifest {
        schema: MANIFEST_SCHEMA.to_owned(),
        snapshot_id: report.snapshot_id.clone(),
        cutoff: report.cutoff.clone(),
        seq_high_water: report.seq_high_water,
        items: report.items,
        provenance_counts: report.provenance_counts.clone(),
        data_sha256: data_sha256.clone(),
    };
    let mut file = File::create_new(staging.join(MANIFEST_FILE))?;
    file.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
    durability.sync_file(&file)?;
    drop(file);
    let findings = validated(staging)?;
    if !findings.is_empty() {
        return Err(CutError::StagedInvalid {
            path: staging.to_path_buf(),
            findings,
        });
    }
    durability.sync_dir(staging)?;
    Ok((data_sha256, bytes))
}

/// This run's own staging directory, removed after a failure other than a finding, so the same
/// argv can run again; one it cannot remove is named instead of the failure being lost.
fn abandoned(staging: PathBuf, failure: CutError) -> CutError {
    match fs::remove_dir_all(&staging) {
        Ok(()) => failure,
        Err(_) => CutError::StagingLeft {
            path: staging,
            after: Some(failure.to_string()),
        },
    }
}

/// Section 3.0: a cut is kept outside the store, whose backup would otherwise copy it.
fn refuse_out_inside(out: &Path, store: &Path) -> Result<(), CutError> {
    let (out, store) = (fs::canonicalize(out)?, fs::canonicalize(store)?);
    if out.starts_with(&store) {
        return Err(CutError::OutInsideStore { out, store });
    }
    Ok(())
}

fn refuse_arguments(ask: &Ask) -> Result<(), CutError> {
    if ask.at > ask.now {
        return Err(CutError::AtInTheFuture {
            at: ask.at.to_rfc3339(),
            now: ask.now.to_rfc3339(),
        });
    }
    if !ask.at.nanosecond().is_multiple_of(NANOS_PER_MICRO) {
        return Err(CutError::AtBelowMicrosecond(ask.at.to_rfc3339()));
    }
    if let Some(asked) = ask.through_seq.filter(|asked| *asked < 1) {
        return Err(CutError::ThroughSeqBelowOne(asked));
    }
    match (&ask.out, ask.dry_run) {
        (None, false) => Err(CutError::OutMissing),
        (Some(out), false) if !out.is_dir() => Err(CutError::OutNotADirectory(out.clone())),
        _ => Ok(()),
    }
}

fn validated(dir: &Path) -> Result<Vec<Finding>, CutError> {
    validate(dir).map_err(|failure| match failure {
        ValidateError::Io { source, .. } => CutError::Io(source),
        other => CutError::Io(std::io::Error::other(other.to_string())),
    })
}

/// The rows of `items` with `after < seq <= through`, joined to their attempt as `items` reads
/// them, at most [`CHUNK_ROWS`] of them, in `seq` order. The provenance is `None` when the attempt
/// has none recorded.
fn chunk(db: &Connection, after: i64, through: i64) -> Result<Vec<Stored>, CutError> {
    let mut statement = db.prepare_cached(
        "SELECT i.seq, i.item_id, i.story_id, i.doc_id, a.source, a.provenance, i.valid_at,
                i.valid_minute, i.valid_day, i.valid_tz, i.observed_at, i.body_blob, b.bytes
         FROM items i
         JOIN attempts a ON a.attempt_id = i.attempt_id
         LEFT JOIN blobs b ON b.digest = i.body_blob
         WHERE i.seq > ?1 AND i.seq <= ?2
         ORDER BY i.seq
         LIMIT ?3",
    )?;
    let rows = statement
        .query_map(params![after, through, CHUNK_ROWS], read_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn read_row(row: &Row<'_>) -> rusqlite::Result<Stored> {
    let provenance: Option<String> = row.get(5)?;
    let bytes: Option<i64> = row.get(12)?;
    Ok((
        SnapshotRow {
            seq: row.get(0)?,
            item_id: row.get(1)?,
            story_id: row.get(2)?,
            doc_id: row.get(3)?,
            source: row.get(4)?,
            provenance: provenance.clone().unwrap_or_default(),
            valid_at: row.get(6)?,
            valid_minute: row.get(7)?,
            valid_day: row.get(8)?,
            valid_tz: row.get(9)?,
            observed_at: row.get(10)?,
            body_blob: row.get(11)?,
            body: None,
        },
        provenance,
        bytes.and_then(|bytes| u64::try_from(bytes).ok()),
    ))
}

/// Every chunk of the prefix in turn, each handed to `each`.
fn each_row(
    db: &Connection,
    through: i64,
    mut each: impl FnMut(SnapshotRow, Option<String>, Option<u64>) -> Result<(), CutError>,
) -> Result<(), CutError> {
    let mut after = 0;
    loop {
        let rows = chunk(db, after, through)?;
        let Some(last) = rows.last().map(|(row, _, _)| row.seq) else {
            return Ok(());
        };
        for (row, provenance, bytes) in rows {
            each(row, provenance, bytes)?;
        }
        after = last;
    }
}

/// The first pass: which rows are visible, their digests and counts. Reads no body; checks only
/// that each selected body is on disk, so a cut refused for an absent body writes nothing.
fn select(
    db: &Connection,
    root: &Path,
    seq_high_water: i64,
    cutoff: Cutoff,
) -> Result<Selection, CutError> {
    let blobs = blob_store(root);
    let mut selection = Selection {
        seq_high_water,
        seqs: Vec::new(),
        digests: Vec::new(),
        stories: 0,
        provenance_counts: PROVENANCES
            .iter()
            .map(|name| ((*name).to_owned(), 0))
            .collect(),
        body_bytes: 0,
    };
    let mut stories = BTreeSet::new();
    let mut unrecorded: Option<(i64, i64, u64)> = None;
    each_row(db, seq_high_water, |row, provenance, bytes| {
        let visible = visible_at(Rule::CURRENT, &row.times(), cutoff).map_err(|error| {
            CutError::StoredTime {
                seq: row.seq,
                error,
            }
        })?;
        if !visible {
            return Ok(());
        }
        let Some(provenance) = provenance else {
            unrecorded = Some(match unrecorded {
                None => (row.seq, row.seq, 1),
                Some((first, _, rows)) => (first, row.seq, rows + 1),
            });
            return Ok(());
        };
        if let Some(digest) = &row.body_blob {
            if blobs.named(digest).is_none() {
                return Err(CutError::BodyAbsent {
                    seq: row.seq,
                    digest: digest.clone(),
                });
            }
        }
        let digest = row.digest().map_err(|why| CutError::RowDigest {
            seq: row.seq,
            why: why.to_string(),
        })?;
        *selection.provenance_counts.entry(provenance).or_default() += 1;
        selection.body_bytes += bytes.unwrap_or_default();
        stories.insert(row.story_id);
        selection.seqs.push(row.seq);
        selection.digests.push(digest.hex().to_owned());
        Ok(())
    })?;
    if let Some((first_seq, last_seq, rows)) = unrecorded {
        return Err(CutError::ProvenanceUnrecorded {
            first_seq,
            last_seq,
            rows,
        });
    }
    if selection.seqs.is_empty() {
        return Err(CutError::NothingVisible {
            cutoff: cutoff_text(cutoff.inner()),
            seq_high_water: selection.seq_high_water,
        });
    }
    selection.stories = u64::try_from(stories.len()).unwrap_or(u64::MAX);
    Ok(selection)
}

/// The second pass: the selected rows again, each with its body read and re-hashed by the blob
/// store, written in bounded batches. The file is fsynced before the caller hashes it.
fn write_data(
    db: &Connection,
    ask: &Ask,
    selection: &Selection,
    path: &Path,
    snapshot_id: &str,
    durability: &dyn Durability,
) -> Result<(), CutError> {
    let mut writer = DataWriter::create(File::create_new(path)?, snapshot_id)?;
    let mut wanted = selection.seqs.iter().peekable();
    each_row(db, selection.seq_high_water, |mut row, _, _| {
        if wanted.peek() != Some(&&row.seq) {
            return Ok(());
        }
        wanted.next();
        if let Some(digest) = row.body_blob.clone() {
            row.body = Some(body_of(&ask.store, row.seq, &digest)?);
        }
        writer.push(row)?;
        Ok(())
    })?;
    writer.finish()?;
    durability.sync_file(writer.file())?;
    Ok(())
}

fn body_of(root: &Path, seq: i64, digest: &str) -> Result<String, CutError> {
    let bytes = match blob_bytes(root, digest) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return Err(CutError::BodyAbsent {
                seq,
                digest: digest.to_owned(),
            })
        }
        Err(failure @ StoreError::DictionaryAbsent { .. }) => {
            return Err(CutError::DictionaryAbsent {
                seq,
                why: failure.to_string(),
            })
        }
        Err(failure) => {
            return Err(CutError::BodyUnreadable {
                seq,
                why: failure.to_string(),
            })
        }
    };
    String::from_utf8(bytes).map_err(|_| CutError::BodyNotText {
        seq,
        digest: digest.to_owned(),
    })
}

/// Attempts opened at or before `at` with no `closed_at` and no sweep's `noticed_at`.
fn open_attempts(db: &Connection, at: DateTime<Utc>) -> Result<u64, CutError> {
    let mut statement = db.prepare(
        "SELECT attempt_id, opened_at FROM attempts WHERE closed_at IS NULL AND noticed_at IS NULL",
    )?;
    let opened: Vec<(String, String)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let mut count = 0_u64;
    for (attempt_id, value) in opened {
        let Ok(opened) = value.parse::<DateTime<Utc>>() else {
            return Err(CutError::AttemptTime { attempt_id, value });
        };
        count += u64::from(opened <= at);
    }
    Ok(count)
}

/// The human rendering of the envelope.
pub fn render_human(report: &Report, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "snapshot         {}", report.snapshot_id)?;
    writeln!(out, "cutoff           {}", report.cutoff)?;
    writeln!(out, "seq high water   {}", report.seq_high_water)?;
    writeln!(out, "items/stories    {}/{}", report.items, report.stories)?;
    for (name, count) in &report.provenance_counts {
        writeln!(out, "  {name:<15}{count}")?;
    }
    writeln!(out, "open attempts    {}", report.open_attempts)?;
    writeln!(out, "body bytes       {}", report.body_bytes)?;
    writeln!(
        out,
        "path             {}{}",
        report.path.as_deref().unwrap_or("-"),
        match (report.dry_run, report.changed) {
            (true, _) => " (dry run)",
            (false, false) => " (already standing)",
            (false, true) => "",
        }
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use news_provider::Provenance;
    use snapshot_contract::PROVENANCES;

    /// The contract crate cannot depend on `news-provider`, so it keeps its own list of names. The
    /// match stops compiling when a variant is added, so the array below cannot fall behind it.
    #[test]
    fn the_contract_names_every_provenance_the_store_can_hold() {
        let listed = |provenance: Provenance| match provenance {
            Provenance::LiveIngest | Provenance::VendorArchive | Provenance::BulkDataset => {
                provenance.name()
            }
        };
        let names = [
            Provenance::LiveIngest,
            Provenance::VendorArchive,
            Provenance::BulkDataset,
        ]
        .map(listed);
        assert_eq!(names, PROVENANCES);
    }
}
