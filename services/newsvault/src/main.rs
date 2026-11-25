//! `newsvault` - command line over the point-in-time news archive.
//!
//! Follows the repo's CLI contract: the result on stdout and nothing else, logs and errors on
//! stderr, exactly one JSON value in JSON mode, and `schema` for a caller that is not human.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::{DateTime, NaiveDate, Utc};
use clap::{Parser, Subcommand, ValueEnum};

use news_provider::contact::{self, ContactError};
use news_provider::{Clock, PaceRoot, PaceRootError, RunId, SystemClock};
use newsvault::cmd_blobs::{self, BlobsError};
use newsvault::cmd_collect::{self, CollectRunError};
use newsvault::cmd_cut::{self, CutError};
use newsvault::cmd_items;
use newsvault::cmd_ledger::{self, Ask, LedgerReadError, Report};
use newsvault::cmd_pack;
use newsvault::cmd_robots::{self, RobotsError};
use newsvault::cmd_roster::{self, RosterError};
use newsvault::cmd_snapshot::{self, SnapshotError};
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
        /// `prometheus` writes node-exporter textfile metrics (newsvault.prom/1) for the watchdog.
        #[arg(long, value_enum, default_value_t = LedgerFormat::Human)]
        format: LedgerFormat,
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
        #[command(flatten)]
        pace: Pace,
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
        /// The last instant a request may be sent at (RFC 3339), after --at and, unless a dry
        /// run, after the clock's now. A request whose pace slot opens later is not asked and is
        /// recorded as deferred, with its host's remaining requests. Absent: open_web_wires stops
        /// at its schedule slot's deadline, even one already passed; edgar_press has none.
        #[arg(long)]
        deadline: Option<String>,
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
    /// The company roster: SEC's daily file, kept as received under the ledger.
    Roster {
        #[command(subcommand)]
        action: RosterAction,
    },
    /// The section 3.0 Snapshot for derive: every row visible at --at within a seq prefix, as
    /// <out>/<snapshot_id>/ with manifest.json and snapshot.parquet. Never rewrites one.
    Cut {
        /// The cutoff (RFC 3339), at microsecond precision and not after the clock's now.
        #[arg(long)]
        at: String,
        /// An existing directory the snapshot directory is created in. Required unless --dry-run.
        #[arg(long)]
        out: Option<PathBuf>,
        /// The seq bound, from an earlier manifest's seq_high_water, to rebuild that cut.
        /// Defaults to the store's highest seq.
        #[arg(long)]
        through_seq: Option<i64>,
        #[arg(long)]
        store: Option<PathBuf>,
        /// Selects and digests, reports the id and counts, reads no body and writes nothing.
        #[arg(long)]
        dry_run: bool,
        /// Accepted and does nothing: no command here prompts or reads stdin.
        #[arg(long)]
        non_interactive: bool,
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
    },
    /// One consistent copy of the commit log, for the daily backup. Writes only --out.
    Snapshot {
        /// The file the copy is written to; refused when it exists.
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        store: Option<PathBuf>,
        /// Checks the store, the lease and --out, and writes nothing.
        #[arg(long)]
        dry_run: bool,
        /// Accepted and does nothing: no command here prompts or reads stdin.
        #[arg(long)]
        non_interactive: bool,
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
    },
    /// The payload store: read every blob back, rewrite raw ones compressed, train a dictionary.
    Blobs {
        #[command(subcommand)]
        action: BlobsAction,
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
        #[command(flatten)]
        pace: Pace,
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

#[derive(Subcommand)]
enum RosterAction {
    /// Fetches each roster file once under an attempt row, after its host's robots.txt, and keeps
    /// the bytes and headers as received. Parses nothing into tables.
    Fetch {
        #[arg(long)]
        store: Option<PathBuf>,
        #[command(flatten)]
        pace: Pace,
        /// Plans one url per file and mutates nothing.
        #[arg(long)]
        dry_run: bool,
        /// Accepted and does nothing: no command here prompts or reads stdin.
        #[arg(long)]
        non_interactive: bool,
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
    },
    /// Per roster file, the newest copy visible at --at: its valid time (Last-Modified, else
    /// Date) and its observation time (received) both at or before it. Exit 1 when a file has none.
    Show {
        /// The cutoff (RFC 3339). Required: a read with no cutoff is not a point-in-time read.
        #[arg(long)]
        at: String,
        #[arg(long)]
        store: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
    },
}

#[derive(Subcommand)]
enum BlobsAction {
    /// Reads every blob back, decoded, against its digest, and every `blobs` row against the
    /// tree. Exit 1 when any does not match. Mutates nothing and takes no lease.
    Verify {
        #[arg(long)]
        store: Option<PathBuf>,
        /// Caps the failures listed. Never the blobs read or counted.
        #[arg(long, default_value_t = cmd_blobs::DEFAULT_FAILURES_SHOWN)]
        limit: usize,
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
    },
    /// Rewrites every raw blob as one zstd frame with the active dictionary, under the store's
    /// lease. A raw file is removed only after its compressed copy reads back to its digest.
    Compress {
        #[arg(long)]
        store: Option<PathBuf>,
        /// Compresses without a dictionary when none is active. Refused otherwise: a file
        /// written now keeps its dictionary for good.
        #[arg(long)]
        allow_no_dictionary: bool,
        /// Counts the raw blobs a run would rewrite and writes nothing.
        #[arg(long)]
        dry_run: bool,
        /// Accepted and does nothing: no command here prompts or reads stdin.
        #[arg(long)]
        non_interactive: bool,
        #[arg(long, default_value_t = cmd_blobs::DEFAULT_FAILURES_SHOWN)]
        limit: usize,
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
    },
    /// Writes the blobs and dictionaries a commit log names that no backup pack holds as one
    /// tar and its manifest. Exit 8 when a pack is lost or unrecorded, or the store could not
    /// give a member; a dry run exits 1 when the folder's packs do not hold everything the log
    /// names. Mutates no store.
    Pack {
        #[arg(long)]
        store: Option<PathBuf>,
        /// The commit log whose rows are packed: the backup's snapshot, or a restored copy.
        #[arg(long)]
        log: PathBuf,
        /// A file of `<pack> <bytes> <sha256>` lines: every pack the backup folder holds.
        #[arg(long)]
        have: PathBuf,
        /// A directory holding every manifest of the backup folder under its own file name.
        #[arg(long)]
        manifests: PathBuf,
        /// The tar to write, with its manifest beside it. Required unless a dry run.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Plans and writes nothing: `packed` counts what a run would pack.
        #[arg(long)]
        dry_run: bool,
        /// Accepted and does nothing: no command here prompts or reads stdin.
        #[arg(long)]
        non_interactive: bool,
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
    },
    /// zstd dictionaries, kept in the store and never deleted.
    Dictionary {
        #[command(subcommand)]
        action: DictionaryAction,
    },
}

#[derive(Subcommand)]
enum DictionaryAction {
    /// Trains a dictionary on stored payloads, stores it under its digest and activates it.
    Train {
        #[arg(long)]
        store: Option<PathBuf>,
        /// Payloads trained on, the first in digest order, each cut to its first 128 KiB.
        #[arg(long, default_value_t = cmd_blobs::DEFAULT_SAMPLES)]
        samples: usize,
        /// The payloads after the samples, compressed with and without the dictionary.
        #[arg(long, default_value_t = cmd_blobs::DEFAULT_HOLDOUT)]
        holdout: usize,
        /// Trains and measures on the held-out payloads; stores and activates nothing.
        #[arg(long)]
        dry_run: bool,
        /// Accepted and does nothing: no command here prompts or reads stdin.
        #[arg(long)]
        non_interactive: bool,
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
    },
}

/// The option of every command that builds a `Wire` (architecture.md section 12.2).
#[derive(clap::Args)]
struct Pace {
    /// The existing absolute directory whose pace/ holds this deployable's pace files, shared by
    /// every command that sends. Defaults to the store. Refused while it is not the store and the
    /// store's own pace/ still holds a file.
    #[arg(long)]
    pace_root: Option<PathBuf>,
}

impl Pace {
    /// A refusal is written to stderr here, stdout left empty, and is the exit.
    fn resolve(&self, store: &Path, format: Format) -> PaceRoot {
        let refused = match PaceRoot::resolve(self.pace_root.as_deref(), store) {
            Ok(root) => return root,
            Err(refused) => refused,
        };
        let object = serde_json::json!({
            "schema": "newsvault.error/1",
            "code": refused.code(),
            "message": refused.to_string(),
            "retryable": false,
            "details": {
                "store": store.display().to_string(),
                "pace_root": self.pace_root.as_ref().map(|root| root.display().to_string()),
                "files": match &refused {
                    PaceRootError::Split { files, .. } => files.clone(),
                    _ => Vec::new(),
                },
            },
            "suggestions": [],
        });
        #[allow(
            clippy::let_underscore_must_use,
            reason = "reporting a refusal must not fail on a closed stderr; the exit code is the verdict"
        )]
        let _ = match format {
            Format::Json => writeln!(std::io::stderr(), "{object}"),
            Format::Human => writeln!(std::io::stderr(), "error: {refused} ({})", refused.code()),
        };
        std::process::exit(i32::from(refused.exit()));
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Human,
    Json,
}

/// The ledger alone has a third rendering: its counts as Prometheus textfile metrics.
#[derive(Clone, Copy, ValueEnum)]
enum LedgerFormat {
    Human,
    Json,
    Prometheus,
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
        || failure
            .downcast_ref::<ContactError>()
            .is_some_and(ContactError::is_usage)
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
    if let Some(roster) = failure.downcast_ref::<RosterError>() {
        return roster.exit();
    }
    if let Some(refused) = failure.downcast_ref::<ContactError>() {
        return match refused {
            ContactError::FileAbsent { .. } => EXIT_NOT_FOUND,
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
            emit_ledger(&report, ask.at, format)?;
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
            pace,
            calendar,
            listing,
            roster,
            hosts,
            rules,
            lag_policy,
            at,
            deadline,
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
                contact: contact(registered)?,
                at: match at {
                    Some(raw) => instant(&raw, "--at"),
                    None => clock.now(),
                },
                deadline: deadline.map(|raw| instant(&raw, "--deadline")),
                dry_run,
            };
            let pace_root = pace.resolve(&ask.store, format);
            let opened = cmd_collect::open_provider(&ask)?;
            let wire = cmd_collect::wire_for(&ask, &opened, &pace_root);
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
                    pace,
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
                contact: contact(registered)?,
                dry_run,
            };
            let pace_root = pace.resolve(&ask.store, format);
            let feed = cmd_robots::open(&ask)?;
            let wire = cmd_robots::wire_for(&ask, &feed, &pace_root);
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
        Command::Roster { action } => run_roster(action),
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
        Command::Snapshot {
            out,
            store,
            dry_run,
            non_interactive: _,
            format,
        } => {
            let ask = cmd_snapshot::Ask {
                store: vault_root(store),
                out,
                dry_run,
            };
            match cmd_snapshot::run(&ask) {
                Ok(snapshot) => {
                    emit_snapshot(&snapshot, format)?;
                    Ok(Verdict::Ok)
                }
                Err(refused) => {
                    emit_snapshot_error(&refused, &ask, format)?;
                    Ok(Verdict::No(refused.exit()))
                }
            }
        }
        Command::Cut {
            at,
            out,
            through_seq,
            store,
            dry_run,
            non_interactive: _,
            format,
        } => {
            let ask = cmd_cut::Ask {
                store: vault_root(store),
                out,
                at: instant(&at, "--at"),
                now: SystemClock.now(),
                through_seq,
                dry_run,
                busy_wait: cmd_cut::BUSY_WAIT,
            };
            match cmd_cut::run(&ask, &Fsync) {
                Ok(report) => {
                    emit_cut(&report, format)?;
                    Ok(Verdict::Ok)
                }
                Err(refused) => {
                    emit_cut_error(&refused, format)?;
                    Ok(Verdict::No(refused.exit()))
                }
            }
        }
        Command::Blobs { action } => run_blobs(action),
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

/// The User-Agent's contact address, from the chosen provider's own variable or the file its
/// `_FILE` twin names; never from a flag, so it is not in a process listing. Neither set reads as
/// empty, which the provider refuses at startup as `credential_contact_missing`.
fn contact(registered: cmd_collect::Registered) -> Result<String, ContactError> {
    contact::read(
        registered.contact_variable(),
        registered.contact_file_variable(),
        |name| std::env::var_os(name),
    )
}

/// `roster fetch` reads the feed's contact: the one file the VPS mounts, which SEC has answered.
fn run_roster(action: RosterAction) -> anyhow::Result<Verdict> {
    match action {
        RosterAction::Fetch {
            store,
            pace,
            dry_run,
            non_interactive: _,
            format,
        } => {
            let ask = cmd_roster::Ask {
                store: vault_root(store),
                contact: contact(cmd_collect::Registered::OpenWebWires)?,
                dry_run,
            };
            let pace_root = pace.resolve(&ask.store, format);
            let wire = cmd_roster::wire_for(&ask, &pace_root);
            let answer = cmd_roster::run(&ask, &wire, &SystemClock)?;
            match answer.error_object(std::env::args().collect()) {
                None => emit_value(&answer.report, format, cmd_roster::render_human)?,
                Some(error) => emit_robots_error(&error, format)?,
            }
            Ok(match answer.exit.code() {
                0 => Verdict::Ok,
                code => Verdict::No(code),
            })
        }
        RosterAction::Show { at, store, format } => {
            let shown = cmd_roster::show(&vault_root(store), instant(&at, "--at"))?;
            emit_value(&shown.report, format, cmd_roster::render_show_human)?;
            Ok(if shown.every_file_visible {
                Verdict::Ok
            } else {
                Verdict::No(EXIT_NO)
            })
        }
    }
}

fn emit_value(
    report: &serde_json::Value,
    format: Format,
    human: fn(&serde_json::Value, &mut std::io::StdoutLock<'static>) -> std::io::Result<()>,
) -> anyhow::Result<()> {
    let mut out = std::io::stdout().lock();
    match format {
        Format::Json => {
            serde_json::to_writer_pretty(&mut out, report)?;
            writeln!(out)?;
        }
        Format::Human => human(report, &mut out)?,
    }
    Ok(())
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

fn emit_snapshot(snapshot: &cmd_snapshot::Snapshot, format: Format) -> anyhow::Result<()> {
    let out = std::io::stdout();
    let mut out = out.lock();
    match format {
        Format::Json => {
            serde_json::to_writer_pretty(&mut out, snapshot)?;
            writeln!(out)?;
        }
        Format::Human => writeln!(
            out,
            "{} {} user_version {} bytes {}{}",
            snapshot.store,
            snapshot.out,
            snapshot.user_version,
            snapshot.bytes,
            if snapshot.dry_run { " (dry run)" } else { "" }
        )?,
    }
    Ok(())
}

/// A refusal on stderr with stdout empty; a retryable one suggests the same argv again.
fn emit_snapshot_error(
    refused: &SnapshotError,
    ask: &cmd_snapshot::Ask,
    format: Format,
) -> anyhow::Result<()> {
    let object = serde_json::json!({
        "schema": "newsvault.error/1",
        "code": refused.code(),
        "message": refused.to_string(),
        "retryable": refused.retryable(),
        "details": {"store": ask.store.display().to_string(), "out": ask.out.display().to_string()},
        "suggestions": if refused.retryable() {
            vec![std::env::args().collect::<Vec<String>>()]
        } else {
            Vec::new()
        },
    });
    let err = std::io::stderr();
    let mut err = err.lock();
    match format {
        Format::Json => writeln!(err, "{object}")?,
        Format::Human => writeln!(err, "error: {refused} ({})", refused.code())?,
    }
    Ok(())
}

fn emit_cut(report: &cmd_cut::Report, format: Format) -> anyhow::Result<()> {
    let out = std::io::stdout();
    let mut out = out.lock();
    match format {
        Format::Json => {
            serde_json::to_writer_pretty(&mut out, report)?;
            writeln!(out)?;
        }
        Format::Human => cmd_cut::render_human(report, &mut out)?,
    }
    Ok(())
}

/// A refusal on stderr with stdout empty. No suggestion: `lease_held` is the same argv again
/// (`retryable`), and `staging_left` is a removal the owner decides.
fn emit_cut_error(refused: &CutError, format: Format) -> anyhow::Result<()> {
    let object = serde_json::json!({
        "schema": "newsvault.error/1",
        "code": refused.code(),
        "message": refused.to_string(),
        "retryable": refused.retryable(),
        "details": refused.details(),
        "suggestions": Vec::<Vec<String>>::new(),
    });
    let err = std::io::stderr();
    let mut err = err.lock();
    match format {
        Format::Json => writeln!(err, "{object}")?,
        Format::Human => writeln!(err, "error: {refused} ({})", refused.code())?,
    }
    Ok(())
}

fn run_blobs(action: BlobsAction) -> anyhow::Result<Verdict> {
    let format_of = |action: &BlobsAction| match action {
        BlobsAction::Verify { format, .. }
        | BlobsAction::Compress { format, .. }
        | BlobsAction::Pack { format, .. }
        | BlobsAction::Dictionary {
            action: DictionaryAction::Train { format, .. },
        } => *format,
    };
    let format = format_of(&action);
    let answered = match action {
        BlobsAction::Verify { store, limit, .. } => cmd_blobs::verify(&vault_root(store), limit)
            .map(|report| {
                let sound = report.is_sound();
                let indexed = report.indexed.map_or_else(
                    || " indexed -".to_owned(),
                    |indexed| {
                        format!(
                            " indexed rows {} absent {} length differs {}",
                            indexed.rows, indexed.absent, indexed.length_differs
                        )
                    },
                );
                let shown: String = report
                    .failures
                    .iter()
                    .map(|failure| format!("\nfailure {} {}", failure.code, failure.blob))
                    .collect();
                (
                    serde_json::to_value(&report),
                    format!(
                        "{} blobs {} verified {} failed {} listing failed {} both copies {} \
                         staging leftovers {}{indexed}{shown}",
                        report.store,
                        report.blobs,
                        report.verified,
                        report.failed,
                        report.listing_failed,
                        report.both_copies,
                        report.staging_leftovers,
                    ),
                    if sound {
                        Verdict::Ok
                    } else {
                        Verdict::No(EXIT_NO)
                    },
                )
            }),
        BlobsAction::Compress {
            store,
            allow_no_dictionary,
            dry_run,
            limit,
            ..
        } => cmd_blobs::compress(&cmd_blobs::CompressAsk {
            store: vault_root(store),
            at: SystemClock.now(),
            allow_no_dictionary,
            dry_run,
            limit,
        })
        .map(|report| {
            (
                serde_json::to_value(&report),
                format!(
                    "{} raw {} compressed {} replaced {} staging removed {} failed {} bytes {} -> {}{}",
                    report.store,
                    report.raw_found,
                    report.compressed,
                    report.replaced,
                    report.staging_removed,
                    report.failed,
                    report.raw_bytes,
                    report.compressed_stored_bytes,
                    if report.dry_run { " (dry run)" } else { "" }
                ),
                if report.is_complete() {
                    Verdict::Ok
                } else {
                    Verdict::No(EXIT_INCOMPLETE)
                },
            )
        }),
        BlobsAction::Pack {
            store,
            log,
            have,
            manifests,
            out,
            dry_run,
            ..
        } => {
            if out.is_none() && !dry_run {
                usage_exit("--out is required unless --dry-run".to_owned());
            }
            let ask = cmd_pack::Ask {
                store: vault_root(store),
                log,
                have,
                manifests,
                out: out.filter(|_| !dry_run),
            };
            return run_pack(&ask, format);
        }
        BlobsAction::Dictionary {
            action:
                DictionaryAction::Train {
                    store,
                    samples,
                    holdout,
                    dry_run,
                    ..
                },
        } => cmd_blobs::train(&cmd_blobs::TrainAsk {
            store: vault_root(store),
            at: SystemClock.now(),
            samples,
            holdout,
            dry_run,
        })
        .map(|report| {
            (
                serde_json::to_value(&report),
                format!(
                    "{} dictionary {} ({} B) held-out ratio {:.2} without, {:.2} with{}",
                    report.store,
                    report.dictionary,
                    report.dictionary_bytes,
                    report.holdout.ratio_without,
                    report.holdout.ratio_with,
                    if report.dry_run { " (dry run)" } else { "" }
                ),
                Verdict::Ok,
            )
        }),
    };
    match answered {
        Ok((object, line, verdict)) => {
            let out = std::io::stdout();
            let mut out = out.lock();
            match format {
                Format::Json => {
                    serde_json::to_writer_pretty(&mut out, &object?)?;
                    writeln!(out)?;
                }
                Format::Human => writeln!(out, "{line}")?,
            }
            Ok(verdict)
        }
        Err(refused) => {
            emit_blobs_error(&refused, format)?;
            Ok(Verdict::No(refused.exit()))
        }
    }
}

/// Exit 8 with the report on stdout when a pack is lost or unrecorded, or the store could not
/// give a member, each such member also one line on stderr: the pack asked for is written all
/// the same. A dry run that would pack anything answers no, exit 1, so a restore reads the
/// folder's verdict from the status. A refusal goes to stderr with stdout empty.
fn run_pack(ask: &cmd_pack::Ask, format: Format) -> anyhow::Result<Verdict> {
    match cmd_pack::run(ask) {
        Ok(report) => {
            let out = std::io::stdout();
            let mut out = out.lock();
            match format {
                Format::Json => {
                    serde_json::to_writer_pretty(&mut out, &report)?;
                    writeln!(out)?;
                }
                Format::Human => writeln!(
                    out,
                    "{} named {} held {} packed {} pack {} bytes {} lost [{}] unrecorded [{}] \
                     not packed [{}]{}",
                    report.log,
                    report.named,
                    report.held,
                    report.packed,
                    report.pack.as_deref().unwrap_or("-"),
                    report.pack_bytes,
                    report.lost.join(" "),
                    report.unrecorded.join(" "),
                    report
                        .not_packed
                        .iter()
                        .map(|left| format!("{} {} {}", left.kind, left.digest, left.reason))
                        .collect::<Vec<_>>()
                        .join(", "),
                    if report.dry_run { " (dry run)" } else { "" }
                )?,
            }
            for left in report.not_packed.iter().filter(|_| !report.dry_run) {
                let details = serde_json::json!({
                    "store": report.store, "log": report.log, "kind": left.kind,
                    "digest": left.digest, "reason": left.reason,
                });
                emit_pack_error(left.code(), &left.message, &details, format)?;
            }
            Ok(if report.dry_run && report.packed > 0 {
                Verdict::No(EXIT_NO)
            } else if report.is_whole() {
                Verdict::Ok
            } else {
                Verdict::No(EXIT_INCOMPLETE)
            })
        }
        Err(refused) => {
            let details = serde_json::json!({
                "store": ask.store.display().to_string(), "log": ask.log.display().to_string(),
            });
            emit_pack_error(refused.code(), &refused.to_string(), &details, format)?;
            Ok(Verdict::No(refused.exit()))
        }
    }
}

fn emit_pack_error(
    code: &str,
    message: &str,
    details: &serde_json::Value,
    format: Format,
) -> anyhow::Result<()> {
    let err = std::io::stderr();
    let mut err = err.lock();
    match format {
        Format::Json => writeln!(
            err,
            "{}",
            serde_json::json!({
                "schema": "newsvault.error/1",
                "code": code,
                "message": message,
                "retryable": false,
                "details": details,
                "suggestions": Vec::<Vec<String>>::new(),
            })
        )?,
        Format::Human => writeln!(err, "error: {message} ({code})")?,
    }
    Ok(())
}

/// A refusal on stderr with stdout empty; a retryable one suggests the same argv again.
fn emit_blobs_error(refused: &BlobsError, format: Format) -> anyhow::Result<()> {
    let object = serde_json::json!({
        "schema": "newsvault.error/1",
        "code": refused.code(),
        "message": refused.to_string(),
        "retryable": refused.retryable(),
        "details": {},
        "suggestions": if refused.retryable() {
            vec![std::env::args().collect::<Vec<String>>()]
        } else {
            Vec::new()
        },
    });
    let err = std::io::stderr();
    let mut err = err.lock();
    match format {
        Format::Json => writeln!(err, "{object}")?,
        Format::Human => writeln!(err, "error: {refused} ({})", refused.code())?,
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

fn emit_ledger(report: &Report, at: DateTime<Utc>, format: LedgerFormat) -> anyhow::Result<()> {
    let out = std::io::stdout();
    let mut out = out.lock();
    match format {
        LedgerFormat::Json => {
            serde_json::to_writer_pretty(&mut out, report)?;
            writeln!(out)?;
        }
        LedgerFormat::Human => cmd_ledger::render_human(report, &mut out)?,
        LedgerFormat::Prometheus => cmd_ledger::render_prometheus(report, at, &mut out)?,
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

/// One text for both `_FILE` variables, so the two cannot describe one rule two ways.
const PACE_ROOT_DOC: &str = "an existing absolute directory whose pace/ holds the pace files of \
    every command of the deployable that sends; defaults to the store; relative or not a \
    directory: invalid_argument (2); absent: input_absent (4); not the store while the store's \
    own pace/ holds a file: pace_root_split (2)";
const CONTACT_FILE_DOC: &str = "a file holding the contact instead, one trailing newline removed; \
    set with its variable: invalid_argument (2); absent: input_absent (4); unreadable or not UTF-8: \
    io_failed (2); empty: credential_contact_missing (2); a control character left: \
    input_unparseable (2)";

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
                    {"name": "format", "type": "enum", "values": ["human", "json", "prometheus"],
                     "doc": "prometheus: node-exporter textfile metrics, schema newsvault.prom/1, \
                             on stdout for the same exits 0, 1 and 8"}
                ],
                "output": cmd_ledger::SCHEMA,
                "outputs": {"json": cmd_ledger::SCHEMA, "prometheus": cmd_ledger::PROMETHEUS_SCHEMA},
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
                    {"name": "pace_root", "type": "path", "doc": PACE_ROOT_DOC},
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
                             a run (not a dry run) whose at is not the clock's UTC day, or is \
                             after the clock's now, is refused"},
                    {"name": "deadline", "type": "rfc3339",
                     "doc": "the last instant a request may be sent at, after at and, unless \
                             dry_run, after the clock's now (else invalid_argument); a request whose pace slot opens later is not \
                             asked and is a deferred row (run_deadline, or host_held when a \
                             server's hold or slow-down moved it), with the rest of its host's \
                             queue; absent, open_web_wires takes min(slot start + period / 2, \
                             next slot) - 30 s of the schedule slot the clock's now is in, one \
                             already passed included, a dry run's too whatever its at, and \
                             edgar_press takes none"},
                    {"name": "dry_run", "type": "flag",
                     "doc": "plans and resolves every target and mutates nothing"},
                    {"name": "non_interactive", "type": "flag",
                     "doc": "accepted and does nothing: no command here prompts or reads stdin"},
                    {"name": "format", "type": "enum", "values": ["human", "json"]}
                ],
                "env": [{"name": "BELLWETHER_SEC_CONTACT", "source": "edgar_press",
                         "doc": "the contact address SEC requires in the User-Agent"},
                        {"name": "BELLWETHER_SEC_CONTACT_FILE", "source": "edgar_press",
                         "doc": CONTACT_FILE_DOC},
                        {"name": "BELLWETHER_FEED_CONTACT", "source": "open_web_wires",
                         "doc": "the contact address the open-web User-Agent carries"},
                        {"name": "BELLWETHER_FEED_CONTACT_FILE", "source": "open_web_wires",
                         "doc": CONTACT_FILE_DOC}],
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
                    {"name": "pace_root", "type": "path", "doc": PACE_ROOT_DOC},
                    {"name": "dry_run", "type": "flag",
                     "doc": "plans one url per row and mutates nothing"},
                    {"name": "non_interactive", "type": "flag",
                     "doc": "accepted and does nothing: no command here prompts or reads stdin"},
                    {"name": "format", "type": "enum", "values": ["human", "json"]}
                ],
                "env": [{"name": "BELLWETHER_FEED_CONTACT", "source": "open_web_wires",
                         "doc": "the contact address the open-web User-Agent carries"},
                        {"name": "BELLWETHER_FEED_CONTACT_FILE", "source": "open_web_wires",
                         "doc": CONTACT_FILE_DOC},
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
            {
                "path": ["roster", "fetch"],
                "summary": "Fetches each roster file (SEC's company_tickers_exchange.json) once under an attempt row of source roster, after its host's robots.txt under one of source robots, and keeps the bytes and headers as an L2 payload. No item rows.",
                "options": [
                    {"name": "store", "type": "path"},
                    {"name": "pace_root", "type": "path", "doc": PACE_ROOT_DOC},
                    {"name": "dry_run", "type": "flag",
                     "doc": "plans one url per file and mutates nothing"},
                    {"name": "non_interactive", "type": "flag",
                     "doc": "accepted and does nothing: no command here prompts or reads stdin"},
                    {"name": "format", "type": "enum", "values": ["human", "json"]}
                ],
                "env": [{"name": "BELLWETHER_FEED_CONTACT",
                         "doc": "the contact address the User-Agent carries"},
                        {"name": "BELLWETHER_FEED_CONTACT_FILE", "doc": CONTACT_FILE_DOC},
                        {"name": "BELLWETHER_VAULT",
                         "doc": "the store when --store is not given, then ~/.bellwether/vault"}],
                "output": cmd_roster::SCHEMA,
                "output_fields": [
                    {"name": "files[].not_asked", "type": "enum",
                     "values": ["robots_unverified", "robots_refused", "robots_disallowed"],
                     "doc": "null when asked; no attempt row when set"},
                    {"name": "files[].raw_blob",
                     "doc": "sha256 of the body as received: the file's universe_version"}
                ],
                "exits": {
                    "0": "every file answered usable",
                    "2": "credential_contact_missing, a store of another version, io",
                    "7": "every fetch failed transiently (vendor_unreachable), or another run holds the lease (lease_held)",
                    "8": "a file failed, was empty, was another shape, or was not asked"
                },
                "mutates": true
            },
            {
                "path": ["roster", "show"],
                "summary": "Per roster file, the newest usable copy visible at at: valid time (Last-Modified, else Date) and observation time (received_at) both at or before it, and the newest attempt opened by then, its outcome and verdict only once closed by then (else in_flight true).",
                "options": [
                    {"name": "at", "type": "rfc3339", "required": true},
                    {"name": "store", "type": "path"},
                    {"name": "format", "type": "enum", "values": ["human", "json"]}
                ],
                "env": [{"name": "BELLWETHER_VAULT",
                         "doc": "the store when --store is not given, then ~/.bellwether/vault"}],
                "output": cmd_roster::SHOW_SCHEMA,
                "exits": {
                    "0": "every roster file has a copy visible at at",
                    "1": "the answer is no: a file has none",
                    "2": "input_schema_mismatch, input_unparseable (a stored time or header), io_failed",
                    "4": "input_absent: no commit log"
                },
                "mutates": false
            },
            {
                "path": ["cut"],
                "summary": "The section 3.0 Snapshot for derive: every row of the seq prefix through seq_high_water that is visible at at, written once as out/<snapshot_id>/ with manifest.json (snapshot.manifest/1) and snapshot.parquet (snapshot.data/1, ZSTD), staged, fsynced, validated by snapshot-contract and renamed. Not the backup copy: that is snapshot.",
                "options": [
                    {"name": "at", "type": "rfc3339", "required": true,
                     "doc": "the cutoff; microsecond precision; after the clock's now is invalid_argument"},
                    {"name": "out", "type": "path", "required_unless": "dry_run",
                     "doc": "an existing directory outside the store; the cut is out/<snapshot_id>/ and a standing one that validates is left as it is (changed false, its own manifest's seq_high_water, items and provenance_counts reported)"},
                    {"name": "through_seq", "type": "integer",
                     "doc": "the seq bound from an earlier manifest's seq_high_water; defaults to the store's highest seq; below 1 or above it is invalid_argument"},
                    {"name": "store", "type": "path"},
                    {"name": "dry_run", "type": "flag",
                     "doc": "selects and digests, reports snapshot_id, the counts and body_bytes, reads no body and writes nothing"},
                    {"name": "non_interactive", "type": "flag",
                     "doc": "accepted and does nothing: no command here prompts or reads stdin"},
                    {"name": "format", "type": "enum", "values": ["human", "json"]}
                ],
                "env": [{"name": "BELLWETHER_VAULT",
                         "doc": "the store when --store is not given, then ~/.bellwether/vault"}],
                "output": cmd_cut::SCHEMA,
                "output_fields": [
                    {"name": "open_attempts",
                     "doc": "attempts opened at or before at, neither closed nor abandoned at the read: a later cut at the same at without through_seq may hold more rows"}
                ],
                "exits": {
                    "0": "written, or already standing and admissible (changed false), or a dry run",
                    "2": "invalid_argument (--out inside the store included), input_schema_mismatch, input_unparseable (a stored time or an attempt's opened_at, a body not UTF-8 or not hashing to its digest, a staged or standing directory that does not validate), staging_left (details.path: an earlier cut did not finish), io_failed",
                    "4": "input_absent (no commit log, a selected body or its dictionary), provenance_unrecorded, nothing_visible",
                    "7": "lease_held (a writer kept the commit log locked past 30 s; retryable)"
                },
                "mutates": true
            },
            {
                "path": ["snapshot"],
                "summary": "One consistent copy of the commit log (SQLite online backup, one step, under the store's lease), checked by PRAGMA quick_check.",
                "options": [
                    {"name": "out", "type": "path", "required": true,
                     "doc": "written as a hidden partial beside it and renamed once checked; an existing file is refused"},
                    {"name": "store", "type": "path"},
                    {"name": "dry_run", "type": "flag",
                     "doc": "checks the store, the lease and out, reports the source's user_version and size, writes nothing"},
                    {"name": "non_interactive", "type": "flag",
                     "doc": "accepted and does nothing: no command here prompts or reads stdin"},
                    {"name": "format", "type": "enum", "values": ["human", "json"]}
                ],
                "env": [{"name": "BELLWETHER_VAULT",
                         "doc": "the store when --store is not given, then ~/.bellwether/vault"}],
                "output": cmd_snapshot::SCHEMA,
                "exits": {
                    "0": "the copy is at out",
                    "2": "invalid_argument (out exists), input_unparseable (the copy fails quick_check), io_failed",
                    "4": "input_absent: no commit log, or out's directory does not exist",
                    "7": "lease_held: a writer holds the store"
                },
                "mutates": true
            },
            {
                "path": ["blobs", "verify"],
                "summary": "Reads every blob back (raw or zstd, any dictionary) against its sha256, and every blobs row against the tree. The real-path reader of BlobStore::stream, the bulk read a re-extraction pass makes.",
                "options": [
                    {"name": "store", "type": "path"},
                    {"name": "limit", "type": "integer", "default": cmd_blobs::DEFAULT_FAILURES_SHOWN,
                     "doc": "caps the failures listed, never the blobs read or counted"},
                    {"name": "format", "type": "enum", "values": ["human", "json"]}
                ],
                "env": [{"name": "BELLWETHER_VAULT",
                         "doc": "the store when --store is not given, then ~/.bellwether/vault"}],
                "output": cmd_blobs::VERIFY_SCHEMA,
                "exits": {
                    "0": "every blob reads back to its digest and every blobs row names a file of its length",
                    "1": "the answer is no: failed, listing_failed, indexed.absent or indexed.length_differs is not 0",
                    "2": "input_schema_mismatch, io_failed",
                    "4": "input_absent: no store directory"
                },
                "mutates": false
            },
            {
                "path": ["blobs", "compress"],
                "summary": "Rewrites every raw blob as one zstd frame with the active dictionary, under the store's LOCK lease; first removes staging files a crash left; removes a raw file only after the compressed copy is renamed, fsynced and read back to the digest, and rewrites a compressed copy left beside it that does not read back. Idempotent and resumable. The backup's blobs pack does not take LOCK: run it under the host lock the systemd units take.",
                "options": [
                    {"name": "store", "type": "path"},
                    {"name": "allow_no_dictionary", "type": "flag",
                     "doc": "compress with no dictionary when none is active; refused otherwise"},
                    {"name": "dry_run", "type": "flag",
                     "doc": "counts the raw blobs and their bytes, names the dictionary, writes nothing and takes no lease"},
                    {"name": "non_interactive", "type": "flag",
                     "doc": "accepted and does nothing: no command here prompts or reads stdin"},
                    {"name": "limit", "type": "integer", "default": cmd_blobs::DEFAULT_FAILURES_SHOWN},
                    {"name": "format", "type": "enum", "values": ["human", "json"]}
                ],
                "output": cmd_blobs::COMPRESS_SCHEMA,
                "exits": {
                    "0": "every raw blob is compressed, or none was left (changed false)",
                    "2": "invalid_argument (no active dictionary without --allow-no-dictionary), input_schema_mismatch, io_failed",
                    "4": "input_absent: no commit log, or the active dictionary's file",
                    "7": "lease_held",
                    "8": "a blob failed; it keeps its raw file and is listed in failures"
                },
                "mutates": true
            },
            {
                "path": ["blobs", "pack"],
                "summary": "Writes the dictionaries and blobs the commit log at log names that no pack of the backup folder holds as one tar of store-relative paths (blobs/dictionaries/<sha256>, blobs/sha256/..), each file as stored and only after it read back to its digest, and then out.manifest: `pack <name> <bytes> <sha256 of the tar>`, one `dictionary <sha256>` or `blob <sha256>` line per member, `end <members>`. A manifest counts only when it is whole, is named <pack>.manifest and have lists its pack at its length and sha256; any other names its pack in lost and its members are packed again. A member that is not in the store, or does not read back to its digest, is left out, named in not_packed and by one error line on stderr, and every other member is packed; it is in no manifest, so every later run tries it again. Reads the store without the lease and writes only out and out.manifest.",
                "options": [
                    {"name": "store", "type": "path"},
                    {"name": "log", "type": "path", "required": true,
                     "doc": "the commit log whose blobs and dictionaries rows are packed"},
                    {"name": "have", "type": "path", "required": true,
                     "doc": "a file of `<pack file name> <bytes> <sha256>` lines, one per pack the folder holds"},
                    {"name": "manifests", "type": "path", "required": true,
                     "doc": "a directory holding every manifest of the folder under its own file name, <pack file name>.manifest"},
                    {"name": "out", "type": "path",
                     "doc": "the tar; required unless dry_run; refused when it or its manifest exists; neither is written when nothing is left to pack or the store gives no member"},
                    {"name": "dry_run", "type": "flag",
                     "doc": "reads log, have and manifests only and writes nothing, out given or not: packed is what a run would pack, not_packed names each such member with the reason dry_run, and above 0 the exit is 1"},
                    {"name": "non_interactive", "type": "flag",
                     "doc": "accepted and does nothing: no command here prompts or reads stdin"},
                    {"name": "format", "type": "enum", "values": ["human", "json"]}
                ],
                "env": [{"name": "BELLWETHER_VAULT",
                         "doc": "the store when --store is not given, then ~/.bellwether/vault"}],
                "output": cmd_pack::SCHEMA,
                "exits": {
                    "0": "every manifest has its pack, every pack its manifest and not_packed is empty; pack is null when nothing was left to pack",
                    "1": "dry_run only: packed is above 0, so the folder's packs do not hold every blob and dictionary the log names",
                    "2": "invalid_argument (out or its manifest exists; no out without dry_run), input_unparseable (a line of have), io_failed",
                    "4": "input_absent: no log, no have file or no manifests directory",
                    "8": "lost, unrecorded or not_packed is not empty; the pack is written and holds the members of every lost one and every member the store gave; not_packed[].reason is absent or digest_mismatch"
                },
                "mutates": true
            },
            {
                "path": ["blobs", "dictionary", "train"],
                "summary": "Trains a zstd dictionary on the first samples stored payloads in digest order, measures it on the next holdout ones, stores it under blobs/dictionaries/<sha256> and activates it for every later write.",
                "options": [
                    {"name": "store", "type": "path"},
                    {"name": "samples", "type": "integer", "default": cmd_blobs::DEFAULT_SAMPLES,
                     "doc": "each cut to its first 131072 bytes"},
                    {"name": "holdout", "type": "integer", "default": cmd_blobs::DEFAULT_HOLDOUT},
                    {"name": "dry_run", "type": "flag",
                     "doc": "trains and reports the held-out ratio; stores and activates nothing"},
                    {"name": "non_interactive", "type": "flag",
                     "doc": "accepted and does nothing: no command here prompts or reads stdin"},
                    {"name": "format", "type": "enum", "values": ["human", "json"]}
                ],
                "output": cmd_blobs::DICTIONARY_SCHEMA,
                "exits": {
                    "0": "trained; changed false when that dictionary was already the active one",
                    "2": "invalid_argument (--samples or --holdout 0), input_unparseable (zstd refused to train), input_schema_mismatch, io_failed",
                    "4": "input_absent: no commit log, or fewer payloads than samples plus holdout",
                    "7": "lease_held"
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
            {"code": "robots_refused", "exit": 2, "retryable": false},
            {"code": "provenance_unrecorded", "exit": 4, "retryable": false},
            {"code": "nothing_visible", "exit": 4, "retryable": false},
            {"code": "staging_left", "exit": 2, "retryable": false},
            {"code": "pace_root_split", "exit": 2, "retryable": false}
        ],
        // Decision D1's companion rule, which `news_provider::Exit` holds as one function.
        "stdout_carries_the_result_for_exits": [0, 1, 8]
    });
    match format {
        Format::Human => println!("{doc:#}"),
        Format::Json => println!("{doc}"),
    }
}
