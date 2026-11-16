//! `newsvault` - command line over the point-in-time news archive.
//!
//! Follows the repo's CLI contract: the result on stdout and nothing else, logs and errors on
//! stderr, exactly one JSON value in JSON mode, and `schema` for a caller that is not human.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use chrono::{DateTime, NaiveDate, Utc};
use clap::{Parser, Subcommand, ValueEnum};

use news_provider::{Clock, RunId, SystemClock};
use newsvault::cmd_collect::{self, CollectRunError};
use newsvault::cmd_items;
use newsvault::cmd_ledger::{self, Ask, LedgerReadError, Report};
use newsvault::cmd_robots::{self, RobotsError};
use newsvault::lake::{Day, LakeError, LakeReader};
use newsvault::publish::{publish_run, Fsync, PublishError};
use newsvault::store::{Committer, StoreError};
use pit::{Cutoff, ObservationTime};

/// Exit codes, kept small and documented. Domain detail belongs in the error body.
/// `1` means THE ANSWER IS NO and nothing else: decision D1, taken by default and recorded in
/// architecture.md section 10. The three registry codes that used to land here - and every other
/// non-usage failure of this binary - are on 2.
const EXIT_NO: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_NOT_FOUND: u8 = 4;
const EXIT_LEASE_HELD: u8 = 7;
const EXIT_INCOMPLETE: u8 = 8;

#[derive(Parser)]
#[command(
    name = "newsvault",
    about = "Point-in-time news archive: complete days, honest timestamps.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Every item the archive holds for one UTC day.
    Day {
        /// UTC day, as YYYY-MM-DD.
        date: String,
        /// Lake root. Defaults to ~/.alphalens/thematic_news_lake.
        #[arg(long)]
        lake: Option<PathBuf>,
        /// Keep only items visible at this instant (RFC 3339). Both the publication
        /// and the observation must be at or before it.
        #[arg(long)]
        at: Option<String>,
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
    },
    /// Coverage over a day range, as of one instant.
    Ledger {
        /// The instant every age is computed against (RFC 3339). Required: a read with no cutoff
        /// is not a point-in-time read.
        #[arg(long)]
        at: String,
        /// First publication-run day, as YYYY-MM-DD.
        #[arg(long)]
        from: String,
        /// Last run day, inclusive.
        #[arg(long)]
        to: String,
        /// Vault root. Defaults to $BELLWETHER_VAULT, then ~/.bellwether/vault.
        #[arg(long)]
        store: Option<PathBuf>,
        #[arg(long)]
        source: Option<String>,
        /// Runs a day was due when its own envelopes are missing, which is the only state a
        /// day's envelopes cannot speak for.
        #[arg(long)]
        expected_runs: Option<u32>,
        /// Caps the rows reported WITHIN a day. Never the number of days.
        #[arg(long)]
        limit: Option<usize>,
        /// Answer one question - is the coverage shortfall over its bound - and exit 1 for no.
        #[arg(long)]
        check: bool,
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
    },
    /// Exports every committed row no segment carries yet, one segment per publication day.
    Publish {
        /// The instant the export is recorded at (RFC 3339).
        #[arg(long)]
        at: String,
        #[arg(long)]
        store: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
    },
    /// Collects one window from one source into the archive. The only command that writes.
    Collect {
        /// Source id: edgar_press or open_web_wires. Repeatable, but a second distinct source is
        /// refused: one source per invocation.
        #[arg(long = "source", required = true)]
        sources: Vec<String>,
        /// A day as YYYY-MM-DD, a FROM..TO range of days, or `today`: the UTC date of `--at`,
        /// refused for edgar_press, which plans session days.
        #[arg(long)]
        window: String,
        /// The schedule slot this run belongs to, which keys its run id.
        #[arg(long)]
        schedule: Option<String>,
        /// How many runs the day is due, recorded in the run envelope for the ledger to read.
        #[arg(long, default_value_t = 1)]
        expected_runs: u32,
        #[arg(long)]
        store: Option<PathBuf>,
        /// The session calendar, hashed: a non-session day is zero requests. Required for
        /// edgar_press, refused for open_web_wires.
        #[arg(long)]
        calendar: Option<PathBuf>,
        /// The vendor's own quarter listing, hashed: an unpublished day is zero requests.
        /// Required for edgar_press, refused for open_web_wires.
        #[arg(long)]
        listing: Option<PathBuf>,
        /// The in-universe CIK roster. Absent, unparseable or empty is a refusal, never an empty
        /// universe. Required for edgar_press, refused for open_web_wires.
        #[arg(long)]
        roster: Option<PathBuf>,
        /// The hosts file, hashed: only its admitted rows are asked. Required for
        /// open_web_wires, refused for edgar_press.
        #[arg(long)]
        hosts: Option<PathBuf>,
        /// The chosen provider's hashed extraction rules.
        #[arg(long)]
        rules: PathBuf,
        /// The measured observation lag per source and provenance, hashed. Without it a window
        /// earlier than this run's own date is refused before its first request, which is what
        /// `probes/observation_lag/` measures (requirement R11, decision D5).
        #[arg(long)]
        lag_policy: Option<PathBuf>,
        /// The instant this run PLANS and classifies provenance against (RFC 3339), not the
        /// instant anything is observed at: an item's observation time is read from the clock
        /// when its fetch returns, so this option cannot backdate what we knew. Defaults to now.
        #[arg(long)]
        at: Option<String>,
        /// Plans, validates and resolves every target, and mutates nothing.
        #[arg(long)]
        dry_run: bool,
        /// Accepted and does nothing: no command here prompts or reads stdin.
        #[arg(long)]
        non_interactive: bool,
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
    },
    /// Every item visible at one instant, ordered by seq.
    Items {
        /// The cutoff (RFC 3339). Required: a read with no cutoff is not a point-in-time read.
        #[arg(long)]
        at: String,
        #[arg(long)]
        store: Option<PathBuf>,
        /// First publication day, as YYYY-MM-DD.
        #[arg(long)]
        from: Option<String>,
        /// Last publication day, inclusive.
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
        /// Comma-separated field names, from the list `schema items` prints.
        #[arg(long, value_delimiter = ',')]
        fields: Option<Vec<String>>,
        /// `all` reports every visible row; `latest` keeps the highest seq per story.
        #[arg(long, value_enum, default_value_t = Story::All)]
        per_story: Story,
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
    },
    /// robots.txt of every hosts-file row, fetched under the ledger.
    Robots {
        #[command(subcommand)]
        action: RobotsAction,
    },
    /// Machine-readable description of this tool.
    Schema {
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },
}

#[derive(Subcommand)]
enum RobotsAction {
    /// Fetches each hosts-file row's robots.txt once, admitted and candidate, under an attempt
    /// row, and compares it with the row's pin. Never writes the hosts file.
    Refresh {
        /// Source id whose hosts file is read: open_web_wires.
        #[arg(long)]
        source: String,
        /// The hosts file, hashed; every row is asked.
        #[arg(long)]
        hosts: PathBuf,
        /// The open-web-feed rules file the hosts rows name.
        #[arg(long)]
        rules: PathBuf,
        #[arg(long)]
        store: Option<PathBuf>,
        /// Plans one url per row and mutates nothing.
        #[arg(long)]
        dry_run: bool,
        /// Accepted and does nothing: no command here prompts or reads stdin.
        #[arg(long)]
        non_interactive: bool,
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Human,
    Json,
}

/// `all` is the default on purpose: a default that silently collapses rows is R1's defect moved
/// into the view (architecture.md section 8 row R1).
#[derive(Clone, Copy, ValueEnum)]
enum Story {
    All,
    Latest,
}

impl Story {
    const fn chosen(self) -> cmd_items::PerStory {
        match self {
            Self::All => cmd_items::PerStory::All,
            Self::Latest => cmd_items::PerStory::Latest,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(Verdict::Ok) => ExitCode::SUCCESS,
        Ok(Verdict::No(code)) => ExitCode::from(code),
        Err(e) => {
            let code = exit_code(&e);
            #[allow(
                clippy::let_underscore_must_use,
                reason = "reporting an error must not fail on a closed stderr; the exit code is the verdict"
            )]
            let _ = writeln!(std::io::stderr(), "{}: {e:#}", prefix_of(&e));
            ExitCode::from(code)
        }
    }
}

/// `usage error` for the caller's own mistake, as `usage_exit` writes it; `error` otherwise.
fn prefix_of(failure: &anyhow::Error) -> &'static str {
    if failure
        .downcast_ref::<CollectRunError>()
        .is_some_and(CollectRunError::is_usage)
        || failure
            .downcast_ref::<RobotsError>()
            .is_some_and(RobotsError::is_usage)
    {
        "usage error"
    } else {
        "error"
    }
}

/// What a successful read answered. A command that asks a question reports its answer in the exit
/// status, which is what `1` now means.
enum Verdict {
    Ok,
    No(u8),
}

/// Every failure of this binary, as the section 5.1 code it is. Nothing maps to 1.
fn exit_code(failure: &anyhow::Error) -> u8 {
    if let Some(lake) = failure.downcast_ref::<LakeError>() {
        return match lake {
            LakeError::MissingRoot(_) | LakeError::MissingDay { .. } => EXIT_NOT_FOUND,
            // input_schema_mismatch, input_unparseable and io_failed, all 2.
            _ => EXIT_USAGE,
        };
    }
    if let Some(ledger) = failure.downcast_ref::<LedgerReadError>() {
        return match ledger {
            LedgerReadError::StoreAbsent(_) => EXIT_NOT_FOUND,
            LedgerReadError::RangeInverted { .. } => EXIT_USAGE,
            LedgerReadError::Sqlite(_) | LedgerReadError::Store(_) => EXIT_USAGE,
        };
    }
    if let Some(collect) = failure.downcast_ref::<CollectRunError>() {
        return match collect {
            CollectRunError::InputAbsent { .. } => EXIT_NOT_FOUND,
            CollectRunError::Refused(
                edgar_press::Refusal::RosterAbsent(_) | edgar_press::Refusal::RosterEmpty(_),
            ) => EXIT_NOT_FOUND,
            // input_absent: an absent hosts or rules file, and a hosts file admitting no host,
            // which would plan zero requests and read as a quiet day (R6).
            CollectRunError::OpenWebRefused(
                open_web_feed::Refusal::HostsAbsent(_)
                | open_web_feed::Refusal::RulesAbsent(_)
                | open_web_feed::Refusal::NoHostAdmitted(_),
            ) => EXIT_NOT_FOUND,
            CollectRunError::Store(StoreError::LeaseHeld(_)) => EXIT_LEASE_HELD,
            // invalid_argument, input_unparseable, credential_contact_missing and io_failed.
            _ => EXIT_USAGE,
        };
    }
    if let Some(robots) = failure.downcast_ref::<RobotsError>() {
        return match robots {
            RobotsError::OpenWebRefused(
                open_web_feed::Refusal::HostsAbsent(_)
                | open_web_feed::Refusal::RulesAbsent(_)
                | open_web_feed::Refusal::NoHostAdmitted(_),
            ) => EXIT_NOT_FOUND,
            RobotsError::Store(StoreError::LeaseHeld(_)) => EXIT_LEASE_HELD,
            _ => EXIT_USAGE,
        };
    }
    if let Some(items) = failure.downcast_ref::<cmd_items::ItemsError>() {
        return match items {
            cmd_items::ItemsError::StoreAbsent(_) => EXIT_NOT_FOUND,
            _ => EXIT_USAGE,
        };
    }
    if let Some(PublishError::Store(StoreError::LeaseHeld(_))) =
        failure.downcast_ref::<PublishError>()
    {
        return EXIT_LEASE_HELD;
    }
    if let Some(StoreError::LeaseHeld(_)) = failure.downcast_ref::<StoreError>() {
        return EXIT_LEASE_HELD;
    }
    EXIT_USAGE
}

/// `$BELLWETHER_VAULT`, then the default beside it. Never the predecessor's lake.
fn vault_root(given: Option<PathBuf>) -> PathBuf {
    given.unwrap_or_else(|| {
        std::env::var_os("BELLWETHER_VAULT").map_or_else(
            || {
                std::env::var_os("HOME")
                    .map_or_else(PathBuf::new, PathBuf::from)
                    .join(".bellwether")
                    .join("vault")
            },
            PathBuf::from,
        )
    })
}

fn instant(raw: &str, flag: &str) -> DateTime<Utc> {
    match raw.parse::<DateTime<Utc>>() {
        Ok(at) => at,
        Err(_) => usage_exit(format!("{flag} {raw:?} is not an RFC 3339 instant")),
    }
}

fn date(raw: &str, flag: &str) -> NaiveDate {
    match raw.parse::<NaiveDate>() {
        Ok(day) => day,
        Err(_) => usage_exit(format!("{flag} {raw:?} is not YYYY-MM-DD")),
    }
}

fn run(cli: Cli) -> anyhow::Result<Verdict> {
    match cli.command {
        Command::Schema { format } => {
            print_schema(format);
            Ok(Verdict::Ok)
        }
        Command::Ledger {
            at,
            from,
            to,
            store,
            source,
            expected_runs,
            limit,
            check,
            format,
        } => {
            let ask = Ask {
                at: instant(&at, "--at"),
                from: date(&from, "--from"),
                to: date(&to, "--to"),
                source,
                expected_runs,
                limit,
                check,
            };
            let report = cmd_ledger::read(&vault_root(store), &ask)?;
            emit_ledger(&report, format)?;
            Ok(if !report.is_complete() {
                Verdict::No(EXIT_INCOMPLETE)
            } else if report.answer_is_no() {
                Verdict::No(EXIT_NO)
            } else {
                Verdict::Ok
            })
        }
        Command::Collect {
            sources,
            window,
            schedule,
            expected_runs,
            store,
            calendar,
            listing,
            roster,
            hosts,
            rules,
            lag_policy,
            at,
            dry_run,
            non_interactive: _,
            format,
        } => {
            let clock = SystemClock;
            let registered = cmd_collect::Registered::chosen(&sources)?;
            let ask = cmd_collect::Ask {
                sources,
                window,
                schedule,
                expected_runs,
                store: vault_root(store),
                calendar,
                listing,
                roster,
                hosts,
                rules,
                lag_policy,
                contact: contact(registered),
                at: match at {
                    Some(raw) => instant(&raw, "--at"),
                    None => clock.now(),
                },
                dry_run,
            };
            let opened = cmd_collect::open_provider(&ask)?;
            let wire = cmd_collect::wire_for(&ask, &opened);
            let answer = match cmd_collect::run(&ask, &opened, &wire, &clock) {
                Ok(answer) => answer,
                Err(refused) => {
                    let Some(facts) = robots_refusal(&refused) else {
                        return Err(refused.into());
                    };
                    emit_robots_refusal(&refused, &facts, &ask, format)?;
                    return Ok(Verdict::No(facts.exit));
                }
            };
            if answer.exit.carries_the_result_on_stdout() {
                emit_collect(&answer.envelope, format)?;
            } else {
                emit_collect_error(&answer, format)?;
            }
            Ok(match answer.exit.code() {
                0 => Verdict::Ok,
                code => Verdict::No(code),
            })
        }
        Command::Robots {
            action:
                RobotsAction::Refresh {
                    source,
                    hosts,
                    rules,
                    store,
                    dry_run,
                    non_interactive: _,
                    format,
                },
        } => {
            let registered = cmd_robots::registered(&source)?;
            let ask = cmd_robots::Ask {
                source,
                hosts,
                rules,
                store: vault_root(store),
                contact: contact(registered),
                dry_run,
            };
            let feed = cmd_robots::open(&ask)?;
            let wire = cmd_robots::wire_for(&ask, &feed);
            let answer = cmd_robots::run(&ask, &feed, &wire, &SystemClock)?;
            match answer.emitted(std::env::args().collect()) {
                cmd_robots::Emitted::Stdout(report) => emit_robots(&report, format)?,
                cmd_robots::Emitted::Stderr(error) => emit_robots_error(&error, format)?,
            }
            Ok(match answer.exit.code() {
                0 => Verdict::Ok,
                code => Verdict::No(code),
            })
        }
        Command::Items {
            at,
            store,
            from,
            to,
            source,
            limit,
            fields,
            per_story,
            format,
        } => {
            let ask = cmd_items::Ask {
                at: instant(&at, "--at"),
                from: from.map(|raw| date(&raw, "--from")),
                to: to.map(|raw| date(&raw, "--to")),
                source,
                limit,
                fields,
                per_story: per_story.chosen(),
            };
            let report = cmd_items::read(&vault_root(store), &ask)?;
            emit_items(&report, format)?;
            Ok(Verdict::Ok)
        }
        Command::Publish { at, store, format } => {
            let at = instant(&at, "--at");
            let root = vault_root(store);
            // The export is itself a run, named by the instant it was asked for, so a rebuild
            // appears in the archive as the run it was.
            let mut committer = Committer::open(&root, &RunId::new(stamped(at)))?;
            let segments = publish_run(&mut committer, &Fsync, ObservationTime::new(at))?;
            emit_published(&segments, format)?;
            Ok(Verdict::Ok)
        }
        Command::Day {
            date,
            lake,
            at,
            format,
        } => {
            let date: NaiveDate = date
                .parse()
                .map_err(|_| usage(format!("date {date:?} is not YYYY-MM-DD")))?;
            let cutoff = match at {
                None => None,
                Some(raw) => {
                    Some(Cutoff::new(raw.parse::<DateTime<Utc>>().map_err(|_| {
                        usage(format!("--at {raw:?} is not an RFC 3339 instant"))
                    })?))
                }
            };
            let reader = LakeReader::new(lake.unwrap_or_else(LakeReader::default_root));
            let mut day = reader.day(date)?;
            let before = day.items.len();
            if let Some(cutoff) = cutoff {
                day.items.retain(|i| i.visibility.visible_at(cutoff));
            }
            emit(&day, before, format)?;
            Ok(Verdict::Ok)
        }
    }
}

/// The User-Agent's contact address, from the chosen provider's own variable. It comes from the
/// environment and never from a flag, so it is not in a process listing; an unset variable reads
/// as empty and the provider refuses it at startup as `credential_contact_missing`.
fn contact(registered: cmd_collect::Registered) -> String {
    std::env::var(registered.contact_variable()).unwrap_or_default()
}

fn emit_collect(envelope: &serde_json::Value, format: Format) -> anyhow::Result<()> {
    let out = std::io::stdout();
    let mut out = out.lock();
    match format {
        Format::Json => {
            serde_json::to_writer_pretty(&mut out, envelope)?;
            writeln!(out)?;
        }
        Format::Human => cmd_collect::render_human(envelope, &mut out)?,
    }
    Ok(())
}

/// The error object for the exits that carry no result, with the suggestion as an argv ARRAY: an
/// agent should get a runnable next command and not a sentence to parse.
fn emit_collect_error(answer: &cmd_collect::Answer, format: Format) -> anyhow::Result<()> {
    let object = serde_json::json!({
        "schema": "newsvault.error/1",
        "code": answer.exit.error_code(),
        "message": "the run reached no usable payload",
        "retryable": answer.exit.retryable(),
        "details": answer.envelope,
        "suggestions": [std::env::args().collect::<Vec<String>>()],
    });
    let err = std::io::stderr();
    let mut err = err.lock();
    match format {
        Format::Json => writeln!(err, "{object}")?,
        Format::Human => writeln!(
            err,
            "error: {} (retryable {})",
            answer.exit.error_code().unwrap_or_default(),
            answer.exit.retryable()
        )?,
    }
    Ok(())
}

/// A collect refused by the robots check: section 5.1's code, its exit, the facts behind it, and
/// whether a `robots refresh` can change the answer (it cannot for a pinned body's own content).
struct RobotsRefusal {
    code: &'static str,
    exit: u8,
    details: serde_json::Value,
    refresh_helps: bool,
}

fn robots_refusal(failure: &CollectRunError) -> Option<RobotsRefusal> {
    let refused = |details, refresh_helps| RobotsRefusal {
        code: "robots_refused",
        exit: EXIT_USAGE,
        details,
        refresh_helps,
    };
    match failure {
        CollectRunError::RobotsUnverified {
            host,
            pinned,
            fetched,
        } => Some(RobotsRefusal {
            code: "robots_unverified",
            exit: EXIT_NOT_FOUND,
            details: serde_json::json!({"host": host, "pinned": pinned, "fetched": fetched}),
            refresh_helps: true,
        }),
        // A refresh records the changed digest again; a person editing the hosts file is the fix.
        CollectRunError::RobotsRefused {
            host,
            pinned,
            fetched,
        } => Some(refused(
            serde_json::json!({"host": host, "pinned": pinned, "fetched": fetched}),
            false,
        )),
        CollectRunError::RobotsUnreadable { host, pinned, why } => Some(refused(
            serde_json::json!({"host": host, "pinned": pinned, "why": why.to_string()}),
            false,
        )),
        CollectRunError::RobotsBlobAltered {
            host,
            pinned,
            found,
        } => Some(refused(
            serde_json::json!({"host": host, "pinned": pinned, "found": found}),
            false,
        )),
        CollectRunError::RobotsDisallowsPath { host, path } => Some(refused(
            serde_json::json!({"host": host, "path": path}),
            false,
        )),
        CollectRunError::PaceBelowFloor {
            host,
            per_host,
            floor,
        } => Some(refused(
            serde_json::json!({
                "host": host,
                "per_host_seconds": per_host.map(|interval| interval.as_secs_f64()),
                "floor_seconds": floor.as_secs_f64(),
            }),
            false,
        )),
        CollectRunError::CrawlDelayOverInterval {
            host,
            crawl_delay,
            interval,
        } => Some(refused(
            serde_json::json!({
                "host": host,
                "crawl_delay_seconds": crawl_delay.as_secs_f64(),
                "interval_seconds": interval.as_secs_f64(),
            }),
            false,
        )),
        _ => None,
    }
}

/// The refusal as an error object; its suggestion, when a refresh can change the answer, is the
/// refresh that would ledger the fetch.
fn emit_robots_refusal(
    refused: &CollectRunError,
    facts: &RobotsRefusal,
    ask: &cmd_collect::Ask,
    format: Format,
) -> anyhow::Result<()> {
    let shown = |path: &std::path::Path| path.display().to_string();
    let refresh = serde_json::json!([
        "newsvault",
        "robots",
        "refresh",
        "--source",
        open_web_feed::SOURCE,
        "--hosts",
        ask.hosts.as_deref().map(shown).unwrap_or_default(),
        "--rules",
        shown(&ask.rules),
        "--store",
        shown(&ask.store),
    ]);
    let object = serde_json::json!({
        "schema": "newsvault.error/1",
        "code": facts.code,
        "message": refused.to_string(),
        "retryable": false,
        "details": facts.details,
        "suggestions": if facts.refresh_helps { vec![refresh] } else { Vec::new() },
    });
    let err = std::io::stderr();
    let mut err = err.lock();
    match format {
        Format::Json => writeln!(err, "{object}")?,
        Format::Human => writeln!(err, "error: {refused}")?,
    }
    Ok(())
}

fn emit_robots(report: &serde_json::Value, format: Format) -> anyhow::Result<()> {
    let out = std::io::stdout();
    let mut out = out.lock();
    match format {
        Format::Json => {
            serde_json::to_writer_pretty(&mut out, report)?;
            writeln!(out)?;
        }
        Format::Human => cmd_robots::render_human(report, &mut out)?,
    }
    Ok(())
}

/// Exit 7, every row failed transiently: the report rides the error object, never stdout.
fn emit_robots_error(object: &serde_json::Value, format: Format) -> anyhow::Result<()> {
    let err = std::io::stderr();
    let mut err = err.lock();
    match format {
        Format::Json => writeln!(err, "{object}")?,
        Format::Human => writeln!(
            err,
            "error: {} (retryable {})",
            object["code"].as_str().unwrap_or_default(),
            object["retryable"]
        )?,
    }
    Ok(())
}

fn emit_items(report: &cmd_items::Report, format: Format) -> anyhow::Result<()> {
    let out = std::io::stdout();
    let mut out = out.lock();
    match format {
        Format::Json => {
            serde_json::to_writer_pretty(&mut out, report)?;
            writeln!(out)?;
        }
        Format::Human => cmd_items::render_human(report, &mut out)?,
    }
    Ok(())
}

fn stamped(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn emit_ledger(report: &Report, format: Format) -> anyhow::Result<()> {
    let out = std::io::stdout();
    let mut out = out.lock();
    match format {
        Format::Json => {
            serde_json::to_writer_pretty(&mut out, report)?;
            writeln!(out)?;
        }
        Format::Human => cmd_ledger::render_human(report, &mut out)?,
    }
    Ok(())
}

fn emit_published(
    segments: &[newsvault::store::PublishedSegment],
    format: Format,
) -> anyhow::Result<()> {
    let out = std::io::stdout();
    let mut out = out.lock();
    match format {
        Format::Json => {
            serde_json::to_writer_pretty(
                &mut out,
                &serde_json::json!({
                    "schema": "newsvault.publish/1",
                    "segments": segments.iter().map(|segment| serde_json::json!({
                        "segment_digest": segment.segment_digest,
                        "run_id": segment.run_id,
                        "valid_date": segment.valid_date,
                        "path": segment.path,
                        "seq_lo": segment.seq_lo,
                        "seq_hi": segment.seq_hi,
                        "rows": segment.rows,
                    })).collect::<Vec<serde_json::Value>>(),
                    "changed": !segments.is_empty(),
                }),
            )?;
            writeln!(out)?;
        }
        Format::Human => {
            for segment in segments {
                writeln!(
                    out,
                    "{} {} rows {} seq {}..{}",
                    segment.valid_date, segment.path, segment.rows, segment.seq_lo, segment.seq_hi
                )?;
            }
            writeln!(out, "segments         {}", segments.len())?;
        }
    }
    Ok(())
}

fn usage(message: String) -> anyhow::Error {
    usage_exit(message)
}

fn usage_exit(message: String) -> ! {
    #[allow(
        clippy::let_underscore_must_use,
        reason = "reporting a usage error must not fail on a closed stderr; EXIT_USAGE is the verdict"
    )]
    let _ = writeln!(std::io::stderr(), "usage error: {message}");
    std::process::exit(i32::from(EXIT_USAGE));
}

fn emit(day: &Day, before_cutoff: usize, format: Format) -> anyhow::Result<()> {
    let out = std::io::stdout();
    let mut out = out.lock();
    match format {
        Format::Json => {
            serde_json::to_writer_pretty(
                &mut out,
                &serde_json::json!({
                    "schema": "newsvault.day/1",
                    "date": day.date,
                    "runs": day.runs,
                    "raw_rows": day.raw_rows,
                    "distinct_items": before_cutoff,
                    "items_returned": day.items.len(),
                    "valid_time_conflict_events": day.valid_time_conflict_events,
                    "items_with_conflicting_valid_time": day.items_with_conflicting_valid_time,
                    "unusable_rows": day.unusable_rows,
                    // The two numbers a second reader of this archive is compared against. Both
                    // describe `items_returned`, so a day asked for with `--at` digests what the
                    // cutoff left.
                    "id_digest": day.id_digest(),
                    "content_digest": day.content_digest(),
                    "items": day.items,
                }),
            )?;
            writeln!(out)?;
        }
        Format::Human => {
            writeln!(out, "day              {}", day.date)?;
            writeln!(out, "run files        {}", day.runs.len())?;
            writeln!(out, "raw rows         {}", day.raw_rows)?;
            writeln!(out, "distinct items   {before_cutoff}")?;
            writeln!(out, "returned         {}", day.items.len())?;
            writeln!(
                out,
                "valid-time clash {} row event(s), {} item(s)",
                day.valid_time_conflict_events, day.items_with_conflicting_valid_time
            )?;
            writeln!(out, "unusable rows    {}", day.unusable_rows)?;
            writeln!(out, "id digest        {}", day.id_digest())?;
            writeln!(out, "content digest   {}", day.content_digest())?;
            if let (Some(first), Some(last)) = (day.items.first(), day.items.last()) {
                writeln!(
                    out,
                    "published        {} .. {}",
                    first.visibility.published(),
                    last.visibility.published()
                )?;
                let earliest = day
                    .items
                    .iter()
                    .map(|i| i.visibility.observed())
                    .min()
                    .map(|o| o.inner());
                if let Some(e) = earliest {
                    writeln!(out, "first sighting   {e}")?;
                }
            }
        }
    }
    Ok(())
}

fn print_schema(format: Format) {
    let doc = serde_json::json!({
        "schema": "newsvault.schema/1",
        "tool": "newsvault",
        "commands": [
            {
                "path": ["day"],
                "summary": "Every item the archive holds for one UTC day.",
                "args": [{"name": "date", "type": "date", "required": true}],
                "options": [
                    {"name": "lake", "type": "path"},
                    {"name": "at", "type": "rfc3339",
                     "doc": "keep only items visible at this instant (publication AND observation)"},
                    {"name": "format", "type": "enum", "values": ["human", "json"]}
                ],
                "output": "newsvault.day/1",
                "output_fields": [
                    {"name": "id_digest",
                     "doc": "sha256 over the newline-joined sorted distinct ids returned"},
                    {"name": "content_digest",
                     "doc": "sha256 over every field of every item returned, in id order, each \
                             length-prefixed; the construction is probes/newsvault_parity/PREREGISTRATION.md"}
                ],
                "mutates": false
            },
            {
                "path": ["ledger"],
                "summary": "Coverage over a day range, as of one instant.",
                "options": [
                    {"name": "at", "type": "rfc3339", "required": true,
                     "doc": "every age in the answer is computed against this instant"},
                    {"name": "from", "type": "date", "required": true},
                    {"name": "to", "type": "date", "required": true},
                    {"name": "store", "type": "path"},
                    {"name": "source", "type": "string"},
                    {"name": "expected_runs", "type": "integer",
                     "doc": "runs a day was due when its own envelopes are missing"},
                    {"name": "limit", "type": "integer",
                     "doc": "caps the rows within a day, never the number of days"},
                    {"name": "check", "type": "flag",
                     "doc": "exit 1 when the answer is no"},
                    {"name": "format", "type": "enum", "values": ["human", "json"]}
                ],
                "output": cmd_ledger::SCHEMA,
                "mutates": false
            },
            {
                "path": ["collect"],
                "summary": "Collects one window from one source into the archive.",
                "options": [
                    {"name": "source", "type": "enum", "required": true, "repeatable": true,
                     "values": cmd_collect::Registered::ALL.map(cmd_collect::Registered::source),
                     "doc": "one source per invocation: a second distinct source is invalid_argument"},
                    {"name": "window", "type": "string", "required": true,
                     "doc": "a day as YYYY-MM-DD, a FROM..TO range of days, or today: the UTC date \
                             of at, refused for edgar_press; the envelope's window is the resolved day"},
                    {"name": "schedule", "type": "string"},
                    {"name": "expected_runs", "type": "integer", "default": 1},
                    {"name": "store", "type": "path"},
                    {"name": "calendar", "type": "path", "required_for": ["edgar_press"],
                     "refused_for": ["open_web_wires"]},
                    {"name": "listing", "type": "path", "required_for": ["edgar_press"],
                     "refused_for": ["open_web_wires"]},
                    {"name": "roster", "type": "path", "required_for": ["edgar_press"],
                     "refused_for": ["open_web_wires"]},
                    {"name": "hosts", "type": "path", "required_for": ["open_web_wires"],
                     "refused_for": ["edgar_press"]},
                    {"name": "rules", "type": "path", "required": true,
                     "doc": "the chosen source's own rules file"},
                    {"name": "lag_policy", "type": "path", "refused_for": ["open_web_wires"],
                     "doc": "the measured observation lag per source and provenance; without it a \
                             window earlier than the run's own date is refused unplanned (R11)"},
                    {"name": "at", "type": "rfc3339",
                     "doc": "the instant the run plans against; defaults to now; for open_web_wires \
                             a run (not a dry run) whose at is not the clock's UTC day is refused"},
                    {"name": "dry_run", "type": "flag",
                     "doc": "plans and resolves every target and mutates nothing"},
                    {"name": "non_interactive", "type": "flag",
                     "doc": "accepted and does nothing: no command here prompts or reads stdin"},
                    {"name": "format", "type": "enum", "values": ["human", "json"]}
                ],
                "env": [{"name": "BELLWETHER_SEC_CONTACT", "source": "edgar_press",
                         "doc": "the contact address SEC requires in the User-Agent"},
                        {"name": "BELLWETHER_FEED_CONTACT", "source": "open_web_wires",
                         "doc": "the contact address the open-web User-Agent carries"}],
                "output": cmd_collect::SCHEMA,
                "mutates": true
            },
            {
                "path": ["items"],
                "summary": "Every item visible at one instant, ordered by seq.",
                "options": [
                    {"name": "at", "type": "rfc3339", "required": true,
                     "doc": "both the publication value and the observation must be at or before it"},
                    {"name": "store", "type": "path"},
                    {"name": "from", "type": "date"},
                    {"name": "to", "type": "date"},
                    {"name": "source", "type": "string"},
                    {"name": "limit", "type": "integer"},
                    {"name": "fields", "type": "list", "values": cmd_items::FIELDS},
                    {"name": "per_story", "type": "enum", "values": ["all", "latest"],
                     "default": "all",
                     "doc": "latest keeps the highest seq among the visible rows of one story"},
                    {"name": "format", "type": "enum", "values": ["human", "json"]}
                ],
                "output": cmd_items::SCHEMA,
                "mutates": false
            },
            {
                "path": ["publish"],
                "summary": "Exports every committed row no segment carries yet.",
                "options": [
                    {"name": "at", "type": "rfc3339", "required": true},
                    {"name": "store", "type": "path"},
                    {"name": "format", "type": "enum", "values": ["human", "json"]}
                ],
                "output": "newsvault.publish/1",
                "mutates": true
            },
            {
                "path": ["robots", "refresh"],
                "summary": "Fetches each hosts-file row's robots.txt under an attempt row and compares it with the row's pin.",
                "options": [
                    {"name": "source", "type": "enum", "required": true,
                     "values": [open_web_feed::SOURCE],
                     "doc": "edgar_press reads no hosts file and is invalid_argument"},
                    {"name": "hosts", "type": "path", "required": true,
                     "doc": "every row, admitted and candidate, is asked once; the file is never written"},
                    {"name": "rules", "type": "path", "required": true},
                    {"name": "store", "type": "path"},
                    {"name": "dry_run", "type": "flag",
                     "doc": "plans one url per row and mutates nothing"},
                    {"name": "non_interactive", "type": "flag",
                     "doc": "accepted and does nothing: no command here prompts or reads stdin"},
                    {"name": "format", "type": "enum", "values": ["human", "json"]}
                ],
                "env": [{"name": "BELLWETHER_FEED_CONTACT", "source": "open_web_wires",
                         "doc": "the contact address the open-web User-Agent carries"},
                        {"name": "BELLWETHER_VAULT",
                         "doc": "the store when --store is not given, then ~/.bellwether/vault"}],
                "output": cmd_robots::SCHEMA,
                "output_fields": [
                    {"name": "rows[].parse_verdict", "type": "enum",
                     "values": cmd_robots::parse_verdicts(),
                     "doc": "how collect would read the answered body: null for a failed fetch \
                             and a dry run; crawl_delay_over_interval and every refused_* value \
                             refuse an admitted host"}
                ],
                "exits": {
                    "0": "every row answered and every admitted pin matches",
                    "1": "every row answered and an admitted pin differs",
                    "2": "invalid_argument (a source with no hosts file), an unparseable hosts or \
                          rules file, credential_contact_missing, a store of another version or \
                          shape, io",
                    "4": "an absent hosts or rules file, or a hosts file admitting no host",
                    "7": "every row failed transiently (vendor_unreachable), or another run holds \
                          the lease (lease_held)",
                    "8": "a row failed"
                },
                "mutates": true
            },
            {"path": ["schema"], "summary": "This document.", "mutates": false}
        ],
        // Decision D1: `1` is the answer and never a failure. What each code means and which
        // command may emit it: architecture.md section 5.1.
        "exit_codes": {
            "0": "ok",
            "1": "the answer is no",
            "2": "usage, unparseable input, schema mismatch, io",
            "4": "not found",
            "7": "another run holds the lease (retryable)",
            "8": "the report is incomplete"
        },
        "error_codes": [
            {"code": "invalid_argument", "exit": 2, "retryable": false},
            {"code": "input_absent", "exit": 4, "retryable": false},
            {"code": "day_absent", "exit": 4, "retryable": false},
            {"code": "roster_absent", "exit": 4, "retryable": false},
            {"code": "input_unparseable", "exit": 2, "retryable": false},
            {"code": "input_schema_mismatch", "exit": 2, "retryable": false},
            {"code": "credential_contact_missing", "exit": 2, "retryable": false},
            {"code": "io_failed", "exit": 2, "retryable": false},
            {"code": "lease_held", "exit": 7, "retryable": true},
            {"code": "vendor_unreachable", "exit": 7, "retryable": true},
            {"code": "partial_collect", "exit": 8, "retryable": false},
            {"code": "vendor_refused_all", "exit": 8, "retryable": false},
            {"code": "commit_failed", "exit": 8, "retryable": false},
            {"code": "robots_unverified", "exit": 4, "retryable": false},
            {"code": "robots_refused", "exit": 2, "retryable": false}
        ],
        // Decision D1's companion rule, which `news_provider::Exit` holds as one function.
        "stdout_carries_the_result_for_exits": [0, 1, 8]
    });
    match format {
        Format::Human => println!("{doc:#}"),
        Format::Json => println!("{doc}"),
    }
}
