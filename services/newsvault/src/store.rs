//! The commit log, the `seq` assigner and the one door an item enters by. The on-disk layout is
//! design.md section 5 and the ledger row is architecture.md section 3.2; neither is restated.
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
//! # use news_provider::{AttemptSlot, Baseline, Outcome, RawResponse, Request, RunId, SourceId,
//! #                     TargetWindow, Verdict};
//! # use pit::{ObservationTime, ValidTime};
//! # fn legal(root: std::path::PathBuf) -> Result<(), Box<dyn std::error::Error>> {
//! let run = RunId::new("2026-11-13T00:00:00Z");
//! let mut committer = Committer::open(root, &run)?;
//! let request = Request {
//!     source: SourceId::new("edgar_press"),
//!     target_window: TargetWindow::new("2026-11-10"),
//!     url: "https://example.test/filing".to_owned(),
//!     headers: Vec::new(),
//!     params: Vec::new(),
//! };
//! let at = ObservationTime::new(DateTime::UNIX_EPOCH);
//! let raw = committer.put_blob(b"{}")?;
//! let mut slot = AttemptSlot::open(&mut committer, &request, &run, 0, at)?;
//! let answer = RawResponse::new(slot.attempt_id().clone(), 200, b"{}".to_vec(), at, at);
//! let outcome = Outcome::decide(&Verdict::Usable, 1, 1, &Baseline::from_trailing_ok(&[1]));
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
//!     valid: ValidTime::new(DateTime::UNIX_EPOCH),
//!     observed: at,
//!     raw_blob: raw,
//!     body_blob: None,
//! }];
//! let _committed = committer.record(closed, &rows, at)?;
//! # Ok(())
//! # }
//! ```

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::PathBuf;

use canon::Digest;
use chrono::{DateTime, SecondsFormat, Utc};
use news_provider::{
    AttemptEvent, AttemptReceipt, Cost, LedgerError, OutcomeKind, RunId, Sink, SourceId,
};
use pit::datedness::{Datedness, Published};
use pit::{ObservationTime, ValidTime, Visibility};
use rusqlite::{params, Connection, OptionalExtension};

use crate::blobs::{BlobDigest, BlobStore};

const LOCK_FILE: &str = "LOCK";
const COMMIT_LOG: &str = "commit.sqlite3";
const BLOB_DIR: &str = "blobs";
const BLOB_SPACE: &str = "sha256";
/// One constant for the label `record` writes and `ok_vendor_rows` reads, so the two cannot drift.
const OUTCOME_OK: &str = "ok";

/// `seq` is `INTEGER PRIMARY KEY AUTOINCREMENT` so the column never reuses a value after a delete,
/// and the value itself comes from a committed `seq_reservations` row: measured 2026-11-13 on
/// SQLite 3.54.0, two inserts inside one transaction took 2 and 3, and after `ROLLBACK`
/// `sqlite_sequence` was back to 1 and the next insert took 2 again.
const SCHEMA: &str = "
PRAGMA foreign_keys = ON;
PRAGMA synchronous = FULL;
CREATE TABLE IF NOT EXISTS attempts (
    attempt_id            TEXT PRIMARY KEY,
    run_id                TEXT NOT NULL,
    source                TEXT NOT NULL,
    target_window         TEXT NOT NULL,
    filter_params         TEXT NOT NULL,
    opened_at             TEXT NOT NULL,
    requested_at          TEXT,
    received_at           TEXT,
    transport_status      INTEGER,
    clock_nonmonotonic    INTEGER,
    outcome               TEXT,
    vendor_rows           INTEGER,
    items_new             INTEGER,
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
    valid_refused     TEXT,
    observed_at       TEXT NOT NULL,
    valid_disputed    INTEGER NOT NULL,
    earliest_valid_at TEXT,
    raw_blob          TEXT NOT NULL REFERENCES blobs(digest),
    body_blob         TEXT REFERENCES blobs(digest),
    committed_at      TEXT NOT NULL,
    UNIQUE(item_id, attempt_id),
    -- `valid_refused` is the publication value an observation preceded: section 5 row 4.
    -- No reachable failure today: `publication()` returns exactly one `Some` on every path, so
    -- nothing in this crate can trip this clause. It binds a later writer and is not enforcement.
    CHECK ((valid_at IS NULL) <> (valid_refused IS NULL))
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
";

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
    /// A replay must be a no-op, which it can only be when it is the SAME claim: a second
    /// `record` of one attempt carrying a different item set is a different claim and was
    /// accepted silently, returning `changed: false` and filing nothing - found by review
    /// 2026-11-13.
    #[error("attempt {attempt} was already recorded with a different item set")]
    ReplayWithDifferentItems { attempt: String },
    #[error("no blob is stored under {digest}")]
    BlobAbsent { digest: String },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
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
    pub valid: ValidTime,
    pub observed: ObservationTime,
    pub raw_blob: BlobDigest,
    pub body_blob: Option<BlobDigest>,
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

/// The single writer. Holds the lease for its whole life, because dropping the lock file releases
/// the flock.
#[derive(Debug)]
pub struct Committer {
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
        let db = Connection::open(root.join(COMMIT_LOG))?;
        db.execute_batch(SCHEMA)?;
        Ok(Self {
            run: run.clone(),
            db,
            blobs: BlobStore::new(root.join(BLOB_DIR).join(BLOB_SPACE)),
            pending: Vec::new(),
            _lease: lease,
        })
    }

    /// Stores a payload under its own digest, before any transaction opens.
    pub fn put_blob(&self, payload: &[u8]) -> Result<BlobDigest, StoreError> {
        self.blobs.put(self.run.as_str(), payload)
    }

    /// The counts `news_provider::Outcome::decide` is asked against, oldest first. How many of
    /// them make a baseline is the caller's: `Baseline::from_trailing_ok` states the attempt
    /// count it used, and no floor fires in this slice (design.md section 2.4).
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
        if !self.was_opened(&attempt)? {
            return Err(StoreError::AttemptNotOpened { attempt });
        }
        if !self.pending.iter().any(|event| closes(event, &attempt)) {
            return Err(StoreError::AttemptNotClosed { attempt });
        }
        if !rows.is_empty() && closed.outcome().kind() == OutcomeKind::Interrupted {
            return Err(StoreError::ItemsUnderAbandonedAttempt { attempt });
        }
        let mut lengths = Vec::new();
        for digest in referenced_blobs(rows) {
            let bytes = self.blobs.len_of(&digest)?;
            lengths.push((digest, bytes));
        }
        let reserved = self.reserve(&attempt, rows.len(), committed_at)?;
        let events: Vec<AttemptEvent> = self
            .pending
            .iter()
            .filter(|event| attempt_of(event) == attempt)
            .cloned()
            .collect();
        let stamped = stamp(committed_at.inner());
        let mut blobs_indexed = 0;
        let mut bytes = 0;
        let transaction = self.db.transaction()?;
        for event in &events {
            apply(&transaction, event)?;
        }
        for (digest, length) in &lengths {
            let inserted = transaction.execute(
                "INSERT INTO blobs (digest, bytes, first_indexed_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(digest) DO NOTHING",
                params![
                    digest.hex(),
                    i64::try_from(*length).unwrap_or(i64::MAX),
                    stamped
                ],
            )?;
            if inserted == 1 {
                blobs_indexed += 1;
                bytes += length;
            }
        }
        let mut items_new: i64 = 0;
        if let Some((seq_lo, _)) = reserved {
            for (offset, row) in rows.iter().enumerate() {
                let story = row.story_id.hex();
                let observed_at = stamp(row.observed.inner());
                let (valid_at, refused) = publication(row);
                let (disputed, earliest) = dispute(&transaction, story, valid_at.as_ref())?;
                if wins(first_observed(&transaction, story)?.as_ref(), &observed_at) {
                    items_new += 1;
                }
                transaction.execute(
                    "INSERT INTO items
                         (seq, item_id, story_id, doc_id, attempt_id, valid_at, valid_refused,
                          observed_at, valid_disputed, earliest_valid_at, raw_blob, body_blob,
                          committed_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    params![
                        seq_lo + i64::try_from(offset).unwrap_or(0),
                        row.item_id.hex(),
                        story,
                        row.doc_id.as_ref().map(Digest::hex),
                        attempt,
                        valid_at,
                        refused,
                        observed_at,
                        disputed,
                        earliest,
                        row.raw_blob.hex(),
                        row.body_blob.as_ref().map(BlobDigest::hex),
                        stamped
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
    /// rolls back is otherwise handed out again (see [`SCHEMA`]).
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
            opened_at,
        } = event
        {
            self.db.execute(
                "INSERT INTO attempts
                     (attempt_id, run_id, source, target_window, filter_params, opened_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(attempt_id) DO NOTHING",
                params![
                    attempt_id.hex(),
                    run.as_str(),
                    source.as_str(),
                    target_window.as_str(),
                    filter_params.hex(),
                    stamp(opened_at.inner())
                ],
            )?;
        }
        Ok(())
    }
}

impl Sink for Committer {
    /// `Opened` commits here, under the `synchronous = FULL` of [`SCHEMA`], so it is on disk
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

/// [`Datedness::Undated`]'s only producer. An item whose observation precedes its publication is
/// stored with its evidence rather than counted as unusable: architecture.md section 5 row 4.
fn datedness(row: &ItemRow) -> Datedness {
    match Visibility::from_live(row.valid, row.observed) {
        Ok(window) => Datedness::Dated(window),
        Err(_) => Datedness::Undated {
            observed: row.observed,
        },
    }
}

/// The publication value as its two columns: the one a cutoff may read, and the refused one kept
/// as evidence. A date has no instant to offer either, so it goes to the same column.
fn publication(row: &ItemRow) -> (Option<String>, Option<String>) {
    match datedness(row).published() {
        Published::At(valid) => (Some(stamp(valid.inner())), None),
        Published::On { .. } | Published::Unknown => (None, Some(stamp(row.valid.inner()))),
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
/// store already holds under its `story_id`. Set on the ARRIVING row, which is what leaves the
/// earlier twin byte-identical: architecture.md section 3.1's two rows.
fn dispute(
    db: &Connection,
    story: &str,
    valid_at: Option<&String>,
) -> Result<(bool, Option<String>), StoreError> {
    let Some(valid_at) = valid_at else {
        return Ok((false, None));
    };
    let (low, high): (Option<String>, Option<String>) = db.query_row(
        "SELECT MIN(valid_at), MAX(valid_at) FROM items
         WHERE story_id = ?1 AND valid_at IS NOT NULL",
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
                     baseline_ratio = ?7, slots = ?8, closed_at = ?9
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
                    stamp(closed_at.inner())
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
    use canon::Stamp;
    use chrono::{DateTime, TimeDelta, Utc};
    use news_provider::{
        abandon, AttemptEvent, AttemptSlot, Baseline, Outcome, RawResponse, Request, SourceId,
        TargetWindow, Verdict,
    };
    use pit::ValidTime;
    use tempfile::TempDir;

    use super::{
        first_observed, fs, params, stamp, Committer, ItemRow, LedgerError, ObservationTime, RunId,
        Sink, StoreError,
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
    const CORRECTED_PAYLOAD: &[u8] = b"{\"filings\": 2}";
    const SENTINEL: &[u8] = b"a staged file a crash left behind";
    const STATUS_OK: u16 = 200;
    /// Distinct values, so a swap of the two columns is visible. 59 is the day the collapse
    /// outcome exists for (design.md section 2.4).
    const VENDOR_ROWS: u32 = 59;
    /// A permanent failure brought nothing back, so the two verdicts carry different counts and a
    /// swap of their outcome labels is visible in what the baseline reader returns.
    const FAILED_VENDOR_ROWS: u32 = 0;
    const ITEMS_NEW: u32 = 7;
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
            url: URL.to_owned(),
            headers: Vec::new(),
            params: Vec::new(),
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
            valid: ValidTime::new(instant(spec.valid)),
            observed: ObservationTime::new(instant(spec.observed)),
            raw_blob: store.put_blob(spec.payload)?,
            body_blob: None,
        })
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
            ITEMS_NEW,
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
            ITEMS_NEW,
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
                 || '|' || IFNULL(valid_refused, '') || '|' || observed_at
                 || '|' || valid_disputed || '|' || IFNULL(earliest_valid_at, '')
                 || '|' || raw_blob || '|' || IFNULL(body_blob, '') || '|' || committed_at
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
            ITEMS_NEW,
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
