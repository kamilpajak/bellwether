//! `newsvault blobs verify | compress | dictionary train`: the payload store's own commands
//! (architecture.md section 3.9, row "compression"). `verify` is the real-path reader of
//! [`BlobStore::stream`], the bulk read a re-extraction pass makes.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use news_provider::RunId;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::blobs::{BlobStore, Encoding, Listed, ReadBack, DICTIONARY_BYTES, LEVEL};
use crate::store::{self, Committer, DictionaryRow, StoreError, Trained, COMMIT_LOG};

pub const VERIFY_SCHEMA: &str = "newsvault.blobs.verify/1";
pub const COMPRESS_SCHEMA: &str = "newsvault.blobs.compress/1";
pub const DICTIONARY_SCHEMA: &str = "newsvault.blobs.dictionary/1";
/// Training samples and held-out blobs when the caller names no count. 2,000 is the sample the
/// 2026-11-21 measurement on the VPS trained on (section 3.9).
pub const DEFAULT_SAMPLES: usize = 2_000;
pub const DEFAULT_HOLDOUT: usize = 500;
/// Each sample's first bytes only: the samples are held once, in one buffer, at most
/// `--samples` × 128 KiB (256 MiB at the default); held-out payloads are read one at a time.
pub const SAMPLE_PREFIX: usize = 131_072;
pub const DEFAULT_FAILURES_SHOWN: usize = 20;

#[derive(Debug, thiserror::Error)]
pub enum BlobsError {
    #[error("no store at {0}")]
    StoreAbsent(PathBuf),
    /// A `.zst` file written now names its dictionary for good, so a store rewritten before the
    /// first dictionary is trained keeps the weaker ratio.
    #[error(
        "no zstd dictionary is active: run `newsvault blobs dictionary train` first, or pass \
         --allow-no-dictionary"
    )]
    NoActiveDictionary,
    #[error("training takes {needed} stored payloads (samples plus held-out) and the store holds {found}")]
    TooFewPayloads { needed: usize, found: usize },
    #[error("--{0} must be at least 1")]
    ZeroCount(&'static str),
    #[error("zstd refused to train a dictionary: {0}")]
    TrainingRefused(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

impl BlobsError {
    /// The section 5.1 registry code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::StoreAbsent(_)
            | Self::TooFewPayloads { .. }
            | Self::Store(StoreError::DictionaryAbsent { .. }) => "input_absent",
            Self::NoActiveDictionary | Self::ZeroCount(_) => "invalid_argument",
            Self::TrainingRefused(_) | Self::Store(StoreError::DictionaryAltered { .. }) => {
                "input_unparseable"
            }
            Self::Store(StoreError::LeaseHeld(_)) => "lease_held",
            Self::Store(
                StoreError::SchemaVersion { .. }
                | StoreError::ShapeDiffers { .. }
                | StoreError::MigrationFailed { .. },
            ) => "input_schema_mismatch",
            Self::Store(_) | Self::Sqlite(_) => "io_failed",
        }
    }

    #[must_use]
    pub const fn exit(&self) -> u8 {
        match self.code().as_bytes() {
            b"input_absent" => 4,
            b"lease_held" => 7,
            _ => 2,
        }
    }

    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(self, Self::Store(StoreError::LeaseHeld(_)))
    }
}

/// One blob, or one file, that did not read back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Failure {
    /// The digest, or the path of a file that names none.
    pub blob: String,
    pub code: &'static str,
    pub message: String,
}

fn failure_code(failure: &StoreError) -> &'static str {
    match failure {
        StoreError::BlobAltered { .. } => "blob_altered",
        StoreError::BlobUndecodable { .. } => "blob_undecodable",
        StoreError::BlobStray { .. } => "blob_stray",
        StoreError::DictionaryAbsent { .. } => "dictionary_absent",
        StoreError::DictionaryAltered { .. } => "dictionary_altered",
        _ => "io_failed",
    }
}

/// Failures as a report carries them: every one counted, the first `limit` shown.
#[derive(Debug, Default)]
struct Failures {
    limit: usize,
    count: u64,
    shown: Vec<Failure>,
}

impl Failures {
    fn add(&mut self, blob: String, failure: &StoreError) {
        self.count += 1;
        if self.shown.len() < self.limit {
            self.shown.push(Failure {
                blob,
                code: failure_code(failure),
                message: failure.to_string(),
            });
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Tally {
    pub blobs: u64,
    pub logical_bytes: u64,
    pub stored_bytes: u64,
}

impl Tally {
    fn add(&mut self, read: &ReadBack) {
        self.blobs += 1;
        self.logical_bytes += u64::try_from(read.payload.len()).unwrap_or(u64::MAX);
        self.stored_bytes += read.stored_bytes;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DictionaryTally {
    /// `null` for the raw copies and the dictionary-less frames.
    pub dictionary: Option<String>,
    #[serde(flatten)]
    pub tally: Tally,
    /// The `dictionaries` row; `null` for no dictionary, or a file the log does not record.
    pub recorded: Option<DictionaryRow>,
}

/// The commit log's `blobs` rows against the tree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Indexed {
    pub rows: u64,
    /// Rows naming a digest with no file in either form.
    pub absent: u64,
    /// Rows whose `bytes` is not the payload's length read back: §11.1's held read compares it.
    pub length_differs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerifyReport {
    pub schema: &'static str,
    pub store: String,
    pub blobs: u64,
    pub verified: u64,
    pub failed: u64,
    /// Walk failures: a directory that could not be listed, or a file in the tree that names no
    /// digest. Either is a part of the store no blob count covers.
    pub listing_failed: u64,
    /// Every copy read, by the file it was read from: `raw` and `zstd`.
    pub by_encoding: BTreeMap<&'static str, Tally>,
    pub by_dictionary: Vec<DictionaryTally>,
    /// Digests held as both files: `blobs compress` stopped between rename and removal.
    pub both_copies: u64,
    pub staging_leftovers: usize,
    pub active_dictionary: Option<String>,
    /// `null` when the store has no commit log.
    pub indexed: Option<Indexed>,
    pub failures: Vec<Failure>,
    pub changed: bool,
}

impl VerifyReport {
    /// Every blob read back to its digest, the whole tree was walked, and every row matches it.
    #[must_use]
    pub fn is_sound(&self) -> bool {
        self.failed == 0
            && self.listing_failed == 0
            && self
                .indexed
                .is_none_or(|indexed| indexed.absent == 0 && indexed.length_differs == 0)
    }
}

/// Reads every blob back through [`BlobStore::stream`]; mutates nothing and takes no lease. A
/// copy that `blobs compress` removes mid-walk is read again from the copy left.
pub fn verify(root: &std::path::Path, limit: usize) -> Result<VerifyReport, BlobsError> {
    if !root.is_dir() {
        return Err(BlobsError::StoreAbsent(root.to_path_buf()));
    }
    let (active, recorded) = store::recorded_dictionaries(root)?;
    let log = root.join(COMMIT_LOG);
    let db = log
        .exists()
        .then(|| Connection::open_with_flags(&log, OpenFlags::SQLITE_OPEN_READ_ONLY))
        .transpose()?;
    let mut indexed = db.as_ref().map(|_| Indexed::default());
    let blobs = store::blob_store(root);
    let mut failures = Failures {
        limit,
        ..Failures::default()
    };
    let mut by_encoding: BTreeMap<&'static str, Tally> = BTreeMap::new();
    let mut by_dictionary: BTreeMap<Option<String>, Tally> = BTreeMap::new();
    let (mut digests, mut verified, mut both_copies, mut listing_failed) = (0, 0, 0, 0);
    for streamed in blobs.stream() {
        let streamed = match streamed {
            Ok(streamed) => streamed,
            Err(failure) => {
                let blob = match &failure {
                    StoreError::BlobStray { path } => path.display().to_string(),
                    _ => root.display().to_string(),
                };
                listing_failed += 1;
                failures.add(blob, &failure);
                continue;
            }
        };
        digests += 1;
        let hex = streamed.listed.digest.hex().to_owned();
        let mut copies = vec![streamed.read];
        if streamed.listed.raw && streamed.listed.zstd {
            both_copies += 1;
            copies.push(blobs.read_copy(&streamed.listed.digest, Encoding::Zstd));
        }
        let mut sound = true;
        for copy in copies {
            match copy {
                Ok(read) => {
                    by_encoding
                        .entry(read.encoding.name())
                        .or_default()
                        .add(&read);
                    by_dictionary
                        .entry(read.dictionary.clone())
                        .or_default()
                        .add(&read);
                    if let (Some(db), Some(indexed)) = (db.as_ref(), indexed.as_mut()) {
                        let bytes: Option<i64> = db
                            .query_row("SELECT bytes FROM blobs WHERE digest = ?1", [&hex], |row| {
                                row.get(0)
                            })
                            .optional()?;
                        if bytes.is_some_and(|bytes| {
                            u64::try_from(bytes).ok() != u64::try_from(read.payload.len()).ok()
                        }) {
                            indexed.length_differs += 1;
                        }
                    }
                }
                Err(failure) => {
                    sound = false;
                    failures.add(hex.clone(), &failure);
                }
            }
        }
        if sound {
            verified += 1;
        }
    }
    if let (Some(db), Some(indexed)) = (db.as_ref(), indexed.as_mut()) {
        let mut rows = db.prepare("SELECT digest FROM blobs ORDER BY digest")?;
        let mut rows = rows.query([])?;
        while let Some(row) = rows.next()? {
            let hex: String = row.get(0)?;
            indexed.rows += 1;
            if blobs.named(&hex).is_none() {
                indexed.absent += 1;
            }
        }
    }
    let by_dictionary = by_dictionary
        .into_iter()
        .map(|(dictionary, tally)| DictionaryTally {
            recorded: dictionary
                .as_ref()
                .and_then(|hex| recorded.iter().find(|row| &row.digest == hex).cloned()),
            dictionary,
            tally,
        })
        .collect();
    Ok(VerifyReport {
        schema: VERIFY_SCHEMA,
        store: root.display().to_string(),
        blobs: digests,
        verified,
        failed: digests - verified,
        listing_failed,
        by_encoding,
        by_dictionary,
        both_copies,
        staging_leftovers: blobs.staging_leftovers()?,
        active_dictionary: active,
        indexed,
        failures: failures.shown,
        changed: false,
    })
}

#[derive(Debug, Clone)]
pub struct CompressAsk {
    pub store: PathBuf,
    pub at: DateTime<Utc>,
    pub allow_no_dictionary: bool,
    pub dry_run: bool,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompressReport {
    pub schema: &'static str,
    pub store: String,
    pub dictionary: Option<String>,
    pub level: i32,
    /// Raw copies found: what a run rewrites, or a dry run would.
    pub raw_found: u64,
    pub raw_bytes: u64,
    pub compressed: u64,
    /// Of `compressed`, those whose `.zst` copy an earlier run had already renamed into place.
    pub resumed: u64,
    /// Of `compressed`, those whose `.zst` copy was there and did not read back to the digest:
    /// rewritten from the raw copy.
    pub replaced: u64,
    /// Staging files removed before the rewrite: what a crash or a failed write left.
    pub staging_removed: u64,
    pub compressed_stored_bytes: u64,
    pub failed: u64,
    pub failures: Vec<Failure>,
    pub dry_run: bool,
    pub changed: bool,
}

impl CompressReport {
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.failed == 0
    }
}

/// Rewrites every raw copy as `.zst` under the store's `LOCK` lease, the one `collect` and
/// `snapshot` take. The backup's tar of `blobs/` holds only the host lock the systemd units take,
/// so a manual run goes under that lock too (§3.9). Running it again rewrites only what is raw.
pub fn compress(ask: &CompressAsk) -> Result<CompressReport, BlobsError> {
    if !ask.store.join(COMMIT_LOG).is_file() {
        return Err(BlobsError::StoreAbsent(ask.store.join(COMMIT_LOG)));
    }
    let (active, _) = store::recorded_dictionaries(&ask.store)?;
    if active.is_none() && !ask.allow_no_dictionary {
        return Err(BlobsError::NoActiveDictionary);
    }
    let mut failures = Failures {
        limit: ask.limit,
        ..Failures::default()
    };
    let mut report = CompressReport {
        schema: COMPRESS_SCHEMA,
        store: ask.store.display().to_string(),
        dictionary: active,
        level: LEVEL,
        raw_found: 0,
        raw_bytes: 0,
        compressed: 0,
        resumed: 0,
        replaced: 0,
        staging_removed: 0,
        compressed_stored_bytes: 0,
        failed: 0,
        failures: Vec::new(),
        dry_run: ask.dry_run,
        changed: false,
    };
    if !ask.dry_run {
        let run = RunId::new(format!("blobs-compress-{}", stamped(ask.at)));
        let committer = Committer::open(&ask.store, &run)?;
        report.dictionary = committer.blobs().active_dictionary();
        report.staging_removed =
            u64::try_from(committer.remove_staging_leftovers()?).unwrap_or(u64::MAX);
        report.changed = report.staging_removed > 0;
        return rewrite(committer.blobs(), run.as_str(), report, failures);
    }
    let blobs = store::blob_store(&ask.store);
    for listed in blobs.listing() {
        match listed {
            Ok(listed) if listed.raw => {
                report.raw_found += 1;
                report.raw_bytes += std::fs::metadata(blobs.path_of(&listed.digest))
                    .map(|stored| stored.len())
                    .unwrap_or_default();
            }
            Ok(_) | Err(StoreError::BlobStray { .. }) => {}
            Err(failure) => failures.add(ask.store.display().to_string(), &failure),
        }
    }
    report.failed = failures.count;
    report.failures = failures.shown;
    Ok(report)
}

fn rewrite(
    blobs: &BlobStore,
    run: &str,
    mut report: CompressReport,
    mut failures: Failures,
) -> Result<CompressReport, BlobsError> {
    for listed in blobs.listing() {
        let listed = match listed {
            Ok(listed) => listed,
            Err(StoreError::BlobStray { .. }) => continue,
            Err(failure) => {
                failures.add(report.store.clone(), &failure);
                continue;
            }
        };
        if !listed.raw {
            continue;
        }
        report.raw_found += 1;
        match blobs.compress_raw(run, &listed.digest) {
            Ok(rewritten) => {
                report.raw_bytes += rewritten.logical_bytes;
                report.compressed += 1;
                report.resumed += u64::from(rewritten.resumed);
                report.replaced += u64::from(rewritten.replaced);
                report.compressed_stored_bytes += rewritten.stored_bytes;
                report.changed = true;
            }
            Err(failure) => failures.add(listed.digest.hex().to_owned(), &failure),
        }
    }
    report.failed = failures.count;
    report.failures = failures.shown;
    Ok(report)
}

#[derive(Debug, Clone)]
pub struct TrainAsk {
    pub store: PathBuf,
    pub at: DateTime<Utc>,
    pub samples: usize,
    pub holdout: usize,
    pub dry_run: bool,
}

/// The held-out blobs compressed at [`LEVEL`] without and with the trained dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Holdout {
    pub blobs: u64,
    pub logical_bytes: u64,
    pub without_dictionary: u64,
    pub with_dictionary: u64,
    pub ratio_without: f64,
    pub ratio_with: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrainReport {
    pub schema: &'static str,
    pub store: String,
    pub dictionary: String,
    pub dictionary_bytes: usize,
    pub level: i32,
    pub samples: u64,
    pub sample_bytes: u64,
    pub holdout: Holdout,
    /// The dictionary `put` compresses with after this run.
    pub active: Option<String>,
    pub dry_run: bool,
    pub changed: bool,
}

/// The first `samples` digests in hex order train and the next `holdout` measure: a sha256 orders
/// payloads as a random draw would, and the same store gives the same split on every run.
pub fn train(ask: &TrainAsk) -> Result<TrainReport, BlobsError> {
    for (flag, count) in [("samples", ask.samples), ("holdout", ask.holdout)] {
        if count == 0 {
            return Err(BlobsError::ZeroCount(flag));
        }
    }
    if !ask.store.join(COMMIT_LOG).is_file() {
        return Err(BlobsError::StoreAbsent(ask.store.join(COMMIT_LOG)));
    }
    let run = RunId::new(format!("blobs-dictionary-{}", stamped(ask.at)));
    let mut committer = if ask.dry_run {
        store::refuse_an_unreadable_version(&ask.store)?;
        None
    } else {
        Some(Committer::open(&ask.store, &run)?)
    };
    let reader = store::blob_store(&ask.store);
    let needed = ask.samples.saturating_add(ask.holdout);
    // One buffer, as `zstd::dict::from_continuous` takes it: `from_samples` copies every sample
    // into a second one (zstd 0.14.3, `dict.rs`), which held the samples twice.
    let mut samples: Vec<u8> = Vec::new();
    let mut sizes: Vec<usize> = Vec::new();
    let mut held_out: Vec<Listed> = Vec::new();
    for listed in reader.listing() {
        if sizes.len() + held_out.len() == needed {
            break;
        }
        let listed = match listed {
            Ok(listed) => listed,
            Err(StoreError::BlobStray { .. }) => continue,
            Err(failure) => return Err(failure.into()),
        };
        if sizes.len() < ask.samples {
            let payload = reader.read_listed(&listed)?.payload;
            let sample = payload.get(..SAMPLE_PREFIX).unwrap_or(&payload);
            samples.extend_from_slice(sample);
            sizes.push(sample.len());
        } else {
            held_out.push(listed);
        }
    }
    let found = sizes.len() + held_out.len();
    if found < needed {
        return Err(BlobsError::TooFewPayloads { needed, found });
    }
    let trained = zstd::dict::from_continuous(&samples, &sizes, DICTIONARY_BYTES)
        .map_err(|failure| BlobsError::TrainingRefused(failure.to_string()))?;
    let counts = Trained {
        samples: u64::try_from(sizes.len()).unwrap_or(u64::MAX),
        sample_bytes: u64::try_from(samples.len()).unwrap_or(u64::MAX),
    };
    drop(samples);
    let holdout = measure(&reader, &held_out, &trained)?;
    let (digest, active, changed) = match committer.as_mut() {
        None => (
            const_hex::encode(Sha256::digest(&trained)),
            store::recorded_dictionaries(&ask.store)?.0,
            false,
        ),
        Some(committer) => {
            let stored = committer.blobs().store_dictionary(run.as_str(), &trained)?;
            let changed = committer.record_dictionary(&stored, &counts, ask.at)?;
            (
                stored.hex().to_owned(),
                committer.blobs().active_dictionary(),
                changed,
            )
        }
    };
    Ok(TrainReport {
        schema: DICTIONARY_SCHEMA,
        store: ask.store.display().to_string(),
        dictionary: digest,
        dictionary_bytes: trained.len(),
        level: LEVEL,
        samples: counts.samples,
        sample_bytes: counts.sample_bytes,
        holdout,
        active,
        dry_run: ask.dry_run,
        changed,
    })
}

/// Each held-out payload read, compressed twice and dropped: one payload in memory at a time.
fn measure(
    reader: &BlobStore,
    held_out: &[Listed],
    dictionary: &[u8],
) -> Result<Holdout, BlobsError> {
    let mut plain = zstd::bulk::Compressor::new(LEVEL).map_err(StoreError::Io)?;
    let mut primed =
        zstd::bulk::Compressor::with_dictionary(LEVEL, dictionary).map_err(StoreError::Io)?;
    let (mut logical, mut without, mut with) = (0_u64, 0_u64, 0_u64);
    for listed in held_out {
        let payload = reader.read_listed(listed)?.payload;
        let size = |bytes: Vec<u8>| u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        logical += u64::try_from(payload.len()).unwrap_or(u64::MAX);
        without += size(plain.compress(&payload).map_err(StoreError::Io)?);
        with += size(primed.compress(&payload).map_err(StoreError::Io)?);
    }
    let ratio = |stored: u64| {
        if stored == 0 {
            0.0
        } else {
            logical as f64 / stored as f64
        }
    };
    Ok(Holdout {
        blobs: u64::try_from(held_out.len()).unwrap_or(u64::MAX),
        logical_bytes: logical,
        without_dictionary: without,
        with_dictionary: with,
        ratio_without: ratio(without),
        ratio_with: ratio(with),
    })
}

fn stamped(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}
