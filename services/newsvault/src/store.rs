//! The commit log, the `seq` assigner and the one door an item enters by. The blob layout is
//! architecture.md section 3.9, the partition key section 10 D3 and the ledger row section 3.2;
//! none is restated.
//!
//! An item with no closed attempt:
//!
//! ```compile_fail
//! use newsvault::store::{Committer, ItemRow};
//! # fn illegal(committer: &mut Committer, rows: &[ItemRow], at: pit::ObservationTime) {
//! let _committed = committer.record(rows, at); // E0061: an argument is missing
//! # }
//! ```
//!
//! **The positive control for the block above.** A `compile_fail` block passes whenever its
//! snippet stops compiling for *any* reason, so on its own it tests "this does not compile" and
//! not "an item needs a closed attempt". This block carries the same `use` line and the legal
//! form. Same pairing, same reason, as `pit`'s and `news-provider`'s pairs - architecture.md
//! section 8 rows R2 and R4.
//!
//! ```no_run
//! use newsvault::store::{Committer, ItemRow};
//! # use canon::digest::{item_id, story_id, ItemFields};
//! # use canon::Stamp;
//! # use chrono::DateTime;
//! # use news_provider::{AttemptSlot, Baseline, Outcome, Provenance, RawResponse, Request,
//! #                     RequestPhase, RunId, SourceId, TargetWindow, ValidSource, Verdict};
//! # use pit::datedness::Published;
//! # use pit::{ObservationTime, ValidTime};
//! # fn legal(root: std::path::PathBuf) -> Result<(), Box<dyn std::error::Error>> {
//! let run = RunId::new("2026-11-13T00:00:00Z");
//! let mut committer = Committer::open(root, &run)?;
//! let request = Request {
//!     source: SourceId::new("edgar_press"),
//!     target_window: TargetWindow::new("2026-11-10"),
//!     phase: RequestPhase::Index,
//!     provenance: Provenance::LiveIngest,
//!     url: "https://example.test/filing".to_owned(),
//!     headers: Vec::new(),
//!     params: Vec::new(),
//!     universe_version: canon::FileDigest::of(b"{}"),
//!     listed_date: None,
//!     hosts_version: None,
//! };
//! let at = ObservationTime::new(DateTime::UNIX_EPOCH);
//! let raw = committer.put_blob(b"{}")?;
//! let mut slot = AttemptSlot::open(&mut committer, &request, &run, 0, at)?;
//! let answer = RawResponse::new(slot.attempt_id().clone(), 200, b"{}".to_vec(), at, at);
//! let outcome = Outcome::decide(&Verdict::Usable, 1, &Baseline::from_trailing_ok(&[1]));
//! let closed = slot.close(&mut committer, outcome, &Ok(answer), at)?;
//! let rows = [ItemRow {
//!     item_id: item_id(&ItemFields {
//!         source: "edgar_press",
//!         source_item_id: "0000320193-26-000071",
//!         url: "https://example.test/filing",
//!         valid: Stamp::Instant(DateTime::UNIX_EPOCH),
//!         title: "A filing",
//!         body_ref: None,
//!     })
//!     .map_err(|failure| failure.to_string())?,
//!     story_id: story_id("edgar_press", "0000320193-26-000071")
//!         .map_err(|failure| failure.to_string())?,
//!     doc_id: None,
//!     valid: Published::At(ValidTime::new(DateTime::UNIX_EPOCH)),
//!     valid_raw: "19700101000000".to_owned(),
//!     valid_field: "ACCEPTANCE-DATETIME".to_owned(),
//!     valid_source: ValidSource::VendorPublished,
//!     observed: at,
//!     raw_blob: raw,
//!     body_blob: None,
//! }];
//! let _committed = committer.record(closed, &rows, at)?;
//! # Ok(())
//! # }
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use canon::{Digest, FileDigest};
use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use news_provider::{
    AttemptEvent, AttemptReceipt, Cost, LedgerError, OutcomeKind, RunId, Sink, SourceId,
    TargetWindow, ValidSource, Verdict,
};
use pit::datedness::{Datedness, Published};
use pit::{ObservationTime, Visibility};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::blobs::{BlobDigest, BlobStore};
use crate::cmd_robots::ROBOTS_SOURCE;

const LOCK_FILE: &str = "LOCK";
/// The layout, in one place because a reader and the writer may not disagree about it.
pub const COMMIT_LOG: &str = "commit.sqlite3";
pub const RUNS_DIR: &str = "runs";
pub const ITEMS_DIR: &str = "items";
/// Where a hashed collect input is kept under its own digest, so a run that dated items by a
/// measured lag keeps the bytes that stated it.
pub const POLICY_DIR: &str = "policy";
const BLOB_DIR: &str = "blobs";
const BLOB_SPACE: &str = "sha256";
/// One constant for the label `record` writes and `ok_vendor_rows` reads, so the two cannot drift.
const OUTCOME_OK: &str = "ok";

/// Per connection, so set on every open and never part of a schema version.
const CONNECTION: &str = "
PRAGMA foreign_keys = ON;
PRAGMA synchronous = FULL;
";

/// The `PRAGMA user_version` this build writes, and the only one a read-only reader accepts.
pub const SCHEMA_VERSION: i32 = 3;

/// Version `i + 1` is reached from version `i` by `MIGRATIONS[i]`, run by [`migrate`] with
/// foreign keys off, as SQLite's twelve-step table rebuild (lang_altertable.html) requires.
const MIGRATIONS: [&str; 3] = [
    "
-- The hosts file a run was admitted under: architecture.md section 2's `hosts_version` row. NULL
-- for a provider that reads no hosts file.
ALTER TABLE attempts ADD COLUMN hosts_version TEXT;
-- A CHECK cannot be altered in place, so `items` is rebuilt. The evidence columns stay nullable:
-- a row committed at user_version 0 has none, and an invented value is the empty fallback.
CREATE TABLE items_v1 (
    seq               INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id           TEXT NOT NULL,
    story_id          TEXT NOT NULL,
    doc_id            TEXT,
    attempt_id        TEXT NOT NULL REFERENCES attempts(attempt_id),
    valid_at          TEXT,
    valid_day         TEXT,
    valid_tz          TEXT,
    -- A publication MINUTE as its first microsecond, the text `item_id` digests beside the tag
    -- `minute`. Never in `valid_at`, where a reader would take it for the instant at :00.
    valid_minute      TEXT,
    valid_refused     TEXT,
    observed_at       TEXT NOT NULL,
    valid_disputed    INTEGER NOT NULL,
    earliest_valid_at TEXT,
    raw_blob          TEXT NOT NULL REFERENCES blobs(digest),
    body_blob         TEXT REFERENCES blobs(digest),
    committed_at      TEXT NOT NULL,
    valid_raw         TEXT,
    valid_field       TEXT,
    valid_source      TEXT,
    UNIQUE(item_id, attempt_id),
    CHECK ((valid_at IS NOT NULL) + (valid_day IS NOT NULL) + (valid_minute IS NOT NULL)
           + (valid_refused IS NOT NULL) = 1),
    CHECK ((valid_day IS NULL) = (valid_tz IS NULL)),
    CHECK (valid_source IN ('vendor_published', 'our_extraction'))
);
INSERT INTO items_v1
    (seq, item_id, story_id, doc_id, attempt_id, valid_at, valid_day, valid_tz, valid_refused,
     observed_at, valid_disputed, earliest_valid_at, raw_blob, body_blob, committed_at)
SELECT seq, item_id, story_id, doc_id, attempt_id, valid_at, valid_day, valid_tz, valid_refused,
       observed_at, valid_disputed, earliest_valid_at, raw_blob, body_blob, committed_at
FROM items;
DROP TABLE items;
ALTER TABLE items_v1 RENAME TO items;
CREATE INDEX items_by_story ON items(story_id);
CREATE INDEX items_by_doc ON items(doc_id);
",
    "
-- One row per answered robots.txt fetch: architecture.md section 3.1's robots table. A blob only
-- when the answer is `present`, which `RobotsAnswer` holds and no CHECK repeats.
CREATE TABLE robots (
    attempt_id TEXT PRIMARY KEY REFERENCES attempts(attempt_id),
    host       TEXT NOT NULL,
    answer     TEXT NOT NULL CHECK (answer IN ('present', 'absent')),
    blob       TEXT REFERENCES blobs(digest)
);
",
    "
-- `news_provider::Verdict::name`, which tells a permanent failure from a transient one where
-- `outcome` says `failed` for both: architecture.md section 11.1. NULL for an abandoned attempt
-- and for every row closed before user_version 3, because a backfill from `failed` is a guess.
ALTER TABLE attempts ADD COLUMN verdict TEXT
    CHECK (verdict IN ('usable', 'empty', 'permanent_failure', 'transient_failure'));
",
];

/// The frozen user_version 0 DDL; how a store reaches user_version 1: architecture.md §3.1.
///
/// `seq` is `INTEGER PRIMARY KEY AUTOINCREMENT` so the column never reuses a value after a delete,
/// and the value itself comes from a committed `seq_reservations` row: measured 2026-11-13 on
/// SQLite 3.54.0, two inserts inside one transaction took 2 and 3, and after `ROLLBACK`
/// `sqlite_sequence` was back to 1 and the next insert took 2 again.
const SCHEMA_V0: &str = "
CREATE TABLE IF NOT EXISTS attempts (
    attempt_id            TEXT PRIMARY KEY,
    run_id                TEXT NOT NULL,
    source                TEXT NOT NULL,
    target_window         TEXT NOT NULL,
    filter_params         TEXT NOT NULL,
    -- The roster the run filtered by, which decides at COLLECTION time how many of a payload's
    -- rows enter the archive: architecture.md section 2's `universe_version` row. NOT NULL,
    -- because an attempt that cannot name its roster cannot say what it looked at.
    universe_version      TEXT NOT NULL,
    opened_at             TEXT NOT NULL,
    requested_at          TEXT,
    received_at           TEXT,
    transport_status      INTEGER,
    clock_nonmonotonic    INTEGER,
    outcome               TEXT,
    vendor_rows           INTEGER,
    items_new             INTEGER,
    -- How many in-universe follow-ups an INDEX attempt named: the coverage denominator §8 row R20
    -- compares committed rows against. NULL on a document attempt and on a sweep, which is how the
    -- reader tells an index attempt apart without a phase column. Not the matched accessions
    -- themselves: those are a read over `items`, computed from the index row, never a second copy.
    matched               INTEGER,
    baseline_count        INTEGER,
    baseline_window       INTEGER,
    baseline_construction TEXT,
    baseline_ratio        REAL,
    slots                 INTEGER,
    closed_at             TEXT,
    noticed_at            TEXT,
    noticed_by            TEXT
);
CREATE TABLE IF NOT EXISTS blobs (
    digest           TEXT PRIMARY KEY,
    bytes            INTEGER NOT NULL,
    first_indexed_at TEXT NOT NULL
);
-- `item_id` is not unique and the dedup key is `story_id`: a vendor that publishes A, corrects it
-- to B and reverts to A files a third row here, where a uniqueness index over the content digest
-- files nothing on the third act and keeps B as current (read of that store's schema, 2026-11-13).
CREATE TABLE IF NOT EXISTS items (
    seq               INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id           TEXT NOT NULL,
    story_id          TEXT NOT NULL,
    doc_id            TEXT,
    attempt_id        TEXT NOT NULL REFERENCES attempts(attempt_id),
    valid_at          TEXT,
    -- A publication DATE with no hour, and the calendar it is a day in. Its own pair of columns
    -- because folding a date into a midnight instant admits the whole day before the close:
    -- architecture.md section 3.1's `valid_precision` row.
    valid_day         TEXT,
    valid_tz          TEXT,
    valid_refused     TEXT,
    observed_at       TEXT NOT NULL,
    valid_disputed    INTEGER NOT NULL,
    earliest_valid_at TEXT,
    raw_blob          TEXT NOT NULL REFERENCES blobs(digest),
    body_blob         TEXT REFERENCES blobs(digest),
    committed_at      TEXT NOT NULL,
    UNIQUE(item_id, attempt_id),
    -- `valid_refused` is the publication value an observation preceded: section 5 row 4.
    -- No reachable failure today: `publication()` returns exactly one of the three on every path,
    -- so nothing in this crate can trip either clause. They bind a later writer.
    CHECK ((valid_at IS NOT NULL) + (valid_day IS NOT NULL) + (valid_refused IS NOT NULL) = 1),
    CHECK ((valid_day IS NULL) = (valid_tz IS NULL))
);
CREATE INDEX IF NOT EXISTS items_by_story ON items(story_id);
CREATE INDEX IF NOT EXISTS items_by_doc ON items(doc_id);
CREATE TABLE IF NOT EXISTS seq_reservations (
    reservation  INTEGER PRIMARY KEY AUTOINCREMENT,
    attempt_id   TEXT NOT NULL,
    seq_lo       INTEGER NOT NULL,
    seq_hi       INTEGER NOT NULL,
    reserved_at  TEXT NOT NULL
);
-- One row per exported segment, committed only once the file is at its final name. The grain is
-- (run, publication day) and not the attempt, as the `publish` module header states.
CREATE TABLE IF NOT EXISTS published (
    segment_digest TEXT PRIMARY KEY,
    run_id         TEXT NOT NULL,
    valid_date     TEXT NOT NULL,
    path           TEXT NOT NULL,
    seq_lo         INTEGER NOT NULL,
    seq_hi         INTEGER NOT NULL,
    rows           INTEGER NOT NULL,
    published_at   TEXT NOT NULL
);
";

/// Which publication day a row is partitioned under: decision D3, taken by default
/// (architecture.md section 10). The items CHECK makes exactly one of the four columns non-null,
/// so every row has one. A minute's day is its UTC day, the calendar its value is stated in.
pub(crate) const VALID_DATE: &str =
    "substr(COALESCE(i.valid_at, i.valid_day, i.valid_minute, i.valid_refused), 1, 10)";

/// Whether a segment already carries a row: the same publication day AND inside that segment's
/// seq range. Never the range alone - one run exports several days and their ranges interleave.
/// Built from [`VALID_DATE`] rather than repeating it: review 2026-11-13 found the expression in
/// three places and changed one of them to a different key with the whole suite green.
fn covered_by_a_segment() -> String {
    format!(
        "EXISTS (SELECT 1 FROM published p
     WHERE p.valid_date = {VALID_DATE}
       AND i.seq BETWEEN p.seq_lo AND p.seq_hi)"
    )
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("another run holds the lease on {0}")]
    LeaseHeld(PathBuf),
    #[error("attempt {attempt} has no opened row in this store")]
    AttemptNotOpened { attempt: String },
    #[error(
        "attempt {attempt} was closed against another sink, so this store holds no finalised \
             event for it"
    )]
    AttemptNotClosed { attempt: String },
    /// `abandon` writes a `Finalised` event too, so an attempt the next run's sweep gave up on
    /// satisfied the door and could be handed item rows - found by review 2026-11-13. Such a row
    /// would carry archive content under an attempt nobody measured.
    #[error("attempt {attempt} was abandoned, so it may not carry items")]
    ItemsUnderAbandonedAttempt { attempt: String },
    /// An item with no publication value has no identity to take over it, which is why its
    /// producer files such a row as a `news_provider::UnusableRow`. The door refuses one rather
    /// than inventing a value for the column the schema requires.
    #[error("item {item} offers no publication value, so it cannot be stored as an item")]
    ItemHasNoPublicationValue { item: String },
    /// A `vendor_archive` item whose observation the provider could not place. The fetch instant
    /// is the wrong answer and the only other one available, so the row is refused: dating an
    /// archived item by when WE read it claims we knew it the moment it was published, which is
    /// the leak §8 row R11 exists to refuse.
    #[error("item {item} is a vendor archive row with no observation the probed lag could place")]
    ArchiveItemHasNoObservation { item: String },
    /// A `live_ingest` item's observation is the fetch instant the driver holds; one the provider
    /// placed would override it, and an earlier one is lookahead.
    #[error("item {item} is a live ingest row that placed an observation of its own")]
    LiveItemPlacedItsOwnObservation { item: String },
    /// A replay must be a no-op, which it can only be when it is the SAME claim: a second
    /// `record` of one attempt carrying a different item set is a different claim and was
    /// accepted silently, returning `changed: false` and filing nothing - found by review
    /// 2026-11-13.
    #[error("attempt {attempt} was already recorded with a different item set")]
    ReplayWithDifferentItems { attempt: String },
    #[error("no blob is stored under {digest}")]
    BlobAbsent { digest: String },
    /// `BlobStore::put` keeps a file that exists, so a refresh cannot repair this one.
    #[error("the blob stored under {digest} now hashes to {found}")]
    BlobAltered { digest: String, found: String },
    /// `record_robots`' replay rule; a robots attempt carries no items, so it is not
    /// `ReplayWithDifferentItems`. Each side is `host answer [digest]`.
    #[error("robots attempt {attempt} was recorded as {recorded}, and its replay says {replayed}")]
    RobotsReplayDiffers {
        attempt: String,
        recorded: String,
        replayed: String,
    },
    /// `canon` refuses a value holding a canonical-form separator and an instant whose seconds
    /// field is 60, and an item whose fields cannot be put in canonical form has no identity.
    #[error("item {item} has no canonical identity: {why}")]
    ItemIdentityRefused { item: String, why: String },
    /// A writer refuses only a version above its own; a read-only reader cannot migrate, so it
    /// refuses every version but its own. Code `input_schema_mismatch` (architecture.md 5.1).
    #[error("the commit log is at schema user_version {found}; this build reads {expected}")]
    SchemaVersion { found: i32, expected: i32 },
    /// SQLite's rebuild procedure checks foreign keys before it commits, because they are off
    /// while it runs; a v0 row naming an absent parent is refused rather than carried forward.
    #[error("the migration found {table} row {rowid} naming a parent row the store does not hold")]
    MigrationFoundADanglingReference { table: String, rowid: i64 },
    /// `SCHEMA_V0` is `CREATE TABLE IF NOT EXISTS`, so it cannot add a column to a v0 table an
    /// older binary wrote: `attempts` before `matched` (067a0bd) migrated to 1 without it.
    #[error(
        "the commit log at user_version {found} differs from {} in {}; no migration repairs \
         that, so the store is left at user_version {found}",
        declared_by(*.found),
        .columns.join(", ")
    )]
    ShapeDiffers { found: usize, columns: Vec<String> },
    /// `cause` is SQLite's message alone: the error rusqlite returns carries the whole batch.
    #[error(
        "migrating the commit log from user_version {from} failed {step}: {cause}; the store is \
         left at user_version {from}"
    )]
    MigrationFailed {
        from: usize,
        step: MigrationStep,
        cause: String,
    },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// What the shape of a store at `found` is compared against, as the refusal names it.
fn declared_by(found: usize) -> String {
    match found {
        0 => "SCHEMA_V0".to_owned(),
        _ => format!("SCHEMA_V0 plus MIGRATIONS[..{found}]"),
    }
}

/// Where a migration stopped: `SCHEMA_V0` is not a step to a version, so it is named on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationStep {
    CreatingTheV0Tables,
    ToUserVersion(usize),
}

impl std::fmt::Display for MigrationStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreatingTheV0Tables => f.write_str("creating the v0 tables the store lacks"),
            Self::ToUserVersion(version) => write!(f, "at the step to user_version {version}"),
        }
    }
}

/// One item as the store takes it. The extraction evidence (title, url, lang, the vendor's own
/// tagging) arrives with its producer, the provider's `extract`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemRow {
    pub item_id: Digest,
    /// The dedup key, over the vendor row's own identity: architecture.md section 2.
    pub story_id: Digest,
    /// Cross-source identity. Absent until a canonicalised url exists for the row, because the
    /// canonicalisation is a hashed rule the provider owns.
    pub doc_id: Option<Digest>,
    /// The publication value with its precision, never an instant a date was folded into:
    /// `Published::On` reaches `items.valid_day` and `items.valid_tz`, `Published::AtMinute`
    /// reaches `items.valid_minute`, and no cutoff reads either as an instant. [`Published::Unknown`] is refused by [`Committer::record`].
    pub valid: Published,
    /// The publication value as the payload spelled it, the field it came from and who stated
    /// it: `news_provider::CollectedItem`'s evidence columns, stored so a replay can re-parse it.
    pub valid_raw: String,
    pub valid_field: String,
    pub valid_source: ValidSource,
    pub observed: ObservationTime,
    pub raw_blob: BlobDigest,
    pub body_blob: Option<BlobDigest>,
}

/// What a robots.txt fetch answered (architecture.md section 11.1): a 200 is `Present` with the
/// body as received, a 404 is `Absent` and pins no digest. Any other status files no robots row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RobotsAnswer {
    Present(BlobDigest),
    Absent,
}

impl RobotsAnswer {
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Present(_) => "present",
            Self::Absent => "absent",
        }
    }

    #[must_use]
    pub const fn blob(&self) -> Option<&BlobDigest> {
        match self {
            Self::Present(digest) => Some(digest),
            Self::Absent => None,
        }
    }
}

/// What one [`Committer::record`] committed. `changed` is false for a replay, which returns the
/// standing range rather than raising, and `blobs_indexed`/`bytes` count only what this
/// transaction added to the index, so a payload already stored costs neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitReceipt {
    pub attempt_id: String,
    pub seq_lo: Option<i64>,
    pub seq_hi: Option<i64>,
    pub items_committed: u32,
    pub blobs_indexed: u32,
    pub bytes: u64,
    pub changed: bool,
}

/// One item row as a segment carries it. Every instant is the stored string and is never
/// re-rendered: `item_id` digests that exact text, so a second rendering could disagree with the
/// value it names (architecture.md section 2, canonical form).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportRow {
    pub seq: i64,
    pub item_id: String,
    pub story_id: String,
    pub doc_id: Option<String>,
    pub attempt_id: String,
    pub source: String,
    pub valid_at: Option<String>,
    pub valid_day: Option<String>,
    pub valid_tz: Option<String>,
    pub valid_minute: Option<String>,
    pub valid_refused: Option<String>,
    pub observed_at: String,
    pub valid_disputed: bool,
    pub earliest_valid_at: Option<String>,
    pub raw_blob: String,
    pub body_blob: Option<String>,
    pub committed_at: String,
    /// The partition the row belongs under, by [`VALID_DATE`].
    pub valid_date: String,
}

/// An inclusive span of `seq`. Both ends, because a span with one end is a claim about rows the
/// store may not hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct SeqRange {
    pub seq_lo: i64,
    pub seq_hi: i64,
}

/// A segment at its final name. The row this describes commits after the rename and never before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedSegment {
    pub segment_digest: String,
    pub run_id: String,
    pub valid_date: String,
    pub path: String,
    pub seq_lo: i64,
    pub seq_hi: i64,
    pub rows: u32,
}

/// Coverage per (source, target window): how many accessions an index attempt planned to cover,
/// against how many distinct stories the store holds a committed item for. Only a window an index
/// attempt recorded a `matched` count for appears, so `covered < planned` is a real gap and not a
/// window nobody read. Point-in-time, like every ledger read: a row committed after the cutoff is
/// not yet covered, and an index attempt opened after it does not count toward the denominator.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WindowCoverage {
    pub source: String,
    pub target_window: String,
    pub planned: i64,
    pub covered: i64,
}

/// The coverage rows up to `at`. `planned` is the MAX `matched` over the window's index attempts,
/// because each run's index records the same count and summing them would multiply by the run
/// number; `covered` is the distinct committed stories. §8 row R20.
pub fn coverage_up_to(db: &Connection, at: &str) -> Result<Vec<WindowCoverage>, StoreError> {
    let mut statement = db.prepare(
        "SELECT a.source, a.target_window, MAX(a.matched),
                (SELECT COUNT(DISTINCT i.story_id)
                   FROM items i JOIN attempts c ON c.attempt_id = i.attempt_id
                  WHERE c.source = a.source AND c.target_window = a.target_window
                    AND i.committed_at <= ?1 AND c.opened_at <= ?1)
         FROM attempts a
         WHERE a.matched IS NOT NULL AND a.opened_at <= ?1
         GROUP BY a.source, a.target_window
         ORDER BY a.source, a.target_window",
    )?;
    let rows = statement
        .query_map([at], |row| {
            Ok(WindowCoverage {
                source: row.get(0)?,
                target_window: row.get(1)?,
                planned: row.get(2)?,
                covered: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<WindowCoverage>, _>>()?;
    Ok(rows)
}

/// The `seq` of every committed item no segment carries, as contiguous spans. A hole in the
/// sequence is a range a transaction reserved and rolled back (see [`SCHEMA_V0`]), so the spans
/// break across it rather than claiming a row the store does not hold.
pub fn unpublished_seq_ranges(db: &Connection) -> Result<Vec<SeqRange>, StoreError> {
    let mut statement = db.prepare(&format!(
        "SELECT i.seq FROM items i WHERE NOT {} ORDER BY i.seq",
        covered_by_a_segment()
    ))?;
    let seqs = statement
        .query_map([], |row| row.get::<_, i64>(0))?
        .collect::<Result<Vec<i64>, _>>()?;
    Ok(contiguous(&seqs))
}

const _: () = assert!(MIGRATIONS.len() == SCHEMA_VERSION as usize);

fn user_version(db: &Connection) -> Result<i32, StoreError> {
    Ok(db.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

/// The check a read-only reader makes before its first query: a reader cannot migrate, so a store
/// at any other version is refused by name rather than failing on a column it lacks.
pub fn require_current_version(db: &Connection) -> Result<(), StoreError> {
    let found = user_version(db)?;
    if found == SCHEMA_VERSION {
        Ok(())
    } else {
        Err(StoreError::SchemaVersion {
            found,
            expected: SCHEMA_VERSION,
        })
    }
}

/// A writer's check before it writes anything into `root`, the lease and a run record included:
/// [`migrate`] refuses the same version and v0 shape only after `Committer::open` has taken the
/// lease. An absent commit log is a new store, which `Committer::open` creates.
pub fn refuse_an_unreadable_version(root: &Path) -> Result<(), StoreError> {
    let log = root.join(COMMIT_LOG);
    if !log.exists() {
        return Ok(());
    }
    let db = Connection::open_with_flags(&log, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let found = user_version(&db)?;
    if !(0..=SCHEMA_VERSION).contains(&found) {
        return Err(StoreError::SchemaVersion {
            found,
            expected: SCHEMA_VERSION,
        });
    }
    match usize::try_from(found) {
        Ok(older) if older < MIGRATIONS.len() => refuse_another_shape(&db, older),
        _ => Ok(()),
    }
}

/// The bytes of the blob `hex` names in the store at `root`, `None` when no such file is there.
/// Read without the commit log: the pinned robots body `collect` parses (section 11.2). Bytes
/// that no longer hash to `hex` are `BlobAltered`.
pub fn blob_bytes(root: &Path, hex: &str) -> Result<Option<Vec<u8>>, StoreError> {
    let blobs = BlobStore::new(root.join(BLOB_DIR).join(BLOB_SPACE));
    match blobs.named(hex) {
        Some(digest) => Ok(Some(blobs.read_verified(&digest)?)),
        None => Ok(None),
    }
}

/// One robots row as `StoreError::RobotsReplayDiffers` names it.
fn described(host: &str, answer: &RobotsAnswer) -> String {
    match answer.blob() {
        Some(blob) => format!("{host} {} {}", answer.name(), blob.hex()),
        None => format!("{host} {}", answer.name()),
    }
}

/// The robots facts `collect` checks before its lease (architecture.md section 11.2), read-only.
/// No commit log, and a log at a version before the `robots` table, hold no fetch.
#[derive(Debug)]
pub struct RobotsLedger {
    db: Option<Connection>,
    blobs: BlobStore,
}

impl RobotsLedger {
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        let blobs = BlobStore::new(root.join(BLOB_DIR).join(BLOB_SPACE));
        let log = root.join(COMMIT_LOG);
        if !log.exists() {
            return Ok(Self { db: None, blobs });
        }
        let db = Connection::open_with_flags(&log, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let found = user_version(&db)?;
        if found > SCHEMA_VERSION {
            return Err(StoreError::SchemaVersion {
                found,
                expected: SCHEMA_VERSION,
            });
        }
        let has_robots: bool = db.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'robots')",
            [],
            |row| row.get(0),
        )?;
        Ok(Self {
            db: has_robots.then_some(db),
            blobs,
        })
    }

    /// The newest ANSWERED fetch of `host`, by `closed_at` then `attempt_id`: a failed fetch files
    /// no robots row and is skipped. A present answer whose blob is not on disk is `BlobAbsent`.
    pub fn newest_robots(&self, host: &str) -> Result<Option<RobotsAnswer>, StoreError> {
        let Some(db) = self.db.as_ref() else {
            return Ok(None);
        };
        let row: Option<Option<String>> = db
            .query_row(
                "SELECT r.blob FROM robots r
                   JOIN attempts a ON a.attempt_id = r.attempt_id
                  WHERE a.source = ?1 AND a.target_window = ?2 AND r.host = ?2
                  ORDER BY a.closed_at DESC, a.attempt_id DESC
                  LIMIT 1",
                params![ROBOTS_SOURCE, host],
                |row| row.get(0),
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some(None) => Ok(Some(RobotsAnswer::Absent)),
            Some(Some(hex)) => match self.blobs.named(&hex) {
                Some(blob) => Ok(Some(RobotsAnswer::Present(blob))),
                None => Err(StoreError::BlobAbsent { digest: hex }),
            },
        }
    }
}

fn refuse_another_shape(db: &Connection, found: usize) -> Result<(), StoreError> {
    let columns = columns_that_differ(db, found)?;
    if columns.is_empty() {
        Ok(())
    } else {
        Err(StoreError::ShapeDiffers { found, columns })
    }
}

/// Brings a store from the version it holds to [`SCHEMA_VERSION`] in one transaction, so a crash
/// mid-migration leaves the old version whole. A version above ours is refused before any write.
fn migrate(db: &mut Connection) -> Result<(), StoreError> {
    let found = user_version(db)?;
    let Some(from) = usize::try_from(found)
        .ok()
        .filter(|from| *from <= MIGRATIONS.len())
    else {
        return Err(StoreError::SchemaVersion {
            found,
            expected: SCHEMA_VERSION,
        });
    };
    if from == MIGRATIONS.len() {
        return Ok(());
    }
    // A no-op inside a transaction (SQLite docs, PRAGMA foreign_keys), so set around it.
    db.pragma_update(None, "foreign_keys", false)?;
    let migrated = migrate_from(db, from);
    db.pragma_update(None, "foreign_keys", true)?;
    migrated
}

fn migrate_from(db: &mut Connection, from: usize) -> Result<(), StoreError> {
    let transaction = db.transaction()?;
    refuse_another_shape(&transaction, from)?;
    if from == 0 {
        transaction
            .execute_batch(SCHEMA_V0)
            .map_err(step_failed(from, MigrationStep::CreatingTheV0Tables))?;
    }
    for (index, step) in MIGRATIONS.iter().enumerate().skip(from) {
        transaction
            .execute_batch(step)
            .map_err(step_failed(from, MigrationStep::ToUserVersion(index + 1)))?;
    }
    let dangling: Option<(String, i64)> = transaction
        .query_row("PRAGMA foreign_key_check", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?;
    if let Some((table, rowid)) = dangling {
        return Err(StoreError::MigrationFoundADanglingReference { table, rowid });
    }
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn step_failed(from: usize, step: MigrationStep) -> impl FnOnce(rusqlite::Error) -> StoreError {
    move |failure| StoreError::MigrationFailed {
        from,
        step,
        cause: match failure {
            rusqlite::Error::SqlInputError { msg, .. } => msg,
            other => other.to_string(),
        },
    }
}

/// A column as `PRAGMA table_info` states it: declared type, NOT NULL, default, primary key rank.
type ColumnShape = (String, bool, Option<String>, i64);

fn columns_of(db: &Connection, table: &str) -> Result<BTreeMap<String, ColumnShape>, StoreError> {
    let mut statement =
        db.prepare("SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?1)")?;
    let columns = statement
        .query_map([table], |row| {
            Ok((
                row.get(0)?,
                (row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?),
            ))
        })?
        .collect::<Result<BTreeMap<String, ColumnShape>, _>>()?;
    Ok(columns)
}

/// Every `table.column` whose shape differs from the one a store at `found` was written with:
/// `SCHEMA_V0` replayed with `MIGRATIONS[..found]`. A table a v0 store lacks is absent here,
/// because `SCHEMA_V0` creates it whole; a table a later version lacks names each of its columns.
fn columns_that_differ(db: &Connection, found: usize) -> Result<Vec<String>, StoreError> {
    let frozen = Connection::open_in_memory()?;
    frozen.execute_batch(SCHEMA_V0)?;
    for step in MIGRATIONS.iter().take(found) {
        frozen.execute_batch(step)?;
    }
    let mut statement = frozen.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
         ORDER BY name",
    )?;
    let tables = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<String>, _>>()?;
    let mut differing = Vec::new();
    for table in tables {
        let held = columns_of(db, &table)?;
        if held.is_empty() && found == 0 {
            continue;
        }
        let declared = columns_of(&frozen, &table)?;
        let names: BTreeSet<&String> = declared.keys().chain(held.keys()).collect();
        differing.extend(
            names
                .into_iter()
                .filter(|name| declared.get(*name) != held.get(*name))
                .map(|name| format!("{table}.{name}")),
        );
    }
    Ok(differing)
}

fn contiguous(seqs: &[i64]) -> Vec<SeqRange> {
    let mut spans: Vec<SeqRange> = Vec::new();
    for &seq in seqs {
        match spans.last_mut() {
            Some(span) if span.seq_hi + 1 == seq => span.seq_hi = seq,
            _ => spans.push(SeqRange {
                seq_lo: seq,
                seq_hi: seq,
            }),
        }
    }
    spans
}

/// The single writer. Holds the lease for its whole life, because dropping the lock file releases
/// the flock.
#[derive(Debug)]
pub struct Committer {
    root: PathBuf,
    run: RunId,
    db: Connection,
    blobs: BlobStore,
    /// `Answered` and `Finalised` wait here until [`Committer::record`] opens the one transaction
    /// that commits them with the item and blob rows. `Opened` does not wait: it is durable
    /// before the socket, which is what records a hang.
    pending: Vec<AttemptEvent>,
    _lease: File,
}

impl Committer {
    /// Takes the lease and opens the commit log, creating both if they are absent.
    pub fn open(root: impl Into<PathBuf>, run: &RunId) -> Result<Self, StoreError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let lock_path = root.join(LOCK_FILE);
        let lease = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        match lease.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(StoreError::LeaseHeld(lock_path)),
            Err(TryLockError::Error(failure)) => return Err(StoreError::Io(failure)),
        }
        let mut db = Connection::open(root.join(COMMIT_LOG))?;
        db.execute_batch(CONNECTION)?;
        migrate(&mut db)?;
        Ok(Self {
            blobs: BlobStore::new(root.join(BLOB_DIR).join(BLOB_SPACE)),
            root,
            run: run.clone(),
            db,
            pending: Vec::new(),
            _lease: lease,
        })
    }

    #[must_use]
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    #[must_use]
    pub fn run(&self) -> &RunId {
        &self.run
    }

    /// Every committed item no segment carries, oldest `seq` first, with the publication day each
    /// one belongs under.
    pub fn unpublished_rows(&self) -> Result<Vec<ExportRow>, StoreError> {
        let mut statement = self.db.prepare(&format!(
            "SELECT i.seq, i.item_id, i.story_id, i.doc_id, i.attempt_id, a.source,
                    i.valid_at, i.valid_day, i.valid_tz, i.valid_minute, i.valid_refused,
                    i.observed_at, i.valid_disputed, i.earliest_valid_at, i.raw_blob,
                    i.body_blob, i.committed_at, {VALID_DATE}
             FROM items i JOIN attempts a ON a.attempt_id = i.attempt_id
             WHERE NOT {}
             ORDER BY i.seq",
            covered_by_a_segment()
        ))?;
        let rows = statement
            .query_map([], |row| {
                Ok(ExportRow {
                    seq: row.get(0)?,
                    item_id: row.get(1)?,
                    story_id: row.get(2)?,
                    doc_id: row.get(3)?,
                    attempt_id: row.get(4)?,
                    source: row.get(5)?,
                    valid_at: row.get(6)?,
                    valid_day: row.get(7)?,
                    valid_tz: row.get(8)?,
                    valid_minute: row.get(9)?,
                    valid_refused: row.get(10)?,
                    observed_at: row.get(11)?,
                    valid_disputed: row.get(12)?,
                    earliest_valid_at: row.get(13)?,
                    raw_blob: row.get(14)?,
                    body_blob: row.get(15)?,
                    committed_at: row.get(16)?,
                    valid_date: row.get(17)?,
                })
            })?
            .collect::<Result<Vec<ExportRow>, _>>()?;
        Ok(rows)
    }

    pub fn unpublished_seq_ranges(&self) -> Result<Vec<SeqRange>, StoreError> {
        unpublished_seq_ranges(&self.db)
    }

    /// Files a segment that is already at its final name. Called after the rename and never
    /// before: a row about a file that is not there is accepted attack A2.6's own failure.
    pub fn record_published(
        &mut self,
        segment: &PublishedSegment,
        at: ObservationTime,
    ) -> Result<(), StoreError> {
        self.db.execute(
            "INSERT INTO published
                 (segment_digest, run_id, valid_date, path, seq_lo, seq_hi, rows, published_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                segment.segment_digest,
                segment.run_id,
                segment.valid_date,
                segment.path,
                segment.seq_lo,
                segment.seq_hi,
                segment.rows,
                stamp(at.inner())
            ],
        )?;
        Ok(())
    }

    /// Stores a payload under its own digest, before any transaction opens.
    pub fn put_blob(&self, payload: &[u8]) -> Result<BlobDigest, StoreError> {
        self.blobs.put(self.run.as_str(), payload)
    }

    /// A payload this store already holds, named by its hex.
    pub fn blob_named(&self, hex: &str) -> Result<BlobDigest, StoreError> {
        self.blobs.named(hex).ok_or_else(|| StoreError::BlobAbsent {
            digest: hex.to_owned(),
        })
    }

    /// The counts `news_provider::Outcome::decide` is asked against, oldest first. How many of
    /// them make a baseline is the caller's: `Baseline::from_trailing_ok` states the attempt
    /// count it used, and no floor fires in this slice (architecture.md section 3.2, `collapsed`).
    pub fn ok_vendor_rows(&self, source: &SourceId) -> Result<Vec<u32>, StoreError> {
        let mut statement = self.db.prepare(
            "SELECT vendor_rows FROM attempts
             WHERE source = ?1 AND outcome = ?2 AND vendor_rows IS NOT NULL
             ORDER BY closed_at, attempt_id",
        )?;
        let counts = statement
            .query_map(params![source.as_str(), OUTCOME_OK], |row| row.get(0))?
            .collect::<Result<Vec<u32>, _>>()?;
        Ok(counts)
    }

    /// The story ids this store already holds a COMMITTED item for, under one (source, target
    /// window). "Covered" is a committed item and never an attempt that merely returned ok: an ok
    /// attempt that committed nothing is a gap the next run must still close, and keying this on
    /// the attempt outcome would hide it. The driver hands this set to the provider so a
    /// `vendor_archive` re-run fetches only the uncovered accessions (§8 row R20). No cutoff: the
    /// driver holds the write lease and plans against everything the store already holds.
    pub fn covered_story_ids(
        &self,
        source: &SourceId,
        target: &TargetWindow,
    ) -> Result<BTreeSet<String>, StoreError> {
        let mut statement = self.db.prepare(
            "SELECT DISTINCT i.story_id FROM items i
             JOIN attempts a ON a.attempt_id = i.attempt_id
             WHERE a.source = ?1 AND a.target_window = ?2",
        )?;
        let covered = statement
            .query_map(params![source.as_str(), target.as_str()], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<BTreeSet<String>, _>>()?;
        Ok(covered)
    }

    /// Records how many in-universe follow-ups an index attempt named. One count per index
    /// attempt, stable across runs, so the coverage reader takes the MAX. A plain annotation
    /// column and not an item-row invariant, so it is its own `UPDATE` rather than part of the
    /// one commit transaction.
    pub fn record_planned_coverage(
        &mut self,
        attempt_id: &str,
        matched: u32,
    ) -> Result<(), StoreError> {
        self.db.execute(
            "UPDATE attempts SET matched = ?2 WHERE attempt_id = ?1",
            params![attempt_id, matched],
        )?;
        Ok(())
    }

    /// Every attempt this store opened and never finalised, oldest first. The startup sweep's
    /// input: without it an attempt killed between `open` and `close` is in flight forever with a
    /// growing age (architecture.md section 3.2's `Abandoned` row).
    pub fn attempts_in_flight(&self) -> Result<Vec<String>, StoreError> {
        let mut statement = self.db.prepare(
            "SELECT attempt_id FROM attempts WHERE closed_at IS NULL ORDER BY opened_at,
                 attempt_id",
        )?;
        let open = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(open)
    }

    /// The ONLY way an item enters the archive. It takes the closed attempt, so there is no
    /// signature that stores a row without storing how it got there, and no `add_items`.
    pub fn record(
        &mut self,
        closed: AttemptReceipt,
        rows: &[ItemRow],
        committed_at: ObservationTime,
    ) -> Result<CommitReceipt, StoreError> {
        let attempt = closed.attempt_id().hex().to_owned();
        if let Some(standing) = self.standing(&attempt)? {
            if !self.holds_exactly(&attempt, rows)? {
                return Err(StoreError::ReplayWithDifferentItems { attempt });
            }
            self.pending.retain(|event| attempt_of(event) != attempt);
            return Ok(standing);
        }
        self.admits(&attempt, &closed, !rows.is_empty())?;
        let lengths = self.lengths_of(referenced_blobs(rows))?;
        let reserved = self.reserve(&attempt, rows.len(), committed_at)?;
        let events = self.pending_of(&attempt);
        let stamped = stamp(committed_at.inner());
        let transaction = self.db.transaction()?;
        for event in &events {
            apply(&transaction, event)?;
        }
        let (blobs_indexed, bytes) = index_blobs(&transaction, &lengths, &stamped)?;
        let mut items_new: i64 = 0;
        if let Some((seq_lo, _)) = reserved {
            for (offset, row) in rows.iter().enumerate() {
                let story = row.story_id.hex();
                let observed_at = stamp(row.observed.inner());
                let published = publication(row)?;
                let (disputed, earliest) = dispute(&transaction, story, &published)?;
                if wins(first_observed(&transaction, story)?.as_ref(), &observed_at) {
                    items_new += 1;
                }
                transaction.execute(
                    "INSERT INTO items
                         (seq, item_id, story_id, doc_id, attempt_id, valid_at, valid_day,
                          valid_tz, valid_refused, observed_at, valid_disputed, earliest_valid_at,
                          raw_blob, body_blob, committed_at, valid_raw, valid_field, valid_source,
                          valid_minute)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                             ?16, ?17, ?18, ?19)",
                    params![
                        seq_lo + i64::try_from(offset).unwrap_or(0),
                        row.item_id.hex(),
                        story,
                        row.doc_id.as_ref().map(Digest::hex),
                        attempt,
                        published.instant,
                        published.day,
                        published.zone,
                        published.refused,
                        observed_at,
                        disputed,
                        earliest,
                        row.raw_blob.hex(),
                        row.body_blob.as_ref().map(BlobDigest::hex),
                        stamped,
                        row.valid_raw,
                        row.valid_field,
                        row.valid_source.name(),
                        published.minute
                    ],
                )?;
            }
        }
        transaction.execute(
            "UPDATE attempts SET items_new = ?2 WHERE attempt_id = ?1",
            params![attempt, items_new],
        )?;
        transaction.commit()?;
        self.pending.retain(|event| attempt_of(event) != attempt);
        Ok(CommitReceipt {
            attempt_id: attempt,
            seq_lo: reserved.map(|(lo, _)| lo),
            seq_hi: reserved.map(|(_, hi)| hi),
            items_committed: u32::try_from(rows.len()).unwrap_or(u32::MAX),
            blobs_indexed,
            bytes,
            changed: true,
        })
    }

    /// The door for a robots.txt answer: `record`'s checks, then the attempt's pending events, the
    /// blob's index row and the `robots` row in one transaction (architecture.md section 3.1).
    pub fn record_robots(
        &mut self,
        closed: AttemptReceipt,
        host: &str,
        answer: &RobotsAnswer,
        committed_at: ObservationTime,
    ) -> Result<CommitReceipt, StoreError> {
        let attempt = closed.attempt_id().hex().to_owned();
        if let Some(standing) = self.standing(&attempt)? {
            let recorded = self.robots_row(&attempt)?;
            let replayed = (host.to_owned(), answer.clone());
            if recorded.as_ref() != Some(&replayed) {
                return Err(StoreError::RobotsReplayDiffers {
                    attempt,
                    recorded: recorded.as_ref().map_or_else(
                        || "no robots row".to_owned(),
                        |(host, answer)| described(host, answer),
                    ),
                    replayed: described(&replayed.0, &replayed.1),
                });
            }
            self.pending.retain(|event| attempt_of(event) != attempt);
            return Ok(standing);
        }
        self.admits(&attempt, &closed, true)?;
        let lengths = self.lengths_of(answer.blob().into_iter().cloned().collect())?;
        let events = self.pending_of(&attempt);
        let stamped = stamp(committed_at.inner());
        let transaction = self.db.transaction()?;
        for event in &events {
            apply(&transaction, event)?;
        }
        let (blobs_indexed, bytes) = index_blobs(&transaction, &lengths, &stamped)?;
        transaction.execute(
            "INSERT INTO robots (attempt_id, host, answer, blob) VALUES (?1, ?2, ?3, ?4)",
            params![
                attempt,
                host,
                answer.name(),
                answer.blob().map(BlobDigest::hex)
            ],
        )?;
        transaction.commit()?;
        self.pending.retain(|event| attempt_of(event) != attempt);
        Ok(CommitReceipt {
            attempt_id: attempt,
            seq_lo: None,
            seq_hi: None,
            items_committed: 0,
            blobs_indexed,
            bytes,
            changed: true,
        })
    }

    /// The robots row an attempt filed, as `record_robots` was handed it.
    fn robots_row(&self, attempt: &str) -> Result<Option<(String, RobotsAnswer)>, StoreError> {
        let row: Option<(String, Option<String>)> = self
            .db
            .query_row(
                "SELECT host, blob FROM robots WHERE attempt_id = ?1",
                params![attempt],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((host, Some(hex))) => Some((host, RobotsAnswer::Present(self.blob_named(&hex)?))),
            Some((host, None)) => Some((host, RobotsAnswer::Absent)),
            None => None,
        })
    }

    /// The checks every door makes: the attempt was opened here, closed here, and an abandoned
    /// one carries nothing.
    fn admits(
        &self,
        attempt: &str,
        closed: &AttemptReceipt,
        carries: bool,
    ) -> Result<(), StoreError> {
        let attempt = attempt.to_owned();
        if !self.was_opened(&attempt)? {
            return Err(StoreError::AttemptNotOpened { attempt });
        }
        if !self.pending.iter().any(|event| closes(event, &attempt)) {
            return Err(StoreError::AttemptNotClosed { attempt });
        }
        if carries && closed.outcome().kind() == OutcomeKind::Interrupted {
            return Err(StoreError::ItemsUnderAbandonedAttempt { attempt });
        }
        Ok(())
    }

    fn lengths_of(&self, digests: Vec<BlobDigest>) -> Result<Vec<(BlobDigest, u64)>, StoreError> {
        let mut lengths = Vec::new();
        for digest in digests {
            let bytes = self.blobs.len_of(&digest)?;
            lengths.push((digest, bytes));
        }
        Ok(lengths)
    }

    fn pending_of(&self, attempt: &str) -> Vec<AttemptEvent> {
        self.pending
            .iter()
            .filter(|event| attempt_of(event) == attempt)
            .cloned()
            .collect()
    }

    /// The standing receipt of an attempt already closed in the log, so a replay is a no-op. A
    /// routine that cannot be run twice is a routine nobody can safely retry: the predecessor's
    /// pre-open feed died mid-file on 2026-09-10 on a UNIQUE violation because it had already run
    /// once that morning.
    fn standing(&self, attempt: &str) -> Result<Option<CommitReceipt>, StoreError> {
        let closed: Option<String> = self
            .db
            .query_row(
                "SELECT closed_at FROM attempts WHERE attempt_id = ?1 AND closed_at IS NOT NULL",
                params![attempt],
                |row| row.get(0),
            )
            .optional()?;
        if closed.is_none() {
            return Ok(None);
        }
        let (seq_lo, seq_hi, items): (Option<i64>, Option<i64>, i64) = self.db.query_row(
            "SELECT MIN(seq), MAX(seq), COUNT(*) FROM items WHERE attempt_id = ?1",
            params![attempt],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok(Some(CommitReceipt {
            attempt_id: attempt.to_owned(),
            seq_lo,
            seq_hi,
            items_committed: u32::try_from(items).unwrap_or(u32::MAX),
            blobs_indexed: 0,
            bytes: 0,
            changed: false,
        }))
    }

    /// Whether the `Opened` event for this attempt reached this store. Without it a receipt from
    /// another store commits nothing and reports success.
    /// Whether the rows already filed under `attempt` are exactly the ones offered again. The
    /// `item_id` set, not the count: two sets of one row each are not the same claim.
    fn holds_exactly(&self, attempt: &str, rows: &[ItemRow]) -> Result<bool, StoreError> {
        let mut statement = self
            .db
            .prepare("SELECT item_id FROM items WHERE attempt_id = ?1")?;
        let filed = statement
            .query_map(params![attempt], |row| row.get::<_, String>(0))?
            .collect::<Result<std::collections::BTreeSet<String>, _>>()?;
        let offered: std::collections::BTreeSet<String> = rows
            .iter()
            .map(|row| row.item_id.hex().to_owned())
            .collect();
        Ok(filed == offered)
    }

    fn was_opened(&self, attempt: &str) -> Result<bool, StoreError> {
        let opened: Option<String> = self
            .db
            .query_row(
                "SELECT opened_at FROM attempts WHERE attempt_id = ?1",
                params![attempt],
                |row| row.get(0),
            )
            .optional()?;
        Ok(opened.is_some())
    }

    /// Claims a `seq` range. Its own committed transaction, because a range the item transaction
    /// rolls back is otherwise handed out again (see [`SCHEMA_V0`]).
    fn reserve(
        &mut self,
        attempt: &str,
        count: usize,
        at: ObservationTime,
    ) -> Result<Option<(i64, i64)>, StoreError> {
        if count == 0 {
            return Ok(None);
        }
        let span = i64::try_from(count).unwrap_or(i64::MAX);
        let transaction = self.db.transaction()?;
        let high: i64 = transaction.query_row(
            "SELECT MAX((SELECT COALESCE(MAX(seq_hi), 0) FROM seq_reservations),
                        (SELECT COALESCE(MAX(seq), 0) FROM items))",
            [],
            |row| row.get(0),
        )?;
        let (seq_lo, seq_hi) = (high + 1, high + span);
        transaction.execute(
            "INSERT INTO seq_reservations (attempt_id, seq_lo, seq_hi, reserved_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![attempt, seq_lo, seq_hi, stamp(at.inner())],
        )?;
        transaction.commit()?;
        Ok(Some((seq_lo, seq_hi)))
    }

    fn commit_opened(&self, event: &AttemptEvent) -> Result<(), StoreError> {
        if let AttemptEvent::Opened {
            attempt_id,
            run,
            source,
            target_window,
            filter_params,
            universe_version,
            hosts_version,
            opened_at,
        } = event
        {
            self.db.execute(
                "INSERT INTO attempts
                     (attempt_id, run_id, source, target_window, filter_params, universe_version,
                      opened_at, hosts_version)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(attempt_id) DO NOTHING",
                params![
                    attempt_id.hex(),
                    run.as_str(),
                    source.as_str(),
                    target_window.as_str(),
                    filter_params.hex(),
                    universe_version.hex(),
                    stamp(opened_at.inner()),
                    hosts_version.as_ref().map(FileDigest::hex)
                ],
            )?;
        }
        Ok(())
    }
}

impl Sink for Committer {
    /// `Opened` commits here, under the `synchronous = FULL` of [`CONNECTION`], so it is on disk
    /// before the socket; the other three wait for [`Committer::record`]'s one transaction.
    fn write(&mut self, event: &AttemptEvent) -> Result<(), LedgerError> {
        match event {
            AttemptEvent::Opened { .. } => {
                self.commit_opened(event)
                    .map_err(|failure| LedgerError::WriteFailed {
                        event: event.name(),
                        detail: failure.to_string(),
                    })
            }
            AttemptEvent::Answered { .. }
            | AttemptEvent::Finalised { .. }
            | AttemptEvent::Abandoned { .. } => {
                self.pending.push(event.clone());
                Ok(())
            }
        }
    }
}

/// Index rows for blobs already on disk; counts only the rows this transaction added.
fn index_blobs(
    db: &Connection,
    lengths: &[(BlobDigest, u64)],
    stamped: &str,
) -> Result<(u32, u64), StoreError> {
    let mut indexed = 0;
    let mut bytes = 0;
    for (digest, length) in lengths {
        let inserted = db.execute(
            "INSERT INTO blobs (digest, bytes, first_indexed_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(digest) DO NOTHING",
            params![
                digest.hex(),
                i64::try_from(*length).unwrap_or(i64::MAX),
                stamped
            ],
        )?;
        if inserted == 1 {
            indexed += 1;
            bytes += length;
        }
    }
    Ok((indexed, bytes))
}

/// Which attempt an event is about.
fn attempt_of(event: &AttemptEvent) -> &str {
    match event {
        AttemptEvent::Opened { attempt_id, .. }
        | AttemptEvent::Answered { attempt_id, .. }
        | AttemptEvent::Finalised { attempt_id, .. }
        | AttemptEvent::Abandoned { attempt_id, .. } => attempt_id.hex(),
    }
}

/// The distinct blob digests a row set references, raw and body alike.
fn referenced_blobs(rows: &[ItemRow]) -> Vec<BlobDigest> {
    let mut digests: Vec<BlobDigest> = rows
        .iter()
        .flat_map(|row| std::iter::once(row.raw_blob.clone()).chain(row.body_blob.iter().cloned()))
        .collect();
    digests.sort();
    digests.dedup();
    digests
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Whether an event closes `attempt`. `record` commits the buffered events, so without this a
/// receipt closed against another `Sink` filed its rows under an attempt still in flight, and the
/// row was byte-identical to a crash between `open` and `close` - review 2026-11-13, 3 item rows.
fn closes(event: &AttemptEvent, attempt: &str) -> bool {
    attempt_of(event) == attempt && matches!(event, AttemptEvent::Finalised { .. })
}

/// Which visibility rule a row's publication value puts it under. [`Datedness::Undated`] is
/// reached by an observation that precedes its publication, which is stored with its evidence
/// rather than counted as unusable: architecture.md section 5 row 4.
fn datedness(row: &ItemRow) -> Datedness {
    match row.valid {
        Published::At(valid) => match Visibility::from_live(valid, row.observed) {
            Ok(window) => Datedness::Dated(window),
            Err(_) => Datedness::Undated {
                observed: row.observed,
            },
        },
        Published::AtMinute(minute) => Datedness::of_minute(minute, row.observed),
        Published::On { day, tz } => Datedness::DayDated {
            day,
            tz,
            observed: row.observed,
        },
        Published::Unknown => Datedness::Undated {
            observed: row.observed,
        },
    }
}

/// The publication value as its columns. Exactly one of the four carries it, which is what the
/// schema's own CHECK says.
#[derive(Default)]
struct Publication {
    instant: Option<String>,
    day: Option<String>,
    zone: Option<&'static str>,
    minute: Option<String>,
    refused: Option<String>,
}

impl Publication {
    fn instant(at: String) -> Self {
        Self {
            instant: Some(at),
            ..Self::default()
        }
    }

    fn minute(starts: String) -> Self {
        Self {
            minute: Some(starts),
            ..Self::default()
        }
    }

    fn day(day: NaiveDate, zone: &'static str) -> Self {
        Self {
            day: Some(day.to_string()),
            zone: Some(zone),
            ..Self::default()
        }
    }

    fn refused(at: String) -> Self {
        Self {
            refused: Some(at),
            ..Self::default()
        }
    }
}

/// A date reaches `valid_day` and `valid_tz`, an instant reaches `valid_at`, a minute reaches
/// `valid_minute`, and a value its own observation preceded reaches `valid_refused`. An item carrying no publication value at all
/// is refused rather than given an invented one.
fn publication(row: &ItemRow) -> Result<Publication, StoreError> {
    match datedness(row) {
        Datedness::Dated(window) => Ok(Publication::instant(stamp(window.valid().inner()))),
        Datedness::MinuteDated { minute, .. } => Ok(Publication::minute(stamp(minute.start()))),
        Datedness::DayDated { day, tz, .. } => Ok(Publication::day(day, tz)),
        // `lower_bound` is `None` for exactly one publication value, the unknown one, so this is
        // where a row with nothing to store in any of the three columns is turned away.
        Datedness::Undated { .. } => match row.valid.lower_bound() {
            Some(refused) => Ok(Publication::refused(stamp(refused))),
            None => Err(StoreError::ItemHasNoPublicationValue {
                item: row.item_id.hex().to_owned(),
            }),
        },
    }
}

/// The kept sighting's observation: first sighting wins, keyed on `story_id` and never on the
/// content digest. No row is updated, so the kept value is a query and not a column.
fn first_observed(db: &Connection, story: &str) -> Result<Option<String>, StoreError> {
    Ok(db.query_row(
        "SELECT MIN(observed_at) FROM items WHERE story_id = ?1",
        params![story],
        |row| row.get(0),
    )?)
}

fn wins(kept: Option<&String>, observed_at: &str) -> bool {
    match kept {
        Some(kept) => observed_at < kept.as_str(),
        None => true,
    }
}

/// `valid_disputed` and `earliest_valid_at` for a row about to be inserted, against the twins the
/// store already holds under its `story_id` IN THE SAME COLUMN, so `earliest_valid_at` has the
/// arriving row's precision. Set on the ARRIVING row, which leaves the earlier twin byte-identical.
fn dispute(
    db: &Connection,
    story: &str,
    published: &Publication,
) -> Result<(bool, Option<String>), StoreError> {
    let (column, valid_at) = match (&published.instant, &published.minute) {
        (Some(instant), _) => ("valid_at", instant),
        (None, Some(minute)) => ("valid_minute", minute),
        (None, None) => return Ok((false, None)),
    };
    let (low, high): (Option<String>, Option<String>) = db.query_row(
        &format!(
            "SELECT MIN({column}), MAX({column}) FROM items
             WHERE story_id = ?1 AND {column} IS NOT NULL"
        ),
        params![story],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let disputed = [low.as_deref(), high.as_deref()]
        .into_iter()
        .flatten()
        .any(|stored| stored != valid_at.as_str());
    let earliest = disputed.then(|| match low {
        Some(low) if low.as_str() < valid_at.as_str() => low,
        _ => valid_at.clone(),
    });
    Ok((disputed, earliest))
}

/// The newest publication value visible at `at` (RFC 3339 UTC, microseconds), for the ledger's
/// publication clock. A minute counts from the start of the NEXT minute, as `pit`'s minute arm
/// counts it, and is reported as the stored minute, never as an instant inside it.
pub fn newest_visible_publication(db: &Connection, at: &str) -> Result<Option<String>, StoreError> {
    Ok(db.query_row(
        "SELECT MAX(value) FROM (
             SELECT valid_at AS value FROM items WHERE valid_at <= ?1 AND observed_at <= ?1
             UNION ALL
             SELECT valid_minute FROM items
              WHERE strftime('%Y-%m-%dT%H:%M:%S.000000Z', valid_minute, '+1 minute') <= ?1
                AND observed_at <= ?1)",
        [at],
        |row| row.get(0),
    )?)
}

fn outcome_name(kind: OutcomeKind) -> &'static str {
    match kind {
        OutcomeKind::Ok => OUTCOME_OK,
        OutcomeKind::Empty => "empty",
        OutcomeKind::Failed => "failed",
        OutcomeKind::Interrupted => "interrupted",
    }
}

/// One event onto the attempt's row. `Opened` is already committed when this runs.
fn apply(db: &Connection, event: &AttemptEvent) -> Result<(), StoreError> {
    match event {
        AttemptEvent::Opened { .. } => Ok(()),
        AttemptEvent::Answered {
            attempt_id,
            requested_at,
            received_at,
            transport_status,
            clock_nonmonotonic,
        } => {
            db.execute(
                "UPDATE attempts SET requested_at = ?2, received_at = ?3, transport_status = ?4,
                     clock_nonmonotonic = ?5
                 WHERE attempt_id = ?1",
                params![
                    attempt_id.hex(),
                    stamp(requested_at.inner()),
                    stamp(received_at.inner()),
                    transport_status,
                    clock_nonmonotonic
                ],
            )?;
            Ok(())
        }
        AttemptEvent::Finalised {
            attempt_id,
            outcome,
            cost,
            closed_at,
        } => {
            let counts = outcome.counts();
            let baseline = outcome.baseline();
            db.execute(
                "UPDATE attempts SET outcome = ?2, vendor_rows = ?3,
                     baseline_count = ?4, baseline_window = ?5, baseline_construction = ?6,
                     baseline_ratio = ?7, slots = ?8, closed_at = ?9, verdict = ?10
                 WHERE attempt_id = ?1",
                params![
                    attempt_id.hex(),
                    outcome_name(outcome.kind()),
                    counts.map(|counts| counts.vendor_rows),
                    baseline.map(|baseline| baseline.count),
                    baseline.map(|baseline| baseline.window),
                    baseline.map(|baseline| baseline.construction),
                    baseline.and_then(|baseline| baseline.ratio),
                    cost.map(|Cost::Slots(slots)| slots),
                    stamp(closed_at.inner()),
                    outcome.verdict().map(Verdict::name)
                ],
            )?;
            Ok(())
        }
        AttemptEvent::Abandoned {
            attempt_id,
            noticed_at,
            by_run,
        } => {
            db.execute(
                "UPDATE attempts SET noticed_at = ?2, noticed_by = ?3 WHERE attempt_id = ?1",
                params![attempt_id.hex(), stamp(noticed_at.inner()), by_run.as_str()],
            )?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt as _;

    use canon::digest::{doc_id, item_id, story_id, ItemFields};
    use canon::{FileDigest, Stamp};
    use chrono::{DateTime, TimeDelta, Utc};
    use news_provider::{
        abandon, AttemptEvent, AttemptSlot, Baseline, Outcome, Provenance, RawResponse, Request,
        RequestPhase, SourceId, TargetWindow, ValidSource, Verdict,
    };
    use pit::datedness::{Minute, Published};
    use pit::ValidTime;
    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::{
        first_observed, fs, newest_visible_publication, params, stamp, user_version, Committer,
        ItemRow, LedgerError, MigrationStep, ObservationTime, RobotsAnswer, RunId, Sink,
        StoreError, COMMIT_LOG, MIGRATIONS, SCHEMA_V0, SCHEMA_VERSION,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const RUN: &str = "2026-11-13T07:00:00Z";
    const SWEEPING_RUN: &str = "2026-11-13T08:00:00Z";
    const SOURCE: &str = "edgar_press";
    const OTHER_SOURCE: &str = "polygon";
    const WINDOW: &str = "2026-11-10";
    const URL: &str = "https://example.test/filing";
    const TITLE: &str = "A filing";
    const CORRECTED_TITLE: &str = "A filing, corrected";
    const STORY: &str = "0000320193-26-000071";
    const PAYLOAD: &[u8] = b"{\"filings\": 1}";
    /// The roster bytes every test attempt names. One value, because these tests are about the
    /// store and `universe_version` only has to be present and stable.
    const ROSTER: &[u8] = b"{\"320193\": \"AAPL\"}";
    const HOSTS: &[u8] = b"host,status\nrss.globenewswire.com,admitted\n";
    /// The acceptance field as an EDGAR filing header spells it, and the field's own name.
    const VALID_RAW: &str = "19700101000140";
    const VALID_FIELD: &str = "ACCEPTANCE-DATETIME";
    const UNKNOWN_HIGHER_VERSION: i32 = SCHEMA_VERSION + 1;
    const CORRECTED_PAYLOAD: &[u8] = b"{\"filings\": 2}";
    const SENTINEL: &[u8] = b"a staged file a crash left behind";
    const STATUS_OK: u16 = 200;
    /// Distinct values, so a swap of the two columns is visible. 59 is the day the collapse
    /// outcome exists for (`news_provider::ledger`'s `TRAILING_MEDIAN` comment).
    const VENDOR_ROWS: u32 = 59;
    /// A permanent failure brought nothing back, so the two verdicts carry different counts and a
    /// swap of their outcome labels is visible in what the baseline reader returns.
    const FAILED_VENDOR_ROWS: u32 = 0;
    const TRAILING_OK: [u32; 3] = [181, 45, 11];
    const ITEM_COUNTS: [usize; 4] = [0, 1, 2, 17];

    const SECOND: i64 = 1_000_000;
    /// Three different vendor row counts, in the order the three attempts close.
    const FIRST_COUNT: u32 = 7;
    const SECOND_COUNT: u32 = 11;
    const THIRD_COUNT: u32 = 23;
    const VALID_AT: i64 = 100 * SECOND;
    const RE_STAMPED_AT: i64 = 160 * SECOND;
    const OBSERVED_AT: i64 = 200 * SECOND;
    const OBSERVED_EARLIER: i64 = 150 * SECOND;
    const OBSERVED_LATER: i64 = 300 * SECOND;
    const OBSERVED_LAST: i64 = 400 * SECOND;
    const OBSERVED_BEFORE_PUBLICATION: i64 = 50 * SECOND;
    /// Not a whole second, so coarsening the stored stamp is visible.
    const VALID_AT_PAST_THE_SECOND: i64 = VALID_AT + 123_456;
    /// A date with no hour, which is what a filing carrying no acceptance instant publishes at.
    const A_PUBLICATION_DAY: &str = "2026-11-10";
    /// 00:02 and 00:03 UTC on 1970-01-01, whole minutes on the same clock as `VALID_AT`.
    const A_PUBLICATION_MINUTE: i64 = 120 * SECOND;
    const A_LATER_PUBLICATION_MINUTE: i64 = 180 * SECOND;
    /// Inside `A_PUBLICATION_MINUTE`, its last microsecond, and the instant it closes.
    const INSIDE_THAT_MINUTE: &str = "1970-01-01T00:02:30.000000Z";
    const ONE_MICROSECOND_BEFORE_THAT_MINUTE_CLOSES: &str = "1970-01-01T00:02:59.999999Z";
    const THAT_MINUTE_CLOSES: &str = "1970-01-01T00:03:00.000000Z";
    const INSERTED_BY_HAND: &str = "INSERT INTO items (item_id, story_id, attempt_id, valid_at,
         valid_minute, observed_at, valid_disputed, raw_blob, committed_at, valid_source)
         SELECT 'by hand', 'by hand', attempt_id, ?1, ?2, ?3, 0, raw_blob, ?3, ?4 FROM items";
    const NEW_YORK: &str = "America/New_York";

    const DOC_IMPORT: &str = "use newsvault::store::{Committer, ItemRow};";
    const COMPILE_FAIL: &str = "compile_fail";
    const NO_RUN: &str = "no_run";
    const RECORD_WITHOUT_AN_ATTEMPT: &str = "committer.record(rows, at)";
    const RECORD_WITH_AN_ATTEMPT: &str = "committer.record(closed, &rows, at)";
    const ILLEGAL_BLOCK: usize = 0;
    const CONTROL_BLOCK: usize = 1;

    /// The header's fenced blocks, in order. The fence is assembled rather than written out, so
    /// this function's own text does not split the file it reads.
    fn doc_blocks() -> Vec<String> {
        let fence = format!("//! {}", "```");
        include_str!("store.rs")
            .split(&fence)
            .skip(1)
            .step_by(2)
            .map(str::to_owned)
            .collect()
    }

    fn block(index: usize) -> String {
        doc_blocks().get(index).cloned().unwrap_or_default()
    }

    fn annotation(block: &str) -> &str {
        block.lines().next().unwrap_or_default()
    }

    fn at(seconds: i64) -> ObservationTime {
        ObservationTime::new(DateTime::UNIX_EPOCH + TimeDelta::seconds(seconds))
    }

    fn instant(micros: i64) -> DateTime<Utc> {
        DateTime::UNIX_EPOCH + TimeDelta::microseconds(micros)
    }

    fn stamped(micros: i64) -> String {
        stamp(instant(micros))
    }

    fn request_from(source: &str) -> Request {
        Request {
            source: SourceId::new(source),
            target_window: TargetWindow::new(WINDOW),
            phase: RequestPhase::Index,
            provenance: Provenance::LiveIngest,
            url: URL.to_owned(),
            headers: Vec::new(),
            params: Vec::new(),
            universe_version: FileDigest::of(ROSTER),
            listed_date: None,
            hosts_version: None,
        }
    }

    fn request() -> Request {
        request_from(SOURCE)
    }

    /// One vendor row as a fixture states it. `source_item_id` fixes the story and `title` and
    /// `valid` fix the content, so a correction is a new `item_id` under one `story_id`.
    #[derive(Debug, Clone, Copy)]
    struct Spec<'a> {
        source: &'a str,
        source_item_id: &'a str,
        title: &'a str,
        valid: i64,
        observed: i64,
        payload: &'a [u8],
    }

    fn spec(source_item_id: &'static str) -> Spec<'static> {
        Spec {
            source: SOURCE,
            source_item_id,
            title: TITLE,
            valid: VALID_AT,
            observed: OBSERVED_AT,
            payload: PAYLOAD,
        }
    }

    fn row(store: &Committer, spec: &Spec) -> Result<ItemRow, Box<dyn std::error::Error>> {
        Ok(ItemRow {
            item_id: item_id(&ItemFields {
                source: spec.source,
                source_item_id: spec.source_item_id,
                url: URL,
                valid: Stamp::Instant(instant(spec.valid)),
                title: spec.title,
                body_ref: None,
            })
            .map_err(|failure| failure.to_string())?,
            story_id: story_id(spec.source, spec.source_item_id)
                .map_err(|failure| failure.to_string())?,
            doc_id: Some(doc_id(URL).map_err(|failure| failure.to_string())?),
            valid: Published::At(ValidTime::new(instant(spec.valid))),
            valid_raw: VALID_RAW.to_owned(),
            valid_field: VALID_FIELD.to_owned(),
            valid_source: ValidSource::VendorPublished,
            observed: ObservationTime::new(instant(spec.observed)),
            raw_blob: store.put_blob(spec.payload)?,
            body_blob: None,
        })
    }

    /// [`row`] restated as a minute: `item_id` digests the minute tag, and `valid` is a minute.
    fn minute_row(store: &Committer, spec: &Spec) -> Result<ItemRow, Box<dyn std::error::Error>> {
        let minute = Minute::new(instant(spec.valid)).map_err(|failure| failure.to_string())?;
        Ok(ItemRow {
            item_id: item_id(&ItemFields {
                source: spec.source,
                source_item_id: spec.source_item_id,
                url: URL,
                valid: Stamp::Minute(minute.start()),
                title: spec.title,
                body_ref: None,
            })
            .map_err(|failure| failure.to_string())?,
            valid: Published::AtMinute(minute),
            ..row(store, spec)?
        })
    }

    fn minute_spec() -> Spec<'static> {
        Spec {
            valid: A_PUBLICATION_MINUTE,
            ..spec(STORY)
        }
    }

    fn story_hex(source: &str, source_item_id: &str) -> String {
        match story_id(source, source_item_id) {
            Ok(digest) => digest.hex().to_owned(),
            Err(_) => String::new(),
        }
    }

    /// A, corrected to B, reverted to A: three attempts over one story, each committing one row.
    /// The third act's `item_id` is the first act's, which is the whole counter-example.
    fn three_act_revert(
        store: &mut Committer,
    ) -> Result<Vec<super::CommitReceipt>, Box<dyn std::error::Error>> {
        let acts = [
            (TITLE, PAYLOAD, OBSERVED_AT),
            (CORRECTED_TITLE, CORRECTED_PAYLOAD, OBSERVED_LATER),
            (TITLE, PAYLOAD, OBSERVED_LAST),
        ];
        let mut receipts = Vec::new();
        for (act, (title, payload, observed)) in acts.iter().enumerate() {
            let ordinal = u32::try_from(act).unwrap_or_default();
            let closed = closed_attempt(store, ordinal, Verdict::Usable)?;
            let committed = row(
                store,
                &Spec {
                    title,
                    payload,
                    observed: *observed,
                    ..spec(STORY)
                },
            )?;
            receipts.push(store.record(closed, &[committed], at(5))?);
        }
        Ok(receipts)
    }

    /// One attempt committing the story, then a second committing the same story's row under a
    /// LATER publication value: the re-stamp 501 ids of the predecessor's archive carry.
    fn commit_a_re_stamp(store: &mut Committer) -> Result<(), Box<dyn std::error::Error>> {
        let first = closed_attempt(store, 0, Verdict::Usable)?;
        let original = row(store, &spec(STORY))?;
        store.record(first, &[original], at(5))?;
        let re_stamped = closed_attempt(store, 1, Verdict::Usable)?;
        let twin = row(
            store,
            &Spec {
                valid: RE_STAMPED_AT,
                observed: OBSERVED_LATER,
                ..spec(STORY)
            },
        )?;
        store.record(re_stamped, &[twin], at(6))?;
        Ok(())
    }

    /// A sink that is not the store. `close` takes `&mut dyn Sink` and nothing in the type system
    /// says `open`, `close` and `record` share one, which is the route review 2026-11-13 found.
    struct Elsewhere;

    impl Sink for Elsewhere {
        fn write(&mut self, _event: &AttemptEvent) -> Result<(), LedgerError> {
            Ok(())
        }
    }

    fn open(root: &TempDir) -> Result<Committer, StoreError> {
        Committer::open(root.path(), &RunId::new(RUN))
    }

    /// Opens one attempt, answers it and closes it under `verdict`. The permit is left untaken:
    /// no socket exists in this crate, so the cost is `Slots(0)`.
    fn closed_attempt(
        store: &mut Committer,
        ordinal: u32,
        verdict: Verdict,
    ) -> Result<news_provider::AttemptReceipt, Box<dyn std::error::Error>> {
        closed_attempt_from(store, SOURCE, ordinal, verdict)
    }

    fn closed_attempt_from(
        store: &mut Committer,
        source: &str,
        ordinal: u32,
        verdict: Verdict,
    ) -> Result<news_provider::AttemptReceipt, Box<dyn std::error::Error>> {
        let slot = AttemptSlot::open(
            store,
            &request_from(source),
            &RunId::new(RUN),
            ordinal,
            at(1),
        )?;
        let answer = RawResponse::new(
            slot.attempt_id().clone(),
            STATUS_OK,
            PAYLOAD.to_vec(),
            at(2),
            at(3),
        );
        let vendor_rows = match verdict {
            Verdict::Usable | Verdict::Empty => VENDOR_ROWS,
            Verdict::PermanentFailure | Verdict::TransientFailure => FAILED_VENDOR_ROWS,
        };
        let outcome = Outcome::decide(
            &verdict,
            vendor_rows,
            &Baseline::from_trailing_ok(&TRAILING_OK),
        );
        Ok(slot.close(store, outcome, &Ok(answer), at(4))?)
    }

    /// Like [`closed_attempt`], with the vendor's row count chosen by the caller: three attempts
    /// carrying one constant make any ordering of them look identical.
    fn closed_attempt_counting(
        store: &mut Committer,
        ordinal: u32,
        vendor_rows: u32,
    ) -> Result<news_provider::AttemptReceipt, Box<dyn std::error::Error>> {
        let slot = AttemptSlot::open(
            store,
            &request_from(SOURCE),
            &RunId::new(RUN),
            ordinal,
            at(1),
        )?;
        let answer = RawResponse::new(
            slot.attempt_id().clone(),
            STATUS_OK,
            PAYLOAD.to_vec(),
            at(2),
            at(3),
        );
        let outcome = Outcome::decide(
            &Verdict::Usable,
            vendor_rows,
            &Baseline::from_trailing_ok(&TRAILING_OK),
        );
        Ok(slot.close(store, outcome, &Ok(answer), at(4 + i64::from(ordinal)))?)
    }

    /// `count` rows, each its own story and all sharing one stored payload.
    fn rows(store: &Committer, count: usize) -> Result<Vec<ItemRow>, Box<dyn std::error::Error>> {
        let mut rows = Vec::new();
        for index in 0..count {
            let source_item_id = index.to_string();
            rows.push(row(
                store,
                &Spec {
                    source_item_id: &source_item_id,
                    ..spec(STORY)
                },
            )?);
        }
        Ok(rows)
    }

    fn count(store: &Committer, table: &str) -> Result<i64, StoreError> {
        Ok(store
            .db
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })?)
    }

    fn finalised(store: &Committer) -> Result<i64, StoreError> {
        Ok(store.db.query_row(
            "SELECT COUNT(*) FROM attempts WHERE closed_at IS NOT NULL",
            [],
            |row| row.get(0),
        )?)
    }

    fn outcome_column(store: &Committer) -> Result<String, StoreError> {
        Ok(store
            .db
            .query_row("SELECT outcome FROM attempts", [], |row| row.get(0))?)
    }

    fn items_new_of(store: &Committer, attempt: &str) -> Result<Option<i64>, StoreError> {
        Ok(store.db.query_row(
            "SELECT items_new FROM attempts WHERE attempt_id = ?1",
            params![attempt],
            |row| row.get(0),
        )?)
    }

    fn disputes(store: &Committer) -> Result<Vec<(i64, Option<String>)>, StoreError> {
        let mut statement = store
            .db
            .prepare("SELECT valid_disputed, earliest_valid_at FROM items ORDER BY seq")?;
        let marked = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(marked)
    }

    /// Every column of one row as one string, so "byte-identical" is an equality and not a list
    /// of the columns somebody remembered to check.
    fn row_text(store: &Committer, seq: i64) -> Result<String, StoreError> {
        Ok(store.db.query_row(
            "SELECT seq || '|' || item_id || '|' || story_id || '|' || IFNULL(doc_id, '')
                 || '|' || attempt_id || '|' || IFNULL(valid_at, '')
                 || '|' || IFNULL(valid_day, '') || '|' || IFNULL(valid_tz, '')
                 || '|' || IFNULL(valid_minute, '')
                 || '|' || IFNULL(valid_refused, '') || '|' || observed_at
                 || '|' || valid_disputed || '|' || IFNULL(earliest_valid_at, '')
                 || '|' || raw_blob || '|' || IFNULL(body_blob, '') || '|' || committed_at
                 || '|' || IFNULL(valid_raw, '') || '|' || IFNULL(valid_field, '')
                 || '|' || IFNULL(valid_source, '')
             FROM items WHERE seq = ?1",
            params![seq],
            |row| row.get(0),
        )?)
    }

    #[test]
    fn there_is_no_way_to_add_an_item_without_a_closed_attempt() {
        let illegal = block(ILLEGAL_BLOCK);
        assert_eq!(annotation(&illegal), COMPILE_FAIL, "{illegal}");
        assert!(illegal.contains(DOC_IMPORT), "{illegal}");
        assert!(illegal.contains(RECORD_WITHOUT_AN_ATTEMPT), "{illegal}");
    }

    #[test]
    fn the_positive_control_for_that_block_records_an_item_through_a_closed_attempt() {
        let control = block(CONTROL_BLOCK);
        assert_eq!(annotation(&control), NO_RUN, "{control}");
        assert!(control.contains(DOC_IMPORT), "{control}");
        assert!(control.contains(RECORD_WITH_AN_ATTEMPT), "{control}");
    }

    /// The roster the run filtered by, on the row. Without it an unrecorded roster change is
    /// drift nothing can see, and the predecessor's own roster read returns an empty map on a read
    /// failure - a silent zero-item day (architecture.md section 2's `universe_version` row).
    #[test]
    fn the_opened_row_carries_the_roster_digest_the_request_was_built_from() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let request = request();
        let slot = AttemptSlot::open(&mut store, &request, &RunId::new(RUN), 0, at(1))?;
        let stored: String = store.db.query_row(
            "SELECT universe_version FROM attempts WHERE attempt_id = ?1",
            params![slot.attempt_id().hex()],
            |row| row.get(0),
        )?;
        assert_eq!(stored, request.universe_version.hex());
        assert_eq!(stored, FileDigest::of(ROSTER).hex());
        Ok(())
    }

    #[test]
    fn the_opened_row_carries_the_hosts_version_the_request_was_built_from() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let request = Request {
            hosts_version: Some(FileDigest::of(HOSTS)),
            ..request()
        };
        let slot = AttemptSlot::open(&mut store, &request, &RunId::new(RUN), 0, at(1))?;
        let stored: Option<String> = store.db.query_row(
            "SELECT hosts_version FROM attempts WHERE attempt_id = ?1",
            params![slot.attempt_id().hex()],
            |row| row.get(0),
        )?;
        assert_eq!(stored, Some(FileDigest::of(HOSTS).hex().to_owned()));
        Ok(())
    }

    /// Every column the user_version 0 `items` table holds, which the rebuild must carry unchanged.
    const V0_ITEM_COLUMNS: [&str; 15] = [
        "seq",
        "item_id",
        "story_id",
        "doc_id",
        "attempt_id",
        "valid_at",
        "valid_day",
        "valid_tz",
        "valid_refused",
        "observed_at",
        "valid_disputed",
        "earliest_valid_at",
        "raw_blob",
        "body_blob",
        "committed_at",
    ];
    const V0_ROWS: usize = 3;

    /// Each `items` row's v0 columns as one SQL-quoted text, so a NULL and an empty string differ.
    fn v0_items(db: &Connection) -> Result<Vec<String>, rusqlite::Error> {
        let quoted = V0_ITEM_COLUMNS.map(|column| format!("quote({column})"));
        let mut statement = db.prepare(&format!(
            "SELECT {} FROM items WHERE seq IN (7, 8, 9) ORDER BY seq",
            quoted.join(" || '|' || ")
        ))?;
        let rows = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>();
        rows
    }

    fn item_indexes(db: &Connection) -> Result<Vec<String>, rusqlite::Error> {
        let mut statement = db.prepare(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'items'
               AND sql IS NOT NULL ORDER BY name",
        )?;
        let names = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>();
        names
    }

    /// A v0 store holding one attempt and one item of each v0 precision (instant, day, refused),
    /// every nullable column set and no `seq` at 1, written with the frozen DDL and no migration.
    fn a_store_at_user_version_zero(temp: &TempDir) -> TestResult {
        let db = Connection::open(temp.path().join(COMMIT_LOG))?;
        db.execute_batch(SCHEMA_V0)?;
        db.execute_batch(
            "INSERT INTO attempts (attempt_id, run_id, source, target_window, filter_params,
                 universe_version, opened_at)
             VALUES ('a v0 attempt', 'a v0 run', 'edgar_press', '2026-11-10', 'f', 'u',
                 '1970-01-01T00:00:01.000000Z');
             INSERT INTO blobs (digest, bytes, first_indexed_at)
             VALUES ('a v0 blob', 1, '1970-01-01T00:00:01.000000Z'),
                    ('a v0 body', 1, '1970-01-01T00:00:01.000000Z');
             INSERT INTO items (seq, item_id, story_id, doc_id, attempt_id, valid_at, valid_day,
                 valid_tz, valid_refused, observed_at, valid_disputed, earliest_valid_at,
                 raw_blob, body_blob, committed_at)
             VALUES
                 (7, 'a v0 instant', 'a v0 story', 'a v0 doc', 'a v0 attempt',
                  '1970-01-01T00:00:01.000000Z', NULL, NULL, NULL, '1970-01-01T00:00:02.000000Z',
                  1, '1970-01-01T00:00:00.500000Z', 'a v0 blob', 'a v0 body',
                  '1970-01-01T00:00:03.000000Z'),
                 (8, 'a v0 day', 'a v0 story', 'a v0 doc', 'a v0 attempt', NULL, '2026-11-10',
                  'America/New_York', NULL, '2026-11-11T05:00:00.000000Z', 1,
                  '1970-01-01T00:00:00.500000Z', 'a v0 blob', 'a v0 body',
                  '2026-11-11T05:00:01.000000Z'),
                 (9, 'a v0 refused', 'a v0 other story', 'a v0 doc', 'a v0 attempt', NULL, NULL,
                  NULL, '2026-11-12T00:00:00.000000Z', '2026-11-11T00:00:00.000000Z', 1,
                  '1970-01-01T00:00:00.500000Z', 'a v0 blob', 'a v0 body',
                  '2026-11-11T00:00:01.000000Z');",
        )?;
        Ok(())
    }

    /// §8 row M1. The v0 row keeps NULL in the evidence columns, the one state they may be NULL
    /// in, and a minute row lands in the column the rebuilt table added.
    #[test]
    fn a_store_at_the_previous_user_version_opens_migrates_and_takes_a_minute_row() -> TestResult {
        let temp = TempDir::new()?;
        a_store_at_user_version_zero(&temp)?;
        let mut store = open(&temp)?;
        let closed = closed_attempt(&mut store, 0, Verdict::Usable)?;
        store.record(closed, &[minute_row(&store, &minute_spec())?], at(5))?;
        let mut statement = store
            .db
            .prepare("SELECT valid_raw, valid_minute FROM items ORDER BY seq")?;
        let stored = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(
            (user_version(&store.db)?, stored),
            (
                SCHEMA_VERSION,
                [
                    vec![(None, None); V0_ROWS],
                    vec![(
                        Some(VALID_RAW.to_owned()),
                        Some(stamped(A_PUBLICATION_MINUTE))
                    )]
                ]
                .concat()
            )
        );
        Ok(())
    }

    /// §8 row M1, the carried half. Mutations 2026-11-15: `seq`, `doc_id`, `earliest_valid_at`
    /// and `body_blob` dropped from the copy, and both `CREATE INDEX` lines dropped, were green.
    #[test]
    fn a_migrated_v0_store_keeps_every_v0_column_of_every_row_and_both_indexes() -> TestResult {
        let temp = TempDir::new()?;
        a_store_at_user_version_zero(&temp)?;
        let v0 = Connection::open(temp.path().join(COMMIT_LOG))?;
        let before = (v0_items(&v0)?, item_indexes(&v0)?);
        drop(v0);
        let store = open(&temp)?;
        assert_eq!(before.0.len(), V0_ROWS);
        assert_eq!(
            before.1,
            vec!["items_by_doc".to_owned(), "items_by_story".to_owned()]
        );
        assert_eq!((v0_items(&store.db)?, item_indexes(&store.db)?), before);
        Ok(())
    }

    /// Foreign keys are off while the table is rebuilt (SQLite's procedure), so nothing but the
    /// closing `foreign_key_check` stops a v0 row whose attempt is absent from being carried on.
    #[test]
    fn a_v0_store_holding_an_item_with_no_attempt_is_refused_rather_than_migrated() -> TestResult {
        let temp = TempDir::new()?;
        a_store_at_user_version_zero(&temp)?;
        let db = Connection::open(temp.path().join(COMMIT_LOG))?;
        db.execute("DELETE FROM attempts", [])?;
        drop(db);
        let refused = open(&temp);
        assert!(
            matches!(
                &refused,
                Err(StoreError::MigrationFoundADanglingReference { table, rowid: 7 })
                    if table == "items"
            ),
            "{refused:?}"
        );
        let db = Connection::open(temp.path().join(COMMIT_LOG))?;
        assert_eq!(user_version(&db)?, 0);
        Ok(())
    }

    /// `attempts` as every binary before 067a0bd (2026-11-13) wrote it: `SCHEMA_V0` less `matched`.
    const ATTEMPTS_BEFORE_MATCHED: &str = "
        CREATE TABLE attempts (
            attempt_id TEXT PRIMARY KEY, run_id TEXT NOT NULL, source TEXT NOT NULL,
            target_window TEXT NOT NULL, filter_params TEXT NOT NULL,
            universe_version TEXT NOT NULL, opened_at TEXT NOT NULL, requested_at TEXT,
            received_at TEXT, transport_status INTEGER, clock_nonmonotonic INTEGER,
            outcome TEXT, vendor_rows INTEGER, items_new INTEGER, baseline_count INTEGER,
            baseline_window INTEGER, baseline_construction TEXT, baseline_ratio REAL,
            slots INTEGER, closed_at TEXT, noticed_at TEXT, noticed_by TEXT
        );";

    /// §8 row M1, a v0 store an older binary wrote. Before this check it migrated to 1 with no
    /// `matched` and `ledger` then failed on `no such column: a.matched` (review 2026-11-15).
    #[test]
    fn a_v0_store_whose_attempts_predate_matched_is_refused_and_left_at_user_version_zero(
    ) -> TestResult {
        let temp = TempDir::new()?;
        let db = Connection::open(temp.path().join(COMMIT_LOG))?;
        db.execute_batch(ATTEMPTS_BEFORE_MATCHED)?;
        db.execute_batch(SCHEMA_V0)?;
        drop(db);
        let refused = open(&temp);
        assert!(
            matches!(
                &refused,
                Err(StoreError::ShapeDiffers { found: 0, columns }) if columns == &["attempts.matched"]
            ),
            "{refused:?}"
        );
        let db = Connection::open(temp.path().join(COMMIT_LOG))?;
        assert_eq!(user_version(&db)?, 0);
        Ok(())
    }

    /// §8 row M1: every part of a column's shape is compared, not its name alone. Each fixture is
    /// `SCHEMA_V0` with one column's type, NOT NULL, default or primary key changed.
    #[test]
    fn a_v0_store_whose_column_differs_in_type_not_null_default_or_key_is_refused() -> TestResult {
        let fixtures = [
            (
                "slots                 INTEGER,",
                "slots TEXT,",
                "attempts.slots",
            ),
            (
                "run_id                TEXT NOT NULL,",
                "run_id TEXT,",
                "attempts.run_id",
            ),
            (
                "outcome               TEXT,",
                "outcome TEXT DEFAULT 'x',",
                "attempts.outcome",
            ),
            (
                "attempt_id            TEXT PRIMARY KEY,",
                "attempt_id TEXT UNIQUE,",
                "attempts.attempt_id",
            ),
        ];
        let mut named = Vec::new();
        for (declared, written, _) in fixtures {
            assert_eq!(SCHEMA_V0.matches(declared).count(), 1, "{declared}");
            let temp = TempDir::new()?;
            let db = Connection::open(temp.path().join(COMMIT_LOG))?;
            db.execute_batch(&SCHEMA_V0.replacen(declared, written, 1))?;
            drop(db);
            let refused = match open(&temp) {
                Err(StoreError::ShapeDiffers { found: 0, columns }) => columns,
                other => vec![format!("{other:?}")],
            };
            let db = Connection::open(temp.path().join(COMMIT_LOG))?;
            named.push((refused, user_version(&db)?));
        }
        assert_eq!(
            named,
            fixtures.map(|(_, _, column)| (vec![column.to_owned()], 0))
        );
        Ok(())
    }

    /// A step that fails names the versions and SQLite's message, never the 2 KB batch rusqlite
    /// puts in its error (review 2026-11-15, `items_v1` left in place).
    #[test]
    fn a_failed_migration_step_is_named_without_its_sql_and_leaves_user_version_zero() -> TestResult
    {
        let temp = TempDir::new()?;
        a_store_at_user_version_zero(&temp)?;
        let db = Connection::open(temp.path().join(COMMIT_LOG))?;
        db.execute_batch("CREATE TABLE items_v1 (x)")?;
        drop(db);
        let refused = open(&temp);
        assert!(
            matches!(
                &refused,
                Err(StoreError::MigrationFailed {
                    from: 0,
                    step: MigrationStep::ToUserVersion(1),
                    cause,
                }) if cause == "table items_v1 already exists"
            ),
            "{refused:?}"
        );
        let db = Connection::open(temp.path().join(COMMIT_LOG))?;
        assert_eq!(user_version(&db)?, 0);
        Ok(())
    }

    /// A failure inside `SCHEMA_V0` is not named as a step to user_version 0 (review 2026-11-15:
    /// "failed at the step to user_version 0", read as a no-op step).
    #[test]
    fn a_failure_creating_the_v0_tables_is_named_as_that_step() -> TestResult {
        let temp = TempDir::new()?;
        let db = Connection::open(temp.path().join(COMMIT_LOG))?;
        db.execute_batch("CREATE TABLE t (x); CREATE INDEX published ON t(x);")?;
        drop(db);
        let said = open(&temp).err().map(|error| error.to_string());
        assert_eq!(
            said.as_deref(),
            Some(
                "migrating the commit log from user_version 0 failed creating the v0 tables the \
                 store lacks: there is already an index named published; the store is left at \
                 user_version 0"
            )
        );
        Ok(())
    }

    /// Every table a user_version 1 store holds, in the order the carried test reads them.
    const V1_TABLES: [&str; 5] = [
        "attempts",
        "blobs",
        "items",
        "seq_reservations",
        "published",
    ];
    const ROBOTS_HOST: &str = "rss.globenewswire.com";
    /// Every table a user_version 2 store holds: the v1 five plus `robots`.
    const V2_TABLES: [&str; 6] = [
        "attempts",
        "blobs",
        "items",
        "seq_reservations",
        "published",
        "robots",
    ];
    const EVERY_VERDICT: [Verdict; 4] = [
        Verdict::Usable,
        Verdict::Empty,
        Verdict::PermanentFailure,
        Verdict::TransientFailure,
    ];

    /// A v1 store written with the frozen DDL replayed to user_version 1 and no later step: one
    /// row in every table, every nullable column set, a minute item and a hosts version.
    fn a_store_at_user_version_one(temp: &TempDir) -> TestResult {
        let db = Connection::open(temp.path().join(COMMIT_LOG))?;
        db.execute_batch(SCHEMA_V0)?;
        db.execute_batch(MIGRATIONS[0])?;
        db.pragma_update(None, "user_version", 1)?;
        db.execute_batch(
            "INSERT INTO attempts VALUES ('a v1 attempt', 'a v1 run', 'open_web_wires',
                 '2026-11-14', 'f', 'u', '1970-01-01T00:00:01.000000Z',
                 '1970-01-01T00:00:02.000000Z', '1970-01-01T00:00:03.000000Z', 200, 0, 'ok', 3, 1,
                 3, 3, 1, 'trailing median', 1.5, 1, '1970-01-01T00:00:04.000000Z',
                 '1970-01-01T00:00:05.000000Z', 'a v1 sweep', 'a v1 hosts file');
             INSERT INTO blobs VALUES ('a v1 blob', 1, '1970-01-01T00:00:01.000000Z'),
                                      ('a v1 body', 2, '1970-01-01T00:00:01.000000Z');
             INSERT INTO items VALUES (11, 'a v1 minute', 'a v1 story', 'a v1 doc', 'a v1 attempt',
                 NULL, NULL, NULL, '2026-11-14T19:00:00.000000Z', NULL,
                 '2026-11-14T19:00:29.000000Z', 1, '2026-11-14T18:59:00.000000Z', 'a v1 blob',
                 'a v1 body', '2026-11-14T19:00:30.000000Z', 'Sat, 14 Nov 2026 19:00 GMT',
                 'pubDate', 'vendor_published');
             INSERT INTO seq_reservations VALUES (5, 'a v1 attempt', 11, 11,
                 '2026-11-14T19:00:30.000000Z');
             INSERT INTO published VALUES ('a v1 segment', 'a v1 run', '2026-11-14', 'items/x',
                 11, 11, 1, '2026-11-14T19:01:00.000000Z');",
        )?;
        Ok(())
    }

    /// Each row of each of `tables` as one SQL-quoted text over every column a store at
    /// `version` declares, in rowid order, so a column a later step adds is not compared.
    fn quoted_rows(
        db: &Connection,
        version: usize,
        tables: &[&str],
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let declared = Connection::open_in_memory()?;
        declared.execute_batch(SCHEMA_V0)?;
        for step in MIGRATIONS.iter().take(version) {
            declared.execute_batch(step)?;
        }
        let mut rows = Vec::new();
        for &table in tables {
            let columns = super::columns_of(&declared, table)?;
            let quoted: Vec<String> = columns
                .keys()
                .map(|column| format!("quote({column})"))
                .collect();
            let mut statement = db.prepare(&format!(
                "SELECT '{table}|' || {} FROM {table} ORDER BY rowid",
                quoted.join(" || '|' || ")
            ))?;
            rows.extend(
                statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<String>, _>>()?,
            );
        }
        Ok(rows)
    }

    /// Opens one attempt for `ROBOTS_HOST` under source `robots` and closes it ok.
    fn closed_robots_attempt(
        store: &mut Committer,
    ) -> Result<news_provider::AttemptReceipt, Box<dyn std::error::Error>> {
        let request = Request {
            source: SourceId::new("robots"),
            target_window: TargetWindow::new(ROBOTS_HOST),
            ..request()
        };
        let slot = AttemptSlot::open(store, &request, &RunId::new(RUN), 0, at(1))?;
        let answer = RawResponse::new(
            slot.attempt_id().clone(),
            STATUS_OK,
            PAYLOAD.to_vec(),
            at(2),
            at(3),
        );
        let outcome = Outcome::decide(&Verdict::Usable, 0, &Baseline::from_trailing_ok(&[]));
        Ok(slot.close(store, outcome, &Ok(answer), at(4))?)
    }

    fn robots_rows(store: &Committer) -> Result<Vec<(String, String, Option<String>)>, StoreError> {
        let mut statement = store
            .db
            .prepare("SELECT host, answer, blob FROM robots ORDER BY attempt_id")?;
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// §8 row M1 from user_version 1: the step to 2 adds `robots`, and a robots answer lands in it.
    #[test]
    fn a_store_at_user_version_one_opens_migrates_to_two_and_takes_a_robots_row() -> TestResult {
        let temp = TempDir::new()?;
        a_store_at_user_version_one(&temp)?;
        let mut store = open(&temp)?;
        let closed = closed_robots_attempt(&mut store)?;
        let body = store.put_blob(PAYLOAD)?;
        store.record_robots(
            closed,
            ROBOTS_HOST,
            &RobotsAnswer::Present(body.clone()),
            at(5),
        )?;
        assert_eq!(
            (user_version(&store.db)?, robots_rows(&store)?),
            (
                SCHEMA_VERSION,
                vec![(
                    ROBOTS_HOST.to_owned(),
                    "present".to_owned(),
                    Some(body.hex().to_owned())
                )]
            )
        );
        Ok(())
    }

    /// §8 row M1, the per-version shape check at user_version 1: a column of another type and a
    /// table the store lacks are each named, and the store stays at 1.
    #[test]
    fn a_v1_store_whose_columns_differ_is_refused_and_left_at_user_version_one() -> TestResult {
        let fixtures = [
            (
                "ALTER TABLE attempts DROP COLUMN hosts_version;
                 ALTER TABLE attempts ADD COLUMN hosts_version INTEGER;",
                vec!["attempts.hosts_version".to_owned()],
            ),
            (
                "DROP TABLE published;",
                [
                    "path",
                    "published_at",
                    "rows",
                    "run_id",
                    "segment_digest",
                    "seq_hi",
                    "seq_lo",
                    "valid_date",
                ]
                .map(|column| format!("published.{column}"))
                .to_vec(),
            ),
        ];
        let mut named = Vec::new();
        for (edit, _) in &fixtures {
            let temp = TempDir::new()?;
            a_store_at_user_version_one(&temp)?;
            let db = Connection::open(temp.path().join(COMMIT_LOG))?;
            db.execute_batch(edit)?;
            drop(db);
            let read_only = super::refuse_an_unreadable_version(temp.path());
            let refused = match open(&temp) {
                Err(StoreError::ShapeDiffers { found: 1, columns }) => columns,
                other => vec![format!("{other:?}")],
            };
            let db = Connection::open(temp.path().join(COMMIT_LOG))?;
            named.push((refused, read_only.is_err(), user_version(&db)?));
        }
        assert_eq!(
            named,
            fixtures
                .into_iter()
                .map(|(_, columns)| (columns, true, 1))
                .collect::<Vec<_>>()
        );
        Ok(())
    }

    /// §8 row M1, the carried half from user_version 1: every column of every row of every v1
    /// table, compared by `quote()` before and after.
    #[test]
    fn a_v1_store_keeps_every_v1_column_of_every_row() -> TestResult {
        let temp = TempDir::new()?;
        a_store_at_user_version_one(&temp)?;
        let v1 = Connection::open(temp.path().join(COMMIT_LOG))?;
        let before = quoted_rows(&v1, 1, &V1_TABLES)?;
        drop(v1);
        let store = open(&temp)?;
        assert_eq!(before.len(), V1_TABLES.len() + 1);
        assert_eq!(quoted_rows(&store.db, 1, &V1_TABLES)?, before);
        Ok(())
    }

    /// A v2 store: the v1 fixture, `MIGRATIONS[1]` and no later step, plus one robots row.
    fn a_store_at_user_version_two(temp: &TempDir) -> TestResult {
        a_store_at_user_version_one(temp)?;
        let db = Connection::open(temp.path().join(COMMIT_LOG))?;
        db.execute_batch(MIGRATIONS[1])?;
        db.pragma_update(None, "user_version", 2)?;
        db.execute_batch(
            "INSERT INTO robots VALUES ('a v1 attempt', 'rss.globenewswire.com', 'present',
                 'a v1 blob');",
        )?;
        Ok(())
    }

    fn verdict_of(store: &Committer, attempt: &str) -> Result<Option<String>, StoreError> {
        Ok(store.db.query_row(
            "SELECT verdict FROM attempts WHERE attempt_id = ?1",
            params![attempt],
            |row| row.get(0),
        )?)
    }

    /// §8 row M1 from user_version 2: the step to 3 adds `attempts.verdict`, and a closed
    /// attempt's verdict lands in it. Mutation 2026-11-15, `MIGRATIONS[2]` emptied: red.
    #[test]
    fn a_store_at_user_version_two_opens_migrates_to_three_and_takes_a_verdict() -> TestResult {
        let temp = TempDir::new()?;
        a_store_at_user_version_two(&temp)?;
        let mut store = open(&temp)?;
        let closed = closed_attempt(&mut store, 0, Verdict::PermanentFailure)?;
        let attempt = closed.attempt_id().hex().to_owned();
        store.record(closed, &[], at(5))?;
        assert_eq!(
            (user_version(&store.db)?, verdict_of(&store, &attempt)?),
            (3, Some("permanent_failure".to_owned()))
        );
        Ok(())
    }

    /// §8 row M1, the carried half from user_version 2: every column of every row of every v2
    /// table, compared by `quote()`; the v2 attempt gains a NULL verdict and nothing else.
    #[test]
    fn a_v2_store_keeps_every_v2_column_of_every_row() -> TestResult {
        let temp = TempDir::new()?;
        a_store_at_user_version_two(&temp)?;
        let v2 = Connection::open(temp.path().join(COMMIT_LOG))?;
        let before = quoted_rows(&v2, 2, &V2_TABLES)?;
        drop(v2);
        let store = open(&temp)?;
        let after = quoted_rows(&store.db, 2, &V2_TABLES)?;
        assert_eq!(before.len(), V2_TABLES.len() + 1);
        assert_eq!((after, verdict_of(&store, "a v1 attempt")?), (before, None));
        Ok(())
    }

    /// §8 row M1, the per-version shape check at user_version 2: a robots column of another type
    /// and a missing `robots` table are each named, and the store stays at 2.
    #[test]
    fn a_v2_store_whose_columns_differ_is_refused_and_left_at_user_version_two() -> TestResult {
        let fixtures = [
            (
                "ALTER TABLE robots DROP COLUMN host;
                 ALTER TABLE robots ADD COLUMN host INTEGER;",
                vec!["robots.host".to_owned()],
            ),
            (
                "DROP TABLE robots;",
                ["answer", "attempt_id", "blob", "host"]
                    .map(|column| format!("robots.{column}"))
                    .to_vec(),
            ),
        ];
        let mut named = Vec::new();
        for (edit, _) in &fixtures {
            let temp = TempDir::new()?;
            a_store_at_user_version_two(&temp)?;
            let db = Connection::open(temp.path().join(COMMIT_LOG))?;
            db.execute_batch(edit)?;
            drop(db);
            let read_only = super::refuse_an_unreadable_version(temp.path());
            let refused = match open(&temp) {
                Err(StoreError::ShapeDiffers { found: 2, columns }) => columns,
                other => vec![format!("{other:?}")],
            };
            let db = Connection::open(temp.path().join(COMMIT_LOG))?;
            named.push((refused, read_only.is_err(), user_version(&db)?));
        }
        assert_eq!(
            named,
            fixtures
                .into_iter()
                .map(|(_, columns)| (columns, true, 2))
                .collect::<Vec<_>>()
        );
        Ok(())
    }

    /// §11.1's held table needs permanence apart from `failed`, and nothing for an attempt nobody
    /// answered. Mutation 2026-11-15, `apply` writing NULL for the verdict: red.
    #[test]
    fn every_verdict_reaches_the_verdict_column_by_its_own_name_and_an_abandoned_attempt_none(
    ) -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let mut stored = Vec::new();
        for (ordinal, verdict) in EVERY_VERDICT.into_iter().enumerate() {
            let closed = closed_attempt(&mut store, u32::try_from(ordinal)?, verdict)?;
            let attempt = closed.attempt_id().hex().to_owned();
            store.record(closed, &[], at(5))?;
            stored.push(verdict_of(&store, &attempt)?);
        }
        let slot = AttemptSlot::open(
            &mut store,
            &request(),
            &RunId::new(RUN),
            u32::try_from(EVERY_VERDICT.len())?,
            at(1),
        )?;
        let in_flight = slot.attempt_id().clone();
        drop(slot);
        let closed = abandon(&mut store, in_flight, at(9), &RunId::new(SWEEPING_RUN))?;
        let attempt = closed.attempt_id().hex().to_owned();
        store.record(closed, &[], at(10))?;
        stored.push(verdict_of(&store, &attempt)?);
        assert_eq!(
            stored,
            vec![
                Some("usable".to_owned()),
                Some("empty".to_owned()),
                Some("permanent_failure".to_owned()),
                Some("transient_failure".to_owned()),
                None
            ]
        );
        Ok(())
    }

    /// The CHECK and `Verdict::name` are two spellings of one vocabulary; `failed` is the
    /// control that the CHECK refuses anything. Mutation 2026-11-15, one name removed: red.
    #[test]
    fn every_verdict_name_is_one_the_verdict_check_admits() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let slot = AttemptSlot::open(&mut store, &request(), &RunId::new(RUN), 0, at(1))?;
        let attempt = slot.attempt_id().hex().to_owned();
        drop(slot);
        let admitted = |name: &str| {
            store
                .db
                .execute(
                    "UPDATE attempts SET verdict = ?2 WHERE attempt_id = ?1",
                    params![attempt, name],
                )
                .is_ok()
        };
        let names: Vec<bool> = EVERY_VERDICT
            .into_iter()
            .map(|verdict| admitted(verdict.name()))
            .collect();
        assert_eq!(
            (names, admitted("failed")),
            (vec![true; EVERY_VERDICT.len()], false)
        );
        Ok(())
    }

    /// The robots row, the blob's index row and the attempt's `Answered` and `Finalised` events
    /// are one transaction: a robots insert refused (a row already filed under the attempt by
    /// hand) leaves the attempt open and the blob unindexed.
    #[test]
    fn a_robots_row_and_its_attempt_events_commit_in_one_transaction() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let closed = closed_robots_attempt(&mut store)?;
        let attempt = closed.attempt_id().hex().to_owned();
        store.db.execute(
            "INSERT INTO robots (attempt_id, host, answer) VALUES (?1, 'by hand', 'absent')",
            params![attempt],
        )?;
        let body = store.put_blob(PAYLOAD)?;
        let refused = store.record_robots(closed, ROBOTS_HOST, &RobotsAnswer::Present(body), at(5));
        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(
            (
                finalised(&store)?,
                count(&store, "blobs")?,
                count(&store, "robots")?
            ),
            (0, 0, 1)
        );
        Ok(())
    }

    /// §3.2: a robots replay must be the same row; an identical one files nothing. Mutation
    /// 2026-11-15, the replay comparison in `record_robots` skipped: red.
    #[test]
    fn a_robots_replay_naming_another_host_or_answer_is_refused() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let body = store.put_blob(PAYLOAD)?;
        let present = RobotsAnswer::Present(body.clone());
        let first = closed_robots_attempt(&mut store)?;
        store.record_robots(first, ROBOTS_HOST, &present, at(5))?;
        let other_host = "www.prnewswire.com";
        let mut refused = Vec::new();
        for (host, answer) in [
            (ROBOTS_HOST, RobotsAnswer::Absent),
            (other_host, present.clone()),
        ] {
            let again = closed_robots_attempt(&mut store)?;
            refused.push(match store.record_robots(again, host, &answer, at(6)) {
                Err(StoreError::RobotsReplayDiffers {
                    recorded, replayed, ..
                }) => (recorded, replayed),
                other => (format!("{other:?}"), String::new()),
            });
        }
        let again = closed_robots_attempt(&mut store)?;
        let same = store.record_robots(again, ROBOTS_HOST, &present, at(7))?;
        let recorded = format!("{ROBOTS_HOST} present {}", body.hex());
        assert_eq!(
            (refused, same.changed, robots_rows(&store)?.len()),
            (
                vec![
                    (recorded.clone(), format!("{ROBOTS_HOST} absent")),
                    (recorded, format!("{other_host} present {}", body.hex())),
                ],
                false,
                1
            )
        );
        Ok(())
    }

    #[test]
    fn a_store_at_an_unknown_higher_user_version_is_refused() -> TestResult {
        let temp = TempDir::new()?;
        let db = Connection::open(temp.path().join(COMMIT_LOG))?;
        db.pragma_update(None, "user_version", UNKNOWN_HIGHER_VERSION)?;
        drop(db);
        let refused = open(&temp);
        assert!(
            matches!(
                refused,
                Err(StoreError::SchemaVersion {
                    found: UNKNOWN_HIGHER_VERSION,
                    expected: SCHEMA_VERSION
                })
            ),
            "{refused:?}"
        );
        Ok(())
    }

    #[test]
    fn one_attempts_items_blob_rows_and_finalised_event_commit_together_or_not_at_all() -> TestResult
    {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let doomed = closed_attempt(&mut store, 0, Verdict::Usable)?;
        let mut duplicated = rows(&store, 1)?;
        duplicated.extend_from_slice(&duplicated.clone());
        let refused = store.record(doomed, &duplicated, at(5));
        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(
            (
                count(&store, "items")?,
                count(&store, "blobs")?,
                finalised(&store)?
            ),
            (0, 0, 0)
        );
        let answered = closed_attempt(&mut store, 1, Verdict::Usable)?;
        store.record(answered, &rows(&store, 1)?, at(6))?;
        assert_eq!(
            (
                count(&store, "items")?,
                count(&store, "blobs")?,
                finalised(&store)?
            ),
            (1, 1, 1)
        );
        Ok(())
    }

    #[test]
    fn an_injected_failure_on_the_nth_write_leaves_no_item_row_and_keeps_the_opened_event(
    ) -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let doomed = closed_attempt(&mut store, 0, Verdict::Usable)?;
        let mut duplicated = rows(&store, 1)?;
        duplicated.extend_from_slice(&duplicated.clone());
        let refused = store.record(doomed, &duplicated, at(5));
        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(
            (count(&store, "items")?, count(&store, "attempts")?),
            (0, 1)
        );
        Ok(())
    }

    #[test]
    fn a_replay_of_the_same_run_returns_the_standing_seq_rather_than_raising() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let first = closed_attempt(&mut store, 0, Verdict::Usable)?;
        let committed = store.record(first, &rows(&store, 2)?, at(5))?;
        let again = closed_attempt(&mut store, 0, Verdict::Usable)?;
        let replayed = store.record(again, &rows(&store, 2)?, at(6))?;
        assert_eq!(
            (replayed.seq_hi, replayed.changed, count(&store, "items")?),
            (committed.seq_hi, false, 2)
        );
        Ok(())
    }

    /// A replay is a no-op only when it is the same claim. Found by review 2026-11-13: a second
    /// `record` of one attempt carrying a DIFFERENT item set filed nothing and returned
    /// `changed: false`, so the caller was told its rows were already in the archive.
    #[test]
    fn a_replay_carrying_a_different_item_set_is_refused_rather_than_reported_as_standing(
    ) -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let first = closed_attempt(&mut store, 0, Verdict::Usable)?;
        store.record(first, &rows(&store, 2)?, at(5))?;
        let again = closed_attempt(&mut store, 0, Verdict::Usable)?;
        let refused = store.record(again, &rows(&store, 3)?, at(6));
        assert!(
            matches!(refused, Err(StoreError::ReplayWithDifferentItems { .. })),
            "{refused:?}"
        );
        Ok(())
    }

    /// Two of the four `outcome` labels had no test: swapping `ok` and `failed` in `outcome_name`
    /// left the whole suite green, measured 2026-11-13, and that column is what tells a quiet day
    /// from a broken collector.
    #[test]
    fn each_outcome_writes_its_own_label_and_not_another_outcomes() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let mut labels = Vec::new();
        for (ordinal, verdict) in [
            Verdict::Usable,
            Verdict::Empty,
            Verdict::PermanentFailure,
            Verdict::TransientFailure,
        ]
        .into_iter()
        .enumerate()
        {
            let closed = closed_attempt_from(&mut store, SOURCE, u32::try_from(ordinal)?, verdict)?;
            let attempt = closed.attempt_id().hex().to_owned();
            store.record(closed, &[], at(5))?;
            labels.push(store.db.query_row(
                "SELECT outcome FROM attempts WHERE attempt_id = ?1",
                params![attempt],
                |row| row.get::<_, String>(0),
            )?);
        }
        assert_eq!(labels, vec!["ok", "empty", "failed", "failed"]);
        Ok(())
    }

    /// `earliest_valid_at` is the EARLIEST publication value the store holds for a story, which
    /// only the arriving-row-is-earlier case tests. Every dispute fixture had the arriving row
    /// carrying the LATER value, so removing the comparison left the suite green - measured
    /// 2026-11-13.
    #[test]
    fn a_story_re_stamped_backwards_keeps_the_arriving_rows_earlier_value() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let first = closed_attempt(&mut store, 0, Verdict::Usable)?;
        let later = row(
            &store,
            &Spec {
                valid: RE_STAMPED_AT,
                ..spec(STORY)
            },
        )?;
        store.record(first, &[later], at(5))?;
        let second = closed_attempt(&mut store, 1, Verdict::Usable)?;
        let earlier = row(
            &store,
            &Spec {
                observed: OBSERVED_LATER,
                ..spec(STORY)
            },
        )?;
        store.record(second, &[earlier], at(6))?;
        let marked = disputes(&store)?;
        // The arriving row is the earlier one, so its own value is the minimum and the column
        // must carry it rather than the twin's later value.
        assert_eq!(
            marked.last().and_then(|(_, earliest)| earliest.clone()),
            Some(stamp(at(VALID_AT / SECOND).inner()))
        );
        Ok(())
    }

    /// `ok_vendor_rows` returns oldest first, which is what makes the caller's trailing slice a
    /// TRAILING slice. Reversing the `ORDER BY` left the suite green - measured 2026-11-13 - and
    /// the baseline that reads it is the only thing a collapse floor could ever be derived from.
    #[test]
    fn ok_vendor_rows_comes_back_oldest_first_so_a_trailing_slice_is_trailing() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        for (ordinal, vendor_rows) in [FIRST_COUNT, SECOND_COUNT, THIRD_COUNT]
            .into_iter()
            .enumerate()
        {
            let closed = closed_attempt_counting(&mut store, u32::try_from(ordinal)?, vendor_rows)?;
            store.record(closed, &[], at(i64::try_from(ordinal)? + 5))?;
        }
        let counts = store.ok_vendor_rows(&SourceId::new(SOURCE))?;
        assert_eq!(counts, vec![FIRST_COUNT, SECOND_COUNT, THIRD_COUNT]);
        Ok(())
    }

    #[test]
    fn a_second_committer_fails_to_take_the_lease_rather_than_interleaving() -> TestResult {
        let temp = TempDir::new()?;
        let holder = open(&temp)?;
        let blocked = Committer::open(temp.path(), &RunId::new(SWEEPING_RUN));
        assert!(
            matches!(blocked, Err(StoreError::LeaseHeld(_))),
            "{blocked:?}"
        );
        drop(holder);
        Ok(())
    }

    #[test]
    fn an_attempt_with_zero_items_commits_and_writes_its_row() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let closed = closed_attempt(&mut store, 0, Verdict::Empty)?;
        let committed = store.record(closed, &[], at(5))?;
        assert_eq!(
            (
                committed.items_committed,
                committed.seq_lo,
                outcome_column(&store)?
            ),
            (0, None, "empty".to_owned())
        );
        Ok(())
    }

    #[test]
    fn an_empty_attempt_commits_its_rows_exactly_like_an_ok_one() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let closed = closed_attempt(&mut store, 0, Verdict::Empty)?;
        let committed = store.record(closed, &rows(&store, 2)?, at(5))?;
        assert_eq!(
            (
                committed.items_committed,
                count(&store, "items")?,
                outcome_column(&store)?
            ),
            (2, 2, "empty".to_owned())
        );
        Ok(())
    }

    #[test]
    fn committing_n_items_yields_n_item_rows_for_every_n_in_the_fixture_table() -> TestResult {
        for wanted in ITEM_COUNTS {
            let temp = TempDir::new()?;
            let mut store = open(&temp)?;
            let closed = closed_attempt(&mut store, 0, Verdict::Usable)?;
            let committed = store.record(closed, &rows(&store, wanted)?, at(5))?;
            assert_eq!(
                (count(&store, "items")?, committed.items_committed as usize),
                (i64::try_from(wanted)?, wanted)
            );
        }
        Ok(())
    }

    /// §8 row R20. "Covered" is a committed item under a story, never an attempt that returned ok
    /// and committed nothing: the second is a gap the next run must still close, so keying the
    /// covered set on the outcome would hide it. Mutation 2026-11-13: turning the inner
    /// `JOIN items` into a `LEFT JOIN` (so coverage counts an attempt that returned but committed
    /// nothing) turns this red - the empty ok attempt then yields a NULL story.
    #[test]
    fn covered_story_ids_names_a_committed_story_and_not_an_ok_attempt_that_committed_nothing(
    ) -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let committed = closed_attempt(&mut store, 0, Verdict::Usable)?;
        store.record(committed, &[row(&store, &spec(STORY))?], at(5))?;
        let empty = closed_attempt(&mut store, 1, Verdict::Usable)?;
        store.record(empty, &[], at(6))?;
        let covered =
            store.covered_story_ids(&SourceId::new(SOURCE), &TargetWindow::new(WINDOW))?;
        assert_eq!(
            covered,
            std::collections::BTreeSet::from([story_hex(SOURCE, STORY)])
        );
        Ok(())
    }

    #[test]
    fn the_public_write_api_has_no_limit_or_window_parameter() {
        let marker = format!("#[cfg({})]", "test");
        let production = include_str!("store.rs")
            .split(&marker)
            .next()
            .unwrap_or_default()
            .to_owned();
        let mut carrying: Vec<&str> = Vec::new();
        for item in production.split("pub fn ").skip(1) {
            let signature = item.split('{').next().unwrap_or_default();
            if signature.contains("limit") || signature.contains("window") {
                carrying.push(signature);
            }
        }
        assert_eq!(carrying, Vec::<&str>::new());
    }

    #[test]
    fn a_reserved_seq_range_is_never_reused_after_its_transaction_rolls_back() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let doomed = closed_attempt(&mut store, 0, Verdict::Usable)?;
        let mut duplicated = rows(&store, 1)?;
        duplicated.extend_from_slice(&duplicated.clone());
        let refused = store.record(doomed, &duplicated, at(5));
        assert!(refused.is_err(), "{refused:?}");
        let next = closed_attempt(&mut store, 1, Verdict::Usable)?;
        let committed = store.record(next, &rows(&store, 1)?, at(6))?;
        assert_eq!(committed.seq_lo, Some(3));
        Ok(())
    }

    #[test]
    fn a_receipt_for_an_attempt_this_store_never_opened_is_refused_rather_than_committed(
    ) -> TestResult {
        let elsewhere = TempDir::new()?;
        let here = TempDir::new()?;
        let mut other_store = open(&elsewhere)?;
        let closed = closed_attempt(&mut other_store, 0, Verdict::Usable)?;
        let mut store = open(&here)?;
        let refused = store.record(closed, &[], at(5));
        assert!(
            matches!(refused, Err(StoreError::AttemptNotOpened { .. })),
            "{refused:?}"
        );
        Ok(())
    }

    #[test]
    fn an_item_row_naming_an_attempt_the_ledger_has_no_row_for_is_refused() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let closed = closed_attempt(&mut store, 0, Verdict::Usable)?;
        let stored = rows(&store, 1)?;
        store.record(closed, &stored, at(5))?;
        let behind_the_door = store.db.execute(
            "INSERT INTO items (seq, item_id, story_id, attempt_id, valid_at, observed_at,
                 valid_disputed, raw_blob, committed_at)
             VALUES (99, 'an item with no attempt', 'a story', 'no such attempt', ?2, ?2, 0,
                 ?1, ?2)",
            params![stored[0].raw_blob.hex(), "1970-01-01T00:00:05.000000Z"],
        );
        assert!(
            behind_the_door
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_default()
                .contains("FOREIGN KEY"),
            "{behind_the_door:?}"
        );
        Ok(())
    }

    #[test]
    fn an_item_row_naming_a_blob_that_is_not_on_disk_is_refused_rather_than_indexed() -> TestResult
    {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let closed = closed_attempt(&mut store, 0, Verdict::Usable)?;
        let stored = rows(&store, 1)?;
        fs::remove_file(store.blobs.path_of(&stored[0].raw_blob))?;
        let refused = store.record(closed, &stored, at(5));
        assert!(
            matches!(refused, Err(StoreError::BlobAbsent { .. })),
            "{refused:?}"
        );
        Ok(())
    }

    #[test]
    fn a_blob_whose_staging_path_already_exists_is_refused_rather_than_truncated() -> TestResult {
        let temp = TempDir::new()?;
        let store = open(&temp)?;
        let digest = store.put_blob(PAYLOAD)?;
        fs::remove_file(store.blobs.path_of(&digest))?;
        let staging = store.blobs.staging_path(RUN, &digest);
        fs::write(&staging, SENTINEL)?;
        let refused = store.put_blob(PAYLOAD);
        assert!(matches!(refused, Err(StoreError::Io(_))), "{refused:?}");
        assert_eq!(fs::read(&staging)?, SENTINEL);
        Ok(())
    }

    #[test]
    fn a_second_put_of_the_same_payload_keeps_the_standing_file_rather_than_rewriting_it(
    ) -> TestResult {
        let temp = TempDir::new()?;
        let store = open(&temp)?;
        let first = store.put_blob(PAYLOAD)?;
        let before = fs::metadata(store.blobs.path_of(&first))?.ino();
        let second = store.put_blob(PAYLOAD)?;
        let after = fs::metadata(store.blobs.path_of(&second))?.ino();
        assert_eq!((first, before), (second, after));
        Ok(())
    }

    /// `vendor_rows` comes from the outcome and `items_new` from the store's own count, so the
    /// stored 1 against `decide`'s 7 is also what says which of the two is the writer.
    #[test]
    fn the_two_counts_land_in_their_own_columns_rather_than_swapped() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let closed = closed_attempt(&mut store, 0, Verdict::Usable)?;
        store.record(closed, &rows(&store, 1)?, at(5))?;
        let stored: (u32, u32) =
            store
                .db
                .query_row("SELECT vendor_rows, items_new FROM attempts", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?;
        assert_eq!(stored, (VENDOR_ROWS, 1));
        Ok(())
    }

    #[test]
    fn the_same_story_in_three_commits_keeps_the_first_observation_and_writes_items_new_zero(
    ) -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let acts = three_act_revert(&mut store)?;
        let mut counted = Vec::new();
        for act in &acts {
            counted.push(items_new_of(&store, &act.attempt_id)?);
        }
        assert_eq!(counted, vec![Some(1), Some(0), Some(0)]);
        assert_eq!(
            first_observed(&store.db, &story_hex(SOURCE, STORY))?,
            Some(stamped(OBSERVED_AT))
        );
        Ok(())
    }

    #[test]
    fn an_earlier_observation_arriving_later_replaces_the_kept_one_and_is_counted() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let first = closed_attempt(&mut store, 0, Verdict::Usable)?;
        store.record(first, &[row(&store, &spec(STORY))?], at(5))?;
        let backfilled = closed_attempt(&mut store, 1, Verdict::Usable)?;
        let earlier = row(
            &store,
            &Spec {
                title: CORRECTED_TITLE,
                observed: OBSERVED_EARLIER,
                ..spec(STORY)
            },
        )?;
        let committed = store.record(backfilled, &[earlier], at(6))?;
        assert_eq!(items_new_of(&store, &committed.attempt_id)?, Some(1));
        assert_eq!(
            first_observed(&store.db, &story_hex(SOURCE, STORY))?,
            Some(stamped(OBSERVED_EARLIER))
        );
        Ok(())
    }

    #[test]
    fn a_re_stamped_publication_time_yields_two_items_under_one_story_id() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        commit_a_re_stamp(&mut store)?;
        let stored: (i64, i64) = store.db.query_row(
            "SELECT COUNT(*), COUNT(DISTINCT story_id) FROM items",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(stored, (2, 1));
        Ok(())
    }

    /// The disputed flag is set on the ARRIVING row, which is what leaves the earlier twin
    /// untouched. No materiality threshold: a different value is a disagreement, because this
    /// project has no measurement that would say how far apart "materially" is.
    #[test]
    fn a_disputed_publication_time_is_marked_on_the_arriving_row_with_the_earlier_twins_value(
    ) -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        commit_a_re_stamp(&mut store)?;
        assert_eq!(
            disputes(&store)?,
            vec![(0, None), (1, Some(stamped(VALID_AT)))],
            "the earlier twin keeps no flag and the arriving row carries its time"
        );
        Ok(())
    }

    #[test]
    fn the_first_row_is_byte_identical_after_the_second_arrives() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let first = closed_attempt(&mut store, 0, Verdict::Usable)?;
        store.record(first, &[row(&store, &spec(STORY))?], at(5))?;
        let before = row_text(&store, 1)?;
        let re_stamped = closed_attempt(&mut store, 1, Verdict::Usable)?;
        let twin = row(
            &store,
            &Spec {
                valid: RE_STAMPED_AT,
                observed: OBSERVED_LATER,
                ..spec(STORY)
            },
        )?;
        store.record(re_stamped, &[twin], at(6))?;
        assert_eq!(row_text(&store, 1)?, before);
        Ok(())
    }

    #[test]
    fn an_a_then_b_then_a_revert_yields_three_items_two_blobs_and_three_attempts() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        three_act_revert(&mut store)?;
        let reverted: i64 = store.db.query_row(
            "SELECT COUNT(*) FROM items WHERE item_id = (SELECT item_id FROM items WHERE seq = 1)",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(
            (
                count(&store, "items")?,
                count(&store, "blobs")?,
                count(&store, "attempts")?,
                reverted
            ),
            (3, 2, 3, 2),
            "a uniqueness index on the content digest files nothing on the third act"
        );
        Ok(())
    }

    #[test]
    fn two_sources_carrying_one_url_share_a_doc_id_and_keep_their_own_story_ids() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let ours = closed_attempt_from(&mut store, SOURCE, 0, Verdict::Usable)?;
        store.record(ours, &[row(&store, &spec(STORY))?], at(5))?;
        let theirs = closed_attempt_from(&mut store, OTHER_SOURCE, 1, Verdict::Usable)?;
        let sibling = row(
            &store,
            &Spec {
                source: OTHER_SOURCE,
                observed: OBSERVED_LATER,
                ..spec(STORY)
            },
        )?;
        let committed = store.record(theirs, &[sibling], at(6))?;
        let stored: (i64, i64, i64) = store.db.query_row(
            "SELECT COUNT(*), COUNT(DISTINCT doc_id), COUNT(DISTINCT story_id) FROM items",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(
            (stored, items_new_of(&store, &committed.attempt_id)?),
            ((2, 1, 2), Some(1))
        );
        Ok(())
    }

    #[test]
    fn an_observation_preceding_its_publication_is_stored_undated_with_its_raw_blob() -> TestResult
    {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let closed = closed_attempt(&mut store, 0, Verdict::Usable)?;
        let inverted = row(
            &store,
            &Spec {
                observed: OBSERVED_BEFORE_PUBLICATION,
                ..spec(STORY)
            },
        )?;
        let blob = inverted.raw_blob.hex().to_owned();
        store.record(closed, &[inverted], at(5))?;
        let stored: (Option<String>, Option<String>, String, String) = store.db.query_row(
            "SELECT valid_at, valid_refused, observed_at, raw_blob FROM items",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        assert_eq!(
            stored,
            (
                None,
                Some(stamped(VALID_AT)),
                stamped(OBSERVED_BEFORE_PUBLICATION),
                blob
            )
        );
        Ok(())
    }

    /// The fatal defect this pair of columns exists to remove: a day-precision publication value
    /// folded into a midnight instant was `visible_at` TRUE at 02:00, 06:00 and 14:59:59 on its
    /// own publication day, 13 h of admitted lookahead, measured 2026-11-12 against the
    /// predecessor's unmodified reader over 273 of 13,074 rows stamped exactly 00:00:00Z.
    #[test]
    fn a_day_precision_publication_value_is_stored_as_a_day_and_never_as_a_fabricated_midnight(
    ) -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let closed = closed_attempt(&mut store, 0, Verdict::Usable)?;
        let mut dated = row(&store, &spec(STORY))?;
        dated.valid = Published::On {
            day: A_PUBLICATION_DAY.parse()?,
            tz: NEW_YORK,
        };
        store.record(closed, &[dated], at(5))?;
        let stored: (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = store.db.query_row(
            "SELECT valid_at, valid_day, valid_tz, valid_refused FROM items",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        assert_eq!(
            stored,
            (
                None,
                Some(A_PUBLICATION_DAY.to_owned()),
                Some(NEW_YORK.to_owned()),
                None
            )
        );
        Ok(())
    }

    /// The partition label has to be a true statement about the rows under it (decision D3), and
    /// a day-dated row has no instant for the label to be read out of.
    #[test]
    fn a_day_dated_row_is_partitioned_under_the_day_it_names() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let closed = closed_attempt(&mut store, 0, Verdict::Usable)?;
        let mut dated = row(&store, &spec(STORY))?;
        dated.valid = Published::On {
            day: A_PUBLICATION_DAY.parse()?,
            tz: NEW_YORK,
        };
        store.record(closed, &[dated], at(5))?;
        let partitions: Vec<String> = store
            .unpublished_rows()?
            .into_iter()
            .map(|row| row.valid_date)
            .collect();
        assert_eq!(partitions, vec![A_PUBLICATION_DAY.to_owned()]);
        Ok(())
    }

    /// An item with no publication value has no identity to take over it, which is why its
    /// producer files it as an unusable row. The door refuses one rather than inventing a value
    /// for the column the schema requires.
    #[test]
    fn an_item_offering_no_publication_value_is_refused_rather_than_stored_with_an_invented_one(
    ) -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let closed = closed_attempt(&mut store, 0, Verdict::Usable)?;
        let mut undated = row(&store, &spec(STORY))?;
        undated.valid = Published::Unknown;
        let refused = store.record(closed, &[undated], at(5));
        assert!(
            matches!(refused, Err(StoreError::ItemHasNoPublicationValue { .. })),
            "{refused:?}"
        );
        assert_eq!(count(&store, "items")?, 0);
        Ok(())
    }

    #[test]
    fn a_minute_precision_value_is_stored_as_a_minute_and_never_as_an_instant() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let closed = closed_attempt(&mut store, 0, Verdict::Usable)?;
        store.record(closed, &[minute_row(&store, &minute_spec())?], at(5))?;
        let stored: (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = store.db.query_row(
            "SELECT valid_at, valid_minute, valid_day, valid_refused FROM items",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        assert_eq!(
            stored,
            (None, Some(stamped(A_PUBLICATION_MINUTE)), None, None)
        );
        Ok(())
    }

    /// The exactly-one CHECK names the minute column: written past `record`, a row carrying both
    /// a minute and an instant is refused by the table itself.
    #[test]
    fn a_row_holding_a_minute_and_an_instant_is_refused_by_the_schema() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let closed = closed_attempt(&mut store, 0, Verdict::Usable)?;
        store.record(closed, &[row(&store, &spec(STORY))?], at(5))?;
        let both = store.db.execute(
            INSERTED_BY_HAND,
            params![
                stamped(A_PUBLICATION_MINUTE),
                stamped(A_PUBLICATION_MINUTE),
                stamped(OBSERVED_AT),
                ValidSource::VendorPublished.name()
            ],
        );
        assert!(both.is_err(), "{both:?}");
        assert_eq!(count(&store, "items")?, 1);
        Ok(())
    }

    #[test]
    fn a_valid_source_no_variant_names_is_refused_by_the_schema() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let closed = closed_attempt(&mut store, 0, Verdict::Usable)?;
        store.record(closed, &[row(&store, &spec(STORY))?], at(5))?;
        let named = |source: &str| {
            store.db.execute(
                INSERTED_BY_HAND,
                params![
                    Option::<String>::None,
                    stamped(A_PUBLICATION_MINUTE),
                    stamped(OBSERVED_AT),
                    source
                ],
            )
        };
        assert!(named("guessed").is_err());
        assert_eq!(named(ValidSource::OurExtraction.name())?, 1);
        Ok(())
    }

    /// R15 within the minute column. Across precisions (minute against instant or day) nothing is
    /// compared, which §8 row R15 records as unguarded.
    #[test]
    fn a_re_stamped_minute_is_disputed_on_the_arriving_row_with_the_earlier_minute() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let first = closed_attempt(&mut store, 0, Verdict::Usable)?;
        store.record(first, &[minute_row(&store, &minute_spec())?], at(5))?;
        let second = closed_attempt(&mut store, 1, Verdict::Usable)?;
        let twin = minute_row(
            &store,
            &Spec {
                valid: A_LATER_PUBLICATION_MINUTE,
                observed: OBSERVED_LATER,
                ..spec(STORY)
            },
        )?;
        store.record(second, &[twin], at(6))?;
        assert_eq!(
            disputes(&store)?,
            vec![(0, None), (1, Some(stamped(A_PUBLICATION_MINUTE)))]
        );
        Ok(())
    }

    /// The ledger's publication clock: a minute is not fresh news until it has closed, and is
    /// then reported as the stored minute and not as an instant inside it.
    #[test]
    fn a_minute_is_not_the_newest_publication_until_its_minute_has_closed() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let closed = closed_attempt(&mut store, 0, Verdict::Usable)?;
        let read_inside = Spec {
            observed: A_PUBLICATION_MINUTE,
            ..minute_spec()
        };
        store.record(closed, &[minute_row(&store, &read_inside)?], at(5))?;
        assert_eq!(
            [
                INSIDE_THAT_MINUTE,
                ONE_MICROSECOND_BEFORE_THAT_MINUTE_CLOSES,
                THAT_MINUTE_CLOSES
            ]
            .map(|at| newest_visible_publication(&store.db, at).ok().flatten()),
            [None, None, Some(stamped(A_PUBLICATION_MINUTE))]
        );
        Ok(())
    }

    /// The observation half: mutation 2026-11-15 dropped `AND observed_at <= ?1` from the minute
    /// branch and nothing was red, because the test above observes at the minute's start.
    #[test]
    fn a_minute_fetched_after_it_closed_is_not_the_newest_publication_before_the_fetch(
    ) -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let closed = closed_attempt(&mut store, 0, Verdict::Usable)?;
        let read_later = Spec {
            observed: OBSERVED_LATER,
            ..minute_spec()
        };
        store.record(closed, &[minute_row(&store, &read_later)?], at(400))?;
        assert_eq!(
            (
                newest_visible_publication(&store.db, THAT_MINUTE_CLOSES)?,
                newest_visible_publication(&store.db, &stamped(OBSERVED_LATER))?
            ),
            (None, Some(stamped(A_PUBLICATION_MINUTE)))
        );
        Ok(())
    }

    /// A1.12: the stored column and the digested value must be the same number, or two
    /// implementations of the canonical form disagree over a row neither can be wrong about.
    #[test]
    fn a_stored_instant_keeps_the_microsecond_the_item_id_digests() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let closed = closed_attempt(&mut store, 0, Verdict::Usable)?;
        let precise = row(
            &store,
            &Spec {
                valid: VALID_AT_PAST_THE_SECOND,
                ..spec(STORY)
            },
        )?;
        store.record(closed, &[precise], at(5))?;
        let stored: String = store
            .db
            .query_row("SELECT valid_at FROM items", [], |row| row.get(0))?;
        assert_eq!(stored, "1970-01-01T00:01:40.123456Z");
        Ok(())
    }

    #[test]
    fn a_receipt_closed_against_another_sink_is_refused_rather_than_filing_items_under_it(
    ) -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let slot = AttemptSlot::open(&mut store, &request(), &RunId::new(RUN), 0, at(1))?;
        let answer = RawResponse::new(
            slot.attempt_id().clone(),
            STATUS_OK,
            PAYLOAD.to_vec(),
            at(2),
            at(3),
        );
        let outcome = Outcome::decide(
            &Verdict::Usable,
            VENDOR_ROWS,
            &Baseline::from_trailing_ok(&TRAILING_OK),
        );
        let closed = slot.close(&mut Elsewhere, outcome, &Ok(answer), at(4))?;
        let stored = rows(&store, 3)?;
        let refused = store.record(closed, &stored, at(5));
        assert!(
            matches!(refused, Err(StoreError::AttemptNotClosed { .. })),
            "{refused:?}"
        );
        assert_eq!(count(&store, "items")?, 0);
        Ok(())
    }

    #[test]
    fn the_baseline_counts_decide_reads_are_this_sources_own_ok_attempts_and_not_its_failures(
    ) -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let usable = closed_attempt(&mut store, 0, Verdict::Usable)?;
        store.record(usable, &[], at(5))?;
        let failed = closed_attempt(&mut store, 1, Verdict::PermanentFailure)?;
        store.record(failed, &[], at(6))?;
        let theirs = closed_attempt_from(&mut store, OTHER_SOURCE, 2, Verdict::Usable)?;
        store.record(theirs, &[], at(7))?;
        assert_eq!(
            store.ok_vendor_rows(&SourceId::new(SOURCE))?,
            vec![VENDOR_ROWS]
        );
        Ok(())
    }

    #[test]
    fn an_abandoned_attempt_records_its_interrupted_outcome_and_no_items() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let slot = AttemptSlot::open(&mut store, &request(), &RunId::new(RUN), 0, at(1))?;
        let in_flight = slot.attempt_id().clone();
        drop(slot);
        let closed = abandon(&mut store, in_flight, at(9), &RunId::new(SWEEPING_RUN))?;
        let committed = store.record(closed, &[], at(10))?;
        assert_eq!(
            (committed.items_committed, outcome_column(&store)?),
            (0, "interrupted".to_owned())
        );
        Ok(())
    }

    /// `abandon` writes a `Finalised` event, so the door accepted item rows under an attempt the
    /// sweep gave up on - found by review 2026-11-13. Those rows would carry archive content
    /// under an attempt whose counts, baseline and outcome nobody measured.
    #[test]
    fn an_abandoned_attempt_may_not_carry_items_even_though_it_was_closed() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        let slot = AttemptSlot::open(&mut store, &request(), &RunId::new(RUN), 0, at(1))?;
        let in_flight = slot.attempt_id().clone();
        drop(slot);
        let closed = abandon(&mut store, in_flight, at(9), &RunId::new(SWEEPING_RUN))?;
        let offered = rows(&store, 2)?;
        let refused = store.record(closed, &offered, at(10));
        assert!(
            matches!(refused, Err(StoreError::ItemsUnderAbandonedAttempt { .. })),
            "{refused:?}"
        );
        Ok(())
    }

    #[test]
    fn a_ledger_write_the_store_refuses_reaches_the_caller_as_a_named_error() -> TestResult {
        let temp = TempDir::new()?;
        let mut store = open(&temp)?;
        store
            .db
            .execute_batch("DROP TABLE items; DROP TABLE attempts;")?;
        let refused = AttemptSlot::open(&mut store, &request(), &RunId::new(RUN), 0, at(1));
        assert!(
            matches!(
                refused,
                Err(LedgerError::WriteFailed {
                    event: "opened",
                    ..
                })
            ),
            "{refused:?}"
        );
        Ok(())
    }
}
