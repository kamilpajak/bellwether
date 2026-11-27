//! The filings store: `LOCK`, `filings.sqlite3` and `blobs/` under `--store` (architecture.md
//! section 12.2). Schema version 1 is created or another version refused; no migration exists
//! before the deploy step. The writer is the attempt sink: `Opened` is committed before the slot
//! hands out its permit, and `Finalised` commits the answer's blob row and listings with it.

use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use news_provider::{
    abandon, AttemptEvent, AttemptId, AttemptSlot, LedgerError, Request, RunId, Sink,
};
use pit::ObservationTime;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use vault_core::blobs::{BlobStore, Stored, WritePolicy};
use vault_core::lease::Lease;
use vault_core::sqlite::{connect, read_only};
use vault_core::LeaseError;

use crate::error::FilingsError;

pub const LOCK: &str = "LOCK";
pub const LOG: &str = "filings.sqlite3";
pub const BLOBS: &str = "blobs";
/// One form for every payload: a plain zstd frame (architecture.md section 12.3).
pub const WRITE_POLICY: WritePolicy = WritePolicy::PlainZstd;
pub const SCHEMA_VERSION: i64 = 1;
const DAY: &str = "%Y-%m-%d";

const SCHEMA: &str = "
CREATE TABLE runs (
    run_id      TEXT PRIMARY KEY,
    kind        TEXT NOT NULL CHECK (kind IN ('collect')),
    started_at  TEXT NOT NULL,
    finished_at TEXT,
    exit        INTEGER,
    stopped_by  TEXT CHECK (stopped_by IN ('deadline', 'robots', 'gate_refused', 'error',
                                           'interrupted'))
);
CREATE TABLE blobs (
    digest       TEXT PRIMARY KEY,
    bytes        INTEGER NOT NULL,
    stored_bytes INTEGER NOT NULL
);
CREATE TABLE attempts (
    seq              INTEGER PRIMARY KEY AUTOINCREMENT,
    attempt_id       TEXT NOT NULL UNIQUE,
    run_id           TEXT NOT NULL REFERENCES runs (run_id),
    kind             TEXT NOT NULL CHECK (kind IN ('robots', 'listing', 'index')),
    target           TEXT NOT NULL,
    url              TEXT NOT NULL,
    forms            TEXT CHECK ((kind = 'index') = (forms IS NOT NULL)),
    body_limit       INTEGER NOT NULL,
    opened_at        TEXT NOT NULL,
    requested_at     TEXT,
    received_at      TEXT,
    status           INTEGER,
    outcome          TEXT CHECK (outcome IN ('ok', 'empty', 'failed', 'interrupted', 'not_sent')),
    verdict          TEXT CHECK (verdict IN ('usable', 'empty', 'permanent_failure', 'transient_failure')),
    closed_at        TEXT,
    truncated        INTEGER CHECK (truncated IN (0, 1)),
    raw_blob         TEXT REFERENCES blobs (digest),
    response_headers TEXT
);
CREATE INDEX attempts_by_target ON attempts (kind, target, seq);
CREATE INDEX attempts_open ON attempts (closed_at) WHERE closed_at IS NULL;
CREATE TABLE listings (
    index_day TEXT NOT NULL CHECK (index_day GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'),
    cik       TEXT NOT NULL,
    accession TEXT NOT NULL,
    form      TEXT NOT NULL,
    filed_day TEXT NOT NULL CHECK (filed_day GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'),
    first_seq INTEGER NOT NULL REFERENCES attempts (seq),
    PRIMARY KEY (index_day, cik, accession)
);
";

/// The RFC 3339 text every instant is stored as: microseconds and `Z`, so text order is time order.
#[must_use]
pub fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

#[must_use]
pub fn day_text(day: NaiveDate) -> String {
    day.format(DAY).to_string()
}

/// A stored day, or `input_unparseable` naming it.
pub fn day_of(text: &str) -> Result<NaiveDate, FilingsError> {
    NaiveDate::parse_from_str(text, DAY).map_err(|why| FilingsError::Unparseable {
        what: format!("the stored day {text:?}"),
        why: why.to_string(),
    })
}

/// What an attempt asks, `attempts.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Robots,
    Listing,
    Index,
}

impl Kind {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Robots => "robots",
            Self::Listing => "listing",
            Self::Index => "index",
        }
    }
}

/// The attempt row's columns the events do not carry, set by [`Writer::open_slot`] alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub kind: Kind,
    pub target: String,
    pub url: String,
    /// The form list an index attempt is asked under; `None` for every other kind.
    pub forms: Option<Vec<String>>,
    pub body_limit: u64,
}

impl Ask {
    /// `<kind>/<target>`, the request's `target_window`.
    #[must_use]
    pub fn window(&self) -> String {
        format!("{}/{}", self.kind.name(), self.target)
    }
}

/// A body as stored: its blob, its length as received and whether it was cut at the limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Body {
    pub stored: Stored,
    pub bytes: u64,
    pub truncated: bool,
    /// `Headers::to_json` of the answer.
    pub headers: String,
}

/// One row of a listed form in a usable index payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub cik: String,
    pub accession: String,
    pub form: String,
    /// The row's own `Date Filed`.
    pub filed_day: NaiveDate,
}

/// What the closing `Finalised` commits beside the attempt's outcome.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Commit {
    pub body: Option<Body>,
    pub index_day: Option<NaiveDate>,
    pub listed: Vec<Listed>,
}

/// The one writer: the lease, the log and the blob tree.
pub struct Writer {
    db: Connection,
    blobs: BlobStore,
    run: RunId,
    pending: Option<Ask>,
    staged: Option<Commit>,
    _lease: Lease,
}

fn write_failed(event: &'static str, detail: impl ToString) -> LedgerError {
    LedgerError::WriteFailed {
        event,
        detail: detail.to_string(),
    }
}

impl Writer {
    /// Takes the lease without waiting, then creates schema version 1 or refuses another.
    pub fn open(store: &Path, run: &RunId) -> Result<Self, FilingsError> {
        std::fs::create_dir_all(store)?;
        let lease = Lease::take(&store.join(LOCK)).map_err(|why| match why {
            LeaseError::LeaseHeld(lock) => FilingsError::LeaseHeld(lock),
            LeaseError::Io(why) => FilingsError::Io(why),
        })?;
        let log = store.join(LOG);
        let db = connect(&log, OpenFlags::default())?;
        db.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;")?;
        create_or_refuse(&db, &log)?;
        Ok(Self {
            db,
            blobs: BlobStore::new(store.join(BLOBS), WRITE_POLICY),
            run: run.clone(),
            pending: None,
            staged: None,
            _lease: lease,
        })
    }

    #[must_use]
    pub const fn db(&self) -> &Connection {
        &self.db
    }

    #[must_use]
    pub const fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    /// Stores a payload before anything parses it.
    pub fn put(&self, payload: &[u8]) -> Result<Stored, FilingsError> {
        Ok(self.blobs.put(self.run.as_str(), payload)?)
    }

    /// Describes the request and opens its slot in one call, so the `Opened` row cannot be
    /// written with another request's url or form list.
    pub fn open_slot(
        &mut self,
        ask: Ask,
        request: &Request,
        ordinal: u32,
        at: ObservationTime,
    ) -> Result<AttemptSlot, LedgerError> {
        self.pending = Some(ask);
        let run = self.run.clone();
        let opened = AttemptSlot::open(self, request, None, &run, ordinal, at);
        self.pending = None;
        opened
    }

    /// What the next `Finalised` commits with the outcome.
    pub fn stage(&mut self, commit: Commit) {
        self.staged = Some(commit);
    }

    pub fn start_run(&self, started: DateTime<Utc>) -> Result<(), FilingsError> {
        self.db.execute(
            "INSERT INTO runs (run_id, kind, started_at) VALUES (?1, 'collect', ?2)",
            params![self.run.as_str(), stamp(started)],
        )?;
        Ok(())
    }

    pub fn finish_run(
        &self,
        at: DateTime<Utc>,
        exit: u8,
        stopped_by: Option<&str>,
    ) -> Result<(), FilingsError> {
        self.db.execute(
            "UPDATE runs SET finished_at = ?2, exit = ?3, stopped_by = ?4 WHERE run_id = ?1",
            params![self.run.as_str(), stamp(at), exit, stopped_by],
        )?;
        Ok(())
    }

    /// Closes every attempt and run a dead writer left open as `interrupted`. Run under the lease
    /// before this run's own row exists. Returns (attempts, runs).
    pub fn sweep(&mut self, at: DateTime<Utc>) -> Result<(usize, usize), FilingsError> {
        let open: Vec<String> = self
            .db
            .prepare("SELECT attempt_id FROM attempts WHERE closed_at IS NULL ORDER BY seq")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        let run = self.run.clone();
        for attempt in &open {
            let closed = abandon(
                self,
                AttemptId::recorded(attempt.as_str()),
                ObservationTime::new(at),
                &run,
            )?;
            drop(closed);
        }
        let runs = self.db.execute(
            "UPDATE runs SET finished_at = ?1, stopped_by = 'interrupted' WHERE finished_at IS NULL",
            [stamp(at)],
        )?;
        Ok((open.len(), runs))
    }

    fn opened(&mut self, event: &AttemptEvent) -> Result<(), LedgerError> {
        let AttemptEvent::Opened {
            attempt_id,
            run,
            target_window,
            opened_at,
            ..
        } = event
        else {
            return Ok(());
        };
        let ask = self
            .pending
            .take()
            .ok_or_else(|| write_failed("opened", "no request was described before the slot"))?;
        if target_window.as_str() != ask.window() {
            return Err(write_failed(
                "opened",
                format!("{} is not {}", target_window.as_str(), ask.window()),
            ));
        }
        let forms = ask
            .forms
            .as_ref()
            .map(|forms| serde_json::Value::from(forms.clone()).to_string());
        self.db
            .execute(
                "INSERT INTO attempts (attempt_id, run_id, kind, target, url, forms, body_limit,
                                       opened_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    attempt_id.hex(),
                    run.as_str(),
                    ask.kind.name(),
                    ask.target,
                    ask.url,
                    forms,
                    i64::try_from(ask.body_limit).unwrap_or(i64::MAX),
                    stamp(opened_at.inner()),
                ],
            )
            .map_err(|why| write_failed("opened", why))?;
        Ok(())
    }

    fn finalised(
        &mut self,
        attempt_id: &AttemptId,
        outcome: &news_provider::Outcome,
        closed_at: ObservationTime,
    ) -> Result<(), rusqlite::Error> {
        let staged = self.staged.take().unwrap_or_default();
        let transaction = self.db.transaction()?;
        let (raw_blob, truncated, headers) = match &staged.body {
            Some(body) => {
                transaction.execute(
                    "INSERT INTO blobs (digest, bytes, stored_bytes) VALUES (?1, ?2, ?3)
                     ON CONFLICT (digest) DO NOTHING",
                    params![
                        body.stored.digest.hex(),
                        i64::try_from(body.bytes).unwrap_or(i64::MAX),
                        i64::try_from(body.stored.stored_bytes).unwrap_or(i64::MAX),
                    ],
                )?;
                (
                    Some(body.stored.digest.hex().to_owned()),
                    Some(body.truncated),
                    Some(body.headers.clone()),
                )
            }
            None => (None, None, None),
        };
        transaction.execute(
            "UPDATE attempts SET outcome = ?2, verdict = ?3, closed_at = ?4, raw_blob = ?5,
                                 truncated = ?6, response_headers = ?7
              WHERE attempt_id = ?1",
            params![
                attempt_id.hex(),
                outcome.kind().name(),
                outcome.verdict().map(news_provider::Verdict::name),
                stamp(closed_at.inner()),
                raw_blob,
                truncated,
                headers,
            ],
        )?;
        if let Some(day) = staged.index_day {
            for row in &staged.listed {
                transaction.execute(
                    "INSERT INTO listings (index_day, cik, accession, form, filed_day, first_seq)
                     SELECT ?2, ?3, ?4, ?5, ?6, seq FROM attempts WHERE attempt_id = ?1
                     ON CONFLICT (index_day, cik, accession) DO NOTHING",
                    params![
                        attempt_id.hex(),
                        day_text(day),
                        row.cik,
                        row.accession,
                        row.form,
                        day_text(row.filed_day),
                    ],
                )?;
            }
        }
        transaction.commit()
    }
}

impl Sink for Writer {
    fn write(&mut self, event: &AttemptEvent) -> Result<(), LedgerError> {
        match event {
            AttemptEvent::Opened { .. } => self.opened(event),
            AttemptEvent::Answered {
                attempt_id,
                requested_at,
                received_at,
                transport_status,
                ..
            } => self
                .db
                .execute(
                    "UPDATE attempts SET requested_at = ?2, received_at = ?3, status = ?4
                      WHERE attempt_id = ?1",
                    params![
                        attempt_id.hex(),
                        stamp(requested_at.inner()),
                        stamp(received_at.inner()),
                        transport_status,
                    ],
                )
                .map(drop)
                .map_err(|why| write_failed("answered", why)),
            AttemptEvent::Finalised {
                attempt_id,
                outcome,
                closed_at,
                ..
            } => self
                .finalised(attempt_id, outcome, *closed_at)
                .map_err(|why| write_failed("finalised", why)),
            // The `Finalised` that follows it closes the row `interrupted`.
            AttemptEvent::Abandoned { .. } => Ok(()),
        }
    }
}

fn create_or_refuse(db: &Connection, log: &Path) -> Result<(), FilingsError> {
    let found: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    match found {
        SCHEMA_VERSION => Ok(()),
        0 => {
            db.execute_batch(&format!(
                "BEGIN IMMEDIATE; {SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
            ))?;
            Ok(())
        }
        found => Err(mismatch(log, found)),
    }
}

fn mismatch(log: &Path, found: i64) -> FilingsError {
    FilingsError::SchemaMismatch {
        path: log.to_path_buf(),
        found,
        expected: SCHEMA_VERSION,
    }
}

/// A reader's connection: read-only, no lease, the version checked. No log is `input_absent`.
pub fn open_read(store: &Path) -> Result<Connection, FilingsError> {
    let log = store.join(LOG);
    if !log.exists() {
        return Err(FilingsError::Absent(log));
    }
    let db = read_only(&log)?;
    let found: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if found == SCHEMA_VERSION {
        Ok(db)
    } else {
        Err(mismatch(&log, found))
    }
}

/// The two attempt reads of the planner and `ledger check`, each served by `attempts_by_target`.
const INDEX_ATTEMPTS: &str =
    "SELECT seq, target, forms, body_limit, outcome, verdict, status, truncated,
       response_headers
  FROM attempts
 WHERE kind = 'index' AND closed_at IS NOT NULL AND (?1 IS NULL OR closed_at <= ?1)
 ORDER BY target, seq";
const HELD_LISTINGS: &str = "SELECT target, raw_blob, max(received_at), max(verdict = 'usable')
  FROM attempts
 WHERE kind = 'listing' AND verdict IN ('usable', 'empty') AND closed_at IS NOT NULL
   AND (?1 IS NULL OR closed_at <= ?1) AND raw_blob IS NOT NULL
 GROUP BY target, raw_blob
 ORDER BY target, min(seq)";

/// A closed index attempt as the planner and `ledger check` read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexAttempt {
    pub seq: i64,
    pub day: NaiveDate,
    pub forms: Vec<String>,
    pub body_limit: u64,
    pub outcome: String,
    pub verdict: Option<String>,
    pub status: Option<i64>,
    pub truncated: bool,
    pub headers: Option<String>,
}

/// Every index attempt closed by `until`, or every closed one; ordered by day, then `seq`.
pub fn index_attempts(
    db: &Connection,
    until: Option<&str>,
) -> Result<Vec<IndexAttempt>, FilingsError> {
    let mut statement = db.prepare(INDEX_ATTEMPTS)?;
    let rows = statement
        .query_map([until], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
                (
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<bool>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ),
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut attempts = Vec::with_capacity(rows.len());
    for (seq, target, forms, body_limit, outcome, (verdict, status, truncated, headers)) in rows {
        let forms: Vec<String> = serde_json::from_str(forms.as_deref().unwrap_or_default())
            .map_err(|why| FilingsError::Unparseable {
                what: format!("the forms of attempt {seq}"),
                why: why.to_string(),
            })?;
        attempts.push(IndexAttempt {
            seq,
            day: day_of(&target)?,
            forms,
            body_limit: u64::try_from(body_limit).unwrap_or(0),
            outcome: outcome.unwrap_or_default(),
            verdict,
            status,
            truncated: truncated.unwrap_or(false),
            headers,
        });
    }
    Ok(attempts)
}

/// One distinct body among the usable or empty listings of a quarter closed by `until`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldListing {
    /// The quarter's label, `attempts.target`.
    pub quarter: String,
    pub blob: String,
    /// The newest `received_at` of the attempts that answered this body.
    pub received: String,
    /// Read `usable`, naming at least one day; else `empty`. One body is one verdict.
    pub usable: bool,
}

/// Every distinct usable or empty listing body of each quarter closed by `until`, oldest first:
/// the ledger and the planner take the union of the days they name (review LBD-1, LBD-2).
pub fn held_listings(
    db: &Connection,
    until: Option<&str>,
) -> Result<Vec<HeldListing>, FilingsError> {
    let mut statement = db.prepare(HELD_LISTINGS)?;
    let rows = statement
        .query_map([until], |row| {
            Ok(HeldListing {
                quarter: row.get(0)?,
                blob: row.get(1)?,
                received: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                usable: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<HeldListing>, _>>()?;
    Ok(rows)
}

/// A stored answer's `Last-Modified`: the instant SEC wrote the index, section 12.3's floor.
pub fn stored_last_modified(headers: Option<&str>) -> Result<Option<String>, FilingsError> {
    let Some(headers) = headers else {
        return Ok(None);
    };
    let headers =
        news_provider::Headers::from_json(headers).map_err(|why| FilingsError::Unparseable {
            what: "stored response headers".to_owned(),
            why: why.to_string(),
        })?;
    Ok(headers.first("last-modified").map(str::to_owned))
}

/// The UTC dates of the collect runs started by `until`, oldest first.
pub fn collect_starts(db: &Connection, until: Option<&str>) -> Result<Vec<String>, FilingsError> {
    let mut statement = db.prepare(
        "SELECT started_at FROM runs
          WHERE kind = 'collect' AND (?1 IS NULL OR started_at <= ?1)
          ORDER BY started_at",
    )?;
    let starts = statement
        .query_map([until], |row| row.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(starts)
}

/// The payload a stored listing names, re-hashed on the way out.
pub fn read_blob(blobs: &BlobStore, hex: &str) -> Result<Vec<u8>, FilingsError> {
    let digest = blobs
        .named(hex)
        .ok_or_else(|| FilingsError::Absent(PathBuf::from(hex)))?;
    Ok(blobs.read_verified(&digest)?)
}

/// The last run started by `until`, for `ledger check`; its close only when made by `until`.
pub fn last_run(db: &Connection, until: &str) -> Result<Option<serde_json::Value>, FilingsError> {
    Ok(db
        .query_row(
            "SELECT run_id, started_at, finished_at, exit, stopped_by FROM runs
              WHERE started_at <= ?1 ORDER BY started_at DESC LIMIT 1",
            [until],
            |row| {
                let finished: Option<String> = row.get(2)?;
                let closed = finished
                    .as_deref()
                    .is_some_and(|finished| finished <= until);
                Ok(serde_json::json!({
                    "run_id": row.get::<_, String>(0)?,
                    "started_at": row.get::<_, String>(1)?,
                    "finished_at": finished.filter(|_| closed),
                    "exit": row.get::<_, Option<i64>>(3)?.filter(|_| closed),
                    "stopped_by": row.get::<_, Option<String>>(4)?.filter(|_| closed),
                }))
            },
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn run() -> RunId {
        RunId::new("filings-test")
    }

    /// Mutation 2026-11-27, any `user_version` accepted: red here.
    #[test]
    fn a_store_is_created_once_and_another_version_is_refused() -> TestResult {
        let temp = TempDir::new()?;
        drop(Writer::open(temp.path(), &run())?);
        drop(Writer::open(temp.path(), &run())?);
        let version = |db: &Connection| -> rusqlite::Result<i64> {
            db.query_row("PRAGMA user_version", [], |row| row.get(0))
        };
        let created = version(&open_read(temp.path())?)?;
        connect(&temp.path().join(LOG), OpenFlags::default())?
            .execute_batch("PRAGMA user_version = 2")?;
        let refused = Writer::open(temp.path(), &run()).err();
        let read = open_read(temp.path()).err();
        assert_eq!(
            (
                created,
                refused.map(|why| why.code().name()),
                read.map(|why| why.code().name())
            ),
            (
                SCHEMA_VERSION,
                Some("input_schema_mismatch"),
                Some("input_schema_mismatch")
            )
        );
        Ok(())
    }

    /// Mutation 2026-11-27, `WRITE_POLICY` `RawUntilDictionary`: red here.
    #[test]
    fn a_filings_store_writes_every_payload_as_a_plain_frame() -> TestResult {
        let temp = TempDir::new()?;
        let writer = Writer::open(temp.path(), &run())?;
        let stored = writer.put(b"a payload of the filings store")?;
        let blobs = writer.blobs();
        assert_eq!(
            (
                blobs.path_of(&stored.digest).exists(),
                std::fs::metadata(blobs.compressed_path_of(&stored.digest))?.len(),
            ),
            (false, stored.stored_bytes)
        );
        Ok(())
    }

    /// `EXPLAIN QUERY PLAN` of the ledger's attempt reads names the index. Mutation 2026-11-27,
    /// `attempts_by_target` dropped from the schema: red here.
    #[test]
    fn each_ledger_read_uses_its_index() -> TestResult {
        let temp = TempDir::new()?;
        let writer = Writer::open(temp.path(), &run())?;
        let plan = |sql: &str| -> rusqlite::Result<String> {
            let mut statement = writer.db().prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
            let details = statement
                .query_map([Option::<String>::None], |row| row.get::<_, String>(3))?
                .collect::<Result<Vec<String>, _>>()?;
            Ok(details.join("; "))
        };
        let reads = [INDEX_ATTEMPTS, HELD_LISTINGS];
        for read in reads {
            let plan = plan(read)?;
            assert!(plan.contains("attempts_by_target"), "{plan}");
        }
        Ok(())
    }

    /// The store refuses a row its keys or CHECKs exclude, and a slot whose described request is
    /// not the one sent. Mutations 2026-11-27, each red here: both pragmas dropped (foreign keys
    /// off); the `forms` CHECK dropped; the slot's window check dropped.
    #[test]
    fn the_store_refuses_a_row_its_schema_or_its_slot_does_not_describe() -> TestResult {
        let temp = TempDir::new()?;
        let mut writer = Writer::open(temp.path(), &run())?;
        writer.start_run(DateTime::UNIX_EPOCH)?;
        let refused = |sql: &str| writer.db().execute(sql, []).is_err();
        let orphan = refused(
            "INSERT INTO listings (index_day, cik, accession, form, filed_day, first_seq)
             VALUES ('2026-11-10', '1', '0000000001-26-000001', '10-K', '2026-11-10', 99)",
        );
        let robots_with_forms = refused(
            "INSERT INTO attempts (attempt_id, run_id, kind, target, url, forms, body_limit,
                                   opened_at)
             VALUES ('a', 'filings-test', 'robots', 'h', 'u', '[]', 1, 't')",
        );
        let listing = crate::discover::listing_ask(sec_edgar::Quarter::of(NaiveDate::MIN));
        let sent = crate::discover::request(&listing, news_provider::Provenance::LiveIngest);
        let at = ObservationTime::new(DateTime::UNIX_EPOCH);
        let other = writer
            .open_slot(crate::discover::robots_ask(), &sent, 0, at)
            .is_err();
        assert_eq!((orphan, robots_with_forms, other), (true, true, true));
        Ok(())
    }
}
