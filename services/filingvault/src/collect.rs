//! `filingvault filings collect`: one ledgered conversation with SEC (architecture.md section
//! 12.2). Order: the sweep, robots.txt, the quarter listings, the trailing index days, then every
//! other listed day of the fill window newest first while the open filings are under the
//! budget. No in-run retry; three consecutive traffic 403 answers stop it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use chrono::{DateTime, Days, NaiveDate, Utc};
use news_provider::robots::Robots;
use news_provider::{
    received, AttemptId, Baseline, BudgetError, Clock, Counts, Credentials, Exchange, Outcome,
    OutcomeKind, PaceRoot, Provenance, RawResponse, Reservation, RunId, SendFailure, SourceId,
    TransportError, TransportProfile, Unreserved, Verdict, Wire,
};
use pit::ObservationTime;
use sec_edgar::{listed_writes, Caller, Quarter};
use serde_json::{json, Value};

use crate::discover::{
    allowed, day_states, days_to_ask, index_ask, index_data_day, index_read, listing_ask,
    listing_read, quarters_to_ask, request, rewritten_days, robots_ask, robots_rules,
    robots_verdict, DaysToAsk, RobotsStop, Written, BODY_LIMIT, QUARTER_SETTLE_DAYS,
};
use crate::error::{ErrorCode, FilingsError};
use crate::rules::Rules;
use crate::store::{self, Ask as Described, Body, Commit, Writer};

pub const COLLECT_SCHEMA: &str = "filingvault.collect/1";
const RUN_PREFIX: &str = "filings";
const RUN_ID_FORMAT: &str = "%Y%m%dT%H%M%S%.6fZ";
/// A dry run's `store`: a mistyped `--store` plans as an empty store would (review EF4).
const STORE_ABSENT: &str = "absent";
const STORE_HELD: &str = "held";
/// `runs.stopped_by` of a run an error ended: its `exit` is the error code's.
const STOPPED_BY_ERROR: &str = "error";
/// Consecutive traffic 403 answers that stop a run: no further attempt is opened (owner decision
/// 2026-11-24, section 12.2).
pub const TRAFFIC_403_STOP: u32 = 3;

#[derive(Debug, Clone)]
pub struct Ask {
    pub store: PathBuf,
    pub rules: Rules,
    /// No request is reserved after it; `None` is no deadline.
    pub deadline: Option<DateTime<Utc>>,
    pub dry_run: bool,
}

/// SEC's profile at the filings row of its interval table.
#[must_use]
pub fn transport() -> TransportProfile {
    sec_edgar::transport(Caller::Filings, BODY_LIMIT)
}

/// SEC's transport, the contact under SEC's credential name, and SEC's one per-IP bucket, which
/// the roster pays into too (section 12.2, `FV-Q4`).
#[must_use]
pub fn wire_for(contact: &str, pace_root: &PaceRoot) -> Wire {
    Wire::new(
        transport(),
        Credentials::default().with(sec_edgar::CONTACT_CREDENTIAL, contact.to_owned()),
        pace_root,
        &SourceId::new(sec_edgar::PER_IP_BUCKET),
    )
}

/// What a run answered: the report, and the exit for a run that was not stopped by an error.
#[derive(Debug)]
pub struct Report {
    pub value: Value,
    pub exit: u8,
}

/// Why a run stopped before its plan ended; `runs.stopped_by`.
#[derive(Debug)]
enum Stop {
    Deadline,
    /// An older day was left unasked: the open filings had reached the rules' `filing_budget`.
    Budget,
    Robots(RobotsStop),
    Traffic403,
    Gate {
        attempt_id: String,
        why: BudgetError,
    },
}

impl Stop {
    const fn name(&self) -> &'static str {
        match self {
            Self::Deadline => "deadline",
            Self::Budget => "budget",
            Self::Robots(_) => "robots",
            Self::Traffic403 => "traffic_403",
            Self::Gate { .. } => "gate_refused",
        }
    }
}

type Answer = Result<RawResponse, TransportError>;

/// One attempt's close, and what its reader took from the answer.
struct Turn<T> {
    row: Value,
    verdict: Verdict,
    got: T,
}

struct Driver<'a> {
    writer: Writer,
    exchange: &'a dyn Exchange,
    clock: &'a dyn Clock,
    deadline: Option<DateTime<Utc>>,
    run: RunId,
    ordinal: u32,
    failed: u32,
    /// Traffic 403 answers since the last other answer; a transport error with no status is
    /// no answer and leaves it as it is.
    traffic_403s: u32,
    robots: Value,
    listings: Vec<Value>,
    days: Vec<Value>,
}

impl Driver<'_> {
    fn now(&self) -> ObservationTime {
        ObservationTime::new(self.clock.now())
    }

    fn abandon(&mut self, attempt_id: &AttemptId) -> Result<(), FilingsError> {
        let at = self.now();
        let run = self.run.clone();
        drop(news_provider::abandon(
            &mut self.writer,
            attempt_id.clone(),
            at,
            &run,
        )?);
        Ok(())
    }

    /// One request: reserved, its row opened before the socket, sent, its body stored before
    /// `read` parses it, and closed with what `read` committed. `Err(Stop)` leaves no open row.
    fn turn<T>(
        &mut self,
        described: Described,
        provenance: Provenance,
        read: impl FnOnce(&Answer) -> (Verdict, Commit, T),
    ) -> Result<Result<Turn<T>, Stop>, FilingsError> {
        if self.traffic_403s >= TRAFFIC_403_STOP {
            return Ok(Err(Stop::Traffic403));
        }
        let request = request(&described, provenance);
        match self.exchange.reserve(&request, self.clock, self.deadline) {
            Ok(Reservation::Go) => {}
            Ok(Reservation::PastDeadline { .. }) => return Ok(Err(Stop::Deadline)),
            Err(Unreserved::CredentialMissing(name)) => return Err(FilingsError::Credential(name)),
            Err(Unreserved::Budget(why)) => return Err(why.into()),
        }
        let opened_at = self.now();
        let mut slot = self
            .writer
            .open_slot(described, &request, self.ordinal, opened_at)?;
        self.ordinal += 1;
        let attempt_id = slot.attempt_id().clone();
        let permit = slot.take_permit()?;
        let answer: Answer = match self.exchange.send(permit, &request, self.clock) {
            Ok(response) => Ok(response),
            Err(SendFailure::Transport(failed)) => Err(failed),
            Err(SendFailure::Budget(why)) if why.is_a_gate_refusal() => {
                let at = self.now();
                drop(slot.close_unsent(&mut self.writer, at)?);
                return Ok(Err(match why.owed_past_the_deadline() {
                    Some(_) => Stop::Deadline,
                    None => Stop::Gate {
                        attempt_id: attempt_id.hex().to_owned(),
                        why,
                    },
                }));
            }
            Err(SendFailure::Budget(why)) => {
                self.abandon(&attempt_id)?;
                return Err(why.into());
            }
            Err(SendFailure::CredentialMissing(name)) => {
                self.abandon(&attempt_id)?;
                return Err(FilingsError::Credential(name));
            }
        };
        match &answer {
            Ok(_) if sec_edgar::is_traffic_403(&answer) => self.traffic_403s += 1,
            Ok(_) => self.traffic_403s = 0,
            // A 403 head whose body was lost: `AccessDenied` cannot be read from it (§12.2).
            Err(failed) if failed.status() == Some(sec_edgar::FORBIDDEN) => {}
            Err(failed) if failed.status().is_some() => self.traffic_403s = 0,
            Err(_) => {}
        }
        let body = match received(&answer) {
            None => None,
            Some(got) => match self.writer.put(got.body) {
                Ok(stored) => Some(Body {
                    stored,
                    bytes: u64::try_from(got.body.len()).unwrap_or(u64::MAX),
                    truncated: got.truncated,
                    headers: got.headers.to_json(),
                }),
                Err(why) => {
                    let kept_nothing = Outcome::decide(
                        &Verdict::TransientFailure,
                        Counts {
                            vendor_rows: 0,
                            listing_unusable: 0,
                            robots_disallowed: 0,
                        },
                        &Baseline::from_trailing_ok(&[]),
                    );
                    let at = self.now();
                    drop(slot.close(&mut self.writer, kept_nothing, &answer, at)?);
                    return Err(why);
                }
            },
        };
        let (verdict, mut commit, got) = read(&answer);
        let truncated = body.as_ref().is_some_and(|body| body.truncated);
        let rows = commit.listed.len();
        commit.body = body;
        let outcome = Outcome::decide(
            &verdict,
            Counts {
                vendor_rows: u32::try_from(rows).unwrap_or(u32::MAX),
                listing_unusable: 0,
                robots_disallowed: 0,
            },
            &Baseline::from_trailing_ok(&[]),
        );
        let kind = outcome.kind();
        self.writer.stage(commit);
        let at = self.now();
        drop(slot.close(&mut self.writer, outcome, &answer, at)?);
        if kind == OutcomeKind::Failed {
            self.failed += 1;
        }
        let status = match &answer {
            Ok(response) => Some(response.status()),
            Err(failed) => failed.status(),
        };
        Ok(Ok(Turn {
            row: json!({
                "attempt_id": attempt_id.hex(),
                "status": status,
                "outcome": kind.name(),
                "verdict": verdict.name(),
                "listed_rows": rows,
                "truncated": truncated,
            }),
            verdict,
            got,
        }))
    }

    fn collect(
        &mut self,
        rules: &Rules,
        run_date: NaiveDate,
    ) -> Result<Option<Stop>, FilingsError> {
        let interval = transport().budget.per_host.unwrap_or_default();
        let robots = match self.turn(robots_ask(), Provenance::LiveIngest, |answer| {
            (
                robots_verdict(answer),
                Commit::default(),
                robots_rules(answer, interval),
            )
        })? {
            Ok(turn) => turn,
            Err(stop) => return Ok(Some(stop)),
        };
        self.robots = robots.row;
        let robots: Option<Robots> = match robots.got {
            Ok(rules) => rules,
            Err(stop) => return Ok(Some(Stop::Robots(stop))),
        };
        let window = Window::of(&self.writer, rules, run_date)?;
        let mut days = window.held_days.clone();
        for quarter in quarters_to_ask(window.fill_start, window.current, &window.complete)? {
            let described = listing_ask(quarter);
            if !allowed(robots.as_ref(), &described.url) {
                return Ok(Some(Stop::Robots(RobotsStop::Disallowed)));
            }
            let turn = match self.turn(described, Provenance::LiveIngest, |answer| {
                let (verdict, days) = listing_read(answer, quarter);
                (verdict, Commit::default(), days)
            })? {
                Ok(turn) => turn,
                Err(stop) => return Ok(Some(stop)),
            };
            let mut row = turn.row;
            row["quarter"] = json!(quarter.label());
            row["days"] = json!(turn.got.len());
            self.listings.push(row);
            if matches!(turn.verdict, Verdict::Usable | Verdict::Empty) {
                merge(days.entry(quarter).or_default(), turn.got);
            }
        }
        let planned = plan(self.writer.db(), &days, rules, window.fill_start, run_date)?;
        for day in planned.trailing {
            if let Err(stop) = self.ask_day(day, rules, robots.as_ref())? {
                return Ok(Some(stop));
            }
        }
        for day in planned.older {
            if store::open_filings(self.writer.db())? >= u64::from(rules.filing_budget) {
                return Ok(Some(Stop::Budget));
            }
            if let Err(stop) = self.ask_day(day, rules, robots.as_ref())? {
                return Ok(Some(stop));
            }
        }
        Ok(None)
    }

    /// One index day: its path checked against robots.txt, then its turn.
    fn ask_day(
        &mut self,
        day: NaiveDate,
        rules: &Rules,
        robots: Option<&Robots>,
    ) -> Result<Result<(), Stop>, FilingsError> {
        let described = index_ask(day, &rules.forms);
        if !allowed(robots, &described.url) {
            return Ok(Err(Stop::Robots(RobotsStop::Disallowed)));
        }
        let forms = rules.forms.clone();
        let turn = match self.turn(described, Provenance::VendorArchive, move |answer| {
            let (verdict, listed) = index_read(answer, &forms);
            let commit = Commit {
                body: None,
                index_day: Some(day),
                listed,
                data_day: index_data_day(answer),
            };
            (verdict, commit, ())
        })? {
            Ok(turn) => turn,
            Err(stop) => return Ok(Err(stop)),
        };
        let mut row = turn.row;
        row["index_day"] = json!(store::day_text(day));
        self.days.push(row);
        Ok(Ok(()))
    }
}

/// The fill window and what the store already holds of its quarters.
struct Window {
    fill_start: NaiveDate,
    current: Quarter,
    /// The days any held listing of each quarter names, each with its newest listed write.
    held_days: BTreeMap<Quarter, Writes>,
    /// Quarters with a usable listing received after their last day and
    /// [`QUARTER_SETTLE_DAYS`]: never asked again.
    complete: BTreeSet<Quarter>,
}

impl Window {
    /// The fill reaches `fill_days` before the first collect run's UTC date, or this one's.
    fn of(writer: &Writer, rules: &Rules, run_date: NaiveDate) -> Result<Self, FilingsError> {
        Self::read(writer.db(), writer.blobs(), rules, run_date)
    }

    fn read(
        db: &rusqlite::Connection,
        blobs: &vault_core::blobs::BlobStore,
        rules: &Rules,
        run_date: NaiveDate,
    ) -> Result<Self, FilingsError> {
        let first = match store::collect_starts(db, None)?.first() {
            Some(started) => instant(started)?.date_naive(),
            None => run_date,
        };
        let fill_start = fill_start(first, rules.fill_days)?;
        let current = Quarter::of(run_date.pred_opt().unwrap_or(run_date));
        let mut held_days: BTreeMap<Quarter, Writes> = BTreeMap::new();
        let mut complete = BTreeSet::new();
        let labels: BTreeMap<String, Quarter> = quarters_to_ask(fill_start, current, &complete)?
            .into_iter()
            .map(|quarter| (quarter.label(), quarter))
            .collect();
        for held in store::held_listings(db, None)? {
            let Some(quarter) = labels.get(&held.quarter) else {
                continue;
            };
            let days = listed_writes(&store::read_blob(blobs, &held.blob)?).map_err(|why| {
                FilingsError::Unparseable {
                    what: format!("the stored listing of {}", held.quarter),
                    why: why.to_string(),
                }
            })?;
            let settled = quarter.last_day() + Days::new(QUARTER_SETTLE_DAYS);
            if held.usable && instant(&held.received)?.date_naive() > settled {
                complete.insert(*quarter);
            }
            merge(held_days.entry(*quarter).or_default(), days);
        }
        Ok(Self {
            fill_start,
            current,
            held_days,
            complete,
        })
    }
}

/// Each listed day with the newest write any listing of it states.
pub(crate) type Writes = BTreeMap<NaiveDate, DateTime<Utc>>;

pub(crate) fn merge(held: &mut Writes, listed: Vec<Written>) {
    for (day, written) in listed {
        let newest = held.entry(day).or_insert(written);
        *newest = (*newest).max(written);
    }
}

/// The index days a run asks from the listed days of every quarter and the attempts held.
fn plan(
    db: &rusqlite::Connection,
    listed: &BTreeMap<Quarter, Writes>,
    rules: &Rules,
    fill_start: NaiveDate,
    run_date: NaiveDate,
) -> Result<DaysToAsk, FilingsError> {
    let writes: Writes = listed.values().flatten().map(|(d, w)| (*d, *w)).collect();
    let days: BTreeSet<NaiveDate> = writes.keys().copied().collect();
    let attempts = store::index_attempts(db, None)?;
    let states = day_states(&attempts, &rules.forms, BODY_LIMIT);
    let rewritten = rewritten_days(&writes, &attempts)?;
    Ok(days_to_ask(
        &days, &states, &rewritten, fill_start, run_date,
    ))
}

/// `fill_days` before `first`, or the error naming a window the calendar does not hold.
fn fill_start(first: NaiveDate, fill_days: u32) -> Result<NaiveDate, FilingsError> {
    first
        .checked_sub_days(Days::new(u64::from(fill_days)))
        .ok_or_else(|| FilingsError::FillWindow(format!("{fill_days} days before {first}")))
}

/// A stored instant, or `input_unparseable` naming it.
pub fn instant(text: &str) -> Result<DateTime<Utc>, FilingsError> {
    DateTime::parse_from_rfc3339(text)
        .map(|at| at.with_timezone(&Utc))
        .map_err(|why| FilingsError::Unparseable {
            what: format!("the stored instant {text:?}"),
            why: why.to_string(),
        })
}

/// One run. A robots stop, the traffic 403 stop and a gate refusal are errors carrying the
/// report; every other run answers its report with exit 0, or 8 for a failed request or the
/// deadline.
pub fn run(ask: &Ask, exchange: &dyn Exchange, clock: &dyn Clock) -> Result<Report, FilingsError> {
    let started = clock.now();
    let run_id = format!("{RUN_PREFIX}-{}", started.format(RUN_ID_FORMAT));
    if ask.dry_run {
        return planned(ask, &run_id, started);
    }
    let run = RunId::new(run_id.clone());
    let mut writer = Writer::open(&ask.store, &run)?;
    let (swept_attempts, swept_runs) = writer.sweep(started)?;
    writer.start_run(started, ask.rules.filing_budget)?;
    let mut driver = Driver {
        writer,
        exchange,
        clock,
        deadline: ask.deadline,
        run,
        ordinal: 0,
        failed: 0,
        traffic_403s: 0,
        robots: Value::Null,
        listings: Vec::new(),
        days: Vec::new(),
    };
    let stop = match driver.collect(&ask.rules, started.date_naive()) {
        Ok(_) if driver.traffic_403s >= TRAFFIC_403_STOP => Some(Stop::Traffic403),
        Ok(stop) => stop,
        Err(why) => {
            let exit = why.code().exit();
            return driver
                .writer
                .finish_run(clock.now(), exit, Some(STOPPED_BY_ERROR), None)
                .and(Err(why));
        }
    };
    let code = match (&stop, driver.failed) {
        (Some(Stop::Robots(_)), _) => Some(ErrorCode::RobotsStopped),
        (Some(Stop::Traffic403), _) => Some(ErrorCode::TrafficStopped),
        (Some(Stop::Gate { .. }), _) => Some(ErrorCode::GateRefused),
        (Some(Stop::Deadline | Stop::Budget), _) | (None, 1..) => Some(ErrorCode::PartialCollect),
        (None, 0) => None,
    };
    let exit = code.map_or(0, ErrorCode::exit);
    let planned = store::open_filings(driver.writer.db())?;
    driver.writer.finish_run(
        clock.now(),
        exit,
        stop.as_ref().map(Stop::name),
        Some(planned),
    )?;
    let value = json!({
        "schema": COLLECT_SCHEMA,
        "run_id": run_id,
        "at": store::stamp(started),
        "dry_run": false,
        "swept": {"attempts": swept_attempts, "runs": swept_runs},
        "robots": driver.robots,
        "listings": driver.listings,
        "days": driver.days,
        "budget": {"budget": ask.rules.filing_budget, "planned": planned},
        "stopped_by": stop.as_ref().map(Stop::name),
        "changed": true,
        "exit": {
            "code": exit,
            "error_code": code.map(ErrorCode::name),
            "retryable": code.is_some_and(ErrorCode::retryable),
        },
    });
    match stop {
        Some(Stop::Robots(reason)) => Err(FilingsError::RobotsStopped {
            reason: reason.reason(),
            report: value,
        }),
        Some(Stop::Traffic403) => Err(FilingsError::TrafficStopped {
            count: driver.traffic_403s,
            report: value,
        }),
        Some(Stop::Gate { attempt_id, why }) => Err(FilingsError::GateRefused {
            attempt_id,
            why,
            report: value,
        }),
        Some(Stop::Deadline | Stop::Budget) | None => Ok(Report { value, exit }),
    }
}

/// `--dry-run`: the quarters and the days a run would ask from what the store holds, read only;
/// no store is created, no lease taken and nothing sent. `store` says whether `--store` held one.
/// Older days are planned only while the open filings are under the budget; how many a run then
/// asks before it reaches the budget is known only from the days' answers.
fn planned(ask: &Ask, run_id: &str, started: DateTime<Utc>) -> Result<Report, FilingsError> {
    let run_date = started.date_naive();
    let current = Quarter::of(run_date.pred_opt().unwrap_or(run_date));
    let (store, quarters, days, open) = match store::open_read(&ask.store) {
        Err(FilingsError::Absent(_)) => {
            let fill_start = fill_start(run_date, ask.rules.fill_days)?;
            (
                STORE_ABSENT,
                quarters_to_ask(fill_start, current, &BTreeSet::new())?,
                Vec::new(),
                0,
            )
        }
        Err(other) => return Err(other),
        Ok(db) => {
            let blobs = vault_core::blobs::BlobStore::new(
                ask.store.join(store::BLOBS),
                store::WRITE_POLICY,
            );
            let window = Window::read(&db, &blobs, &ask.rules, run_date)?;
            let planned = plan(
                &db,
                &window.held_days,
                &ask.rules,
                window.fill_start,
                run_date,
            )?;
            let open = store::open_filings(&db)?;
            let older = if open < u64::from(ask.rules.filing_budget) {
                planned.older
            } else {
                Vec::new()
            };
            (
                STORE_HELD,
                quarters_to_ask(window.fill_start, window.current, &window.complete)?,
                [planned.trailing, older].concat(),
                open,
            )
        }
    };
    let value = json!({
        "schema": COLLECT_SCHEMA,
        "run_id": run_id,
        "at": store::stamp(started),
        "dry_run": true,
        "store": store,
        "planned": {
            "listings": quarters.iter().map(|quarter| quarter.label()).collect::<Vec<String>>(),
            "days": days.into_iter().map(store::day_text).collect::<Vec<String>>(),
        },
        "budget": {"budget": ask.rules.filing_budget, "planned": open},
        "changed": false,
        "exit": {"code": 0, "error_code": null, "retryable": false},
    });
    Ok(Report { value, exit: 0 })
}
