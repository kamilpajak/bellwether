//! `modelgate shadow run`: one `decide` request per story of a `newsvault cut` snapshot, recorded
//! and never acted on. Selection, retry and exit rules: `docs/design/architecture.md` section 3.5,
//! "The shadow run".

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::Duration;

use canon::FileDigest;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use snapshot_contract::{SnapshotRow, ValidateError};

use crate::cap::{parse_usd, refusal, worst_case, Caps, Spent};
use crate::ledger::{AttemptSlot, Opening};
use crate::model::ModelId;
use crate::question::QuestionDoc;
use crate::request::{build, DecideRequest};
use crate::response::{buckets, read, Buckets, Outcome, Reading};
use crate::store::{stamp, Store, StoreError};
use crate::wire::ModelExchange;

pub const SCHEMA: &str = "modelgate.shadow_run/1";
pub const ERROR_SCHEMA: &str = "modelgate.error/1";
/// Sends of one request within one run. A work cap, not fitted to a failure rate.
pub const MAX_ATTEMPTS: u32 = 3;
const RETRY_WAIT: Duration = Duration::from_secs(2);
const RETRY_AFTER_CEILING: Duration = Duration::from_secs(30);
/// Statuses a later attempt may not repeat. Every other non-2xx is final for the run.
const RETRYABLE_STATUSES: [u16; 6] = [429, 500, 502, 503, 504, 529];
/// Stories in a row that end `failed` before the run stops asking: an outage otherwise costs
/// every remaining story its three attempts and their waits.
const FAILED_STORIES_BEFORE_STOP: u32 = 3;
/// The 10% sample: a story is in it when the first byte of sha256(story_id ‖ salt) is below 26
/// (26/256). Chosen by code from the id alone, before any answer exists.
const SAMPLE_SALT: &str = "ripple-shadow-sample-1";
const SAMPLE_BELOW: u8 = 26;
const SECONDS_PER_HOUR: i64 = 3_600;

pub const EXIT_OK: u8 = 0;
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_NOT_FOUND: u8 = 4;
pub const EXIT_TRANSIENT: u8 = 7;
pub const EXIT_INCOMPLETE: u8 = 8;

#[derive(Debug, Clone)]
pub struct Args {
    pub snapshot: PathBuf,
    pub questions: PathBuf,
    pub model: String,
    pub run_cap_usd: String,
    pub day_cap_usd: String,
    pub total_cap_usd: String,
    pub input_price_per_mtok: String,
    pub give_up_after_hours: u32,
    pub store: PathBuf,
    pub from_seq: Option<i64>,
    pub limit: Option<u64>,
    pub sample_only: bool,
    pub repeat_index: u32,
    pub ack_cost_model: Option<String>,
    pub dry_run: bool,
}

/// What the command reaches outside itself through.
pub struct Env<'a> {
    pub exchange: &'a dyn ModelExchange,
    pub now: &'a dyn Fn() -> DateTime<Utc>,
    pub sleep: &'a dyn Fn(Duration),
}

#[derive(Debug, thiserror::Error)]
pub enum ShadowError {
    #[error("{0}")]
    InvalidArgument(String),
    #[error("{0}")]
    InputAbsent(String),
    #[error("{0}")]
    InputUnparseable(String),
    #[error("the snapshot is not admissible: {}", .0.join(", "))]
    SnapshotInadmissible(Vec<&'static str>),
    #[error("{0}")]
    Io(String),
    #[error(transparent)]
    Store(StoreError),
    /// Nothing was answered and every failure was transient. Carries the run's envelope.
    #[error("no request was answered and every failure was transient")]
    VendorUnreachable(Box<Value>),
}

impl From<StoreError> for ShadowError {
    fn from(failure: StoreError) -> Self {
        Self::Store(failure)
    }
}

impl ShadowError {
    /// The section 5.1 code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidArgument(_) => "invalid_argument",
            Self::InputAbsent(_) => "input_absent",
            Self::InputUnparseable(_) => "input_unparseable",
            Self::SnapshotInadmissible(_) | Self::Store(StoreError::SchemaVersion { .. }) => {
                "input_schema_mismatch"
            }
            Self::Store(StoreError::LeaseHeld(_)) => "lease_held",
            Self::Io(_) | Self::Store(_) => "io_failed",
            Self::VendorUnreachable(_) => "vendor_unreachable",
        }
    }

    #[must_use]
    pub const fn exit(&self) -> u8 {
        match self {
            Self::InputAbsent(_) => EXIT_NOT_FOUND,
            Self::Store(StoreError::LeaseHeld(_)) | Self::VendorUnreachable(_) => EXIT_TRANSIENT,
            _ => EXIT_USAGE,
        }
    }

    #[must_use]
    pub const fn retryable(&self) -> bool {
        self.exit() == EXIT_TRANSIENT
    }

    #[must_use]
    pub fn details(&self) -> Value {
        match self {
            Self::VendorUnreachable(envelope) => (**envelope).clone(),
            Self::SnapshotInadmissible(codes) => json!({ "findings": codes }),
            _ => json!({}),
        }
    }
}

/// The envelope and the exit it goes out with: 0, or 8 with `exit.error_code` naming why.
#[derive(Debug, Clone, PartialEq)]
pub struct Finished {
    pub envelope: Value,
    pub exit: u8,
}

#[must_use]
pub fn in_sample(story_id: &str) -> bool {
    let digest = FileDigest::of(format!("{story_id}{SAMPLE_SALT}").as_bytes());
    u8::from_str_radix(&digest.hex()[..2], 16).is_ok_and(|first| first < SAMPLE_BELOW)
}

#[derive(Default)]
struct Stories {
    rows: u64,
    repeat_of_story: u64,
    below_floor: u64,
    outside_sample: u64,
    no_body: u64,
    build_postdates_story: u64,
    already_labelled: u64,
    gave_up: u64,
    selected: u64,
    labelled: u64,
    truncated: u64,
    left: u64,
}

#[derive(Default)]
struct Requests {
    planned: u64,
    planned_bytes: u64,
    planned_worst_case: i64,
    sent: u64,
    already_answered: u64,
    failed_stories: u64,
    refused_by_cap: u64,
}

struct Run<'a> {
    args: &'a Args,
    env: &'a Env<'a>,
    store: Store,
    doc: QuestionDoc,
    model: ModelId,
    caps: Caps,
    price: i64,
    run_id: String,
    started: DateTime<Utc>,
    floor: i64,
    spent: Spent,
    /// A dry run's simulation of `spent`, so its envelope still reports what was really spent.
    planned_spent: Spent,
    ordinal: u32,
    stories: Stories,
    requests: Requests,
    consecutive_failed: u32,
    every_failure_transient: bool,
    stopped_by: Option<&'static str>,
}

enum Sent {
    Answered,
    Failed,
    Refused,
}

fn invalid(why: impl ToString) -> ShadowError {
    ShadowError::InvalidArgument(why.to_string())
}

fn read_input(path: &std::path::Path) -> Result<Vec<u8>, ShadowError> {
    std::fs::read(path).map_err(|why| {
        let text = format!("{}: {why}", path.display());
        if why.kind() == std::io::ErrorKind::NotFound {
            ShadowError::InputAbsent(text)
        } else {
            ShadowError::Io(text)
        }
    })
}

fn admissible(dir: &std::path::Path) -> Result<(), ShadowError> {
    let findings = snapshot_contract::validate(dir).map_err(|why| match why {
        ValidateError::Absent(_) | ValidateError::NotADirectory(_) => {
            ShadowError::InputAbsent(why.to_string())
        }
        ValidateError::Io { .. } => ShadowError::Io(why.to_string()),
    })?;
    if findings.is_empty() {
        return Ok(());
    }
    Err(ShadowError::SnapshotInadmissible(
        findings.iter().map(|finding| finding.code).collect(),
    ))
}

fn instant(text: &str) -> Result<DateTime<Utc>, ShadowError> {
    text.parse()
        .map_err(|_| ShadowError::InputUnparseable(format!("{text:?} is not an instant")))
}

fn unreadable(why: snapshot_contract::ReadError) -> ShadowError {
    ShadowError::InputUnparseable(why.to_string())
}

/// Per story, the `seq` of its first-sighted row: lowest `observed_at`, then lowest `seq`.
fn first_sightings(dir: &std::path::Path) -> Result<HashMap<String, i64>, ShadowError> {
    let mut first: HashMap<String, (String, i64)> = HashMap::new();
    for row in snapshot_contract::rows(dir).map_err(unreadable)? {
        let row = row.map_err(unreadable)?;
        let sighting = (row.observed_at, row.seq);
        match first.get_mut(&row.story_id) {
            Some(kept) if *kept <= sighting => {}
            Some(kept) => *kept = sighting,
            None => {
                first.insert(row.story_id, sighting);
            }
        }
    }
    Ok(first.into_iter().map(|(id, (_, seq))| (id, seq)).collect())
}

pub fn run(args: &Args, env: &Env) -> Result<Finished, ShadowError> {
    let model = ModelId::parse(&args.model).map_err(invalid)?;
    let caps = Caps {
        run: parse_usd(&args.run_cap_usd).map_err(invalid)?,
        day: parse_usd(&args.day_cap_usd).map_err(invalid)?,
        total: parse_usd(&args.total_cap_usd).map_err(invalid)?,
    };
    let price = parse_usd(&args.input_price_per_mtok).map_err(invalid)?;
    let file = read_input(&args.questions)?;
    let doc = QuestionDoc::parse(&file)
        .map_err(|why| ShadowError::InputUnparseable(format!("--questions: {why}")))?;
    admissible(&args.snapshot)?;
    let manifest = snapshot_contract::manifest(&args.snapshot).map_err(unreadable)?;
    let started = (env.now)();
    let run_id = stamp(started);
    let store = if args.dry_run {
        Store::read_only(&args.store)?
    } else {
        Store::open(&args.store)?
    };
    let stored_floor = store.floor(doc.version(), model.as_str())?;
    let floor = match (stored_floor, args.from_seq) {
        (Some(fixed), Some(asked)) if asked != fixed => {
            return Err(invalid(format!(
                "--from-seq {asked} differs from {fixed}, the floor this question version and \
                 build were given by their first run"
            )))
        }
        (Some(fixed), _) => fixed,
        (None, asked) => asked.unwrap_or(manifest.seq_high_water),
    };
    let mut swept = 0;
    if !args.dry_run {
        swept = store.sweep(started, &run_id)?;
        store.record_question_doc(&doc, &file)?;
        store.run_event(
            &run_id,
            "started",
            started,
            &json!({
                "snapshot_id": manifest.snapshot_id, "cutoff": manifest.cutoff,
                "seq_high_water": manifest.seq_high_water,
                "question_version": doc.version(), "model": model.as_str(),
                "caps_micro_usd": {"run": caps.run, "day": caps.day, "total": caps.total},
                "input_price_micro_usd_per_mtok": price,
            }),
        )?;
        if let Some(acknowledged) = &args.ack_cost_model {
            let detail = json!({ "acknowledges": acknowledged });
            store.run_event(&run_id, "cost_model_acked", started, &detail)?;
        }
        if stored_floor.is_none() {
            store.set_floor(doc.version(), model.as_str(), floor, &run_id)?;
        }
    }
    let stopped_by = store.unacked_cost_break()?.map(|_| "cost_estimate_broken");
    let spent = store.spent(&run_id, started)?;
    let mut run = Run {
        args,
        env,
        store,
        doc,
        model,
        caps,
        price,
        run_id,
        started,
        floor,
        spent,
        planned_spent: spent,
        ordinal: 0,
        stories: Stories::default(),
        requests: Requests::default(),
        consecutive_failed: 0,
        every_failure_transient: true,
        stopped_by,
    };
    let first = first_sightings(&args.snapshot)?;
    for row in snapshot_contract::rows(&args.snapshot).map_err(unreadable)? {
        run.consider(&row.map_err(unreadable)?, &first)?;
    }
    run.finish(&manifest, stored_floor.is_none(), swept)
}

impl Run<'_> {
    fn consider(
        &mut self,
        row: &SnapshotRow,
        first: &HashMap<String, i64>,
    ) -> Result<(), ShadowError> {
        self.stories.rows += 1;
        let version = self.doc.version().to_owned();
        let (model, repeat) = (self.model.as_str().to_owned(), self.args.repeat_index);
        let counted = if first.get(&row.story_id) != Some(&row.seq) {
            &mut self.stories.repeat_of_story
        } else if row.seq <= self.floor {
            &mut self.stories.below_floor
        } else if self.args.sample_only && !in_sample(&row.story_id) {
            &mut self.stories.outside_sample
        } else if row.body.is_none() {
            &mut self.stories.no_body
        } else if !self.model.predates(instant(&row.observed_at)?) {
            &mut self.stories.build_postdates_story
        } else {
            let (linked, answered) =
                self.store
                    .story_state(&row.story_id, &version, &model, repeat)?;
            let age = self.started - instant(&row.observed_at)?;
            let limit = i64::from(self.args.give_up_after_hours) * SECONDS_PER_HOUR;
            if answered {
                &mut self.stories.already_labelled
            } else if linked && age.num_seconds() > limit {
                &mut self.stories.gave_up
            } else {
                return self.label(row);
            }
        };
        *counted += 1;
        Ok(())
    }

    fn label(&mut self, row: &SnapshotRow) -> Result<(), ShadowError> {
        self.stories.selected += 1;
        let over_limit = self
            .args
            .limit
            .is_some_and(|limit| self.requests.planned >= limit);
        if over_limit && self.stopped_by.is_none() {
            self.stopped_by = Some("limit");
        }
        if self.stopped_by.is_some() {
            self.stories.left += 1;
            return Ok(());
        }
        let body = row.body.as_deref().unwrap_or_default();
        let request = build(&self.doc, &self.model, body, self.args.repeat_index)
            .map_err(|why| ShadowError::InputUnparseable(why.to_string()))?;
        self.stories.truncated += u64::from(request.state_truncated);
        if self.store.request_answered(&request.idempotency_key)? {
            self.requests.already_answered += 1;
            self.stories.labelled += 1;
            return self.link(row, &request);
        }
        let worst = worst_case(request.bytes.len(), self.price);
        self.requests.planned += 1;
        self.requests.planned_bytes += u64::try_from(request.bytes.len()).unwrap_or(u64::MAX);
        self.requests.planned_worst_case += worst;
        if self.args.dry_run {
            if refusal(self.caps, self.planned_spent, worst).is_some() {
                self.requests.refused_by_cap += 1;
            } else {
                self.planned_spent.add(worst);
            }
            return Ok(());
        }
        self.store
            .insert_request(&request, self.doc.version(), self.model.as_str())?;
        self.link(row, &request)?;
        match self.send(&request, worst)? {
            Sent::Answered => {
                self.stories.labelled += 1;
                self.consecutive_failed = 0;
            }
            Sent::Refused => self.stories.left += 1,
            Sent::Failed => {
                self.requests.failed_stories += 1;
                self.consecutive_failed += 1;
                if self.consecutive_failed >= FAILED_STORIES_BEFORE_STOP
                    && self.stopped_by.is_none()
                {
                    self.stopped_by = Some("vendor_failing");
                }
            }
        }
        Ok(())
    }

    fn link(&self, row: &SnapshotRow, request: &DecideRequest) -> Result<(), ShadowError> {
        if self.args.dry_run {
            return Ok(());
        }
        Ok(self.store.link_label(
            &row.story_id,
            &row.item_id,
            row.seq,
            &row.observed_at,
            &request.idempotency_key,
            &self.run_id,
        )?)
    }

    fn send(&mut self, request: &DecideRequest, worst: i64) -> Result<Sent, ShadowError> {
        for attempt in 1..=MAX_ATTEMPTS {
            if refusal(self.caps, self.spent, worst).is_some() {
                self.requests.refused_by_cap += 1;
                self.stopped_by = Some("cost_cap_reached");
                return Ok(Sent::Refused);
            }
            let mut slot = AttemptSlot::open(
                &self.store,
                &Opening {
                    run_id: &self.run_id,
                    ordinal: self.ordinal,
                    idempotency_key: &request.idempotency_key,
                    worst_case_micro_usd: worst,
                    at: (self.env.now)(),
                },
            )?;
            self.ordinal += 1;
            self.spent.add(worst);
            let exchanged = self.env.exchange.send(slot.take_permit()?, &request.bytes);
            self.requests.sent += 1;
            let reading = match &exchanged.answer {
                Ok(answer) => read(answer.status, &answer.body),
                Err(_) => Reading {
                    outcome: Outcome::Failed,
                    cost_micro_usd: None,
                    answering_model: None,
                },
            };
            slot.close(&self.store, &exchanged, &reading, (self.env.now)())?;
            if let Some(cost) = reading.cost_micro_usd {
                self.spent.add(cost - worst);
                if cost > worst {
                    let detail = json!({
                        "idempotency_key": request.idempotency_key,
                        "vendor_micro_usd": cost, "worst_case_micro_usd": worst,
                    });
                    self.store.run_event(
                        &self.run_id,
                        "cost_estimate_broken",
                        (self.env.now)(),
                        &detail,
                    )?;
                    self.stopped_by = Some("cost_estimate_broken");
                }
            }
            if reading.outcome.answered() {
                return Ok(Sent::Answered);
            }
            let (retryable, wait) = match &exchanged.answer {
                Err(_) => (true, None),
                Ok(answer) => (
                    RETRYABLE_STATUSES.contains(&answer.status),
                    answer.retry_after_seconds,
                ),
            };
            self.every_failure_transient &= retryable;
            if !retryable || attempt == MAX_ATTEMPTS || self.stopped_by.is_some() {
                break;
            }
            let wait = wait.map_or(RETRY_WAIT * attempt, Duration::from_secs);
            (self.env.sleep)(wait.min(RETRY_AFTER_CEILING));
        }
        Ok(Sent::Failed)
    }

    fn finish(
        self,
        manifest: &snapshot_contract::Manifest,
        floor_set: bool,
        swept: u64,
    ) -> Result<Finished, ShadowError> {
        let closed = self.store.closed_by(&self.run_id)?;
        let count = |outcome: Outcome| {
            closed
                .iter()
                .filter(|c| c.outcome == outcome.name())
                .count()
        };
        let mut read_back = Buckets::default();
        for attempt in closed.iter().filter(|c| c.outcome == Outcome::Ok.name()) {
            if let Some(hex) = &attempt.response_sha256 {
                let found = buckets(&self.doc, &self.store.read_blob(hex)?);
                read_back.answered += found.answered;
                read_back.not_answered += found.not_answered;
                read_back.missing += found.missing;
            }
        }
        let answering: BTreeSet<&str> = closed
            .iter()
            .filter_map(|c| c.answering_model.as_deref())
            .collect();
        let another_id = closed
            .iter()
            .filter(|c| {
                c.answering_model
                    .as_ref()
                    .is_some_and(|id| *id != c.sent_model)
            })
            .count();
        let mut lags = Vec::new();
        for (observed_at, received_at) in self.store.label_times(&self.run_id)? {
            lags.push((instant(&received_at)? - instant(&observed_at)?).num_seconds());
        }
        lags.sort_unstable();
        // Nearest rank: the smallest value with at least `percent` of the values at or below it.
        let rank = |percent: usize| {
            let position = (lags.len() * percent).div_ceil(100).max(1) - 1;
            lags.get(position).copied()
        };
        let estimated = self
            .store
            .measured_cost(self.doc.version(), self.model.as_str())?
            .map(|(cost, bytes)| {
                let planned = i128::from(self.requests.planned_bytes);
                let bytes = i128::from(bytes);
                i64::try_from((i128::from(cost) * planned + bytes - 1) / bytes).unwrap_or(i64::MAX)
            });
        let answered_none = self.stories.labelled == 0;
        let error_code = match self.stopped_by {
            Some("cost_estimate_broken") => Some("cost_estimate_broken"),
            Some("cost_cap_reached") => Some("cost_cap_reached"),
            _ if self.requests.failed_stories > 0 => Some("partial_shadow"),
            _ => None,
        };
        let unreachable =
            error_code == Some("partial_shadow") && answered_none && self.every_failure_transient;
        let exit = if error_code.is_some() {
            EXIT_INCOMPLETE
        } else {
            EXIT_OK
        };
        let (stories, requests) = (&self.stories, &self.requests);
        let envelope = json!({
            "schema": SCHEMA,
            "dry_run": self.args.dry_run,
            "changed": !self.args.dry_run
                && (requests.sent > 0 || requests.already_answered > 0 || floor_set),
            "run_id": self.run_id,
            "question_version": self.doc.version(),
            "question_document": self.doc.name(),
            "model": self.model.as_str(),
            "repeat_index": self.args.repeat_index,
            "sample_only": self.args.sample_only,
            "snapshot": {
                "snapshot_id": manifest.snapshot_id, "cutoff": manifest.cutoff,
                "seq_high_water": manifest.seq_high_water,
            },
            "floor": { "start_seq": self.floor, "set_by_this_run": floor_set },
            "stories": {
                "rows": stories.rows, "repeat_of_story": stories.repeat_of_story,
                "below_floor": stories.below_floor, "outside_sample": stories.outside_sample,
                "no_body": stories.no_body,
                "build_postdates_story": stories.build_postdates_story,
                "already_labelled": stories.already_labelled, "gave_up": stories.gave_up,
                "selected": stories.selected, "labelled": stories.labelled,
                "truncated": stories.truncated, "left": stories.left,
            },
            "requests": {
                "planned": requests.planned, "planned_bytes": requests.planned_bytes,
                "sent": requests.sent, "already_answered": requests.already_answered,
                "ok": count(Outcome::Ok), "empty": count(Outcome::Empty),
                "unparseable": count(Outcome::Unparseable), "failed": count(Outcome::Failed),
                "failed_stories": requests.failed_stories,
                "refused_by_cap": requests.refused_by_cap,
            },
            "questions": {
                "answered": read_back.answered, "not_answered": read_back.not_answered,
                "missing": read_back.missing,
            },
            "cost_micro_usd": {
                "run": self.spent.run, "day": self.spent.day, "total": self.spent.total,
                "run_cap": self.caps.run, "day_cap": self.caps.day, "total_cap": self.caps.total,
                "planned_worst_case": requests.planned_worst_case,
                "planned_estimate": estimated,
            },
            "answering_models": answering,
            "answered_by_another_id": another_id,
            "label_lag_seconds": { "p50": rank(50), "p90": rank(90), "max": lags.last() },
            "store": {
                "attempts_abandoned_by_sweep": swept,
                "runs_left_open": self.store.runs_left_open(&self.run_id)?,
            },
            "stopped_by": self.stopped_by,
            "exit": { "code": if unreachable { EXIT_TRANSIENT } else { exit },
                      "error_code": if unreachable { Some("vendor_unreachable") } else { error_code } },
        });
        if !self.args.dry_run {
            let detail = json!({ "exit": envelope["exit"], "stopped_by": self.stopped_by });
            self.store
                .run_event(&self.run_id, "ended", (self.env.now)(), &detail)?;
        }
        if unreachable {
            return Err(ShadowError::VendorUnreachable(Box::new(envelope)));
        }
        Ok(Finished { envelope, exit })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeMap, VecDeque};
    use std::fs::File;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use canon::FileDigest;
    use chrono::{DateTime, TimeDelta, Utc};
    use rusqlite::Connection;
    use serde_json::{json, Value};
    use snapshot_contract::{
        cutoff_text, file_sha256, snapshot_id_of, DataWriter, Manifest, SnapshotRow, DATA_FILE,
        MANIFEST_FILE, MANIFEST_SCHEMA, PROVENANCES,
    };
    use tempfile::TempDir;

    use super::{in_sample, run, Args, Env, Finished, ShadowError, FAILED_STORIES_BEFORE_STOP};
    use crate::ledger::SendPermit;
    use crate::question::tests::TWO_QUESTIONS;
    use crate::store::{BLOB_DIR, DB_FILE};
    use crate::wire::tests::{http, sentinel, Loopback, SENTINEL_KEY};
    use crate::wire::{Exchanged, HttpAnswer, ModelExchange, ModelWire};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    /// Dated before every observation below, so it may label them.
    const MODEL: &str = "example/decide-1-20261014";
    const NOW: &str = "2026-11-21T00:10:00Z";
    const CUTOFF: &str = "2026-11-21T00:00:00Z";
    const DAY_ONE: &str = "2026-11-20T10:00:00.000000Z";
    /// 5 micro-USD, under the worst case of the smallest request here (about 500 bytes at
    /// 0.042 USD per million: 16 micro-USD).
    const COST_USD: f64 = 0.000_005;

    fn answer_json(model: &str, cost: f64) -> String {
        json!({
            "model": model, "usage": {"cost": cost, "input_tokens": 100},
            "answers": {
                "scope": {"type": "choice", "choice": "issuer_only",
                          "probabilities": {"issuer_only": 0.9, "whole_category": 0.1}},
                "states_fact": {"type": "noul", "noul": 0.8}
            }
        })
        .to_string()
    }

    fn status(code: u16, body: &str) -> Result<HttpAnswer, &'static str> {
        Ok(HttpAnswer {
            status: code,
            body: body.as_bytes().to_vec(),
            retry_after_seconds: None,
        })
    }

    /// Scripted answers in order, then a sound 200 for every further send.
    #[derive(Default)]
    struct Stub {
        script: RefCell<VecDeque<Result<HttpAnswer, &'static str>>>,
        bodies: RefCell<Vec<Vec<u8>>>,
    }

    impl Stub {
        fn scripted(answers: Vec<Result<HttpAnswer, &'static str>>) -> Self {
            Self {
                script: RefCell::new(answers.into()),
                bodies: RefCell::default(),
            }
        }

        fn sends(&self) -> usize {
            self.bodies.borrow().len()
        }
    }

    impl ModelExchange for Stub {
        fn send(&self, permit: SendPermit, body: &[u8]) -> Exchanged {
            self.bodies.borrow_mut().push(body.to_vec());
            let answer = self
                .script
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| status(200, &answer_json(MODEL, COST_USD)));
            let at: DateTime<Utc> = NOW.parse().unwrap_or_default();
            Exchanged::new(permit, at, at + TimeDelta::seconds(2), answer)
        }
    }

    fn story(seq: i64, id: &str, observed_at: &str, body: Option<&str>) -> SnapshotRow {
        SnapshotRow {
            seq,
            item_id: format!("item-{seq}"),
            story_id: id.to_owned(),
            doc_id: None,
            source: "open_web_wires".to_owned(),
            provenance: PROVENANCES[0].to_owned(),
            valid_at: Some(observed_at.to_owned()),
            valid_minute: None,
            valid_day: None,
            valid_tz: None,
            observed_at: observed_at.to_owned(),
            body_blob: body.map(|text| FileDigest::of(text.as_bytes()).hex().to_owned()),
            body: body.map(str::to_owned),
        }
    }

    /// A snapshot directory written the way `newsvault cut` writes one.
    fn cut(rows: &[SnapshotRow], cutoff: &str) -> TestResult<TempDir> {
        let dir = TempDir::new()?;
        let cutoff: DateTime<Utc> = cutoff.parse()?;
        let digests = rows
            .iter()
            .map(|row| row.digest().map(|digest| digest.hex().to_owned()))
            .collect::<Result<Vec<_>, _>>()?;
        let hexes: Vec<&str> = digests.iter().map(String::as_str).collect();
        let id = snapshot_id_of(cutoff, &hexes)
            .map_err(|why| why.to_string())?
            .hex()
            .to_owned();
        let mut writer = DataWriter::create(File::create_new(dir.path().join(DATA_FILE))?, &id)?;
        for row in rows {
            writer.push(row.clone())?;
        }
        writer.finish()?;
        let mut counts: BTreeMap<String, u64> = PROVENANCES
            .iter()
            .map(|name| ((*name).to_owned(), 0))
            .collect();
        counts.insert(PROVENANCES[0].to_owned(), u64::try_from(rows.len())?);
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA.to_owned(),
            snapshot_id: id,
            cutoff: cutoff_text(cutoff),
            seq_high_water: rows.iter().map(|row| row.seq).max().unwrap_or(1),
            items: u64::try_from(rows.len())?,
            provenance_counts: counts,
            data_sha256: file_sha256(&dir.path().join(DATA_FILE))?.0,
        };
        std::fs::write(
            dir.path().join(MANIFEST_FILE),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        assert_eq!(snapshot_contract::validate(dir.path())?, Vec::new());
        Ok(dir)
    }

    /// Three stories on one day, each with its own synthetic body.
    fn three_stories() -> Vec<SnapshotRow> {
        vec![
            story(5, "story-a", DAY_ONE, Some("Synthetic release one.")),
            story(6, "story-b", DAY_ONE, Some("Synthetic release two.")),
            story(7, "story-c", DAY_ONE, Some("Synthetic release three.")),
        ]
    }

    struct Bench {
        root: TempDir,
        now: Cell<DateTime<Utc>>,
    }

    impl Bench {
        fn new() -> TestResult<Self> {
            let root = TempDir::new()?;
            std::fs::write(root.path().join("questions.json"), TWO_QUESTIONS)?;
            Ok(Self {
                root,
                now: Cell::new(NOW.parse()?),
            })
        }

        fn store(&self) -> PathBuf {
            self.root.path().join("gate")
        }

        /// Caps of 1 USD each and a floor of 0, so every row of a cut is in play.
        fn args(&self, snapshot: &Path) -> Args {
            Args {
                snapshot: snapshot.to_path_buf(),
                questions: self.root.path().join("questions.json"),
                model: MODEL.to_owned(),
                run_cap_usd: "1".to_owned(),
                day_cap_usd: "1".to_owned(),
                total_cap_usd: "1".to_owned(),
                input_price_per_mtok: "0.042".to_owned(),
                give_up_after_hours: 24,
                store: self.store(),
                from_seq: Some(0),
                limit: None,
                sample_only: false,
                repeat_index: 0,
                ack_cost_model: None,
                dry_run: false,
            }
        }

        /// One run at the bench's instant, which then moves on a second: a run id is its start.
        fn go(&self, args: &Args, exchange: &dyn ModelExchange) -> Result<Finished, ShadowError> {
            let done = run(
                args,
                &Env {
                    exchange,
                    now: &|| self.now.get(),
                    sleep: &|_| {},
                },
            );
            self.now.set(self.now.get() + TimeDelta::seconds(1));
            done
        }

        fn count(&self, sql: &str) -> TestResult<i64> {
            let db = Connection::open(self.store().join(DB_FILE))?;
            Ok(db.query_row(sql, [], |row| row.get(0))?)
        }

        /// Every file under the bench, as (path, bytes).
        fn files(&self) -> TestResult<Vec<(PathBuf, Vec<u8>)>> {
            let mut found = Vec::new();
            let mut pending = vec![self.root.path().to_path_buf()];
            while let Some(dir) = pending.pop() {
                for entry in std::fs::read_dir(dir)? {
                    let path = entry?.path();
                    if path.is_dir() {
                        pending.push(path);
                    } else {
                        found.push((path.clone(), std::fs::read(&path)?));
                    }
                }
            }
            found.sort();
            Ok(found)
        }
    }

    fn at(envelope: &Value, pointer: &str) -> Value {
        envelope.pointer(pointer).cloned().unwrap_or(Value::Null)
    }

    #[test]
    fn a_run_labels_each_new_story_once_and_reads_its_own_store_back() -> TestResult {
        let (bench, stub) = (Bench::new()?, Stub::default());
        let snapshot = cut(&three_stories(), CUTOFF)?;
        let done = bench.go(&bench.args(snapshot.path()), &stub)?;
        assert_eq!(done.exit, 0);
        assert_eq!(at(&done.envelope, "/stories/labelled"), 3);
        assert_eq!(at(&done.envelope, "/requests/ok"), 3);
        assert_eq!(at(&done.envelope, "/questions/answered"), 6);
        assert_eq!(at(&done.envelope, "/cost_micro_usd/run"), 15);
        assert_eq!(at(&done.envelope, "/answering_models"), json!([MODEL]));
        assert_eq!(at(&done.envelope, "/answered_by_another_id"), 0);
        // Received 2 s after NOW; observed 14 h 10 min before it.
        assert_eq!(at(&done.envelope, "/label_lag_seconds/p50"), 51_002);
        assert_eq!(
            at(&done.envelope, "/exit"),
            json!({"code": 0, "error_code": null})
        );
        assert_eq!(bench.count("SELECT COUNT(*) FROM run_events")?, 2);
        Ok(())
    }

    /// Section 8 row MG2. Mutation 2026-11-23, `ModelId::parse` moved after `Store::open`: red.
    #[test]
    fn an_alias_model_id_is_refused_before_any_row() -> TestResult {
        let (bench, stub) = (Bench::new()?, Stub::default());
        let snapshot = cut(&three_stories(), CUTOFF)?;
        for alias in ["~example/decide-latest", "example/decide-1"] {
            let mut args = bench.args(snapshot.path());
            args.model = alias.to_owned();
            let refused = bench.go(&args, &stub).err();
            assert_eq!(
                refused.as_ref().map(ShadowError::code),
                Some("invalid_argument")
            );
            assert_eq!(refused.map(|why| why.exit()), Some(2));
        }
        assert!(!bench.store().exists());
        assert_eq!(stub.sends(), 0);
        Ok(())
    }

    /// Section 8 row MG3. Mutation 2026-11-23, the `refusal` check in `send` removed: red.
    #[test]
    fn a_call_that_would_pass_a_cap_is_refused_and_nothing_is_sent() -> TestResult {
        let snapshot = cut(&three_stories(), CUTOFF)?;
        for cap in ["run", "day", "total"] {
            let (bench, stub) = (Bench::new()?, Stub::default());
            let mut args = bench.args(snapshot.path());
            // 0.00001 USD is 10 micro-USD, under one request's worst case of 16.
            match cap {
                "run" => args.run_cap_usd = "0.00001".to_owned(),
                "day" => args.day_cap_usd = "0.00001".to_owned(),
                _ => args.total_cap_usd = "0.00001".to_owned(),
            }
            let done = bench.go(&args, &stub)?;
            assert_eq!(stub.sends(), 0, "{cap}");
            assert_eq!(bench.count("SELECT COUNT(*) FROM attempt_events")?, 0);
            assert_eq!(done.exit, 8);
            assert_eq!(at(&done.envelope, "/stopped_by"), "cost_cap_reached");
            assert_eq!(at(&done.envelope, "/exit/error_code"), "cost_cap_reached");
            assert_eq!(at(&done.envelope, "/requests/refused_by_cap"), 1);
            assert_eq!(at(&done.envelope, "/stories/left"), 3);
        }
        Ok(())
    }

    /// The cap counts an unanswered attempt at its worst case: 30 micro-USD admits one failed
    /// attempt (16) and refuses its retry. Mutation 2026-11-23, `spent.add(worst)` after the
    /// open removed: red (three sends).
    #[test]
    fn a_failed_attempt_spends_its_worst_case_against_the_cap() -> TestResult {
        let bench = Bench::new()?;
        let stub = Stub::scripted(vec![Err("timeout")]);
        let snapshot = cut(&three_stories(), CUTOFF)?;
        let mut args = bench.args(snapshot.path());
        args.run_cap_usd = "0.00003".to_owned();
        let done = bench.go(&args, &stub)?;
        assert_eq!(stub.sends(), 1);
        assert_eq!(at(&done.envelope, "/stopped_by"), "cost_cap_reached");
        assert_eq!(at(&done.envelope, "/cost_micro_usd/run"), 16);
        Ok(())
    }

    /// Section 8 row MG4. Mutation 2026-11-23, `story_state` answering `(false, false)`: red.
    #[test]
    fn a_request_already_answered_is_not_sent_again() -> TestResult {
        let (bench, stub) = (Bench::new()?, Stub::default());
        let snapshot = cut(&three_stories(), CUTOFF)?;
        bench.go(&bench.args(snapshot.path()), &stub)?;
        let again = bench.go(&bench.args(snapshot.path()), &stub)?;
        assert_eq!(stub.sends(), 3);
        assert_eq!(at(&again.envelope, "/stories/already_labelled"), 3);
        assert_eq!(at(&again.envelope, "/requests/sent"), 0);
        assert_eq!(at(&again.envelope, "/changed"), false);
        assert_eq!(bench.count("SELECT COUNT(*) FROM requests")?, 3);
        Ok(())
    }

    /// B4. Mutation 2026-11-23, `request_answered` in `label` answering false: red (two sends).
    #[test]
    fn two_stories_with_one_body_get_two_label_rows_and_one_request() -> TestResult {
        let (bench, stub) = (Bench::new()?, Stub::default());
        let twins = [
            story(5, "story-a", DAY_ONE, Some("One wire text.")),
            story(6, "story-b", DAY_ONE, Some("One wire text.")),
        ];
        let snapshot = cut(&twins, CUTOFF)?;
        let done = bench.go(&bench.args(snapshot.path()), &stub)?;
        assert_eq!(stub.sends(), 1);
        assert_eq!(bench.count("SELECT COUNT(*) FROM requests")?, 1);
        assert_eq!(
            bench.count("SELECT COUNT(DISTINCT story_id) FROM labels")?,
            2
        );
        assert_eq!(at(&done.envelope, "/requests/already_answered"), 1);
        assert_eq!(at(&done.envelope, "/stories/labelled"), 2);
        Ok(())
    }

    /// B1. A day-dated row enters a later cut with a `seq` below one already labelled. Mutation
    /// 2026-11-23, selection by `seq > highest labelled seq`: red (story-late never sent).
    #[test]
    fn a_row_that_becomes_visible_late_is_still_labelled() -> TestResult {
        let (bench, stub) = (Bench::new()?, Stub::default());
        let early = story(
            5,
            "story-a",
            "2026-11-19T10:00:00.000000Z",
            Some("Early text."),
        );
        let later = story(
            7,
            "story-c",
            "2026-11-19T16:00:00.000000Z",
            Some("Later text."),
        );
        let dated = SnapshotRow {
            valid_at: None,
            valid_day: Some("2026-11-19".to_owned()),
            valid_tz: Some("America/New_York".to_owned()),
            ..story(
                6,
                "story-late",
                "2026-11-19T15:00:00.000000Z",
                Some("Day-dated text."),
            )
        };
        let first = cut(&[early.clone(), later.clone()], "2026-11-19T20:00:00Z")?;
        let second = cut(&[early, dated, later], "2026-11-20T06:00:00Z")?;
        bench.go(&bench.args(first.path()), &stub)?;
        let done = bench.go(&bench.args(second.path()), &stub)?;
        assert_eq!(at(&done.envelope, "/stories/already_labelled"), 2);
        assert_eq!(at(&done.envelope, "/stories/labelled"), 1);
        assert_eq!(
            bench.count("SELECT COUNT(*) FROM labels WHERE story_id = 'story-late' AND seq = 6")?,
            1
        );
        Ok(())
    }

    /// First sighting wins: a later row of a story is counted and never sent, in either order
    /// of `seq`.
    #[test]
    fn only_the_first_sighted_row_of_a_story_is_labelled() -> TestResult {
        let (bench, stub) = (Bench::new()?, Stub::default());
        let rows = [
            story(
                5,
                "story-a",
                "2026-11-20T12:00:00.000000Z",
                Some("Corrected text."),
            ),
            story(
                6,
                "story-a",
                "2026-11-20T10:00:00.000000Z",
                Some("First text."),
            ),
        ];
        let snapshot = cut(&rows, CUTOFF)?;
        let done = bench.go(&bench.args(snapshot.path()), &stub)?;
        assert_eq!(at(&done.envelope, "/stories/repeat_of_story"), 1);
        assert_eq!(bench.count("SELECT seq FROM labels")?, 6);
        let sent = String::from_utf8(stub.bodies.borrow()[0].clone())?;
        assert!(sent.contains("First text."), "{sent}");
        Ok(())
    }

    /// B3. Mutations 2026-11-23, each red: `Failed` made an answered outcome (run two sends
    /// nothing); the age test dropped (run three sends again).
    #[test]
    fn a_failed_story_is_retried_by_a_later_run_until_the_age_limit() -> TestResult {
        let bench = Bench::new()?;
        let refused = || status(400, "{\"error\":\"bad request\"}");
        let stub = Stub::scripted(vec![refused(), refused()]);
        let snapshot = cut(&three_stories()[..1], CUTOFF)?;
        let first = bench.go(&bench.args(snapshot.path()), &stub)?;
        assert_eq!((first.exit, stub.sends()), (8, 1));
        assert_eq!(at(&first.envelope, "/exit/error_code"), "partial_shadow");
        let second = bench.go(&bench.args(snapshot.path()), &stub)?;
        assert_eq!((second.exit, stub.sends()), (8, 2));
        // 24 h after the observation at 10:00 the story is still inside the limit; one second on,
        // it is not.
        bench.now.set("2026-11-21T10:00:01Z".parse()?);
        let third = bench.go(&bench.args(snapshot.path()), &stub)?;
        assert_eq!((third.exit, stub.sends()), (0, 2));
        assert_eq!(at(&third.envelope, "/stories/gave_up"), 1);
        assert_eq!(bench.count("SELECT COUNT(*) FROM labels")?, 1);
        Ok(())
    }

    /// Section 8 row MG2b. Mutation 2026-11-23, the `predates` arm of `consider` removed: red.
    #[test]
    fn a_build_dated_on_or_after_a_storys_observation_day_does_not_label_it() -> TestResult {
        let (bench, stub) = (Bench::new()?, Stub::default());
        let snapshot = cut(&three_stories(), CUTOFF)?;
        let mut args = bench.args(snapshot.path());
        args.model = "example/decide-2-20261120".to_owned();
        let done = bench.go(&args, &stub)?;
        assert_eq!(stub.sends(), 0);
        assert_eq!(at(&done.envelope, "/stories/build_postdates_story"), 3);
        assert_eq!(bench.count("SELECT COUNT(*) FROM requests")?, 0);
        Ok(())
    }

    /// Forward only: with no `--from-seq` the first run fixes the floor at the cut's high water
    /// and labels nothing, and no later run may move it.
    #[test]
    fn the_first_run_fixes_the_floor_and_a_later_from_seq_cannot_move_it() -> TestResult {
        let (bench, stub) = (Bench::new()?, Stub::default());
        let snapshot = cut(&three_stories(), CUTOFF)?;
        let mut args = bench.args(snapshot.path());
        args.from_seq = None;
        let first = bench.go(&args, &stub)?;
        assert_eq!(
            at(&first.envelope, "/floor"),
            json!({"start_seq": 7, "set_by_this_run": true})
        );
        assert_eq!(at(&first.envelope, "/stories/below_floor"), 3);
        args.from_seq = Some(0);
        let moved = bench.go(&args, &stub).err();
        assert_eq!(
            moved.as_ref().map(ShadowError::code),
            Some("invalid_argument")
        );
        let mut grown = three_stories();
        grown.push(story(
            8,
            "story-d",
            DAY_ONE,
            Some("Synthetic release four."),
        ));
        let later = cut(&grown, CUTOFF)?;
        args.from_seq = None;
        args.snapshot = later.path().to_path_buf();
        let second = bench.go(&args, &stub)?;
        assert_eq!(at(&second.envelope, "/stories/labelled"), 1);
        assert_eq!(stub.sends(), 1);
        Ok(())
    }

    /// Section 8 row MG6. The blob is the bytes as received, whatever they parse to. Mutation
    /// 2026-11-23, `put_blob` skipped in `AttemptSlot::close`: red.
    #[test]
    fn the_stored_response_blob_hashes_to_response_sha256_even_when_it_does_not_parse() -> TestResult
    {
        let bench = Bench::new()?;
        let gateway = "<html>bad gateway text served with a 200</html>";
        let stub = Stub::scripted(vec![status(200, gateway)]);
        let snapshot = cut(&three_stories()[..2], CUTOFF)?;
        let done = bench.go(&bench.args(snapshot.path()), &stub)?;
        assert_eq!(at(&done.envelope, "/requests/unparseable"), 1);
        assert_eq!(at(&done.envelope, "/requests/ok"), 1);
        let db = Connection::open(bench.store().join(DB_FILE))?;
        let mut statement = db.prepare(
            "SELECT response_sha256 FROM attempt_events WHERE event = 'answered' ORDER BY rowid",
        )?;
        let named: Vec<String> = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        assert_eq!(named.len(), 2);
        for hex in &named {
            let bytes = std::fs::read(bench.store().join(BLOB_DIR).join(hex))?;
            assert_eq!(FileDigest::of(&bytes).hex(), hex);
        }
        assert_eq!(
            std::fs::read(bench.store().join(BLOB_DIR).join(&named[0]))?,
            gateway.as_bytes()
        );
        let sent = FileDigest::of(&stub.bodies.borrow()[0]);
        assert!(bench.store().join(BLOB_DIR).join(sent.hex()).is_file());
        Ok(())
    }

    /// B9 and section 8 row MG7. The answering id is stored raw and compared when read; the
    /// answer is kept. Mutation 2026-11-23, `answering_model` written as the id sent: red.
    #[test]
    fn an_answer_from_another_id_is_kept_and_counted_when_read() -> TestResult {
        let bench = Bench::new()?;
        let undated = "example/decide-1";
        let stub = Stub::scripted(vec![status(200, &answer_json(undated, COST_USD))]);
        let snapshot = cut(&three_stories()[..2], CUTOFF)?;
        let done = bench.go(&bench.args(snapshot.path()), &stub)?;
        assert_eq!(done.exit, 0);
        assert_eq!(at(&done.envelope, "/answered_by_another_id"), 1);
        assert_eq!(
            at(&done.envelope, "/answering_models"),
            json!([undated, MODEL])
        );
        assert_eq!(at(&done.envelope, "/stories/labelled"), 2);
        Ok(())
    }

    #[test]
    fn a_retryable_status_is_sent_again_as_its_own_attempt_and_waits_what_it_was_told() -> TestResult
    {
        let bench = Bench::new()?;
        let slow = Ok(HttpAnswer {
            status: 429,
            body: b"{}".to_vec(),
            retry_after_seconds: Some(900),
        });
        let stub = Stub::scripted(vec![slow, Err("timeout")]);
        let snapshot = cut(&three_stories()[..1], CUTOFF)?;
        let waits = RefCell::new(Vec::new());
        let done = run(
            &bench.args(snapshot.path()),
            &Env {
                exchange: &stub,
                now: &|| bench.now.get(),
                sleep: &|wait| waits.borrow_mut().push(wait),
            },
        )?;
        assert_eq!((done.exit, stub.sends()), (0, 3));
        // The vendor's 900 s is capped at 30; a transport failure waits 2 s x the attempt.
        assert_eq!(
            *waits.borrow(),
            [Duration::from_secs(30), Duration::from_secs(4)]
        );
        assert_eq!(at(&done.envelope, "/requests/failed"), 2);
        assert_eq!(
            bench.count("SELECT COUNT(*) FROM attempt_events WHERE event = 'opened'")?,
            3
        );
        Ok(())
    }

    #[test]
    fn a_vendor_that_answers_nothing_stops_the_run_and_exits_seven() -> TestResult {
        let bench = Bench::new()?;
        let stub = Stub::scripted(vec![Err("connection_failed"); 20]);
        let mut rows = three_stories();
        rows.push(story(
            8,
            "story-d",
            DAY_ONE,
            Some("Synthetic release four."),
        ));
        let snapshot = cut(&rows, CUTOFF)?;
        let failure = bench.go(&bench.args(snapshot.path()), &stub).err();
        let Some(failure) = failure else {
            return Err("the run answered".into());
        };
        assert_eq!((failure.code(), failure.exit()), ("vendor_unreachable", 7));
        assert!(failure.retryable());
        assert_eq!(stub.sends(), 9);
        let details = failure.details();
        assert_eq!(at(&details, "/stopped_by"), "vendor_failing");
        assert_eq!(at(&details, "/stories/left"), 1);
        assert_eq!(
            at(&details, "/requests/failed_stories"),
            FAILED_STORIES_BEFORE_STOP
        );
        assert_eq!(
            bench.count("SELECT COUNT(*) FROM run_events WHERE event = 'ended'")?,
            1
        );
        Ok(())
    }

    /// C5. Mutations 2026-11-23, each red: the `cost > worst` branch removed; the latch read
    /// removed from `run`.
    #[test]
    fn a_vendor_cost_above_the_worst_case_latches_until_acknowledged() -> TestResult {
        let bench = Bench::new()?;
        // 0.0001 USD is 100 micro-USD against a worst case of 16.
        let stub = Stub::scripted(vec![status(200, &answer_json(MODEL, 0.000_1))]);
        let snapshot = cut(&three_stories(), CUTOFF)?;
        let broke = bench.go(&bench.args(snapshot.path()), &stub)?;
        assert_eq!((broke.exit, stub.sends()), (8, 1));
        assert_eq!(
            at(&broke.envelope, "/exit/error_code"),
            "cost_estimate_broken"
        );
        assert_eq!(at(&broke.envelope, "/stories/labelled"), 1);
        assert_eq!(at(&broke.envelope, "/cost_micro_usd/run"), 100);
        let latched = bench.go(&bench.args(snapshot.path()), &stub)?;
        assert_eq!((latched.exit, stub.sends()), (8, 1));
        assert_eq!(at(&latched.envelope, "/stories/left"), 2);
        let mut args = bench.args(snapshot.path());
        args.ack_cost_model = at(&broke.envelope, "/run_id").as_str().map(str::to_owned);
        let acked = bench.go(&args, &stub)?;
        assert_eq!((acked.exit, stub.sends()), (0, 3));
        Ok(())
    }

    /// Mutation 2026-11-23, `Store::open` used for a dry run: red (the directory appears).
    #[test]
    fn a_dry_run_sends_nothing_and_writes_nothing() -> TestResult {
        let (bench, stub) = (Bench::new()?, Stub::default());
        let snapshot = cut(&three_stories(), CUTOFF)?;
        let mut args = bench.args(snapshot.path());
        args.dry_run = true;
        let fresh = bench.go(&args, &stub)?;
        assert!(!bench.store().exists());
        assert_eq!(at(&fresh.envelope, "/requests/planned"), 3);
        assert_eq!(
            at(&fresh.envelope, "/cost_micro_usd/planned_estimate"),
            Value::Null
        );
        args.dry_run = false;
        args.limit = Some(1);
        let limited = bench.go(&args, &stub)?;
        assert_eq!(at(&limited.envelope, "/stopped_by"), "limit");
        assert_eq!((limited.exit, stub.sends()), (0, 1));
        let before = bench.files()?;
        args.dry_run = true;
        args.limit = None;
        let planned = bench.go(&args, &stub)?;
        assert_eq!(bench.files()?, before);
        assert_eq!(stub.sends(), 1);
        assert_eq!(at(&planned.envelope, "/dry_run"), true);
        assert_eq!(at(&planned.envelope, "/changed"), false);
        assert_eq!(at(&planned.envelope, "/requests/planned"), 2);
        assert_eq!(at(&planned.envelope, "/stories/already_labelled"), 1);
        Ok(())
    }

    #[test]
    fn a_repeat_index_is_another_request_with_the_same_bytes_and_the_sample_is_a_tenth(
    ) -> TestResult {
        let (bench, stub) = (Bench::new()?, Stub::default());
        let snapshot = cut(&three_stories()[..1], CUTOFF)?;
        let mut args = bench.args(snapshot.path());
        bench.go(&args, &stub)?;
        args.repeat_index = 1;
        let repeat = bench.go(&args, &stub)?;
        assert_eq!(at(&repeat.envelope, "/stories/labelled"), 1);
        assert_eq!(stub.sends(), 2);
        assert_eq!(stub.bodies.borrow()[0], stub.bodies.borrow()[1]);
        assert_eq!(bench.count("SELECT COUNT(*) FROM requests")?, 2);
        let sampled = (0..2_560)
            .filter(|index| in_sample(&format!("story-{index}")))
            .count();
        // 26/256 of 2,560 is 260; a sha256 prefix over 2,560 ids lands within 60 of it.
        assert!((200..=320).contains(&sampled), "{sampled}");
        args.sample_only = true;
        args.repeat_index = 2;
        let outside = bench.go(&args, &stub)?;
        assert_eq!(
            at(&outside.envelope, "/stories/outside_sample"),
            u64::from(!in_sample("story-a"))
        );
        Ok(())
    }

    #[test]
    fn an_absent_or_inadmissible_input_is_refused_with_its_code_and_exit() -> TestResult {
        let (bench, stub) = (Bench::new()?, Stub::default());
        let snapshot = cut(&three_stories(), CUTOFF)?;
        let base = bench.args(snapshot.path());
        let broken = TempDir::new()?;
        std::fs::write(broken.path().join(MANIFEST_FILE), "{}")?;
        let bad_questions = bench.root.path().join("bad.json");
        std::fs::write(&bad_questions, "{\"schema\": 1}")?;
        let cases = [
            Args {
                snapshot: bench.root.path().join("no-cut"),
                ..base.clone()
            },
            Args {
                snapshot: broken.path().to_path_buf(),
                ..base.clone()
            },
            Args {
                questions: bench.root.path().join("none.json"),
                ..base.clone()
            },
            Args {
                questions: bad_questions,
                ..base.clone()
            },
            Args {
                run_cap_usd: "1e3".to_owned(),
                ..base.clone()
            },
        ];
        let refusals: Vec<(&str, u8)> = cases
            .iter()
            .filter_map(|args| bench.go(args, &stub).err())
            .map(|why| (why.code(), why.exit()))
            .collect();
        assert_eq!(
            refusals,
            [
                ("input_absent", 4),
                ("input_schema_mismatch", 2),
                ("input_absent", 4),
                ("input_unparseable", 2),
                ("invalid_argument", 2),
            ]
        );
        assert_eq!(stub.sends(), 0);
        Ok(())
    }

    /// Section 8 row MG5, over the real wire against a loopback listener: 200, 401, 500, a 302
    /// that must not be followed, a dropped connection, 200. Mutations 2026-11-23, each red: the
    /// wire appending the key to the body it returns (a blob then holds it); `max_redirects(0)`
    /// raised to 1 (the 302 is followed and the statuses shift).
    #[test]
    fn the_key_appears_in_no_row_blob_envelope_or_error() -> TestResult {
        let bench = Bench::new()?;
        let ok = answer_json(MODEL, COST_USD);
        let served = Loopback::serving(vec![
            http("200 OK", "", &ok),
            http("401 Unauthorized", "", "{\"error\":\"no such key\"}"),
            http("500 Internal Server Error", "", "{}"),
            http("302 Found", "location: /api/v1/elsewhere\r\n", ""),
            String::new(),
            http("200 OK", "", &ok),
        ])?;
        let wire = ModelWire::loopback(sentinel(), served.url.clone());
        let mut rows = three_stories();
        rows.push(story(
            8,
            "story-d",
            DAY_ONE,
            Some("Synthetic release four."),
        ));
        let snapshot = cut(&rows, CUTOFF)?;
        let done = bench.go(&bench.args(snapshot.path()), &wire)?;
        let heads = served.requests();
        assert_eq!(heads.len(), 6);
        for head in &heads {
            let lower = head.to_lowercase();
            assert!(lower.contains(&format!("authorization: bearer {SENTINEL_KEY}")));
            assert!(lower.starts_with("post /api/v1/systemone "), "{lower}");
        }
        let db = Connection::open(bench.store().join(DB_FILE))?;
        let mut statement = db.prepare(
            "SELECT COALESCE(http_status, 0) FROM attempt_events WHERE event = 'answered'
             ORDER BY rowid",
        )?;
        let statuses: Vec<i64> = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        assert_eq!(statuses, [200, 401, 500, 302, 0, 200]);
        assert_eq!(at(&done.envelope, "/stories/labelled"), 2);
        // A closed port: every attempt is a transport failure and the run is an error object.
        let closed = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?;
        let dead = ModelWire::loopback(sentinel(), format!("http://{closed}/api/v1/systemone"));
        let other = Bench::new()?;
        let failure = other.go(&other.args(snapshot.path()), &dead).err();
        let said = failure
            .as_ref()
            .map(|why| format!("{why} {}", why.details()))
            .unwrap_or_default();
        let shown = format!(
            "{done:?} {} {failure:?} {said} {:?}",
            done.envelope,
            sentinel()
        );
        assert_eq!(failure.map(|why| why.code()), Some("vendor_unreachable"));
        assert!(!shown.contains(SENTINEL_KEY), "{shown}");
        for (path, bytes) in bench.files()?.into_iter().chain(other.files()?) {
            let leaked = bytes
                .windows(SENTINEL_KEY.len())
                .any(|window| window == SENTINEL_KEY.as_bytes());
            assert!(!leaked, "{}", path.display());
        }
        Ok(())
    }
}
