//! The gate's own store: `gate.sqlite` and `blobs/<sha256>`, in a directory that is never the
//! vault. Every table is insert-only by trigger. What that does not cover is
//! `docs/design/architecture.md` section 8 row MG9.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};

use canon::FileDigest;
use chrono::{DateTime, Utc};
use rusqlite::backup::{Backup, StepResult};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::cap::Spent;
use crate::coverage::{PriorCoverage, WindowCount};
use crate::question::QuestionDoc;
use crate::request::DecideRequest;
use crate::response::{Outcome, Reading};

pub const DB_FILE: &str = "gate.sqlite";
pub const BLOB_DIR: &str = "blobs";
const LOCK_FILE: &str = "LOCK";
const STAGING_PREFIX: &str = ".staging-";
const USER_VERSION: i64 = 2;
const ALL_PAGES: i32 = -1;
const QUICK_CHECK_OK: &str = "ok";
/// canon's instant rendering, so text order is time order and the first ten bytes are the UTC day.
const INSTANT_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.6fZ";
const DAY_BYTES: usize = 10;
pub const TABLES: [&str; 7] = [
    "question_docs",
    "floors",
    "run_events",
    "requests",
    "labels",
    "prior_coverage",
    "attempt_events",
];
/// Answered requests of one question version and build before a measured cost per byte is
/// reported. Below it `shadow run --dry-run` prints `null`, never a guess.
pub const ESTIMATE_MIN_REQUESTS: i64 = 100;
/// SQLite's name for a database that is no file.
const IN_MEMORY: &str = ":memory:";

/// The one place this crate opens SQLite. `temp_store = MEMORY`:
/// `docs/design/architecture.md` section 8 row T1.
fn connect(path: &Path, flags: OpenFlags) -> rusqlite::Result<Connection> {
    let db = Connection::open_with_flags(path, flags)?;
    db.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(db)
}

const SCHEMA: &str = "
CREATE TABLE question_docs (
    question_version TEXT PRIMARY KEY, name TEXT NOT NULL, file_sha256 TEXT NOT NULL);
CREATE TABLE floors (
    question_version TEXT NOT NULL, model TEXT NOT NULL, start_seq INTEGER NOT NULL,
    run_id TEXT NOT NULL, PRIMARY KEY (question_version, model));
CREATE TABLE run_events (
    run_id TEXT NOT NULL,
    event TEXT NOT NULL CHECK (event IN ('started','ended','cost_estimate_broken','cost_model_acked')),
    at TEXT NOT NULL, detail TEXT NOT NULL, PRIMARY KEY (run_id, event));
CREATE TABLE requests (
    idempotency_key TEXT PRIMARY KEY, question_version TEXT NOT NULL, model TEXT NOT NULL,
    repeat_index INTEGER NOT NULL, state_digest TEXT NOT NULL, state_bytes INTEGER NOT NULL,
    body_bytes INTEGER NOT NULL, state_truncated INTEGER NOT NULL, request_bytes INTEGER NOT NULL,
    request_sha256 TEXT NOT NULL);
CREATE TABLE labels (
    story_id TEXT NOT NULL, item_id TEXT NOT NULL, seq INTEGER NOT NULL, observed_at TEXT NOT NULL,
    idempotency_key TEXT NOT NULL REFERENCES requests(idempotency_key), run_id TEXT NOT NULL,
    PRIMARY KEY (story_id, idempotency_key));
CREATE TABLE prior_coverage (
    story_id TEXT NOT NULL, coverage_version TEXT NOT NULL, snapshot_id TEXT NOT NULL,
    run_id TEXT NOT NULL, tag_count INTEGER NOT NULL, tags TEXT, windows TEXT,
    PRIMARY KEY (story_id, coverage_version),
    CHECK ((tag_count = 0 AND tags IS NULL AND windows IS NULL)
        OR (tag_count > 0 AND tags IS NOT NULL AND windows IS NOT NULL
            AND tag_count = json_array_length(tags))));
CREATE TABLE attempt_events (
    attempt_id TEXT NOT NULL,
    event TEXT NOT NULL CHECK (event IN ('opened','answered','finalised','abandoned')),
    at TEXT NOT NULL,
    idempotency_key TEXT, run_id TEXT, worst_case_micro_usd INTEGER,
    requested_at TEXT, http_status INTEGER, transport_failure TEXT, response_sha256 TEXT,
    outcome TEXT, cost_micro_usd INTEGER, answering_model TEXT,
    PRIMARY KEY (attempt_id, event));
CREATE INDEX attempt_events_by_key ON attempt_events (idempotency_key) WHERE event = 'opened';
";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{}: {why}", path.display())]
    Io { path: PathBuf, why: String },
    #[error("the store refused a statement: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("another run holds {}", .0.display())]
    LeaseHeld(PathBuf),
    #[error("{} is at user_version {found}; this build reads {USER_VERSION}", path.display())]
    SchemaVersion { path: PathBuf, found: i64 },
    #[error("blob {hex} does not hold the bytes its name states")]
    BlobAltered { hex: String },
    #[error("this store has no blob directory (a dry run of an absent store)")]
    NoBlobs,
    #[error("the attempt's permit was already taken")]
    PermitAlreadyTaken,
    #[error("an answer for attempt {answered} was handed to the slot of attempt {slot}")]
    AnswerFromAnotherAttempt { slot: String, answered: String },
    #[error("{} is not a whole database: {why}", path.display())]
    Malformed { path: PathBuf, why: String },
    #[error(
        "the prior-coverage row of story {0} does not read back as the record it was written from"
    )]
    CoverageUnreadable(String),
}

fn io(path: &Path) -> impl Fn(std::io::Error) -> StoreError + '_ {
    move |why| StoreError::Io {
        path: path.to_path_buf(),
        why: why.to_string(),
    }
}

#[must_use]
pub fn stamp(at: DateTime<Utc>) -> String {
    at.format(INSTANT_FORMAT).to_string()
}

/// A blob that is on disk under its sha256. Its field is private and only [`Store::put_blob`]
/// builds one, so a row naming a response blob cannot be written before the blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBlob(String);

impl StoredBlob {
    #[must_use]
    pub fn hex(&self) -> &str {
        &self.0
    }
}

/// Where a story stands under one question version, build and repeat index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoryState {
    /// No request linked to it was ever opened: unlinked, or linked and refused by the cap.
    NeverAsked,
    /// A linked request has an attempt and none closed with an answer.
    Asked,
    /// A linked request closed with this outcome; `Ok` wins over the two unusable ones.
    Answered(Outcome),
}

/// One finalised attempt of a run, as the envelope reads it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Closed {
    pub outcome: String,
    pub cost_micro_usd: Option<i64>,
    pub response_sha256: Option<String>,
    pub answering_model: Option<String>,
    pub sent_model: String,
}

#[derive(Debug)]
pub struct Store {
    db: Connection,
    blobs: Option<PathBuf>,
    _lease: Option<File>,
}

impl Store {
    /// The writer's door: creates the directory on first use and holds its `LOCK` until dropped.
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        let blobs = root.join(BLOB_DIR);
        std::fs::create_dir_all(&blobs).map_err(io(&blobs))?;
        let lock_path = root.join(LOCK_FILE);
        let lease = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(io(&lock_path))?;
        match lease.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(StoreError::LeaseHeld(lock_path)),
            Err(TryLockError::Error(why)) => return Err(io(&lock_path)(why)),
        }
        let path = root.join(DB_FILE);
        let db = connect(&path, OpenFlags::default())?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        let found: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        match found {
            0 => create(&db)?,
            USER_VERSION => {}
            found => return Err(StoreError::SchemaVersion { path, found }),
        }
        Ok(Self {
            db,
            blobs: Some(blobs),
            _lease: Some(lease),
        })
    }

    /// A dry run's door: read-only, no lease, nothing created. An absent store reads as an empty
    /// one held in memory.
    pub fn read_only(root: &Path) -> Result<Self, StoreError> {
        let path = root.join(DB_FILE);
        if !path.is_file() {
            let db = connect(Path::new(IN_MEMORY), OpenFlags::default())?;
            create(&db)?;
            return Ok(Self {
                db,
                blobs: None,
                _lease: None,
            });
        }
        let db = connect(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let found: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if found != USER_VERSION {
            return Err(StoreError::SchemaVersion { path, found });
        }
        Ok(Self {
            db,
            blobs: Some(root.join(BLOB_DIR)),
            _lease: None,
        })
    }

    /// A copy's door: an existing store read-only, under its `LOCK` so no writer is inside a
    /// transaction or between a blob and its row. `LOCK` is opened without write access: the
    /// backup mounts the store read-only. No `LOCK` or no database is `None`.
    pub fn leased_read(root: &Path) -> Result<Option<Self>, StoreError> {
        let lock_path = root.join(LOCK_FILE);
        if !lock_path.is_file() || !root.join(DB_FILE).is_file() {
            return Ok(None);
        }
        let lease = File::open(&lock_path).map_err(io(&lock_path))?;
        match lease.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(StoreError::LeaseHeld(lock_path)),
            Err(TryLockError::Error(why)) => return Err(io(&lock_path)(why)),
        }
        let mut store = Self::read_only(root)?;
        // Under `LOCK` a locked database is a writer that took no lease: refused, not waited for.
        store.db.busy_timeout(std::time::Duration::ZERO)?;
        store._lease = Some(lease);
        Ok(Some(store))
    }

    /// SQLite's online backup into a new file at `target`, then the copy's `quick_check`. A
    /// source another connection has locked is `LeaseHeld`.
    pub fn copy_database(&self, target: &Path) -> Result<(), StoreError> {
        let mut copy = Connection::open(target)?;
        {
            let backup = Backup::new(&self.db, &mut copy)?;
            loop {
                match backup.step(ALL_PAGES)? {
                    StepResult::Done => break,
                    StepResult::More => {}
                    _ => {
                        let held = self.db.path().unwrap_or(DB_FILE);
                        return Err(StoreError::LeaseHeld(PathBuf::from(held)));
                    }
                }
            }
        }
        quick_check(&copy, target)?;
        copy.close().map_err(|(_, failure)| failure)?;
        Ok(())
    }

    /// `PRAGMA quick_check`, which reads every table page; `COUNT(*)` reads an index alone.
    pub fn quick_check(&self) -> Result<(), StoreError> {
        quick_check(&self.db, Path::new(self.db.path().unwrap_or(DB_FILE)))
    }

    /// Rows per table of [`TABLES`].
    pub fn row_counts(&self) -> Result<BTreeMap<&'static str, i64>, StoreError> {
        TABLES
            .iter()
            .map(|table| {
                let rows =
                    self.db
                        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
                Ok((*table, rows))
            })
            .collect()
    }

    /// Every blob a row names, once: question files, requests and responses. Three plain scans:
    /// as one `UNION … ORDER BY`, SQLite's sorter needed a temp file at 100,000 names and the
    /// backup's container has none (`disk I/O error`, measured 2026-11-24, section 3.5).
    pub fn blob_names(&self) -> Result<BTreeSet<String>, StoreError> {
        let mut names = BTreeSet::new();
        for select in [
            "SELECT file_sha256 FROM question_docs",
            "SELECT request_sha256 FROM requests",
            "SELECT response_sha256 FROM attempt_events WHERE response_sha256 IS NOT NULL",
        ] {
            let mut statement = self.db.prepare(select)?;
            for name in statement.query_map([], |row| row.get(0))? {
                names.insert(name?);
            }
        }
        Ok(names)
    }

    /// How many entries the blob directory holds, staging files of a killed write among them.
    pub fn blob_files(&self) -> Result<u64, StoreError> {
        let dir = self.blobs.as_ref().ok_or(StoreError::NoBlobs)?;
        let mut files = 0;
        for entry in std::fs::read_dir(dir).map_err(io(dir))? {
            entry.map_err(io(dir))?;
            files += 1;
        }
        Ok(files)
    }

    /// Stores `bytes` under their sha256: staged, fsynced, renamed, and the directory fsynced. A
    /// standing file must already hold these bytes.
    pub fn put_blob(&self, bytes: &[u8]) -> Result<StoredBlob, StoreError> {
        let dir = self.blobs.as_ref().ok_or(StoreError::NoBlobs)?;
        let hex = FileDigest::of(bytes).hex().to_owned();
        let path = dir.join(&hex);
        if path.exists() {
            self.read_blob(&hex)?;
            return Ok(StoredBlob(hex));
        }
        let staging = dir.join(format!("{STAGING_PREFIX}{hex}"));
        let mut file = File::create(&staging).map_err(io(&staging))?;
        file.write_all(bytes).map_err(io(&staging))?;
        file.sync_all().map_err(io(&staging))?;
        std::fs::rename(&staging, &path).map_err(io(&path))?;
        File::open(dir)
            .and_then(|dir| dir.sync_all())
            .map_err(io(dir))?;
        Ok(StoredBlob(hex))
    }

    /// The blob's bytes, refused when they no longer hash to their name.
    pub fn read_blob(&self, hex: &str) -> Result<Vec<u8>, StoreError> {
        let dir = self.blobs.as_ref().ok_or(StoreError::NoBlobs)?;
        let path = dir.join(hex);
        let bytes = std::fs::read(&path).map_err(io(&path))?;
        if FileDigest::of(&bytes).hex() != hex {
            return Err(StoreError::BlobAltered {
                hex: hex.to_owned(),
            });
        }
        Ok(bytes)
    }

    pub fn record_question_doc(&self, doc: &QuestionDoc, file: &[u8]) -> Result<(), StoreError> {
        self.put_blob(file)?;
        self.db.execute(
            "INSERT OR IGNORE INTO question_docs (question_version, name, file_sha256)
             VALUES (?1, ?2, ?3)",
            params![doc.version(), doc.name(), doc.file_sha256()],
        )?;
        Ok(())
    }

    pub fn floor(&self, question_version: &str, model: &str) -> Result<Option<i64>, StoreError> {
        Ok(self
            .db
            .query_row(
                "SELECT start_seq FROM floors WHERE question_version = ?1 AND model = ?2",
                params![question_version, model],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn set_floor(
        &self,
        question_version: &str,
        model: &str,
        start_seq: i64,
        run_id: &str,
    ) -> Result<(), StoreError> {
        self.db.execute(
            "INSERT INTO floors (question_version, model, start_seq, run_id)
             VALUES (?1, ?2, ?3, ?4)",
            params![question_version, model, start_seq, run_id],
        )?;
        Ok(())
    }

    pub fn run_event(
        &self,
        run_id: &str,
        event: &str,
        at: DateTime<Utc>,
        detail: &serde_json::Value,
    ) -> Result<(), StoreError> {
        self.db.execute(
            "INSERT INTO run_events (run_id, event, at, detail) VALUES (?1, ?2, ?3, ?4)",
            params![run_id, event, stamp(at), detail.to_string()],
        )?;
        Ok(())
    }

    /// The run whose vendor cost exceeded its worst case and that no later run acknowledged.
    pub fn unacked_cost_break(&self) -> Result<Option<String>, StoreError> {
        Ok(self
            .db
            .query_row(
                "SELECT b.run_id FROM run_events b WHERE b.event = 'cost_estimate_broken'
                   AND NOT EXISTS (SELECT 1 FROM run_events a WHERE a.event = 'cost_model_acked'
                                   AND json_extract(a.detail, '$.acknowledges') = b.run_id)
                 ORDER BY b.at LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Whether `run_id` broke the cost estimate (`None`: it did not) and, if so, whether a run has
    /// acknowledged it.
    pub fn cost_break(&self, run_id: &str) -> Result<Option<bool>, StoreError> {
        Ok(self
            .db
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM run_events a WHERE a.event = 'cost_model_acked'
                                AND json_extract(a.detail, '$.acknowledges') = b.run_id)
                   FROM run_events b WHERE b.event = 'cost_estimate_broken' AND b.run_id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Runs with a `started` row and no `ended` one: killed, or still running under another lease.
    pub fn runs_left_open(&self, but_not: &str) -> Result<u64, StoreError> {
        let open: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM run_events s WHERE s.event = 'started' AND s.run_id <> ?1
               AND NOT EXISTS (SELECT 1 FROM run_events e
                               WHERE e.event = 'ended' AND e.run_id = s.run_id)",
            [but_not],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(open).unwrap_or_default())
    }

    pub fn insert_request(
        &self,
        request: &DecideRequest,
        question_version: &str,
        model: &str,
    ) -> Result<(), StoreError> {
        self.put_blob(&request.bytes)?;
        let size = |bytes: usize| i64::try_from(bytes).unwrap_or(i64::MAX);
        self.db.execute(
            "INSERT OR IGNORE INTO requests (idempotency_key, question_version, model,
                 repeat_index, state_digest, state_bytes, body_bytes, state_truncated,
                 request_bytes, request_sha256)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                request.idempotency_key,
                question_version,
                model,
                request.repeat_index,
                request.state_digest,
                size(request.state_bytes),
                size(request.body_bytes),
                request.state_truncated,
                size(request.bytes.len()),
                request.request_sha256,
            ],
        )?;
        Ok(())
    }

    /// Links a story to the request that labels it. A second link of the same pair is a no-op.
    pub fn link_label(
        &self,
        story_id: &str,
        item_id: &str,
        seq: i64,
        observed_at: &str,
        idempotency_key: &str,
        run_id: &str,
    ) -> Result<(), StoreError> {
        self.db.execute(
            "INSERT OR IGNORE INTO labels (story_id, item_id, seq, observed_at, idempotency_key,
                 run_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![story_id, item_id, seq, observed_at, idempotency_key, run_id],
        )?;
        Ok(())
    }

    pub fn has_coverage(&self, story_id: &str, coverage_version: &str) -> Result<bool, StoreError> {
        Ok(self.db.query_row(
            "SELECT EXISTS (SELECT 1 FROM prior_coverage
                             WHERE story_id = ?1 AND coverage_version = ?2)",
            params![story_id, coverage_version],
            |row| row.get(0),
        )?)
    }

    /// One record per story and coverage version; the first written stays (section 3.5).
    pub fn insert_coverage(
        &self,
        story_id: &str,
        coverage_version: &str,
        snapshot_id: &str,
        run_id: &str,
        record: &PriorCoverage,
    ) -> Result<(), StoreError> {
        let unreadable = |_| StoreError::CoverageUnreadable(story_id.to_owned());
        let (count, tags, windows) = match record {
            PriorCoverage::NoTag => (0, None, None),
            PriorCoverage::Tagged { tags, windows } => (
                i64::try_from(tags.len()).unwrap_or(i64::MAX),
                Some(serde_json::to_string(tags).map_err(unreadable)?),
                Some(serde_json::to_string(windows).map_err(unreadable)?),
            ),
        };
        self.db.execute(
            "INSERT OR IGNORE INTO prior_coverage (story_id, coverage_version, snapshot_id,
                 run_id, tag_count, tags, windows) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                story_id,
                coverage_version,
                snapshot_id,
                run_id,
                count,
                tags,
                windows
            ],
        )?;
        Ok(())
    }

    /// The records this run wrote under one coverage version, in the order it wrote them.
    pub fn coverage_by(
        &self,
        run_id: &str,
        coverage_version: &str,
    ) -> Result<Vec<PriorCoverage>, StoreError> {
        let mut statement = self.db.prepare(
            "SELECT story_id, tags, windows FROM prior_coverage
              WHERE run_id = ?1 AND coverage_version = ?2 ORDER BY rowid",
        )?;
        let rows: Vec<(String, Option<String>, Option<String>)> = statement
            .query_map(params![run_id, coverage_version], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<_, _>>()?;
        rows.into_iter()
            .map(|(story_id, tags, windows)| {
                let (Some(tags), Some(windows)) = (tags, windows) else {
                    return Ok(PriorCoverage::NoTag);
                };
                let tags = serde_json::from_str::<Vec<String>>(&tags);
                let windows = serde_json::from_str::<Vec<WindowCount>>(&windows);
                match (tags, windows) {
                    (Ok(tags), Ok(windows)) => Ok(PriorCoverage::Tagged { tags, windows }),
                    _ => Err(StoreError::CoverageUnreadable(story_id)),
                }
            })
            .collect()
    }

    /// The answer an attempt of this request closed with, by `Outcome::answered`; `Ok` first.
    pub fn request_outcome(&self, idempotency_key: &str) -> Result<Option<Outcome>, StoreError> {
        let mut statement = self.db.prepare_cached(
            "SELECT f.outcome FROM attempt_events o
               JOIN attempt_events f ON f.attempt_id = o.attempt_id AND f.event = 'finalised'
              WHERE o.event = 'opened' AND o.idempotency_key = ?1",
        )?;
        let names: Vec<String> = statement
            .query_map([idempotency_key], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        let mut answers = names
            .iter()
            .filter_map(|name| Outcome::named(name))
            .filter(|outcome| outcome.answered());
        let first = answers.next();
        Ok(if answers.any(|outcome| outcome == Outcome::Ok) {
            Some(Outcome::Ok)
        } else {
            first
        })
    }

    fn request_asked(&self, idempotency_key: &str) -> Result<bool, StoreError> {
        Ok(self.db.query_row(
            "SELECT EXISTS (SELECT 1 FROM attempt_events
                             WHERE event = 'opened' AND idempotency_key = ?1)",
            [idempotency_key],
            |row| row.get(0),
        )?)
    }

    pub fn story_state(
        &self,
        story_id: &str,
        question_version: &str,
        model: &str,
        repeat_index: u32,
    ) -> Result<StoryState, StoreError> {
        let mut statement = self.db.prepare_cached(
            "SELECT l.idempotency_key FROM labels l
               JOIN requests r ON r.idempotency_key = l.idempotency_key
              WHERE l.story_id = ?1 AND r.question_version = ?2 AND r.model = ?3
                AND r.repeat_index = ?4",
        )?;
        let keys: Vec<String> = statement
            .query_map(
                params![story_id, question_version, model, repeat_index],
                |row| row.get(0),
            )?
            .collect::<Result<_, _>>()?;
        let (mut unusable, mut asked) = (None, false);
        for key in &keys {
            match self.request_outcome(key)? {
                Some(Outcome::Ok) => return Ok(StoryState::Answered(Outcome::Ok)),
                Some(other) => unusable = Some(other),
                None => asked |= self.request_asked(key)?,
            }
        }
        Ok(match unusable {
            Some(outcome) => StoryState::Answered(outcome),
            None if asked => StoryState::Asked,
            None => StoryState::NeverAsked,
        })
    }

    /// Runs `work` in one transaction: a process killed inside it leaves none of its rows.
    pub(crate) fn atomically(
        &self,
        work: impl FnOnce() -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let transaction = self.db.unchecked_transaction()?;
        work()?;
        Ok(transaction.commit()?)
    }

    pub(crate) fn write_opened(
        &self,
        attempt_id: &str,
        idempotency_key: &str,
        run_id: &str,
        worst_case_micro_usd: i64,
        at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.db.execute(
            "INSERT INTO attempt_events (attempt_id, event, at, idempotency_key, run_id,
                 worst_case_micro_usd) VALUES (?1, 'opened', ?2, ?3, ?4, ?5)",
            params![
                attempt_id,
                stamp(at),
                idempotency_key,
                run_id,
                worst_case_micro_usd
            ],
        )?;
        Ok(())
    }

    pub(crate) fn write_answered(
        &self,
        attempt_id: &str,
        requested_at: DateTime<Utc>,
        received_at: DateTime<Utc>,
        http_status: Option<u16>,
        transport_failure: Option<&str>,
        response: Option<&StoredBlob>,
    ) -> Result<(), StoreError> {
        self.db.execute(
            "INSERT INTO attempt_events (attempt_id, event, at, requested_at, http_status,
                 transport_failure, response_sha256) VALUES (?1, 'answered', ?2, ?3, ?4, ?5, ?6)",
            params![
                attempt_id,
                stamp(received_at),
                stamp(requested_at),
                http_status,
                transport_failure,
                response.map(StoredBlob::hex)
            ],
        )?;
        Ok(())
    }

    pub(crate) fn write_finalised(
        &self,
        attempt_id: &str,
        reading: &Reading,
        at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.db.execute(
            "INSERT INTO attempt_events (attempt_id, event, at, outcome, cost_micro_usd,
                 answering_model) VALUES (?1, 'finalised', ?2, ?3, ?4, ?5)",
            params![
                attempt_id,
                stamp(at),
                reading.outcome.name(),
                reading.cost_micro_usd,
                reading.answering_model
            ],
        )?;
        Ok(())
    }

    /// Closes every attempt a dead process left open: `abandoned`, then `finalised` with no cost,
    /// so its worst case stays counted as spent. Returns how many.
    pub fn sweep(&self, at: DateTime<Utc>, by_run: &str) -> Result<u64, StoreError> {
        let mut statement = self.db.prepare(
            "SELECT o.attempt_id FROM attempt_events o WHERE o.event = 'opened'
               AND NOT EXISTS (SELECT 1 FROM attempt_events f
                               WHERE f.attempt_id = o.attempt_id AND f.event = 'finalised')",
        )?;
        let open: Vec<String> = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        for attempt_id in &open {
            self.db.execute(
                "INSERT INTO attempt_events (attempt_id, event, at, run_id)
                 VALUES (?1, 'abandoned', ?2, ?3)",
                params![attempt_id, stamp(at), by_run],
            )?;
            let reading = Reading {
                outcome: Outcome::Abandoned,
                cost_micro_usd: None,
                answering_model: None,
            };
            self.write_finalised(attempt_id, &reading, at)?;
        }
        Ok(u64::try_from(open.len()).unwrap_or(u64::MAX))
    }

    /// Micro-USD spent by this run, on this UTC day and ever. The vendor's cost where an attempt
    /// closed with one, the attempt's worst case otherwise.
    pub fn spent(&self, run_id: &str, at: DateTime<Utc>) -> Result<Spent, StoreError> {
        let day = &stamp(at)[..DAY_BYTES];
        let sum = |scope: &str, value: &str| -> Result<i64, StoreError> {
            Ok(self.db.query_row(
                &format!(
                    "SELECT COALESCE(SUM(COALESCE(f.cost_micro_usd, o.worst_case_micro_usd)), 0)
                       FROM attempt_events o
                       LEFT JOIN attempt_events f
                         ON f.attempt_id = o.attempt_id AND f.event = 'finalised'
                      WHERE o.event = 'opened' AND {scope}"
                ),
                [value],
                |row| row.get(0),
            )?)
        };
        Ok(Spent {
            run: sum("o.run_id = ?1", run_id)?,
            day: sum("substr(o.at, 1, 10) = ?1", day)?,
            total: sum("?1 = ?1", "")?,
        })
    }

    /// `(micro-USD, request bytes)` summed over this version's and build's attempts that closed
    /// with a vendor cost, once [`ESTIMATE_MIN_REQUESTS`] of them exist.
    pub fn measured_cost(
        &self,
        question_version: &str,
        model: &str,
    ) -> Result<Option<(i64, i64)>, StoreError> {
        let (count, cost, bytes): (i64, i64, i64) = self.db.query_row(
            "SELECT COUNT(*), COALESCE(SUM(f.cost_micro_usd), 0), COALESCE(SUM(r.request_bytes), 0)
               FROM attempt_events o
               JOIN attempt_events f ON f.attempt_id = o.attempt_id AND f.event = 'finalised'
               JOIN requests r ON r.idempotency_key = o.idempotency_key
              WHERE o.event = 'opened' AND f.cost_micro_usd IS NOT NULL
                AND r.question_version = ?1 AND r.model = ?2",
            params![question_version, model],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok((count >= ESTIMATE_MIN_REQUESTS && bytes > 0).then_some((cost, bytes)))
    }

    /// Every attempt this run closed, in the order it opened them.
    pub fn closed_by(&self, run_id: &str) -> Result<Vec<Closed>, StoreError> {
        let mut statement = self.db.prepare(
            "SELECT f.outcome, f.cost_micro_usd, a.response_sha256, f.answering_model, r.model
               FROM attempt_events o
               JOIN attempt_events f ON f.attempt_id = o.attempt_id AND f.event = 'finalised'
               LEFT JOIN attempt_events a ON a.attempt_id = o.attempt_id AND a.event = 'answered'
               JOIN requests r ON r.idempotency_key = o.idempotency_key
              WHERE o.event = 'opened' AND o.run_id = ?1 ORDER BY o.rowid",
        )?;
        let closed = statement
            .query_map([run_id], |row| {
                Ok(Closed {
                    outcome: row.get(0)?,
                    cost_micro_usd: row.get(1)?,
                    response_sha256: row.get(2)?,
                    answering_model: row.get(3)?,
                    sent_model: row.get(4)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(closed)
    }

    /// `(observed_at, received_at)` per story whose request this run closed with an answer.
    pub fn label_times(&self, run_id: &str) -> Result<Vec<(String, String)>, StoreError> {
        let mut statement = self.db.prepare(
            "SELECT l.observed_at, a.at, f.outcome
               FROM attempt_events o
               JOIN attempt_events a ON a.attempt_id = o.attempt_id AND a.event = 'answered'
               JOIN attempt_events f ON f.attempt_id = o.attempt_id AND f.event = 'finalised'
               JOIN labels l ON l.idempotency_key = o.idempotency_key
              WHERE o.event = 'opened' AND o.run_id = ?1",
        )?;
        let times: Vec<(String, String, String)> = statement
            .query_map([run_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<_, _>>()?;
        Ok(times
            .into_iter()
            .filter(|(_, _, outcome)| Outcome::named(outcome).is_some_and(Outcome::answered))
            .map(|(observed_at, received_at, _)| (observed_at, received_at))
            .collect())
    }
}

fn quick_check(db: &Connection, path: &Path) -> Result<(), StoreError> {
    let verdicts: Vec<String> = db
        .prepare("PRAGMA quick_check")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    if verdicts != [QUICK_CHECK_OK] {
        return Err(StoreError::Malformed {
            path: path.to_path_buf(),
            why: verdicts.join("; "),
        });
    }
    Ok(())
}

/// The schema, its triggers and the version in one transaction: a process killed inside it leaves
/// `user_version` 0 and no table, so the next open starts again.
fn create(db: &Connection) -> Result<(), StoreError> {
    let mut batch = format!("BEGIN IMMEDIATE;{SCHEMA}");
    for table in TABLES {
        for verb in ["UPDATE", "DELETE"] {
            batch.push_str(&format!(
                "CREATE TRIGGER {table}_no_{verb} BEFORE {verb} ON {table}
                 BEGIN SELECT RAISE(ABORT, '{table} is insert-only'); END;"
            ));
        }
    }
    batch.push_str(&format!("PRAGMA user_version = {USER_VERSION}; COMMIT;"));
    Ok(db.execute_batch(&batch)?)
}

#[cfg(test)]
mod tests {
    use super::{Store, StoreError, BLOB_DIR, DB_FILE, TABLES, USER_VERSION};
    use crate::coverage::{PriorCoverage, WindowCount};
    use chrono::{DateTime, Utc};
    use rusqlite::Connection;
    use tempfile::TempDir;

    type TestResult = Result<(), Box<dyn std::error::Error>>;
    const REFUSED_WITHIN: std::time::Duration = std::time::Duration::from_secs(1);

    fn at(text: &str) -> Result<DateTime<Utc>, chrono::ParseError> {
        text.parse()
    }

    /// One row in every table, written through the store's own doors.
    fn seeded(root: &TempDir) -> TestResult {
        let store = Store::open(root.path())?;
        let noon = at("2026-11-23T12:00:00Z")?;
        store.db.execute_batch(
            "INSERT INTO question_docs VALUES ('v', 'n', 'f');
             INSERT INTO requests VALUES ('k', 'v', 'm', 0, 'd', 1, 1, 0, 1, 's');
             INSERT INTO labels VALUES ('story', 'item', 1, 'o', 'k', 'run');",
        )?;
        store.insert_coverage("story", "c", "snapshot", "run", &PriorCoverage::NoTag)?;
        store.set_floor("v", "m", 0, "run")?;
        store.run_event("run", "started", noon, &serde_json::json!({}))?;
        store.write_opened("run#0", "k", "run", 7, noon)?;
        Ok(())
    }

    /// Section 8 row MG9. The table list is read from the database, so a table `SCHEMA` creates
    /// and `TABLES` omits is red. Mutations 2026-11-23, each red: the `DELETE` trigger loop
    /// skipped; `floors` dropped from `TABLES`.
    #[test]
    fn an_update_or_delete_of_any_gate_table_is_refused() -> TestResult {
        let root = TempDir::new()?;
        seeded(&root)?;
        let raw = Connection::open(root.path().join(DB_FILE))?;
        let mut created: Vec<String> = raw
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        created.sort();
        let mut listed = TABLES.to_vec();
        listed.sort_unstable();
        assert_eq!(created, listed);
        for table in &created {
            let rows: i64 =
                raw.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
            assert_eq!(rows, 1, "{table}");
            for statement in [
                format!("UPDATE {table} SET rowid = rowid + 100"),
                format!("DELETE FROM {table}"),
            ] {
                let refused = raw.execute_batch(&statement).err().map(|e| e.to_string());
                assert_eq!(
                    refused,
                    Some(format!("{table} is insert-only")),
                    "{statement}"
                );
            }
        }
        Ok(())
    }

    /// Mutations 2026-11-23, each red: any `user_version` accepted by `open`; the `foreign_keys`
    /// pragma removed; `read_only` opening an existing database read-write.
    #[test]
    fn another_schema_version_an_unknown_request_and_a_write_through_the_dry_door_are_refused(
    ) -> TestResult {
        let (root, newer) = (TempDir::new()?, TempDir::new()?);
        let store = Store::open(root.path())?;
        let orphan = store.link_label("story", "item", 1, "o", "no-such-request", "run");
        assert!(matches!(orphan, Err(StoreError::Sqlite(_))), "{orphan:?}");
        drop(store);
        let dry = Store::read_only(root.path())?;
        let written = dry.set_floor("v", "m", 0, "run");
        assert!(matches!(written, Err(StoreError::Sqlite(_))), "{written:?}");
        // 1 is the store of the build before `prior_coverage`; section 3.5 names 2.
        assert_eq!(USER_VERSION, 2);
        for other in [1, USER_VERSION + 1] {
            Connection::open(newer.path().join(DB_FILE))?.pragma_update(
                None,
                "user_version",
                other,
            )?;
            for opened in [Store::open(newer.path()), Store::read_only(newer.path())] {
                assert!(
                    matches!(opened, Err(StoreError::SchemaVersion { found, .. }) if found == other),
                    "{other}"
                );
            }
        }
        Ok(())
    }

    /// Section 8 row MG12, where the mutations of 2026-11-23 are listed.
    #[test]
    fn a_coverage_row_is_no_tag_or_counts_never_both_and_the_first_record_stays() -> TestResult {
        let root = TempDir::new()?;
        let store = Store::open(root.path())?;
        let counted = PriorCoverage::Tagged {
            tags: vec!["LRGP".to_owned(), "SMQ".to_owned()],
            windows: vec![WindowCount {
                days: 1,
                stories: 1,
                complete: true,
                by_tag: [("LRGP".to_owned(), 1), ("SMQ".to_owned(), 0)].into(),
            }],
        };
        store.insert_coverage("tagged", "c/1", "cut-1", "run", &counted)?;
        store.insert_coverage("tagged", "c/1", "cut-2", "run", &PriorCoverage::NoTag)?;
        store.insert_coverage("bare", "c/1", "cut-1", "run", &PriorCoverage::NoTag)?;
        store.insert_coverage("bare", "c/2", "cut-1", "run", &counted)?;
        store.insert_coverage("elsewhere", "c/1", "cut-1", "another-run", &counted)?;
        assert_eq!(
            store.coverage_by("run", "c/1")?,
            [counted, PriorCoverage::NoTag]
        );
        assert!(store.has_coverage("bare", "c/2")?);
        assert!(!store.has_coverage("tagged", "c/2")?);
        assert!(!store.has_coverage("absent", "c/1")?);
        let tag_count: i64 = store.db.query_row(
            "SELECT tag_count FROM prior_coverage WHERE story_id = 'tagged'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(tag_count, 2);
        for row in [
            "('s', 'c', 'cut', 'run', 0, NULL, '[]')",
            "('s', 'c', 'cut', 'run', 0, '[]', NULL)",
            "('s', 'c', 'cut', 'run', 0, '[]', '[]')",
            "('s', 'c', 'cut', 'run', 1, NULL, NULL)",
            "('s', 'c', 'cut', 'run', 1, '[\"A\",\"B\"]', '[]')",
        ] {
            let refused = store
                .db
                .execute_batch(&format!("INSERT INTO prior_coverage VALUES {row}"));
            assert!(refused.is_err(), "{row}");
        }
        Ok(())
    }

    /// Section 8 row MG19. The copy goes through SQLite's own locks, so a writer that took no
    /// lease and holds the file is refused at once, not waited for and not copied mid-write.
    #[test]
    fn a_copy_of_a_database_a_leaseless_writer_holds_is_refused_and_taken_once_it_lets_go(
    ) -> TestResult {
        let (root, out) = (TempDir::new()?, TempDir::new()?);
        seeded(&root)?;
        let Some(reader) = Store::leased_read(root.path())? else {
            return Err("no store".into());
        };
        let writer = Connection::open(root.path().join(DB_FILE))?;
        writer.execute_batch("BEGIN EXCLUSIVE")?;
        let asked = std::time::Instant::now();
        let refused = reader.copy_database(&out.path().join(DB_FILE));
        assert!(
            matches!(refused, Err(StoreError::LeaseHeld(_))),
            "{refused:?}"
        );
        // rusqlite's default busy timeout is 5 s.
        assert!(asked.elapsed() < REFUSED_WITHIN, "{:?}", asked.elapsed());
        writer.execute_batch("ROLLBACK")?;
        let again = TempDir::new()?;
        reader.copy_database(&again.path().join(DB_FILE))?;
        let rows: i64 = Connection::open(again.path().join(DB_FILE))?.query_row(
            "SELECT COUNT(*) FROM question_docs",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(rows, 1);
        Ok(())
    }

    /// Section 8 row MG19: a run cannot write a blob and its row between the copy of the database
    /// and the copy of the blobs.
    #[test]
    fn a_copys_reader_keeps_the_lease_until_it_is_dropped() -> TestResult {
        let root = TempDir::new()?;
        seeded(&root)?;
        let Some(reader) = Store::leased_read(root.path())? else {
            return Err("no store".into());
        };
        let writer = Store::open(root.path());
        assert!(
            matches!(writer, Err(StoreError::LeaseHeld(_))),
            "{:?}",
            writer.err()
        );
        drop(reader);
        Store::open(root.path())?;
        Ok(())
    }

    #[test]
    fn a_second_writer_is_refused_while_the_first_holds_the_lease() -> TestResult {
        let root = TempDir::new()?;
        let first = Store::open(root.path())?;
        assert!(matches!(
            Store::open(root.path()),
            Err(StoreError::LeaseHeld(_))
        ));
        drop(first);
        Store::open(root.path())?;
        Ok(())
    }

    /// Section 8 row MG6. Mutation 2026-11-23, `read_blob` returning without the hash check: red.
    #[test]
    fn a_blob_whose_bytes_changed_is_refused_on_read_and_on_a_second_put() -> TestResult {
        let root = TempDir::new()?;
        let store = Store::open(root.path())?;
        let blob = store.put_blob(b"as received")?;
        let hex = blob.hex();
        assert_eq!(store.put_blob(b"as received")?, blob);
        assert_eq!(store.read_blob(hex)?, b"as received");
        std::fs::write(root.path().join(BLOB_DIR).join(hex), b"edited")?;
        assert!(matches!(
            store.read_blob(hex),
            Err(StoreError::BlobAltered { .. })
        ));
        assert!(matches!(
            store.put_blob(b"as received"),
            Err(StoreError::BlobAltered { .. })
        ));
        Ok(())
    }

    /// Section 8 row MG3, the half the comparison cannot see. Mutations 2026-11-23: the
    /// `LEFT JOIN` made an inner join (an open attempt drops out of `spent`): red; `COALESCE`
    /// reduced to `f.cost_micro_usd` (a cost-less close counts as free): red; the day scope
    /// summing this run only: red (run `prior` is the same day and another run).
    #[test]
    fn an_open_an_abandoned_and_a_costless_attempt_each_count_at_their_worst_case() -> TestResult {
        use crate::response::{Outcome, Reading};
        let root = TempDir::new()?;
        let store = Store::open(root.path())?;
        let (yesterday, today) = (at("2026-11-22T23:59:59Z")?, at("2026-11-23T00:00:01Z")?);
        store.db.execute_batch(
            "INSERT INTO requests VALUES ('k', 'v', 'm', 0, 'd', 1, 1, 0, 1, 's')",
        )?;
        let close = |attempt: &str, outcome, cost| {
            store.write_finalised(
                attempt,
                &Reading {
                    outcome,
                    cost_micro_usd: cost,
                    answering_model: None,
                },
                today,
            )
        };
        store.write_opened("old#0", "k", "old", 1_000, yesterday)?;
        close("old#0", Outcome::Ok, Some(300))?;
        store.write_opened("prior#0", "k", "prior", 1_000, today)?;
        close("prior#0", Outcome::Ok, Some(40))?;
        store.write_opened("run#0", "k", "run", 1_000, today)?;
        close("run#0", Outcome::Ok, Some(250))?;
        store.write_opened("run#1", "k", "run", 1_000, today)?;
        close("run#1", Outcome::Failed, None)?;
        store.write_opened("run#2", "k", "run", 1_000, today)?;
        let spent = store.spent("run", today)?;
        assert_eq!((spent.run, spent.day, spent.total), (2_250, 2_290, 2_590));
        assert_eq!(store.sweep(today, "next")?, 1);
        assert_eq!(store.spent("run", today)?, spent);
        Ok(())
    }

    #[test]
    fn a_cost_break_latches_until_a_run_acknowledges_that_run() -> TestResult {
        let root = TempDir::new()?;
        let store = Store::open(root.path())?;
        let noon = at("2026-11-23T12:00:00Z")?;
        let detail = serde_json::json!({});
        assert_eq!(store.unacked_cost_break()?, None);
        store.run_event("run-1", "cost_estimate_broken", noon, &detail)?;
        assert_eq!(store.unacked_cost_break()?.as_deref(), Some("run-1"));
        let other = serde_json::json!({"acknowledges": "run-0"});
        store.run_event("run-2", "cost_model_acked", noon, &other)?;
        assert_eq!(store.unacked_cost_break()?.as_deref(), Some("run-1"));
        let this = serde_json::json!({"acknowledges": "run-1"});
        assert_eq!(store.cost_break("run-1")?, Some(false));
        store.run_event("run-3", "cost_model_acked", noon, &this)?;
        assert_eq!(store.unacked_cost_break()?, None);
        assert_eq!(store.cost_break("run-1")?, Some(true));
        assert_eq!(store.cost_break("run-0")?, None);
        Ok(())
    }

    /// `PRAGMA temp_store` reads 2 for MEMORY. Section 8 row T1, the gate's three doors: the
    /// writer, a reader of a store on disk and a dry run's store held in memory.
    #[test]
    fn every_connection_the_gate_opens_keeps_its_temporary_data_in_memory() -> TestResult {
        const MEMORY: i64 = 2;
        let temp_store = |store: &Store| -> rusqlite::Result<i64> {
            store
                .db
                .query_row("PRAGMA temp_store", [], |row| row.get(0))
        };
        let (root, absent) = (TempDir::new()?, TempDir::new()?);
        let writer = temp_store(&Store::open(root.path())?)?;
        let reader = temp_store(&Store::read_only(root.path())?)?;
        let in_memory = temp_store(&Store::read_only(absent.path())?)?;
        assert_eq!((writer, reader, in_memory), (MEMORY, MEMORY, MEMORY));
        Ok(())
    }
}
