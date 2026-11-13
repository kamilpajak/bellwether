//! `newsvault collect` - the one command that writes to the archive. What it reports and which
//! exit each state takes are design.md section 4; the exit table itself is
//! `news_provider::Tally::exit` and is not restated here.
//!
//! The run envelope is on disk with `finished_at` null BEFORE the lease is attempted, so a run
//! that dies holding nothing still left a record that it started (accepted attack A3.13).

use std::path::{Path, PathBuf};

use canon::digest::{doc_id, item_id, story_id, ItemFields};
use chrono::{DateTime, NaiveDate, Utc};
use edgar_press::EdgarPress;
use news_provider::provider::{Calendar, CalendarError, Listing, PlanRefusal};
use news_provider::{
    stamp_of, Archive, ArchiveError, AttemptReceipt, Clock, CollectError, Collected, CollectedItem,
    Committed, Exchange, Exit, Provider, Run, RunId, SourceId, TargetWindow,
};
use pit::ObservationTime;
use serde_json::{json, Value};

use crate::cmd_ledger::{Envelope, RUN_ENVELOPE_SCHEMA};
use crate::store::{Committer, ItemRow, StoreError, RUNS_DIR};

pub const SCHEMA: &str = "newsvault.collect/1";
/// What a run with no schedule slot is keyed by, so an ad-hoc run still has a run id and a
/// partition key (accepted attack A1.7).
const ADHOC: &str = "adhoc";
const RUN_ID_FORMAT: &str = "%Y%m%dT%H%M%SZ";
const STAGING_PREFIX: &str = ".tmp-";

/// What a caller asked `collect` for. `calendar` and `listing` are files for the same reason the
/// rules file is one: a hashed input a second reader can reproduce.
#[derive(Debug, Clone)]
pub struct Ask {
    pub sources: Vec<String>,
    pub window: String,
    pub schedule: Option<String>,
    pub expected_runs: u32,
    pub store: PathBuf,
    pub calendar: PathBuf,
    pub listing: PathBuf,
    /// `edgar_press`'s own inputs. A second provider makes this a policy file; with one, a policy
    /// file would be a shape nothing reads.
    pub roster: PathBuf,
    pub rules: PathBuf,
    pub contact: String,
    pub at: DateTime<Utc>,
    pub dry_run: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum CollectRunError {
    #[error("no provider is registered under {0}")]
    UnknownSource(String),
    #[error("no {what} at {path}")]
    InputAbsent { what: &'static str, path: PathBuf },
    #[error("the calendar at {path} is unusable: {why}")]
    UnusableCalendar { path: PathBuf, why: CalendarError },
    /// Every refusal `plan` can give, carried by its own message. One blanket `map_err` used to
    /// report "neither a day nor a FROM..TO range" for a well-formed day whose observation lag had
    /// no probe behind it - found by review 2026-11-13 on the `--dry-run` path, which is the one an
    /// agent reads.
    #[error(transparent)]
    PlanRefused(#[from] PlanRefusal),
    #[error(transparent)]
    Refused(#[from] edgar_press::Refusal),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Driver(#[from] CollectError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
}

/// The envelope a caller gets and the exit it takes. One object rendered two ways, so the human
/// and the machine output cannot carry different facts.
#[derive(Debug)]
pub struct Answer {
    pub envelope: Value,
    pub exit: Exit,
}

/// What one run collected, with the input digests the envelope names it by.
#[derive(Debug)]
pub struct Driven {
    pub collected: Collected,
    pub calendar_version: String,
}

/// The one provider registered here. A second one makes this a table.
pub fn open_provider(ask: &Ask) -> Result<EdgarPress, CollectRunError> {
    for source in &ask.sources {
        if source != edgar_press::SOURCE {
            return Err(CollectRunError::UnknownSource(source.clone()));
        }
    }
    Ok(EdgarPress::open(&ask.roster, &ask.rules, &ask.contact)?)
}

fn read_input(what: &'static str, path: &Path) -> Result<Vec<u8>, CollectRunError> {
    std::fs::read(path).map_err(|_| CollectRunError::InputAbsent {
        what,
        path: path.to_path_buf(),
    })
}

fn inputs(ask: &Ask) -> Result<(Calendar, Listing), CollectRunError> {
    let calendar = Calendar::read(&read_input("calendar", &ask.calendar)?).map_err(|why| {
        CollectRunError::UnusableCalendar {
            path: ask.calendar.clone(),
            why,
        }
    })?;
    let listing = edgar_press::read_listing(&read_input("quarter listing", &ask.listing)?)?;
    Ok((calendar, listing))
}

/// Drives one run against an archive the caller holds. `run` takes the lease and calls this; a
/// test passes an archive that refuses a commit, which is the only honest way to reach the
/// `commit_failed` bucket of the exit table.
pub fn drive<A: Archive>(
    ask: &Ask,
    provider: &dyn Provider,
    archive: &mut A,
    exchange: &dyn Exchange,
    clock: &dyn Clock,
    run: &RunId,
) -> Result<Driven, CollectRunError> {
    let (calendar, listing) = inputs(ask)?;
    let target = TargetWindow::new(&ask.window);
    let collected = news_provider::collect(
        provider,
        archive,
        exchange,
        clock,
        &Run {
            run,
            target: &target,
            calendar: &calendar,
            listing: &listing,
            at: ObservationTime::new(ask.at),
        },
    )?;
    Ok(Driven {
        collected,
        calendar_version: calendar.version().hex().to_owned(),
    })
}

/// Plans and collects one window. A dry run stops after the plan: it takes no lease, writes no
/// envelope and sends nothing.
pub fn run(
    ask: &Ask,
    provider: &dyn Provider,
    exchange: &dyn Exchange,
    clock: &dyn Clock,
) -> Result<Answer, CollectRunError> {
    let run_id = run_id(ask);
    let run_date = ask.at.date_naive();
    if ask.dry_run {
        return planned(ask, provider, &run_id, run_date);
    }
    let run = RunId::new(run_id.clone());
    let envelope = Envelope {
        schema: RUN_ENVELOPE_SCHEMA.to_owned(),
        run_id: run_id.clone(),
        run_date,
        expected_runs: ask.expected_runs,
        started_at: ask.at,
        finished_at: None,
    };
    // Before the lease, so a run that never gets one is a record rather than a hole.
    write_envelope(&ask.store, &envelope)?;
    let mut store = Committer::open(&ask.store, &run)?;
    let driven = drive(ask, provider, &mut store, exchange, clock, &run)?;
    let finished = Envelope {
        finished_at: Some(clock.now()),
        ..envelope
    };
    write_envelope(&ask.store, &finished)?;
    Ok(Answer {
        exit: driven.collected.exit(),
        envelope: envelope_of(ask, provider, &run_id, run_date, &driven, true),
    })
}

/// The plan alone, which is what `--dry-run` returns: everything except the mutation.
fn planned(
    ask: &Ask,
    provider: &dyn Provider,
    run_id: &str,
    run_date: NaiveDate,
) -> Result<Answer, CollectRunError> {
    let (calendar, listing) = inputs(ask)?;
    let target = TargetWindow::new(&ask.window);
    let plan = provider
        .plan(&target, &calendar, &listing, ObservationTime::new(ask.at))
        .map_err(CollectRunError::PlanRefused)?;
    let driven = Driven {
        collected: Collected::of_plan(&plan, &provider.source()),
        calendar_version: calendar.version().hex().to_owned(),
    };
    Ok(Answer {
        // A dry run answers the plan question and nothing else, so its exit is the plan's.
        exit: driven.collected.exit(),
        envelope: envelope_of(ask, provider, run_id, run_date, &driven, false),
    })
}

fn run_id(ask: &Ask) -> String {
    format!(
        "{}-{}",
        ask.schedule.as_deref().unwrap_or(ADHOC),
        ask.at.format(RUN_ID_FORMAT)
    )
}

/// tmp + rename, like every other file in the layout: a run killed while its envelope is rewritten
/// otherwise leaves unparseable JSON and disables the command whose job is to report the kill.
fn write_envelope(root: &Path, envelope: &Envelope) -> Result<(), CollectRunError> {
    let runs = root.join(RUNS_DIR);
    std::fs::create_dir_all(&runs)?;
    let staging = runs.join(format!("{STAGING_PREFIX}{}.json", envelope.run_id));
    // `File::create` TRUNCATES, which is right HERE and wrong for a blob or a segment: one run
    // writes its envelope twice by design, and refusing a staged file a crash left behind would
    // wedge that run id for good. The archive's content files keep `create_new`.
    let mut staged = std::fs::File::create(&staging)?;
    std::io::Write::write_all(&mut staged, &serde_json::to_vec_pretty(envelope)?)?;
    staged.sync_all()?;
    drop(staged);
    std::fs::rename(&staging, runs.join(format!("{}.json", envelope.run_id)))?;
    // A rename is durable only once the directory holding the new name is fsynced, which is the
    // pair `blobs.rs` and `publish.rs` already keep.
    std::fs::File::open(&runs)?.sync_all()?;
    Ok(())
}

fn envelope_of(
    ask: &Ask,
    provider: &dyn Provider,
    run_id: &str,
    run_date: NaiveDate,
    driven: &Driven,
    changed: bool,
) -> Value {
    let collected = &driven.collected;
    let exit = collected.exit();
    json!({
        "schema": SCHEMA,
        "run_id": run_id,
        "run_date": run_date.to_string(),
        "schedule_id": ask.schedule,
        "collector_version": env!("CARGO_PKG_VERSION"),
        "calendar_version": driven.calendar_version,
        "universe_version": provider.universe_version().hex(),
        "extract_rules_version": provider.extract_rules_version().hex(),
        "at": ask.at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        "dry_run": ask.dry_run,
        "plan": {
            "requests": collected.planned_requests,
            "not_planned": collected.not_planned.iter().map(|row| json!({
                "target_window": row.target_window,
                "reason": row.reason,
                "clean": row.clean,
            })).collect::<Vec<Value>>(),
        },
        "swept": collected.swept,
        "attempts": collected.attempts.iter().map(|row| json!({
            "attempt_id": row.attempt_id,
            "source": row.source,
            "target_window": row.target_window,
            "phase": row.phase,
            "provenance": row.provenance,
            "url": row.url,
            "transport_status": row.transport_status,
            "outcome": row.outcome,
            "vendor_rows": row.vendor_rows,
            "filtered_out": row.filtered_out,
            "unusable_rows": row.unusable_rows,
            "items_committed": row.items_committed,
            "commit_refused": row.commit_refused,
        })).collect::<Vec<Value>>(),
        "per_source": collected.per_source.iter().map(|row| json!({
            "source": row.source,
            "planned_requests": row.planned_requests,
            "not_planned": row.not_planned,
            "attempts": row.attempts,
            "ok": row.ok,
            "empty": row.empty,
            "failed": row.failed,
            "vendor_rows": row.vendor_rows,
            "filtered_out": row.filtered_out,
            "unusable_rows": row.unusable_rows,
            "items_committed": row.items_committed,
        })).collect::<Vec<Value>>(),
        "provenance_counts": provenance_counts(collected),
        "items_committed": collected.tally.committed,
        "blobs_written": collected.blobs_written,
        "bytes_written": collected.bytes_written,
        "blobs_indexed": collected.blobs_indexed,
        "bytes_indexed": collected.bytes_indexed,
        "seq_high_water": collected.seq_high_water,
        "changed": changed && collected.tally.committed > 0,
        "exit": {
            "code": exit.code(),
            "error_code": exit.error_code(),
            "retryable": exit.retryable(),
        },
    })
}

/// The mix of retrieval models this run read under, because two of them are two populations
/// (architecture.md section 8 row R10's disclosure clause).
fn provenance_counts(collected: &Collected) -> Value {
    let mut counts: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
    for attempt in &collected.attempts {
        *counts.entry(attempt.provenance).or_default() += 1;
    }
    json!(counts)
}

/// The human rendering of the same object the JSON carries.
pub fn render_human(envelope: &Value, out: &mut impl std::io::Write) -> std::io::Result<()> {
    writeln!(out, "run              {}", text(envelope, "run_id"))?;
    writeln!(out, "run date         {}", text(envelope, "run_date"))?;
    writeln!(
        out,
        "plan             {} request(s), {} not planned",
        envelope["plan"]["requests"],
        envelope["plan"]["not_planned"]
            .as_array()
            .map_or(0, Vec::len)
    )?;
    let none = Vec::new();
    for attempt in envelope["attempts"].as_array().unwrap_or(&none) {
        writeln!(
            out,
            "  {:<9} {:<8} rows {} filtered {} items {}",
            text(attempt, "phase"),
            text(attempt, "outcome"),
            attempt["vendor_rows"],
            attempt["filtered_out"],
            attempt["items_committed"],
        )?;
    }
    writeln!(out, "items committed  {}", envelope["items_committed"])?;
    writeln!(
        out,
        "blobs            {} written, {} indexed",
        envelope["blobs_written"], envelope["blobs_indexed"]
    )?;
    writeln!(
        out,
        "swept            {}",
        envelope["swept"].as_array().map_or(0, Vec::len)
    )?;
    writeln!(out, "exit             {}", envelope["exit"]["code"])?;
    Ok(())
}

fn text<'a>(of: &'a Value, key: &str) -> &'a str {
    of[key].as_str().unwrap_or_default()
}

/// The store side of the driver. This is where a provider's row becomes an archive row, which is
/// the first production caller of `canon`'s three item digests (architecture.md section 8 row R7).
impl Archive for Committer {
    fn put_blob(&self, payload: &[u8]) -> Result<String, ArchiveError> {
        Committer::put_blob(self, payload)
            .map(|digest| digest.hex().to_owned())
            .map_err(|why| refused("store a payload", &why))
    }

    fn commit(
        &mut self,
        closed: AttemptReceipt,
        items: &[CollectedItem],
        raw_blob: Option<&str>,
        observed: ObservationTime,
    ) -> Result<Committed, ArchiveError> {
        let rows = self
            .rows_of(items, raw_blob, observed)
            .map_err(|why| refused("build an item row", &why))?;
        // The attempt's own close instant, so the row's `committed_at` is not a third clock.
        let at = closed.closed_at();
        let receipt =
            Committer::record(self, closed, &rows, at).map_err(|why| refused("commit", &why))?;
        Ok(Committed {
            items_committed: receipt.items_committed,
            blobs_indexed: receipt.blobs_indexed,
            bytes_indexed: receipt.bytes,
            seq_hi: receipt.seq_hi,
            changed: receipt.changed,
        })
    }

    fn ok_vendor_rows(&self, source: &SourceId) -> Result<Vec<u32>, ArchiveError> {
        Committer::ok_vendor_rows(self, source).map_err(|why| refused("read the baseline", &why))
    }

    fn attempts_in_flight(&self) -> Result<Vec<String>, ArchiveError> {
        Committer::attempts_in_flight(self)
            .map_err(|why| refused("read the attempts in flight", &why))
    }
}

fn refused(act: &'static str, why: &StoreError) -> ArchiveError {
    ArchiveError {
        act,
        detail: why.to_string(),
    }
}

impl Committer {
    /// One provider row as an archive row: the body is a blob of its own, because `item_id`'s
    /// `body_ref` is that blob's digest and an edit to the body has to move the identity.
    fn rows_of(
        &self,
        items: &[CollectedItem],
        raw_blob: Option<&str>,
        observed: ObservationTime,
    ) -> Result<Vec<ItemRow>, StoreError> {
        let mut rows = Vec::with_capacity(items.len());
        for item in items {
            let Some(raw) = raw_blob else {
                return Err(StoreError::BlobAbsent {
                    digest: item.source_item_id.clone(),
                });
            };
            let body = match &item.body {
                Some(body) => Some(Committer::put_blob(self, body.text.as_bytes())?),
                None => None,
            };
            let Some(valid) = stamp_of(&item.valid) else {
                return Err(StoreError::ItemHasNoPublicationValue {
                    item: item.source_item_id.clone(),
                });
            };
            rows.push(ItemRow {
                item_id: named(
                    item,
                    item_id(&ItemFields {
                        source: item.source.as_str(),
                        source_item_id: &item.source_item_id,
                        url: &item.url,
                        valid,
                        title: &item.title,
                        body_ref: body.as_ref().map(crate::blobs::BlobDigest::hex),
                    }),
                )?,
                story_id: named(item, story_id(item.source.as_str(), &item.source_item_id))?,
                doc_id: Some(named(item, doc_id(&item.canonical_url))?),
                valid: item.valid,
                observed,
                raw_blob: self.blob_named(raw)?,
                body_blob: body,
            });
        }
        Ok(rows)
    }
}

/// A refused canonical form names the row it refused, so a reader of the error knows which item
/// of a payload has no identity.
fn named(
    item: &CollectedItem,
    digest: Result<canon::Digest, canon::CanonError>,
) -> Result<canon::Digest, StoreError> {
    digest.map_err(|why| StoreError::ItemIdentityRefused {
        item: item.source_item_id.clone(),
        why: why.to_string(),
    })
}
