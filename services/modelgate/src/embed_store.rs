//! The `embed` kind's own file, `embed.sqlite` with `embed-blobs/<sha256>`, beside `gate.sqlite`
//! and under the same `LOCK`. It holds its own attempt ledger, so an embed call is never counted
//! against a `decide` cap and removing the two paths removes the kind
//! (`docs/design/architecture.md` section 8 row MG23).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{params, OptionalExtension};

use crate::embed::{from_blob, to_blob, EmbedRequest, Embedding};
use crate::response::Outcome;
use crate::store::{Layout, Store, StoreError};

pub const EMBED_DB_FILE: &str = "embed.sqlite";
pub const EMBED_BLOB_DIR: &str = "embed-blobs";
pub const LIST_NEAREST: &str = "nearest";
pub const LIST_GUARDED: &str = "guarded";
pub const EMBED_TABLES: [&str; 8] = [
    "embed_requests",
    "embed_links",
    "vectors",
    "canaries",
    "comparisons",
    "comparison_candidates",
    "attempt_events",
    "run_events",
];

const SCHEMA: &str = "
CREATE TABLE embed_requests (
    idempotency_key TEXT PRIMARY KEY, space TEXT NOT NULL, repeat_index INTEGER NOT NULL,
    input_digest TEXT NOT NULL, input_bytes INTEGER NOT NULL, body_bytes INTEGER NOT NULL,
    input_truncated INTEGER NOT NULL, request_bytes INTEGER NOT NULL,
    request_sha256 TEXT NOT NULL);
CREATE TABLE embed_links (
    story_id TEXT NOT NULL, item_id TEXT NOT NULL, seq INTEGER NOT NULL, observed_at TEXT NOT NULL,
    idempotency_key TEXT NOT NULL REFERENCES embed_requests(idempotency_key),
    run_id TEXT NOT NULL, PRIMARY KEY (story_id, idempotency_key));
CREATE TABLE vectors (
    idempotency_key TEXT PRIMARY KEY REFERENCES embed_requests(idempotency_key),
    dims INTEGER NOT NULL, norm REAL NOT NULL, encoding TEXT NOT NULL, vector BLOB NOT NULL,
    answering_model TEXT, answering_provider TEXT, run_id TEXT NOT NULL);
CREATE TABLE canaries (
    name TEXT NOT NULL, space TEXT NOT NULL, repeat_index INTEGER NOT NULL,
    idempotency_key TEXT NOT NULL REFERENCES embed_requests(idempotency_key),
    run_id TEXT NOT NULL, self_similarity_ppm INTEGER, pair_similarity_ppm INTEGER,
    PRIMARY KEY (name, space, repeat_index));
CREATE TABLE comparisons (
    id INTEGER PRIMARY KEY, story_id TEXT NOT NULL, compare_version TEXT NOT NULL,
    space TEXT NOT NULL, snapshot_id TEXT NOT NULL, run_id TEXT NOT NULL,
    window_stories INTEGER NOT NULL, candidates INTEGER NOT NULL, unembedded INTEGER NOT NULL,
    own_hosts INTEGER NOT NULL, own_numbers INTEGER NOT NULL, own_tickers INTEGER NOT NULL,
    UNIQUE (story_id, compare_version, space));
CREATE TABLE comparison_candidates (
    comparison_id INTEGER NOT NULL REFERENCES comparisons(id),
    list TEXT NOT NULL CHECK (list IN ('nearest','guarded')),
    rank INTEGER NOT NULL, candidate_story_id TEXT NOT NULL, similarity_ppm INTEGER NOT NULL,
    age_seconds INTEGER NOT NULL, hosts_shared TEXT NOT NULL, numbers_candidate INTEGER NOT NULL,
    numbers_shared INTEGER NOT NULL, tickers_candidate INTEGER NOT NULL,
    tickers_shared INTEGER NOT NULL, length_ratio_ppm INTEGER NOT NULL,
    same_source INTEGER NOT NULL, PRIMARY KEY (comparison_id, list, rank));
";

const EMBED: Layout = Layout {
    db_file: EMBED_DB_FILE,
    blob_dir: EMBED_BLOB_DIR,
    schema: SCHEMA,
    tables: &EMBED_TABLES,
    user_version: 1,
};

/// One finalised embed attempt of a run, as the envelope reads it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedEmbed {
    pub outcome: String,
    pub cost_micro_usd: Option<i64>,
    pub answering_model: Option<String>,
}

/// One stored candidate of a comparison. No field is a verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub list: &'static str,
    pub rank: u32,
    pub story_id: String,
    pub similarity_ppm: i64,
    pub age_seconds: i64,
    pub hosts_shared: Vec<String>,
    pub numbers_candidate: usize,
    pub numbers_shared: usize,
    pub tickers_candidate: usize,
    pub tickers_shared: usize,
    pub length_ratio_ppm: i64,
    pub same_source: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    pub story_id: String,
    /// Earlier stories with a body inside the window, with a vector, and with none.
    pub window_stories: usize,
    pub candidates: usize,
    pub unembedded: usize,
    pub own_hosts: usize,
    pub own_numbers: usize,
    pub own_tickers: usize,
    pub listed: Vec<Candidate>,
}

/// A stored comparison's best candidate, as `dedup distribution` reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Best {
    pub compare_version: String,
    pub space: String,
    pub story_id: String,
    pub unembedded: i64,
    pub own_hosts: i64,
    pub own_numbers: i64,
    /// `None` when the window held no vector at all.
    pub nearest: Option<Candidate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanaryRow {
    pub name: String,
    pub space: String,
    pub repeat_index: u32,
    pub run_id: String,
    pub self_similarity_ppm: Option<i64>,
    pub pair_similarity_ppm: Option<i64>,
}

fn size(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

#[derive(Debug)]
pub struct EmbedStore {
    ledger: Store,
}

impl EmbedStore {
    /// The writer's door; holds the store directory's `LOCK` until dropped.
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        Ok(Self {
            ledger: Store::open_as(root, &EMBED)?,
        })
    }

    /// Read-only, no lease, nothing created. An absent file reads as an empty one in memory.
    pub fn read_only(root: &Path) -> Result<Self, StoreError> {
        Ok(Self {
            ledger: Store::read_only_as(root, &EMBED)?,
        })
    }

    /// The attempt ledger, run events, blobs and spend of this file alone.
    #[must_use]
    pub fn ledger(&self) -> &Store {
        &self.ledger
    }

    pub fn insert_request(&self, request: &EmbedRequest, space: &str) -> Result<(), StoreError> {
        self.ledger.put_blob(&request.bytes)?;
        self.ledger.connection().execute(
            "INSERT OR IGNORE INTO embed_requests (idempotency_key, space, repeat_index,
                 input_digest, input_bytes, body_bytes, input_truncated, request_bytes,
                 request_sha256) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                request.idempotency_key,
                space,
                request.repeat_index,
                request.input_digest,
                size(request.input_bytes),
                size(request.body_bytes),
                request.input_truncated,
                size(request.bytes.len()),
                request.request_sha256,
            ],
        )?;
        Ok(())
    }

    pub fn link(
        &self,
        story_id: &str,
        item_id: &str,
        seq: i64,
        observed_at: &str,
        idempotency_key: &str,
        run_id: &str,
    ) -> Result<(), StoreError> {
        self.ledger.connection().execute(
            "INSERT OR IGNORE INTO embed_links (story_id, item_id, seq, observed_at,
                 idempotency_key, run_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![story_id, item_id, seq, observed_at, idempotency_key, run_id],
        )?;
        Ok(())
    }

    pub fn has_vector(&self, idempotency_key: &str) -> Result<bool, StoreError> {
        Ok(self.ledger.connection().query_row(
            "SELECT EXISTS (SELECT 1 FROM vectors WHERE idempotency_key = ?1)",
            [idempotency_key],
            |row| row.get(0),
        )?)
    }

    /// The decoded copy of an `ok` answer. The record is the response blob; this row is rebuilt
    /// from it by [`Self::ok_response`] when a run died between the two writes.
    pub fn insert_vector(
        &self,
        idempotency_key: &str,
        embedding: &Embedding,
        answering_model: Option<&str>,
        answering_provider: Option<&str>,
        run_id: &str,
    ) -> Result<(), StoreError> {
        self.ledger.connection().execute(
            "INSERT OR IGNORE INTO vectors (idempotency_key, dims, norm, encoding, vector,
                 answering_model, answering_provider, run_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                idempotency_key,
                size(embedding.unit.len()),
                embedding.norm,
                embedding.encoding,
                to_blob(&embedding.unit),
                answering_model,
                answering_provider,
                run_id
            ],
        )?;
        Ok(())
    }

    pub fn vector(&self, idempotency_key: &str) -> Result<Option<Vec<f32>>, StoreError> {
        let mut statement = self
            .ledger
            .connection()
            .prepare_cached("SELECT vector FROM vectors WHERE idempotency_key = ?1")?;
        let blob: Option<Vec<u8>> = statement
            .query_row([idempotency_key], |row| row.get(0))
            .optional()?;
        Ok(blob.as_deref().and_then(from_blob))
    }

    /// The response blob of an attempt of this request that closed `ok`.
    pub fn ok_response(&self, idempotency_key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .ledger
            .connection()
            .query_row(
                "SELECT a.response_sha256 FROM attempt_events o
                   JOIN attempt_events f ON f.attempt_id = o.attempt_id AND f.event = 'finalised'
                   JOIN attempt_events a ON a.attempt_id = o.attempt_id AND a.event = 'answered'
                  WHERE o.event = 'opened' AND o.idempotency_key = ?1 AND f.outcome = ?2
                    AND a.response_sha256 IS NOT NULL LIMIT 1",
                params![idempotency_key, Outcome::Ok.name()],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Per story of this space with a vector, the request that carries it.
    pub fn embedded_stories(&self, space: &str) -> Result<HashMap<String, String>, StoreError> {
        let mut statement = self.ledger.connection().prepare(
            "SELECT l.story_id, l.idempotency_key FROM embed_links l
               JOIN embed_requests r ON r.idempotency_key = l.idempotency_key
               JOIN vectors v ON v.idempotency_key = l.idempotency_key
              WHERE r.space = ?1",
        )?;
        let found = statement
            .query_map([space], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(found)
    }

    /// Per story of this space with an attempt, the instant its first attempt opened.
    pub fn first_attempts(&self, space: &str) -> Result<HashMap<String, String>, StoreError> {
        let mut statement = self.ledger.connection().prepare(
            "SELECT l.story_id, MIN(o.at) FROM embed_links l
               JOIN embed_requests r ON r.idempotency_key = l.idempotency_key
               JOIN attempt_events o ON o.event = 'opened'
                AND o.idempotency_key = l.idempotency_key
              WHERE r.space = ?1 GROUP BY l.story_id",
        )?;
        let found = statement
            .query_map([space], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(found)
    }

    /// Stories of this space whose request closed `failed` on an answered status outside
    /// `retryable` in at least `runs` runs: the provider refuses the text, not the moment.
    pub fn refused_stories(
        &self,
        space: &str,
        retryable: &[u16],
        runs: u32,
    ) -> Result<HashSet<String>, StoreError> {
        let listed: Vec<String> = retryable.iter().map(u16::to_string).collect();
        let mut statement = self.ledger.connection().prepare(&format!(
            "SELECT l.story_id FROM embed_links l
               JOIN embed_requests r ON r.idempotency_key = l.idempotency_key
               JOIN attempt_events o ON o.event = 'opened'
                AND o.idempotency_key = l.idempotency_key
               JOIN attempt_events a ON a.attempt_id = o.attempt_id AND a.event = 'answered'
               JOIN attempt_events f ON f.attempt_id = o.attempt_id AND f.event = 'finalised'
              WHERE r.space = ?1 AND f.outcome = ?2 AND a.http_status NOT IN ({})
              GROUP BY l.story_id HAVING COUNT(DISTINCT o.run_id) >= ?3",
            listed.join(",")
        ))?;
        let found = statement
            .query_map(params![space, Outcome::Failed.name(), runs], |row| {
                row.get(0)
            })?
            .collect::<Result<_, _>>()?;
        Ok(found)
    }

    /// The start of the first run that named this space, which is where the space begins.
    pub fn space_started(&self, space: &str) -> Result<Option<String>, StoreError> {
        Ok(self.ledger.connection().query_row(
            "SELECT MIN(at) FROM run_events
              WHERE event = 'started' AND json_extract(detail, '$.space') = ?1",
            [space],
            |row| row.get(0),
        )?)
    }

    /// Every story linked to this request: stories with one cut text share it.
    pub fn stories_of(&self, idempotency_key: &str) -> Result<Vec<String>, StoreError> {
        let mut statement = self
            .ledger
            .connection()
            .prepare_cached("SELECT story_id FROM embed_links WHERE idempotency_key = ?1")?;
        let found = statement
            .query_map([idempotency_key], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(found)
    }

    pub fn compared_stories(
        &self,
        compare_version: &str,
        space: &str,
    ) -> Result<HashSet<String>, StoreError> {
        let mut statement = self.ledger.connection().prepare(
            "SELECT story_id FROM comparisons WHERE compare_version = ?1 AND space = ?2",
        )?;
        let found = statement
            .query_map([compare_version, space], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(found)
    }

    /// One comparison and its candidates in one transaction. The first written stays.
    pub fn insert_comparison(
        &self,
        comparison: &Comparison,
        compare_version: &str,
        space: &str,
        snapshot_id: &str,
        run_id: &str,
    ) -> Result<(), StoreError> {
        let db = self.ledger.connection();
        self.ledger.atomically(|| {
            let written = db.execute(
                "INSERT OR IGNORE INTO comparisons (story_id, compare_version, space, snapshot_id,
                     run_id, window_stories, candidates, unembedded, own_hosts, own_numbers,
                     own_tickers) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    comparison.story_id,
                    compare_version,
                    space,
                    snapshot_id,
                    run_id,
                    size(comparison.window_stories),
                    size(comparison.candidates),
                    size(comparison.unembedded),
                    size(comparison.own_hosts),
                    size(comparison.own_numbers),
                    size(comparison.own_tickers),
                ],
            )?;
            if written == 0 {
                return Ok(());
            }
            let id = db.last_insert_rowid();
            for candidate in &comparison.listed {
                db.execute(
                    "INSERT INTO comparison_candidates (comparison_id, list, rank,
                         candidate_story_id, similarity_ppm, age_seconds, hosts_shared,
                         numbers_candidate, numbers_shared, tickers_candidate, tickers_shared,
                         length_ratio_ppm, same_source)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    params![
                        id,
                        candidate.list,
                        candidate.rank,
                        candidate.story_id,
                        candidate.similarity_ppm,
                        candidate.age_seconds,
                        serde_json::Value::from(candidate.hosts_shared.clone()).to_string(),
                        size(candidate.numbers_candidate),
                        size(candidate.numbers_shared),
                        size(candidate.tickers_candidate),
                        size(candidate.tickers_shared),
                        candidate.length_ratio_ppm,
                        candidate.same_source,
                    ],
                )?;
            }
            Ok(())
        })
    }

    /// Every stored candidate of one list, with the comparison it belongs to.
    pub fn candidates(&self, list: &'static str) -> Result<Vec<(Best, Candidate)>, StoreError> {
        let mut statement = self.ledger.connection().prepare(
            "SELECT c.compare_version, c.space, c.story_id, c.unembedded, c.own_hosts,
                    c.own_numbers, k.rank, k.candidate_story_id, k.similarity_ppm, k.age_seconds,
                    k.hosts_shared, k.numbers_candidate, k.numbers_shared, k.tickers_candidate,
                    k.tickers_shared, k.length_ratio_ppm, k.same_source
               FROM comparison_candidates k JOIN comparisons c ON c.id = k.comparison_id
              WHERE k.list = ?1 ORDER BY c.id, k.rank",
        )?;
        let count = |value: i64| usize::try_from(value).unwrap_or_default();
        let found = statement
            .query_map([list], |row| {
                let hosts: String = row.get(10)?;
                let candidate = Candidate {
                    list,
                    rank: row.get(6)?,
                    story_id: row.get(7)?,
                    similarity_ppm: row.get(8)?,
                    age_seconds: row.get(9)?,
                    hosts_shared: serde_json::from_str(&hosts).unwrap_or_default(),
                    numbers_candidate: count(row.get(11)?),
                    numbers_shared: count(row.get(12)?),
                    tickers_candidate: count(row.get(13)?),
                    tickers_shared: count(row.get(14)?),
                    length_ratio_ppm: row.get(15)?,
                    same_source: row.get(16)?,
                };
                let best = Best {
                    compare_version: row.get(0)?,
                    space: row.get(1)?,
                    story_id: row.get(2)?,
                    unembedded: row.get(3)?,
                    own_hosts: row.get(4)?,
                    own_numbers: row.get(5)?,
                    nearest: None,
                };
                Ok((best, candidate))
            })?
            .collect::<Result<_, _>>()?;
        Ok(found)
    }

    /// Every comparison with its rank-1 nearest candidate, or none.
    pub fn best(&self) -> Result<Vec<Best>, StoreError> {
        let mut nearest: HashMap<(String, String, String), Candidate> = self
            .candidates(LIST_NEAREST)?
            .into_iter()
            .filter(|(_, candidate)| candidate.rank == 1)
            .map(|(of, candidate)| ((of.compare_version, of.space, of.story_id), candidate))
            .collect();
        let mut statement = self.ledger.connection().prepare(
            "SELECT compare_version, space, story_id, unembedded, own_hosts, own_numbers
               FROM comparisons ORDER BY id",
        )?;
        let rows: Vec<Best> = statement
            .query_map([], |row| {
                Ok(Best {
                    compare_version: row.get(0)?,
                    space: row.get(1)?,
                    story_id: row.get(2)?,
                    unembedded: row.get(3)?,
                    own_hosts: row.get(4)?,
                    own_numbers: row.get(5)?,
                    nearest: None,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows
            .into_iter()
            .map(|mut best| {
                let key = (
                    best.compare_version.clone(),
                    best.space.clone(),
                    best.story_id.clone(),
                );
                best.nearest = nearest.remove(&key);
                best
            })
            .collect())
    }

    /// How many answers of this canary are recorded, which is its next repeat index.
    pub fn canary_count(&self, name: &str, space: &str) -> Result<u32, StoreError> {
        Ok(self.ledger.connection().query_row(
            "SELECT COUNT(*) FROM canaries WHERE name = ?1 AND space = ?2",
            [name, space],
            |row| row.get(0),
        )?)
    }

    /// Whether a run that started on `day` (`YYYY-MM-DD`) recorded this canary.
    pub fn canary_on(&self, name: &str, space: &str, day: &str) -> Result<bool, StoreError> {
        Ok(self.ledger.connection().query_row(
            "SELECT EXISTS (SELECT 1 FROM canaries
                             WHERE name = ?1 AND space = ?2 AND substr(run_id, 1, 10) = ?3)",
            [name, space, day],
            |row| row.get(0),
        )?)
    }

    /// The vector of this canary's repeat 0, which every later answer is compared with.
    pub fn canary_reference(
        &self,
        name: &str,
        space: &str,
    ) -> Result<Option<Vec<f32>>, StoreError> {
        let key: Option<String> = self
            .ledger
            .connection()
            .query_row(
                "SELECT idempotency_key FROM canaries
                  WHERE name = ?1 AND space = ?2 AND repeat_index = 0",
                [name, space],
                |row| row.get(0),
            )
            .optional()?;
        match key {
            Some(key) => self.vector(&key),
            None => Ok(None),
        }
    }

    pub fn insert_canary(&self, row: &CanaryRow, idempotency_key: &str) -> Result<(), StoreError> {
        self.ledger.connection().execute(
            "INSERT INTO canaries (name, space, repeat_index, idempotency_key, run_id,
                 self_similarity_ppm, pair_similarity_ppm) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                row.name,
                row.space,
                row.repeat_index,
                idempotency_key,
                row.run_id,
                row.self_similarity_ppm,
                row.pair_similarity_ppm
            ],
        )?;
        Ok(())
    }

    pub fn canaries(&self) -> Result<Vec<CanaryRow>, StoreError> {
        let mut statement = self.ledger.connection().prepare(
            "SELECT name, space, repeat_index, run_id, self_similarity_ppm, pair_similarity_ppm
               FROM canaries ORDER BY space, name, repeat_index",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(CanaryRow {
                    name: row.get(0)?,
                    space: row.get(1)?,
                    repeat_index: row.get(2)?,
                    run_id: row.get(3)?,
                    self_similarity_ppm: row.get(4)?,
                    pair_similarity_ppm: row.get(5)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// `(encoding, answering provider)` of each vector this run wrote.
    pub fn vectors_by(&self, run_id: &str) -> Result<Vec<(String, Option<String>)>, StoreError> {
        let mut statement = self.ledger.connection().prepare(
            "SELECT encoding, answering_provider FROM vectors WHERE run_id = ?1 ORDER BY rowid",
        )?;
        let written = statement
            .query_map([run_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(written)
    }

    /// Every attempt this run closed, in the order it opened them.
    pub fn closed_by(&self, run_id: &str) -> Result<Vec<ClosedEmbed>, StoreError> {
        let mut statement = self.ledger.connection().prepare(
            "SELECT f.outcome, f.cost_micro_usd, f.answering_model
               FROM attempt_events o
               JOIN attempt_events f ON f.attempt_id = o.attempt_id AND f.event = 'finalised'
              WHERE o.event = 'opened' AND o.run_id = ?1 ORDER BY o.rowid",
        )?;
        let closed = statement
            .query_map([run_id], |row| {
                Ok(ClosedEmbed {
                    outcome: row.get(0)?,
                    cost_micro_usd: row.get(1)?,
                    answering_model: row.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(closed)
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::{
        CanaryRow, Candidate, Comparison, EmbedStore, EMBED_BLOB_DIR, EMBED_DB_FILE, EMBED_TABLES,
        LIST_NEAREST,
    };
    use crate::embed::tests::{axis, space};
    use crate::embed::{build, Embedding, ENCODING_ARRAY};
    use crate::store::{Store, StoreError, DB_FILE};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn candidate(rank: u32) -> Candidate {
        Candidate {
            list: LIST_NEAREST,
            rank,
            story_id: format!("earlier-{rank}"),
            similarity_ppm: 900_000,
            age_seconds: 60,
            hosts_shared: vec!["issuer.example".to_owned()],
            numbers_candidate: 4,
            numbers_shared: 3,
            tickers_candidate: 1,
            tickers_shared: 1,
            length_ratio_ppm: 990_000,
            same_source: true,
        }
    }

    /// One row in every table of the embed file, through the store's own doors.
    fn seeded(root: &TempDir) -> TestResult<String> {
        let store = EmbedStore::open(root.path())?;
        let space = space()?;
        let request = build(&space, "a synthetic body", 0)?;
        let key = request.idempotency_key.clone();
        store.insert_request(&request, &space.name())?;
        store.link("story", "item", 1, "o", &key, "run")?;
        let embedding = Embedding {
            unit: axis(0, 0.6, 1, 0.8),
            norm: 5.0,
            encoding: ENCODING_ARRAY,
        };
        store.insert_vector(&key, &embedding, Some("m"), None, "run")?;
        let row = CanaryRow {
            name: "short".to_owned(),
            space: space.name(),
            repeat_index: 0,
            run_id: "2026-11-23T12:00:00.000000Z".to_owned(),
            self_similarity_ppm: None,
            pair_similarity_ppm: None,
        };
        store.insert_canary(&row, &key)?;
        let comparison = Comparison {
            story_id: "story".to_owned(),
            window_stories: 2,
            candidates: 1,
            unembedded: 1,
            own_hosts: 1,
            own_numbers: 4,
            own_tickers: 1,
            listed: vec![candidate(1)],
        };
        store.insert_comparison(&comparison, "c/1", &space.name(), "cut", "run")?;
        let noon = "2026-11-23T12:00:00Z".parse()?;
        store
            .ledger()
            .run_event("run", "started", noon, &serde_json::json!({}))?;
        store.ledger().write_opened("run#0", &key, "run", 7, noon)?;
        Ok(key)
    }

    /// Section 8 row MG23. Mutations 2026-11-23, each alone: `canaries` dropped from
    /// `EMBED_TABLES`; the embed layout given `gate.sqlite`'s file name: red each.
    #[test]
    fn the_embed_file_is_its_own_insert_only_database_and_the_gate_file_is_not_touched(
    ) -> TestResult {
        let root = TempDir::new()?;
        seeded(&root)?;
        assert!(!root.path().join(DB_FILE).exists());
        assert!(root.path().join(EMBED_BLOB_DIR).is_dir());
        let raw = Connection::open(root.path().join(EMBED_DB_FILE))?;
        let mut created: Vec<String> = raw
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        created.sort();
        let mut listed = EMBED_TABLES.to_vec();
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
                assert_eq!(refused, Some(format!("{table} is insert-only")));
            }
        }
        Ok(())
    }

    /// Section 8 row MG23, the money half. Mutation 2026-11-23, the embed layout opened on
    /// `gate.sqlite`: red (the gate's `spent` then counts the embed attempt).
    #[test]
    fn an_embed_attempt_is_spent_in_the_embed_file_only_and_both_files_share_one_lease(
    ) -> TestResult {
        let root = TempDir::new()?;
        seeded(&root)?;
        let noon = "2026-11-23T12:00:00Z".parse()?;
        let embed = EmbedStore::open(root.path())?;
        assert_eq!(embed.ledger().spent("run", noon)?.total, 7);
        assert!(matches!(
            Store::open(root.path()),
            Err(StoreError::LeaseHeld(_))
        ));
        embed
            .ledger()
            .run_event("run", "cost_estimate_broken", noon, &serde_json::json!({}))?;
        drop(embed);
        let gate = Store::open(root.path())?;
        assert_eq!(gate.spent("run", noon)?.total, 0);
        assert_eq!(gate.unacked_cost_break()?, None);
        Ok(())
    }

    /// Mutation 2026-11-24, `compare_version` left out of the `comparisons` key: red.
    #[test]
    fn a_story_is_embedded_once_its_request_has_a_vector_and_the_first_comparison_stays(
    ) -> TestResult {
        let root = TempDir::new()?;
        let key = seeded(&root)?;
        let store = EmbedStore::open(root.path())?;
        let name = space()?.name();
        let embedded = store.embedded_stories(&name)?;
        assert_eq!(embedded.get("story"), Some(&key));
        assert!(store.embedded_stories("another space")?.is_empty());
        assert_eq!(
            store
                .first_attempts(&name)?
                .get("story")
                .map(String::as_str),
            Some("2026-11-23T12:00:00.000000Z")
        );
        assert_eq!(store.stories_of(&key)?, ["story"]);
        assert_eq!(store.vector(&key)?, Some(axis(0, 0.6, 1, 0.8)));
        assert_eq!(store.ok_response(&key)?, None);
        let later = Comparison {
            story_id: "story".to_owned(),
            window_stories: 9,
            candidates: 9,
            unembedded: 0,
            own_hosts: 0,
            own_numbers: 0,
            own_tickers: 0,
            listed: vec![candidate(1), candidate(2)],
        };
        store.insert_comparison(&later, "c/1", &name, "cut-2", "run-2")?;
        let best = store.best()?;
        assert_eq!(best.len(), 1);
        assert_eq!(best[0].unembedded, 1);
        assert_eq!(best[0].nearest, Some(candidate(1)));
        assert_eq!(store.candidates(LIST_NEAREST)?.len(), 1);
        assert!(store.compared_stories("c/1", &name)?.contains("story"));
        assert!(store.compared_stories("c/2", &name)?.is_empty());
        // Another version's record of the same story is a second row, not a refused one.
        store.insert_comparison(&later, "c/2", &name, "cut-2", "run-2")?;
        assert!(store.compared_stories("c/2", &name)?.contains("story"));
        assert_eq!(store.best()?.len(), 2);
        assert_eq!(store.candidates(LIST_NEAREST)?.len(), 3);
        assert_eq!(store.canary_count("short", &name)?, 1);
        assert!(store.canary_on("short", &name, "2026-11-23")?);
        assert!(!store.canary_on("short", &name, "2026-11-24")?);
        assert_eq!(
            store.canary_reference("short", &name)?,
            Some(axis(0, 0.6, 1, 0.8))
        );
        Ok(())
    }
}
