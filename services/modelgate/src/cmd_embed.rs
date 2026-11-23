//! `modelgate embed run`: one `embed` request per first-sighted story of a `newsvault cut` that
//! has no vector yet, oldest first, then the comparison record of each story whose earlier window
//! is settled. Record only: no verdict is written and `shadow run` reads none of it. Rules:
//! `docs/design/architecture.md` section 3.5, "The `embed` kind" and "Repeated releases".

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{json, Value};
use snapshot_contract::SnapshotRow;

use crate::cap::{parse_usd, refusal, worst_case, Caps, Spent};
use crate::cmd_shadow::{
    admissible, instant, invalid, unreadable, Env, Finished, ShadowError, EXIT_INCOMPLETE, EXIT_OK,
    EXIT_TRANSIENT, FAILED_STORIES_BEFORE_STOP, RETRYABLE_STATUSES, RETRY_AFTER_CEILING,
    RETRY_WAIT, SECONDS_PER_HOUR,
};
use crate::coverage;
use crate::embed::{
    build, dot, ppm, read_embedding, EmbedReading, EmbedRequest, EmbedSpace, EMBED_FORMAT,
};
use crate::embed_store::{
    CanaryRow, Candidate, Comparison, EmbedStore, LIST_GUARDED, LIST_NEAREST,
};
use crate::ledger::{AttemptSlot, Opening};
use crate::request::STATE_BUDGET_BYTES;
use crate::response::{Outcome, Reading};
use crate::signals::{Signals, HOST_GRAMMAR, NUMBER_GRAMMAR};
use crate::store::{stamp, StoreError};
use crate::wire::EMBED_CALL_TIMEOUT;

pub const SCHEMA: &str = "modelgate.embed_run/1";
/// Sends of one story's request within one run. A work cap, not fitted to a failure rate.
pub const MAX_ATTEMPTS: u32 = 2;
const COMPARE: &str = "dedup.compare/1";
/// Days before a story inside which an earlier story is a candidate. Chosen 2026-11-23 before
/// any similarity was read; `dedup distribution` reports the age of each best candidate.
pub const WINDOW_DAYS: i64 = 7;
/// Candidates kept per list.
pub const KEEP: usize = 5;
/// Stories compared by one run: 256 unit vectors are 4 MiB held while the window is streamed.
const COMPARE_BATCH: usize = 256;
const DAY_BYTES: usize = 10;
const UNIX_PREFIX: char = '@';

/// The token every comparison row carries. Built from the constants and the three grammars, so a
/// changed window, list length or grammar is another token and never pools with these rows.
#[must_use]
pub fn compare_version() -> String {
    format!(
        "{COMPARE}(window_days={WINDOW_DAYS};keep={KEEP};dot=8-lane-f32;{HOST_GRAMMAR};\
         {NUMBER_GRAMMAR};{})",
        coverage::version()
    )
}

/// A fixed synthetic text embedded again and again. Its name is its identity: a changed text
/// takes a new name, or its answers would be compared with another text's first answer.
struct Canary {
    name: &'static str,
    /// Sent once per UTC day; otherwise by every run.
    daily: bool,
}

const SHORT: &str = "short/1";
const LONG: &str = "long/1";
const PAIR_EN: &str = "pair-en/1";
const PAIR_DE: &str = "pair-de/1";
const CANARIES: [Canary; 4] = [
    Canary {
        name: SHORT,
        daily: false,
    },
    Canary {
        name: LONG,
        daily: true,
    },
    Canary {
        name: PAIR_EN,
        daily: true,
    },
    Canary {
        name: PAIR_DE,
        daily: true,
    },
];
const SHORT_TEXT: &str = "Canary text, not a release. Northfield Tooling, a company that does not exist, says it will open a second plant in spring and hire forty people.";
const PAIR_EN_TEXT: &str = "Canary text, not a release. Brightwater Ferries, a company that does not exist, has ordered three electric vessels for delivery in 2028 and will retire its two oldest ships. The order is worth 85 million euros. Contact: https://www.brightwater-ferries.example";
const PAIR_DE_TEXT: &str = "Kanarientext, keine Mitteilung. Brightwater Ferries, ein Unternehmen, das es nicht gibt, hat drei elektrische Schiffe zur Lieferung im Jahr 2028 bestellt und wird seine zwei ältesten Schiffe außer Dienst stellen. Der Auftrag hat einen Wert von 85 Millionen Euro. Kontakt: https://www.brightwater-ferries.example";

/// The long canary is over the state budget, so it exercises the cut and the largest input.
fn canary_text(name: &str) -> String {
    match name {
        SHORT => SHORT_TEXT.to_owned(),
        PAIR_EN => PAIR_EN_TEXT.to_owned(),
        PAIR_DE => PAIR_DE_TEXT.to_owned(),
        _ => {
            let mut text = String::from("Canary text, not a release.");
            let mut week = 0;
            while text.len() <= STATE_BUDGET_BYTES {
                week += 1;
                text.push_str(&format!(
                    " Northfield Tooling, a company that does not exist, reports no event in \
                     week {week}."
                ));
            }
            text
        }
    }
}

#[derive(Debug, Clone)]
pub struct Args {
    pub snapshot: PathBuf,
    pub model: String,
    pub provider: String,
    pub quantization: String,
    pub epoch: u32,
    pub run_cap_usd: String,
    pub day_cap_usd: String,
    pub total_cap_usd: String,
    pub input_price_per_mtok: String,
    pub give_up_after_hours: u32,
    pub limit: u64,
    /// An instant, or `@` and whole seconds since the Unix epoch.
    pub deadline: String,
    pub store: PathBuf,
    pub ack_cost_model: Option<String>,
    pub dry_run: bool,
}

fn deadline(text: &str) -> Result<DateTime<Utc>, ShadowError> {
    let refused = || {
        invalid(format!(
            "--deadline {text:?} is not an instant or @<unix seconds>"
        ))
    };
    match text.strip_prefix(UNIX_PREFIX) {
        Some(seconds) => seconds
            .parse::<i64>()
            .ok()
            .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
            .ok_or_else(refused),
        None => text.parse().map_err(|_| refused()),
    }
}

/// `(observed_at, seq)`: text order is time order, and the lower `seq` is the earlier of a tie.
type Key = (String, i64);

struct First {
    key: Key,
    has_body: bool,
}

/// A first-sighted row with a body, inside the horizon the comparisons of this run can reach.
struct Seen {
    story_id: String,
    key: Key,
    at: DateTime<Utc>,
    source: String,
    signals: Signals,
    /// The request whose vector represents this story, once it has one.
    vector_key: Option<String>,
}

#[derive(Default)]
struct Stories {
    rows: u64,
    repeat_of_story: u64,
    no_body: u64,
    already_embedded: u64,
    gave_up: u64,
    selected: u64,
    embedded: u64,
    twin_of_embedded: u64,
    unusable: u64,
    truncated: u64,
    left: u64,
}

#[derive(Default)]
struct Requests {
    planned: u64,
    planned_bytes: u64,
    planned_worst_case: i64,
    sent: u64,
    recovered: u64,
    failed_stories: u64,
    refused_by_cap: u64,
}

#[derive(Default)]
struct Compared {
    written: u64,
    waiting: u64,
    left: u64,
    vectors_read: u64,
}

enum Attempted {
    Read {
        found: EmbedReading,
        retryable: bool,
        retry_after: Option<u64>,
    },
    Refused,
    Deadline,
}

enum Sent {
    Embedded,
    Unusable,
    Failed,
    Stopped,
}

struct Run<'a> {
    args: &'a Args,
    env: &'a Env<'a>,
    store: EmbedStore,
    space: EmbedSpace,
    space_name: String,
    compare_version: String,
    caps: Caps,
    price: i64,
    run_id: String,
    started: DateTime<Utc>,
    deadline: DateTime<Utc>,
    snapshot_id: String,
    spent: Spent,
    planned_spent: Spent,
    ordinal: u32,
    stories: Stories,
    requests: Requests,
    compared: Compared,
    canaries: Vec<Value>,
    consecutive_failed: u32,
    every_failure_transient: bool,
    stopped_by: Option<&'static str>,
    store_failure: Option<String>,
}

pub fn run(args: &Args, env: &Env) -> Result<Finished, ShadowError> {
    let space = EmbedSpace::parse(&args.model, &args.provider, &args.quantization, args.epoch)
        .map_err(invalid)?;
    let caps = Caps {
        run: parse_usd(&args.run_cap_usd).map_err(invalid)?,
        day: parse_usd(&args.day_cap_usd).map_err(invalid)?,
        total: parse_usd(&args.total_cap_usd).map_err(invalid)?,
    };
    let price = parse_usd(&args.input_price_per_mtok).map_err(invalid)?;
    if price == 0 {
        return Err(invalid(
            "--input-price-per-mtok 0 makes every worst case 0, so no cap could refuse a call",
        ));
    }
    if args.limit == 0 {
        return Err(invalid("--limit 0 embeds nothing"));
    }
    let deadline = deadline(&args.deadline)?;
    admissible(&args.snapshot)?;
    let manifest = snapshot_contract::manifest(&args.snapshot).map_err(unreadable)?;
    let started = (env.now)();
    let run_id = stamp(started);
    let store = if args.dry_run {
        EmbedStore::read_only(&args.store)?
    } else {
        EmbedStore::open(&args.store)?
    };
    let ledger = store.ledger();
    let mut acknowledges = None;
    if let Some(named) = &args.ack_cost_model {
        match ledger.cost_break(named)? {
            None => {
                return Err(invalid(format!(
                    "--ack-cost-model {named}: no embed run of that id broke the cost estimate"
                )))
            }
            Some(false) => acknowledges = Some(named),
            Some(true) => {}
        }
    }
    let space_name = space.name();
    let mut swept = 0;
    if !args.dry_run {
        swept = ledger.sweep(started, &run_id)?;
        ledger.run_event(
            &run_id,
            "started",
            started,
            &json!({
                "kind": "embed", "snapshot_id": manifest.snapshot_id, "cutoff": manifest.cutoff,
                "space": space_name, "deadline": stamp(deadline),
                "caps_micro_usd": {"run": caps.run, "day": caps.day, "total": caps.total},
                "input_price_micro_usd_per_mtok": price,
            }),
        )?;
        if let Some(acknowledged) = acknowledges {
            let detail = json!({ "acknowledges": acknowledged });
            ledger.run_event(&run_id, "cost_model_acked", started, &detail)?;
        }
    }
    let stopped_by = ledger.unacked_cost_break()?.map(|_| "cost_estimate_broken");
    let spent = ledger.spent(&run_id, started)?;
    let mut run = Run {
        args,
        env,
        store,
        space,
        space_name,
        compare_version: compare_version(),
        caps,
        price,
        run_id,
        started,
        deadline,
        snapshot_id: manifest.snapshot_id.clone(),
        spent,
        planned_spent: spent,
        ordinal: 0,
        stories: Stories::default(),
        requests: Requests::default(),
        compared: Compared::default(),
        canaries: Vec::new(),
        consecutive_failed: 0,
        every_failure_transient: true,
        stopped_by,
        store_failure: None,
    };
    match run.work() {
        Err(ShadowError::Store(why)) if run.requests.sent > 0 => {
            run.store_failure = Some(why.to_string());
            run.stopped_by = Some("store_failed");
        }
        other => other?,
    }
    run.finish(&manifest, swept)
}

fn rows(
    dir: &std::path::Path,
) -> Result<impl Iterator<Item = Result<SnapshotRow, ShadowError>>, ShadowError> {
    Ok(snapshot_contract::rows(dir)
        .map_err(unreadable)?
        .map(|row| row.map_err(unreadable)))
}

/// Keeps the `KEEP` highest similarities; of two equal ones the earlier offered stays ahead.
fn offer(kept: &mut Vec<(f32, usize)>, similarity: f32, index: usize) {
    let position = kept.partition_point(|(held, _)| *held >= similarity);
    if position < KEEP {
        kept.insert(position, (similarity, index));
        kept.truncate(KEEP);
    }
}

struct Query {
    index: usize,
    from: Key,
    unit: Vec<f32>,
    nearest: Vec<(f32, usize)>,
    guarded: Vec<(f32, usize)>,
    window_stories: usize,
    candidates: usize,
}

impl Run<'_> {
    fn work(&mut self) -> Result<(), ShadowError> {
        let mut embedded = self.store.embedded_stories(&self.space_name)?;
        let attempted = self.store.attempted_stories(&self.space_name)?;
        let compared = self
            .store
            .compared_stories(&self.compare_version, &self.space_name)?;
        let give_up = i64::from(self.args.give_up_after_hours) * SECONDS_PER_HOUR;
        let mut first: HashMap<String, First> = HashMap::new();
        for row in rows(&self.args.snapshot)? {
            let row = row?;
            let sighting = First {
                key: (row.observed_at, row.seq),
                has_body: row.body.is_some(),
            };
            match first.get_mut(&row.story_id) {
                Some(kept) if kept.key <= sighting.key => {}
                Some(kept) => *kept = sighting,
                None => {
                    first.insert(row.story_id, sighting);
                }
            }
        }
        let started = self.started;
        let gave_up = |story_id: &str, key: &Key| -> Result<bool, ShadowError> {
            let age = (started - instant(&key.0)?).num_seconds();
            Ok(attempted.contains(story_id) && age > give_up)
        };
        // The earliest story a comparison of this run could concern, less the window.
        let mut horizon: Option<Key> = None;
        for (story_id, sighting) in &first {
            let settled = !sighting.has_body
                || compared.contains(story_id)
                || (!embedded.contains_key(story_id) && gave_up(story_id, &sighting.key)?);
            if !settled && horizon.as_ref().is_none_or(|kept| sighting.key < *kept) {
                horizon = Some(sighting.key.clone());
            }
        }
        let horizon = match horizon {
            Some((observed_at, _)) => {
                Some(stamp(instant(&observed_at)? - TimeDelta::days(WINDOW_DAYS)))
            }
            None => None,
        };
        let limit = usize::try_from(self.args.limit).unwrap_or(usize::MAX);
        let mut kept: BTreeMap<Key, SnapshotRow> = BTreeMap::new();
        let mut pending: BTreeSet<Key> = BTreeSet::new();
        let mut seen: Vec<Seen> = Vec::new();
        for row in rows(&self.args.snapshot)? {
            let row = row?;
            self.stories.rows += 1;
            let key: Key = (row.observed_at.clone(), row.seq);
            if first.get(&row.story_id).map(|sighting| &sighting.key) != Some(&key) {
                self.stories.repeat_of_story += 1;
                continue;
            }
            let Some(body) = row.body.as_deref() else {
                self.stories.no_body += 1;
                continue;
            };
            if horizon.as_ref().is_some_and(|from| *from <= key.0) {
                seen.push(Seen {
                    story_id: row.story_id.clone(),
                    key: key.clone(),
                    at: instant(&row.observed_at)?,
                    source: row.source.clone(),
                    signals: Signals::of(body),
                    vector_key: embedded.get(&row.story_id).cloned(),
                });
            }
            if embedded.contains_key(&row.story_id) {
                self.stories.already_embedded += 1;
            } else if gave_up(&row.story_id, &key)? {
                self.stories.gave_up += 1;
            } else {
                self.stories.selected += 1;
                pending.insert(key.clone());
                kept.insert(key, row);
                if kept.len() > limit {
                    kept.pop_last();
                }
            }
        }
        self.stories.left = self.stories.selected - u64::try_from(kept.len()).unwrap_or(u64::MAX);
        self.send_canaries()?;
        for (key, row) in kept {
            if self.stopped_by.is_some() {
                self.stories.left += 1;
                continue;
            }
            if let Some(vector_key) = self.embed(&row)? {
                pending.remove(&key);
                embedded.insert(row.story_id, vector_key);
            }
        }
        seen.sort_by(|left, right| left.key.cmp(&right.key));
        for story in &mut seen {
            if story.vector_key.is_none() {
                story.vector_key = embedded.get(&story.story_id).cloned();
            }
        }
        self.compare(&seen, &pending, &compared)
    }

    /// The request whose vector now represents the story, or `None` when it has none yet.
    fn embed(&mut self, row: &SnapshotRow) -> Result<Option<String>, ShadowError> {
        let body = row.body.as_deref().unwrap_or_default();
        let request = build(&self.space, body, 0)
            .map_err(|why| ShadowError::InputUnparseable(why.to_string()))?;
        self.stories.truncated += u64::from(request.input_truncated);
        let key = request.idempotency_key.clone();
        if self.store.has_vector(&key)? {
            self.stories.twin_of_embedded += 1;
            self.link(row, &key)?;
            return Ok(Some(key));
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
            return Ok(None);
        }
        self.store.insert_request(&request, &self.space_name)?;
        self.link(row, &key)?;
        match self.send(&request, worst)? {
            Sent::Embedded => {
                self.stories.embedded += 1;
                self.consecutive_failed = 0;
                return Ok(Some(key));
            }
            Sent::Unusable => {
                self.stories.unusable += 1;
                self.consecutive_failed = 0;
            }
            Sent::Stopped => self.stories.left += 1,
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
        Ok(None)
    }

    fn link(&self, row: &SnapshotRow, key: &str) -> Result<(), ShadowError> {
        if self.args.dry_run {
            return Ok(());
        }
        Ok(self.store.link(
            &row.story_id,
            &row.item_id,
            row.seq,
            &row.observed_at,
            key,
            &self.run_id,
        )?)
    }

    /// An `ok` answer of this request already on disk, whose rows a dead run did not write.
    fn recovered(&mut self, request: &EmbedRequest) -> Result<Option<EmbedReading>, ShadowError> {
        let Some(hex) = self.store.ok_response(&request.idempotency_key)? else {
            return Ok(None);
        };
        let found = read_embedding(200, &self.store.ledger().read_blob(&hex)?);
        self.requests.recovered += u64::from(found.embedding.is_some());
        Ok(found.embedding.is_some().then_some(found))
    }

    fn keep(&self, request: &EmbedRequest, found: &EmbedReading) -> Result<(), StoreError> {
        let Some(embedding) = &found.embedding else {
            return Ok(());
        };
        self.store.insert_vector(
            &request.idempotency_key,
            embedding,
            found.reading.answering_model.as_deref(),
            found.provider.as_deref(),
            &self.run_id,
        )
    }

    /// One send inside an attempt row, refused first by the deadline and then by the caps.
    fn attempt(&mut self, request: &EmbedRequest, worst: i64) -> Result<Attempted, ShadowError> {
        let call = TimeDelta::from_std(EMBED_CALL_TIMEOUT).unwrap_or(TimeDelta::MAX);
        if (self.env.now)() + call > self.deadline {
            self.stopped_by = Some("deadline");
            return Ok(Attempted::Deadline);
        }
        if refusal(self.caps, self.spent, worst).is_some() {
            self.requests.refused_by_cap += 1;
            self.stopped_by = Some("cost_cap_reached");
            return Ok(Attempted::Refused);
        }
        let mut slot = AttemptSlot::open(
            self.store.ledger(),
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
        let found = match &exchanged.answer {
            Ok(answer) => read_embedding(answer.status, &answer.body),
            Err(_) => EmbedReading {
                reading: Reading {
                    outcome: Outcome::Failed,
                    cost_micro_usd: None,
                    answering_model: None,
                },
                provider: None,
                embedding: None,
            },
        };
        slot.close(
            self.store.ledger(),
            &exchanged,
            &found.reading,
            (self.env.now)(),
        )?;
        if let Some(cost) = found.reading.cost_micro_usd {
            self.spent.add(cost - worst);
            if cost > worst {
                let detail = json!({
                    "kind": "embed", "idempotency_key": request.idempotency_key,
                    "vendor_micro_usd": cost, "worst_case_micro_usd": worst,
                });
                self.store.ledger().run_event(
                    &self.run_id,
                    "cost_estimate_broken",
                    (self.env.now)(),
                    &detail,
                )?;
                self.stopped_by = Some("cost_estimate_broken");
            }
        }
        let (retryable, retry_after) = match &exchanged.answer {
            Err(_) => (true, None),
            Ok(answer) => (
                RETRYABLE_STATUSES.contains(&answer.status),
                answer.retry_after_seconds,
            ),
        };
        Ok(Attempted::Read {
            found,
            retryable,
            retry_after,
        })
    }

    /// An `empty` or `unparseable` answer ends this run's attempts and is asked again by a later
    /// run until the story's age limit: one vendor glitch must not unseat a story for good.
    fn send(&mut self, request: &EmbedRequest, worst: i64) -> Result<Sent, ShadowError> {
        if let Some(found) = self.recovered(request)? {
            self.keep(request, &found)?;
            return Ok(Sent::Embedded);
        }
        for attempt in 1..=MAX_ATTEMPTS {
            let (found, retryable, retry_after) = match self.attempt(request, worst)? {
                Attempted::Refused | Attempted::Deadline => return Ok(Sent::Stopped),
                Attempted::Read {
                    found,
                    retryable,
                    retry_after,
                } => (found, retryable, retry_after),
            };
            match found.reading.outcome {
                Outcome::Ok => {
                    self.keep(request, &found)?;
                    return Ok(Sent::Embedded);
                }
                Outcome::Empty | Outcome::Unparseable => return Ok(Sent::Unusable),
                Outcome::Failed | Outcome::Abandoned => {}
            }
            self.every_failure_transient &= retryable;
            if !retryable || attempt == MAX_ATTEMPTS || self.stopped_by.is_some() {
                break;
            }
            let wait = retry_after.map_or(RETRY_WAIT * attempt, Duration::from_secs);
            (self.env.sleep)(wait.min(RETRY_AFTER_CEILING));
        }
        Ok(Sent::Failed)
    }

    /// Each canary once, with no retry. A canary that fails is reported and stops nothing.
    fn send_canaries(&mut self) -> Result<(), ShadowError> {
        let day = self.run_id[..DAY_BYTES].to_owned();
        let mut english: Option<Vec<f32>> = None;
        for canary in &CANARIES {
            let name = canary.name;
            if self.stopped_by.is_some() {
                break;
            }
            if canary.daily && self.store.canary_on(name, &self.space_name, &day)? {
                continue;
            }
            let repeat_index = self.store.canary_count(name, &self.space_name)?;
            let request = build(&self.space, &canary_text(name), repeat_index)
                .map_err(|why| ShadowError::InputUnparseable(why.to_string()))?;
            let worst = worst_case(request.bytes.len(), self.price);
            if self.args.dry_run {
                self.canaries.push(json!({
                    "name": name, "repeat_index": repeat_index, "outcome": "planned",
                    "worst_case_micro_usd": worst,
                }));
                continue;
            }
            self.store.insert_request(&request, &self.space_name)?;
            let (outcome, found) = match self.recovered(&request)? {
                Some(found) => (Outcome::Ok.name(), Some(found)),
                None => match self.attempt(&request, worst)? {
                    Attempted::Refused => ("refused_by_cap", None),
                    Attempted::Deadline => ("deadline", None),
                    Attempted::Read { found, .. } => (found.reading.outcome.name(), Some(found)),
                },
            };
            // Only a canary's first answer keeps its vector: every later one is compared with it.
            if let Some(found) = found.as_ref().filter(|_| repeat_index == 0) {
                self.keep(&request, found)?;
            }
            let embedding = found.and_then(|found| found.embedding);
            let mut row = CanaryRow {
                name: name.to_owned(),
                space: self.space_name.clone(),
                repeat_index,
                run_id: self.run_id.clone(),
                self_similarity_ppm: None,
                pair_similarity_ppm: None,
            };
            if let Some(embedding) = &embedding {
                if repeat_index > 0 {
                    let reference = self.store.canary_reference(name, &self.space_name)?;
                    row.self_similarity_ppm =
                        reference.map(|first| ppm(dot(&first, &embedding.unit)));
                }
                if name == PAIR_DE {
                    row.pair_similarity_ppm = english
                        .as_ref()
                        .map(|other| ppm(dot(other, &embedding.unit)));
                }
                if name == PAIR_EN {
                    english = Some(embedding.unit.clone());
                }
                self.store.insert_canary(&row, &request.idempotency_key)?;
            }
            self.canaries.push(json!({
                "name": name, "repeat_index": repeat_index, "outcome": outcome,
                "self_similarity_ppm": row.self_similarity_ppm,
                "pair_similarity_ppm": row.pair_similarity_ppm,
            }));
        }
        Ok(())
    }

    /// Writes the comparison of each embedded story with no pending story left in its window:
    /// while an earlier story may still get a vector, its later copies wait.
    fn compare(
        &mut self,
        seen: &[Seen],
        pending: &BTreeSet<Key>,
        compared: &std::collections::HashSet<String>,
    ) -> Result<(), ShadowError> {
        let mut queries = Vec::new();
        for (index, story) in seen.iter().enumerate() {
            let Some(vector_key) = &story.vector_key else {
                continue;
            };
            if compared.contains(&story.story_id) {
                continue;
            }
            let from: Key = (stamp(story.at - TimeDelta::days(WINDOW_DAYS)), i64::MIN);
            if pending
                .range(from.clone()..story.key.clone())
                .next()
                .is_some()
            {
                self.compared.waiting += 1;
            } else if queries.len() >= COMPARE_BATCH || self.args.dry_run {
                self.compared.left += 1;
            } else if let Some(unit) = self.store.vector(vector_key)? {
                queries.push(Query {
                    index,
                    from,
                    unit,
                    nearest: Vec::new(),
                    guarded: Vec::new(),
                    window_stories: 0,
                    candidates: 0,
                });
            }
        }
        if queries.is_empty() {
            return Ok(());
        }
        if (self.env.now)() > self.deadline {
            self.compared.left += u64::try_from(queries.len()).unwrap_or(u64::MAX);
            self.stopped_by = self.stopped_by.or(Some("deadline"));
            return Ok(());
        }
        let (Some(earliest), Some(latest)) = (
            queries.iter().map(|query| &query.from).min().cloned(),
            queries
                .iter()
                .map(|query| &seen[query.index].key)
                .max()
                .cloned(),
        ) else {
            return Ok(());
        };
        for (index, candidate) in seen.iter().enumerate() {
            if candidate.key < earliest || candidate.key >= latest {
                continue;
            }
            let unit = match &candidate.vector_key {
                Some(vector_key) => self.store.vector(vector_key)?,
                None => None,
            };
            self.compared.vectors_read += u64::from(unit.is_some());
            for query in &mut queries {
                let story = &seen[query.index];
                let earlier = candidate.key >= query.from && candidate.key < story.key;
                if !earlier || candidate.story_id == story.story_id {
                    continue;
                }
                query.window_stories += 1;
                let Some(unit) = &unit else { continue };
                query.candidates += 1;
                let similarity = dot(&query.unit, unit);
                offer(&mut query.nearest, similarity, index);
                if story.signals.structurally_alike(&candidate.signals) {
                    offer(&mut query.guarded, similarity, index);
                }
            }
        }
        for query in &queries {
            let story = &seen[query.index];
            let mut listed = Vec::new();
            for (list, kept) in [
                (LIST_NEAREST, &query.nearest),
                (LIST_GUARDED, &query.guarded),
            ] {
                for (rank, (similarity, index)) in kept.iter().enumerate() {
                    let other = &seen[*index];
                    let shared =
                        |a: &BTreeSet<String>, b: &BTreeSet<String>| a.intersection(b).count();
                    listed.push(Candidate {
                        list,
                        rank: u32::try_from(rank + 1).unwrap_or(u32::MAX),
                        story_id: other.story_id.clone(),
                        similarity_ppm: ppm(*similarity),
                        age_seconds: (story.at - other.at).num_seconds(),
                        hosts_shared: story.signals.shared_hosts(&other.signals),
                        numbers_candidate: other.signals.numbers.len(),
                        numbers_shared: shared(&story.signals.numbers, &other.signals.numbers),
                        tickers_candidate: other.signals.tickers.len(),
                        tickers_shared: shared(&story.signals.tickers, &other.signals.tickers),
                        length_ratio_ppm: story.signals.length_ratio_ppm(&other.signals),
                        same_source: story.source == other.source,
                    });
                }
            }
            let comparison = Comparison {
                story_id: story.story_id.clone(),
                window_stories: query.window_stories,
                candidates: query.candidates,
                unembedded: query.window_stories - query.candidates,
                own_hosts: story.signals.hosts.len(),
                own_numbers: story.signals.numbers.len(),
                own_tickers: story.signals.tickers.len(),
                listed,
            };
            self.store.insert_comparison(
                &comparison,
                &self.compare_version,
                &self.space_name,
                &self.snapshot_id,
                &self.run_id,
            )?;
            self.compared.written += 1;
        }
        Ok(())
    }

    fn finish(
        self,
        manifest: &snapshot_contract::Manifest,
        swept: u64,
    ) -> Result<Finished, ShadowError> {
        let closed = self.store.closed_by(&self.run_id)?;
        let count = |outcome: Outcome| {
            closed
                .iter()
                .filter(|c| c.outcome == outcome.name())
                .count()
        };
        let answering: BTreeSet<&str> = closed
            .iter()
            .filter_map(|c| c.answering_model.as_deref())
            .collect();
        let another_model = closed
            .iter()
            .filter_map(|c| c.answering_model.as_deref())
            .filter(|id| *id != self.space.model())
            .count();
        let written = self.store.vectors_by(&self.run_id)?;
        let providers: BTreeSet<&str> = written
            .iter()
            .filter_map(|(_, provider)| provider.as_deref())
            .collect();
        let stated = written
            .iter()
            .filter_map(|(_, provider)| provider.as_deref());
        let another_provider = stated
            .filter(|provider| *provider != self.space.provider())
            .count();
        let unstated = written.iter().filter(|(_, p)| p.is_none()).count();
        let mut encodings: BTreeMap<&str, u64> = BTreeMap::new();
        for (encoding, _) in &written {
            *encodings.entry(encoding.as_str()).or_default() += 1;
        }
        let costless = closed
            .iter()
            .filter(|c| c.outcome == Outcome::Ok.name() && c.cost_micro_usd.is_none())
            .count();
        let closed_unusable = count(Outcome::Empty) + count(Outcome::Unparseable);
        let error_code = match self.stopped_by {
            Some("store_failed") => Some("store_failed"),
            Some("cost_estimate_broken") => Some("cost_estimate_broken"),
            Some("cost_cap_reached") => Some("cost_cap_reached"),
            _ if self.requests.failed_stories > 0 => Some("partial_embed"),
            _ if closed_unusable > 0 => Some("unusable_answer"),
            _ => None,
        };
        let unreachable = error_code == Some("partial_embed")
            && count(Outcome::Ok) == 0
            && closed_unusable == 0
            && self.every_failure_transient;
        let exit = if error_code.is_some() {
            EXIT_INCOMPLETE
        } else {
            EXIT_OK
        };
        let (stories, requests, compared) = (&self.stories, &self.requests, &self.compared);
        let changed = requests.sent > 0 || stories.twin_of_embedded > 0 || compared.written > 0;
        let envelope = json!({
            "schema": SCHEMA,
            "dry_run": self.args.dry_run,
            "changed": !self.args.dry_run && changed,
            "run_id": self.run_id,
            "embed_format": EMBED_FORMAT,
            "space": self.space_name,
            "compare_version": self.compare_version,
            "deadline": stamp(self.deadline),
            "snapshot": {
                "snapshot_id": manifest.snapshot_id, "cutoff": manifest.cutoff,
                "seq_high_water": manifest.seq_high_water,
            },
            "stories": {
                "rows": stories.rows, "repeat_of_story": stories.repeat_of_story,
                "no_body": stories.no_body, "already_embedded": stories.already_embedded,
                "gave_up": stories.gave_up, "selected": stories.selected,
                "embedded": stories.embedded, "twin_of_embedded": stories.twin_of_embedded,
                "unusable": stories.unusable, "truncated": stories.truncated,
                "left": stories.left,
            },
            "requests": {
                "planned": requests.planned, "planned_bytes": requests.planned_bytes,
                "sent": requests.sent, "recovered": requests.recovered,
                "ok": count(Outcome::Ok), "empty": count(Outcome::Empty),
                "unparseable": count(Outcome::Unparseable), "failed": count(Outcome::Failed),
                "costless": costless, "failed_stories": requests.failed_stories,
                "refused_by_cap": requests.refused_by_cap,
            },
            "vectors": {
                "written": written.len(), "encodings": encodings,
                "answering_models": answering, "answered_by_another_model": another_model,
                "answering_providers": providers,
                "answered_by_another_provider": another_provider,
                "provider_not_stated": unstated,
            },
            "canaries": self.canaries,
            "comparisons": {
                "written": compared.written, "waiting": compared.waiting, "left": compared.left,
                "vectors_read": compared.vectors_read,
            },
            "cost_micro_usd": {
                "run": self.spent.run, "day": self.spent.day, "total": self.spent.total,
                "run_cap": self.caps.run, "day_cap": self.caps.day, "total_cap": self.caps.total,
                "planned_worst_case": requests.planned_worst_case,
            },
            "store": {
                "attempts_abandoned_by_sweep": swept,
                "runs_left_open": self.store.ledger().runs_left_open(&self.run_id)?,
                "failure": self.store_failure,
            },
            "stopped_by": self.stopped_by,
            "exit": { "code": if unreachable { EXIT_TRANSIENT } else { exit },
                      "error_code": if unreachable { Some("vendor_unreachable") } else { error_code } },
        });
        if !self.args.dry_run {
            let detail = json!({ "exit": envelope["exit"], "stopped_by": self.stopped_by });
            self.store
                .ledger()
                .run_event(&self.run_id, "ended", (self.env.now)(), &detail)?;
        }
        if unreachable {
            return Err(ShadowError::VendorUnreachable(Box::new(envelope)));
        }
        Ok(Finished { envelope, exit })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::{Cell, RefCell};
    use std::path::{Path, PathBuf};

    use canon::FileDigest;
    use chrono::{DateTime, TimeDelta, Utc};
    use rusqlite::Connection;
    use serde_json::Value;
    use snapshot_contract::SnapshotRow;
    use tempfile::TempDir;

    use super::{canary_text, compare_version, offer, run, Args, LONG, PAIR_DE, PAIR_EN, SHORT};
    use crate::cmd_shadow::tests::{at, cut, story, TestResult};
    use crate::cmd_shadow::{Env, Finished, ShadowError};
    use crate::embed::tests::{answer, axis, space, MODEL};
    use crate::embed::{build, read_embedding};
    use crate::embed_store::{EmbedStore, EMBED_BLOB_DIR, EMBED_DB_FILE};
    use crate::ledger::{AttemptSlot, Opening, SendPermit};
    use crate::request::STATE_BUDGET_BYTES;
    use crate::signals::tests::{FIRM_ONE, FIRM_TWO, FUNDING};
    use crate::store::{Store, DB_FILE};
    use crate::wire::tests::{http, sentinel, Loopback, SENTINEL_KEY};
    use crate::wire::{Exchanged, HttpAnswer, ModelExchange, ModelWire};

    const NOW: &str = "2026-11-21T00:10:00Z";
    const CUTOFF: &str = "2026-11-21T00:00:00Z";
    const UNRELATED: &str = "Quillfeather Mining (NYSE: QFM) reports 41 new drill holes at a site that does not exist. https://quillfeather.example";
    /// 2 micro-USD, under the worst case of the smallest request here at 0.01 USD per million.
    const COST_USD: f64 = 0.000_002;

    fn hour(of_day: u32) -> String {
        format!("2026-11-20T{of_day:02}:00:00.000000Z")
    }

    /// Answers each request by the first marker its `input` holds: a constructed vector, or a
    /// status with a body. Any other text gets a vector orthogonal to every marked one.
    pub(crate) struct Stub {
        vectors: Vec<(&'static str, Vec<f32>)>,
        refusing: RefCell<Vec<(&'static str, u16, &'static str)>>,
        inputs: RefCell<Vec<String>>,
        as_base64: bool,
        cost: Option<f64>,
    }

    impl Stub {
        pub(crate) fn of(vectors: Vec<(&'static str, Vec<f32>)>) -> Self {
            Self {
                vectors,
                refusing: RefCell::default(),
                inputs: RefCell::default(),
                as_base64: true,
                cost: Some(COST_USD),
            }
        }

        fn sends(&self) -> usize {
            self.inputs.borrow().len()
        }

        /// Sends whose input holds `marker`.
        fn sent(&self, marker: &str) -> usize {
            let inputs = self.inputs.borrow();
            inputs.iter().filter(|input| input.contains(marker)).count()
        }
    }

    impl ModelExchange for Stub {
        fn send(&self, permit: SendPermit, body: &[u8]) -> Exchanged {
            let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            let input = parsed["input"].as_str().unwrap_or_default().to_owned();
            let refused = self
                .refusing
                .borrow()
                .iter()
                .find(|(marker, _, _)| input.contains(marker))
                .map(|(_, status, body)| (*status, (*body).to_owned()));
            let (status, body) = refused.unwrap_or_else(|| {
                let vector = self
                    .vectors
                    .iter()
                    .find(|(marker, _)| input.contains(marker))
                    .map_or_else(|| axis(4000, 1.0, 4001, 0.0), |(_, vector)| vector.clone());
                (200, answer(&vector, self.as_base64, self.cost))
            });
            self.inputs.borrow_mut().push(input);
            let now: DateTime<Utc> = NOW.parse().unwrap_or_default();
            let answer = HttpAnswer {
                status,
                body: body.into_bytes(),
                retry_after_seconds: None,
            };
            Exchanged::new(permit, now, now, Ok(answer))
        }
    }

    pub(crate) struct Bench {
        root: TempDir,
        now: Cell<DateTime<Utc>>,
    }

    impl Bench {
        pub(crate) fn new() -> TestResult<Self> {
            Ok(Self {
                root: TempDir::new()?,
                now: Cell::new(NOW.parse()?),
            })
        }

        pub(crate) fn store(&self) -> PathBuf {
            self.root.path().join("gate")
        }

        /// Caps of 1 USD each, a limit above every cut here, a deadline an hour after `NOW`.
        fn args(&self, snapshot: &Path) -> Args {
            Args {
                snapshot: snapshot.to_path_buf(),
                model: MODEL.to_owned(),
                provider: "example-host".to_owned(),
                quantization: "bf16".to_owned(),
                epoch: 1,
                run_cap_usd: "1".to_owned(),
                day_cap_usd: "1".to_owned(),
                total_cap_usd: "1".to_owned(),
                input_price_per_mtok: "0.01".to_owned(),
                give_up_after_hours: 24,
                limit: 100,
                deadline: "2026-11-21T01:10:00Z".to_owned(),
                store: self.store(),
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
            let db = Connection::open(self.store().join(EMBED_DB_FILE))?;
            Ok(db.query_row(sql, [], |row| row.get(0))?)
        }

        /// `(list, rank, candidate, similarity_ppm)` of one story's stored candidates.
        fn candidates(&self, story_id: &str) -> TestResult<Vec<(String, u32, String, i64)>> {
            let db = Connection::open(self.store().join(EMBED_DB_FILE))?;
            let mut statement = db.prepare(
                "SELECT k.list, k.rank, k.candidate_story_id, k.similarity_ppm
                   FROM comparison_candidates k JOIN comparisons c ON c.id = k.comparison_id
                  WHERE c.story_id = ?1 ORDER BY k.list DESC, k.rank",
            )?;
            let found = statement
                .query_map([story_id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<Result<_, _>>()?;
            Ok(found)
        }
    }

    /// A release, its German copy, two firms on one lawsuit and an unrelated notice, an hour
    /// apart. The copy is at cosine 0.6 of the release and the second firm at 0.8 of the first.
    fn five_stories() -> Vec<SnapshotRow> {
        vec![
            story(5, "release-en", &hour(10), Some(FUNDING[0].1)),
            story(6, "release-de", &hour(11), Some(FUNDING[1].1)),
            story(7, "firm-one", &hour(12), Some(FIRM_ONE)),
            story(8, "firm-two", &hour(13), Some(FIRM_TWO)),
            story(9, "unrelated", &hour(14), Some(UNRELATED)),
        ]
    }

    pub(crate) fn five_vectors() -> Vec<(&'static str, Vec<f32>)> {
        vec![
            ("said on November 23", axis(0, 1.0, 1, 0.0)),
            ("teilte am", axis(0, 0.6, 1, 0.8)),
            ("Dunmere", axis(2, 1.0, 3, 0.0)),
            ("Okonjo", axis(2, 0.8, 3, 0.6)),
            ("Quillfeather", axis(5, 1.0, 6, 0.0)),
        ]
    }

    /// A store holding the five stories' vectors and comparisons, and the cut they came from.
    pub(crate) fn compared() -> TestResult<(Bench, TempDir)> {
        let (bench, stub) = (Bench::new()?, Stub::of(five_vectors()));
        let snapshot = cut(&five_stories(), CUTOFF)?;
        let done = bench.go(&bench.args(snapshot.path()), &stub)?;
        assert_eq!(done.exit, 0, "{:#}", done.envelope);
        Ok((bench, snapshot))
    }

    fn nearest(ppm: i64, candidate: &str) -> (String, u32, String, i64) {
        ("nearest".to_owned(), 1, candidate.to_owned(), ppm)
    }

    /// Section 8 rows MG19 and MG22. Mutations 2026-11-23, each alone: `structurally_alike`
    /// answering `true`; `offer` keeping the lowest; the comparison written without its
    /// candidates: red each.
    #[test]
    fn a_run_embeds_each_story_inside_an_attempt_row_and_records_its_nearest_earlier_stories(
    ) -> TestResult {
        let (bench, stub) = (Bench::new()?, Stub::of(five_vectors()));
        let snapshot = cut(&five_stories(), CUTOFF)?;
        let done = bench.go(&bench.args(snapshot.path()), &stub)?;
        let envelope = &done.envelope;
        assert_eq!(done.exit, 0, "{envelope:#}");
        assert_eq!(at(envelope, "/stories/embedded"), 5);
        assert_eq!(at(envelope, "/requests/sent"), 9);
        assert_eq!(at(envelope, "/requests/ok"), 9);
        assert_eq!(at(envelope, "/vectors/written"), 9);
        assert_eq!(at(envelope, "/vectors/encodings/base64"), 9);
        assert_eq!(at(envelope, "/vectors/provider_not_stated"), 9);
        assert_eq!(at(envelope, "/vectors/answered_by_another_model"), 0);
        assert_eq!(at(envelope, "/comparisons/written"), 5);
        assert_eq!(at(envelope, "/cost_micro_usd/run"), 18);
        assert_eq!(at(envelope, "/compare_version"), compare_version());
        assert_eq!(stub.sends(), 9);
        assert_eq!(
            bench.count("SELECT COUNT(*) FROM attempt_events WHERE event = 'opened'")?,
            9
        );
        assert_eq!(bench.candidates("release-en")?, []);
        assert_eq!(
            bench.candidates("release-de")?,
            [
                nearest(600_000, "release-en"),
                ("guarded".to_owned(), 1, "release-en".to_owned(), 600_000)
            ]
        );
        // The second firm's nearest text is the first firm's, and no guard calls them alike.
        assert_eq!(
            bench.candidates("firm-two")?,
            [
                nearest(800_000, "firm-one"),
                ("nearest".to_owned(), 2, "release-en".to_owned(), 0),
                ("nearest".to_owned(), 3, "release-de".to_owned(), 0),
            ]
        );
        assert_eq!(
            bench.count(
                "SELECT k.hosts_shared = '[\"harrowgate-robotics.example\"]'
                    AND k.age_seconds = 3600 AND k.tickers_shared = 1 AND k.numbers_shared = 6
                    AND k.numbers_candidate = 6 AND c.own_numbers = 7 AND c.window_stories = 1
                    AND c.candidates = 1 AND c.unembedded = 0 AND k.same_source = 1
                   FROM comparison_candidates k JOIN comparisons c ON c.id = k.comparison_id
                  WHERE c.story_id = 'release-de' AND k.list = 'nearest'"
            )?,
            1
        );
        let again = bench.go(&bench.args(snapshot.path()), &stub)?;
        assert_eq!(at(&again.envelope, "/stories/already_embedded"), 5);
        assert_eq!(at(&again.envelope, "/comparisons/written"), 0);
        assert_eq!(stub.sends(), 10);
        assert!(!bench.store().join(DB_FILE).exists());
        Ok(())
    }

    /// Section 8 row MG21. Mutations, each alone: the upper bound `candidate.key < story.key`
    /// dropped (2026-11-24); the window's lower bound dropped; `embedded_stories` not filtered
    /// by space (both 2026-11-23): red each.
    #[test]
    fn a_candidate_is_observed_strictly_earlier_inside_the_window_and_in_the_same_space(
    ) -> TestResult {
        let (bench, stub) = (Bench::new()?, Stub::of(Vec::new()));
        let rows = vec![
            story(
                1,
                "eight-days-before",
                "2026-11-12T11:59:59.000000Z",
                Some("old text"),
            ),
            story(2, "an-hour-before", &hour(11), Some("earlier text")),
            story(3, "same-instant-lower-seq", &hour(12), Some("tie text one")),
            story(4, "the-story", &hour(12), Some("the text")),
            story(
                5,
                "same-instant-higher-seq",
                &hour(12),
                Some("tie text two"),
            ),
            story(6, "an-hour-after", &hour(13), Some("later text")),
        ];
        let snapshot = cut(&rows, CUTOFF)?;
        let mut args = bench.args(snapshot.path());
        assert_eq!(bench.go(&args, &stub)?.exit, 0);
        let named = |found: Vec<(String, u32, String, i64)>| -> Vec<String> {
            found.into_iter().map(|(_, _, story, _)| story).collect()
        };
        assert_eq!(
            named(bench.candidates("the-story")?),
            ["an-hour-before", "same-instant-lower-seq"]
        );
        assert_eq!(
            bench.count("SELECT window_stories FROM comparisons WHERE story_id = 'the-story'")?,
            2
        );
        // Another epoch is another space: the earlier story fails there and is given up, and
        // its vector of epoch 1 is no candidate.
        args.epoch = 2;
        args.give_up_after_hours = 0;
        stub.refusing.borrow_mut().push(("earlier text", 400, "{}"));
        assert_eq!(bench.go(&args, &stub)?.exit, 8);
        let second = bench.go(&args, &stub)?;
        assert_eq!(at(&second.envelope, "/stories/gave_up"), 1);
        assert_eq!(
            bench.count(
                "SELECT COUNT(*) FROM comparison_candidates k
                   JOIN comparisons c ON c.id = k.comparison_id
                  WHERE c.space LIKE '%epoch=2' AND k.candidate_story_id = 'an-hour-before'"
            )?,
            0
        );
        assert_eq!(
            bench.count(
                "SELECT unembedded FROM comparisons
                  WHERE story_id = 'the-story' AND space LIKE '%epoch=2'"
            )?,
            1
        );
        Ok(())
    }

    /// Section 8 row MG22, the order. Mutation 2026-11-23, the `pending` test removed from
    /// `compare`: red (the copy's comparison is written with no candidate and then stays).
    #[test]
    fn a_story_waits_for_its_comparison_while_an_earlier_story_has_no_vector_yet() -> TestResult {
        let (bench, stub) = (Bench::new()?, Stub::of(five_vectors()));
        let snapshot = cut(&five_stories()[..2], CUTOFF)?;
        stub.refusing
            .borrow_mut()
            .push(("said on November 23", 503, "{}"));
        let first = bench.go(&bench.args(snapshot.path()), &stub)?;
        assert_eq!(first.exit, 8);
        assert_eq!(at(&first.envelope, "/exit/error_code"), "partial_embed");
        assert_eq!(at(&first.envelope, "/stories/embedded"), 1);
        assert_eq!(at(&first.envelope, "/comparisons/waiting"), 1);
        assert_eq!(at(&first.envelope, "/comparisons/written"), 0);
        assert_eq!(stub.sent("said on November 23"), 2);
        stub.refusing.borrow_mut().clear();
        let second = bench.go(&bench.args(snapshot.path()), &stub)?;
        assert_eq!(at(&second.envelope, "/comparisons/written"), 2);
        assert_eq!(
            bench.candidates("release-de")?[0],
            nearest(600_000, "release-en")
        );
        Ok(())
    }

    /// The oldest story first, and a limit leaves the rest for the next run, whatever their age:
    /// an age counts failed attempts. Mutations, each alone: `pop_last` made `pop_first`
    /// (2026-11-23); the `attempted` test dropped from `gave_up` (2026-11-24): red each.
    #[test]
    fn a_limit_embeds_the_oldest_stories_and_two_stories_with_one_text_share_a_vector() -> TestResult
    {
        let (bench, stub) = (Bench::new()?, Stub::of(five_vectors()));
        let mut rows = five_stories();
        rows.push(story(10, "twin-of-en", &hour(15), Some(FUNDING[0].1)));
        let snapshot = cut(&rows, CUTOFF)?;
        let mut args = bench.args(snapshot.path());
        args.limit = 2;
        let first = bench.go(&args, &stub)?;
        assert_eq!(first.exit, 0);
        assert_eq!(at(&first.envelope, "/stories/embedded"), 2);
        assert_eq!(at(&first.envelope, "/stories/left"), 4);
        assert_eq!((stub.sent("November 23"), stub.sent("teilte am")), (1, 1));
        assert_eq!(stub.sent("Dunmere"), 0);
        args.limit = 100;
        args.give_up_after_hours = 1;
        let second = bench.go(&args, &stub)?;
        assert_eq!(at(&second.envelope, "/stories/embedded"), 3);
        assert_eq!(at(&second.envelope, "/stories/twin_of_embedded"), 1);
        assert_eq!(stub.sent("November 23"), 1);
        assert_eq!(
            bench.candidates("twin-of-en")?[0],
            nearest(1_000_000, "release-en")
        );
        Ok(())
    }

    /// Decision D25. Mutation 2026-11-23, `Empty | Unparseable` treated as embedded: red.
    #[test]
    fn an_unusable_answer_is_asked_again_by_a_later_run_until_the_storys_age_limit() -> TestResult {
        let (bench, stub) = (Bench::new()?, Stub::of(five_vectors()));
        let snapshot = cut(&five_stories()[..1], CUTOFF)?;
        stub.refusing
            .borrow_mut()
            .push(("said on November 23", 200, r#"{"data":[]}"#));
        let mut args = bench.args(snapshot.path());
        let first = bench.go(&args, &stub)?;
        assert_eq!(first.exit, 8);
        assert_eq!(at(&first.envelope, "/exit/error_code"), "unusable_answer");
        assert_eq!(at(&first.envelope, "/stories/unusable"), 1);
        assert_eq!(stub.sent("November 23"), 1);
        bench.go(&args, &stub)?;
        assert_eq!(stub.sent("November 23"), 2);
        assert_eq!(bench.count("SELECT COUNT(*) FROM vectors")?, 4);
        args.give_up_after_hours = 14;
        let last = bench.go(&args, &stub)?;
        assert_eq!(at(&last.envelope, "/stories/gave_up"), 1);
        assert_eq!(stub.sent("November 23"), 2);
        Ok(())
    }

    /// Section 8 row MG19, the caps and the deadline. Mutations 2026-11-23, each alone: the
    /// cap test removed from `attempt`; the deadline test removed: red each.
    #[test]
    fn a_call_over_a_cap_or_too_close_to_the_deadline_is_not_sent_and_opens_no_attempt(
    ) -> TestResult {
        let snapshot = cut(&five_stories(), CUTOFF)?;
        let (bench, stub) = (Bench::new()?, Stub::of(five_vectors()));
        let mut args = bench.args(snapshot.path());
        args.run_cap_usd = "0.000001".to_owned();
        let capped = bench.go(&args, &stub)?;
        assert_eq!(capped.exit, 8);
        assert_eq!(at(&capped.envelope, "/exit/error_code"), "cost_cap_reached");
        assert_eq!(at(&capped.envelope, "/stories/left"), 5);
        let (late, idle) = (Bench::new()?, Stub::of(five_vectors()));
        let mut args = late.args(snapshot.path());
        args.deadline = "@1795219819".to_owned();
        let stopped = late.go(&args, &idle)?;
        assert_eq!(
            at(&stopped.envelope, "/deadline"),
            "2026-11-21T00:10:19.000000Z"
        );
        assert_eq!(stopped.exit, 0);
        assert_eq!(at(&stopped.envelope, "/stopped_by"), "deadline");
        assert_eq!(at(&stopped.envelope, "/stories/left"), 5);
        assert_eq!(stub.sends() + idle.sends(), 0);
        for bench in [&bench, &late] {
            assert_eq!(bench.count("SELECT COUNT(*) FROM attempt_events")?, 0);
        }
        args.deadline = "@1795219821".to_owned();
        assert_eq!(at(&late.go(&args, &idle)?.envelope, "/stories/embedded"), 5);
        Ok(())
    }

    /// Section 8 row MG23, a run's half. Mutation 2026-11-23, the latch test removed from
    /// `run`: red on the second run's sends.
    #[test]
    fn an_embed_cost_above_its_worst_case_stops_embed_runs_and_never_a_decide_run() -> TestResult {
        let (bench, mut stub) = (Bench::new()?, Stub::of(five_vectors()));
        stub.cost = Some(0.5);
        let snapshot = cut(&five_stories(), CUTOFF)?;
        let broken = bench.go(&bench.args(snapshot.path()), &stub)?;
        assert_eq!(
            at(&broken.envelope, "/exit/error_code"),
            "cost_estimate_broken"
        );
        assert_eq!(stub.sends(), 1);
        let next = bench.go(&bench.args(snapshot.path()), &stub)?;
        assert_eq!(at(&next.envelope, "/stopped_by"), "cost_estimate_broken");
        assert_eq!(stub.sends(), 1);
        let run_id = at(&broken.envelope, "/run_id");
        let mut args = bench.args(snapshot.path());
        args.ack_cost_model = run_id.as_str().map(str::to_owned);
        stub.cost = Some(COST_USD);
        assert_eq!(
            at(&bench.go(&args, &stub)?.envelope, "/stories/embedded"),
            5
        );
        assert!(!bench.store().join(DB_FILE).exists());
        let gate = Store::open(&bench.store())?;
        assert_eq!(gate.unacked_cost_break()?, None);
        assert_eq!(gate.spent("any", bench.now.get())?.total, 0);
        Ok(())
    }

    /// Mutation 2026-11-23, the writer's door used for a dry run: red.
    #[test]
    fn a_dry_run_plans_the_requests_and_the_canaries_and_writes_and_sends_nothing() -> TestResult {
        let (bench, stub) = (Bench::new()?, Stub::of(five_vectors()));
        let snapshot = cut(&five_stories(), CUTOFF)?;
        let mut args = bench.args(snapshot.path());
        args.dry_run = true;
        let done = bench.go(&args, &stub)?;
        assert_eq!(done.exit, 0);
        assert_eq!(at(&done.envelope, "/requests/planned"), 5);
        assert_eq!(at(&done.envelope, "/changed"), false);
        assert_eq!(at(&done.envelope, "/canaries/3/outcome"), "planned");
        assert_eq!(at(&done.envelope, "/canaries/1/worst_case_micro_usd"), 132);
        assert_eq!(stub.sends(), 0);
        assert!(!bench.store().exists());
        Ok(())
    }

    /// The canaries' names are their texts' identities. A changed text needs a new name, and
    /// this literal with it.
    #[test]
    fn each_canary_text_is_the_one_its_name_was_given_for() {
        let digests = [SHORT, LONG, PAIR_EN, PAIR_DE]
            .map(|name| FileDigest::of(canary_text(name).as_bytes()).hex()[..12].to_owned());
        assert_eq!(
            digests,
            [
                "f590a2b44332",
                "cbca645741fc",
                "574ebf700cab",
                "066cded16b2b"
            ]
        );
        assert!(canary_text(LONG).len() > STATE_BUDGET_BYTES);
    }

    /// Section 8 row MG24. Mutations 2026-11-23, each alone: the daily test removed; every
    /// canary's vector kept; the pair compared with its own first answer: red each.
    #[test]
    fn the_short_canary_is_sent_by_every_run_the_others_once_a_day_each_against_its_first_answer(
    ) -> TestResult {
        let stub = Stub::of(vec![
            ("Brightwater Ferries, a company", axis(8, 1.0, 9, 0.0)),
            ("Kanarientext", axis(8, 0.6, 9, 0.8)),
        ]);
        let bench = Bench::new()?;
        let snapshot = cut(&[story(1, "bodiless", &hour(10), None)], CUTOFF)?;
        let first = bench.go(&bench.args(snapshot.path()), &stub)?;
        let read = |envelope: &Value, index: usize, key: &str| {
            at(envelope, &format!("/canaries/{index}/{key}"))
        };
        assert_eq!(read(&first.envelope, 3, "name"), PAIR_DE);
        assert_eq!(read(&first.envelope, 3, "pair_similarity_ppm"), 600_000);
        assert_eq!(read(&first.envelope, 3, "self_similarity_ppm"), Value::Null);
        assert_eq!(stub.sends(), 4);
        let second = bench.go(&bench.args(snapshot.path()), &stub)?;
        assert_eq!(at(&second.envelope, "/canaries/1"), Value::Null);
        assert_eq!(read(&second.envelope, 0, "name"), SHORT);
        assert_eq!(read(&second.envelope, 0, "repeat_index"), 1);
        assert_eq!(read(&second.envelope, 0, "self_similarity_ppm"), 1_000_000);
        assert_eq!(stub.sends(), 5);
        bench.now.set(bench.now.get() + TimeDelta::days(1));
        let mut args = bench.args(snapshot.path());
        args.deadline = "2026-11-23T00:00:00Z".to_owned();
        let third = bench.go(&args, &stub)?;
        assert_eq!(read(&third.envelope, 3, "repeat_index"), 1);
        assert_eq!(read(&third.envelope, 3, "self_similarity_ppm"), 1_000_000);
        assert_eq!(stub.sends(), 9);
        assert_eq!(bench.count("SELECT COUNT(*) FROM vectors")?, 4);
        assert_eq!(bench.count("SELECT COUNT(*) FROM canaries")?, 9);
        assert_eq!(bench.count("SELECT COUNT(*) FROM embed_requests")?, 9);
        Ok(())
    }

    /// An attempt closed `ok` with no vector row is a run killed between the two writes.
    /// Mutation 2026-11-23, `recovered` returning `None`: red (the request is paid again).
    #[test]
    fn an_ok_answer_whose_vector_row_is_missing_is_read_from_its_blob_and_not_paid_again(
    ) -> TestResult {
        let (bench, stub) = (Bench::new()?, Stub::of(five_vectors()));
        let snapshot = cut(&five_stories()[..1], CUTOFF)?;
        let request = build(&space()?, FUNDING[0].1, 0)?;
        {
            let store = EmbedStore::open(&bench.store())?;
            store.insert_request(&request, &space()?.name())?;
            let now = bench.now.get();
            let opening = Opening {
                run_id: "killed",
                ordinal: 0,
                idempotency_key: &request.idempotency_key,
                worst_case_micro_usd: 3,
                at: now,
            };
            let mut slot = AttemptSlot::open(store.ledger(), &opening)?;
            let body = answer(&axis(0, 1.0, 1, 0.0), false, Some(COST_USD)).into_bytes();
            let reading = read_embedding(200, &body).reading;
            let answer = HttpAnswer {
                status: 200,
                body,
                retry_after_seconds: None,
            };
            let exchanged = Exchanged::new(slot.take_permit()?, now, now, Ok(answer));
            slot.close(store.ledger(), &exchanged, &reading, now)?;
        }
        let done = bench.go(&bench.args(snapshot.path()), &stub)?;
        assert_eq!(at(&done.envelope, "/stories/embedded"), 1);
        assert_eq!(at(&done.envelope, "/requests/recovered"), 1);
        assert_eq!(stub.sent("November 23"), 0);
        assert_eq!(
            bench.count("SELECT COUNT(*) FROM vectors WHERE encoding = 'array'")?,
            1
        );
        Ok(())
    }

    #[test]
    fn each_refusal_before_the_store_names_its_argument_and_writes_nothing() -> TestResult {
        let (bench, stub) = (Bench::new()?, Stub::of(Vec::new()));
        let snapshot = cut(&five_stories(), CUTOFF)?;
        let plain = bench.args(snapshot.path());
        type Change = fn(&mut Args);
        let cases: [(&str, Change); 6] = [
            ("alias", |args| args.model = "~example/embed".to_owned()),
            ("quantization", |args| args.quantization = "bf17".to_owned()),
            ("0", |args| args.input_price_per_mtok = "0".to_owned()),
            ("--limit 0", |args| args.limit = 0),
            ("--deadline", |args| args.deadline = "soon".to_owned()),
            ("--ack-cost-model", |args| {
                args.ack_cost_model = Some("no-such-run".to_owned());
            }),
        ];
        for (named, change) in cases {
            let mut args = plain.clone();
            change(&mut args);
            let refused = bench.go(&args, &stub).err();
            let text = refused
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            assert_eq!(refused.map(|why| why.code()), Some("invalid_argument"));
            assert!(text.contains(named), "{named}: {text}");
        }
        assert_eq!(stub.sends(), 0);
        assert!(!bench.store().join(EMBED_BLOB_DIR).join("x").exists());
        assert_eq!(bench.count("SELECT COUNT(*) FROM run_events").ok(), Some(0));
        Ok(())
    }

    /// Section 8 row MG5, the embed half: the real wire against a `127.0.0.1` listener.
    /// Mutation 2026-11-23, the key appended to the stored response body: red.
    #[test]
    fn the_key_appears_in_no_embed_row_blob_or_envelope() -> TestResult {
        let bench = Bench::new()?;
        let snapshot = cut(&five_stories()[..1], CUTOFF)?;
        let sound = http(
            "200 OK",
            "",
            &answer(&axis(0, 1.0, 1, 0.0), true, Some(COST_USD)),
        );
        let listener = Loopback::serving(vec![sound; 5])?;
        let wire = ModelWire::loopback(sentinel(), listener.url.clone());
        let done = bench.go(&bench.args(snapshot.path()), &wire)?;
        assert_eq!(at(&done.envelope, "/requests/ok"), 5, "{:#}", done.envelope);
        let heads = listener.requests();
        assert_eq!(heads.len(), 5);
        for head in &heads {
            assert!(head.contains(&format!("Bearer {SENTINEL_KEY}")), "{head}");
        }
        let mut pending = vec![bench.root.path().to_path_buf()];
        let mut files = 0;
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(dir)? {
                let path = entry?.path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                files += 1;
                let bytes = std::fs::read(&path)?;
                let leaked = bytes
                    .windows(SENTINEL_KEY.len())
                    .any(|window| window == SENTINEL_KEY.as_bytes());
                assert!(!leaked, "{}", path.display());
            }
        }
        assert!(files > 5, "{files}");
        assert!(!done.envelope.to_string().contains("sentinel"));
        Ok(())
    }

    /// Section 8 row MG22: the labelling reads no vector and no comparison. Mutation 2026-11-24,
    /// `use crate::embed_store::EmbedStore;` added to `cmd_shadow.rs`: red.
    #[test]
    fn the_shadow_run_names_nothing_of_the_embed_kind() {
        let labelling = include_str!("cmd_shadow.rs");
        for named in ["embed", "dedup", "signals", "vector"] {
            assert!(!labelling.contains(named), "{named}");
        }
    }

    #[test]
    fn the_five_highest_similarities_are_kept_and_an_equal_one_stays_behind_the_earlier() {
        let mut kept = Vec::new();
        for (index, similarity) in [0.1, 0.9, 0.5, 0.9, 0.3, 0.2, 0.95, 0.05]
            .iter()
            .enumerate()
        {
            offer(&mut kept, *similarity, index);
        }
        assert_eq!(kept, [(0.95, 6), (0.9, 1), (0.9, 3), (0.5, 2), (0.3, 4)]);
    }
}
