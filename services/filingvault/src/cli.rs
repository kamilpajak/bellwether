//! The command line: one parser, one renderer per output, and `schema` generated from the
//! parser's own metadata and [`ErrorCode::ALL`], so the manifest cannot drift from the options.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use news_provider::{contact, Clock, SystemClock};
use serde_json::{json, Value};

use crate::collect::{self, COLLECT_SCHEMA};
use crate::error::{ErrorCode, FilingsError};
use crate::ledger::{self, LEDGER_SCHEMA};
use crate::pace_option::Pace;
use crate::rules;

pub const SCHEMA_SCHEMA: &str = "filingvault.schema/1";
/// The contact every SEC request carries: the feed's, which the roster reads too (section 12.2).
/// Unguarded: nothing ties these names to `newsvault`'s copies (section 12.6).
pub const CONTACT_VARIABLE: &str = "BELLWETHER_FEED_CONTACT";
pub const CONTACT_FILE_VARIABLE: &str = "BELLWETHER_FEED_CONTACT_FILE";
/// Section 5.1's iff rule: stdout carries the result for these exits only.
pub const STDOUT_EXITS: [u8; 3] = [0, 1, 8];

#[derive(Parser, Debug)]
#[command(
    name = "filingvault",
    about = "The filings context: which annual reports SEC lists per index day, as of an instant."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Ask SEC.
    Filings {
        #[command(subcommand)]
        action: FilingsAction,
    },
    /// Read the coverage ledger.
    Ledger {
        #[command(subcommand)]
        action: LedgerAction,
    },
    /// The command tree, its outputs, exits and error codes.
    Schema {
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },
}

#[derive(Subcommand, Debug)]
pub enum FilingsAction {
    /// One ledgered run: robots.txt, the quarter listings and the daily form indexes.
    Collect(CollectArgs),
}

#[derive(Subcommand, Debug)]
pub enum LedgerAction {
    /// Which listed index days were asked, as of --at.
    Check(CheckArgs),
}

#[derive(Args, Debug)]
pub struct CollectArgs {
    /// The store directory: LOCK, filings.sqlite3 and blobs/.
    #[arg(long)]
    pub store: PathBuf,
    #[command(flatten)]
    pub pace: Pace,
    /// The rules file: the form list and the fill window.
    #[arg(long)]
    pub rules: PathBuf,
    /// No request is reserved after this RFC 3339 instant; without it, no deadline.
    #[arg(long, value_parser = instant)]
    pub deadline: Option<DateTime<Utc>>,
    /// Plan from the store and ask nothing; creates nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Accepted for scripts: the command never prompts.
    #[arg(long)]
    pub non_interactive: bool,
    #[arg(long, value_enum, default_value_t = Format::Human)]
    pub format: Format,
}

#[derive(Args, Debug)]
pub struct CheckArgs {
    /// The RFC 3339 instant the ledger answers as of; not after now.
    #[arg(long, value_parser = instant)]
    pub at: DateTime<Utc>,
    /// The store directory.
    #[arg(long)]
    pub store: PathBuf,
    /// The rules file whose form list "asked" is read under.
    #[arg(long)]
    pub rules: PathBuf,
    #[arg(long, value_enum, default_value_t = Format::Human)]
    pub format: Format,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Human,
    Json,
}

fn instant(text: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(text)
        .map(|at| at.with_timezone(&Utc))
        .map_err(|why| format!("not an RFC 3339 instant: {why}"))
}

/// Each command's exits, the one table `schema` prints.
const EXITS: [(&str, &[(u8, &str)]); 3] = [
    (
        "filings collect",
        &[
            (0, "every request of the plan answered usable or empty"),
            (2, "invalid_argument, input_unparseable, input_schema_mismatch, credential_contact_missing, pace_root_split, io_failed; before any request"),
            (4, "input_absent: the rules file, the pace root or the contact file"),
            (7, "lease_held, robots_stopped, gate_refused: retryable, stdout empty, the run's report in details"),
            (8, "partial_collect: a request failed, or the deadline stopped the run"),
        ],
    ),
    (
        "ledger check",
        &[
            (0, "no forward index day is unasked or unlisted"),
            (1, "the answer is no: a forward day is unasked, or no listing held by --at could name it yet, or no collect run had started by --at"),
            (2, "invalid_argument, input_unparseable, input_schema_mismatch, io_failed"),
            (4, "input_absent: no store"),
        ],
    ),
    ("schema", &[(0, "the schema")]),
];

const OUTPUTS: [(&str, &str); 3] = [
    ("filings collect", COLLECT_SCHEMA),
    ("ledger check", LEDGER_SCHEMA),
    ("schema", SCHEMA_SCHEMA),
];

/// The manifest, generated from the parser: every leaf command with its options.
#[must_use]
pub fn schema() -> Value {
    let mut commands = Vec::new();
    let root = Cli::command();
    for group in root.get_subcommands() {
        let leaves: Vec<(String, &clap::Command)> = if group.has_subcommands() {
            group
                .get_subcommands()
                .map(|leaf| (format!("{} {}", group.get_name(), leaf.get_name()), leaf))
                .collect()
        } else {
            vec![(group.get_name().to_owned(), group)]
        };
        for (path, leaf) in leaves {
            let options: Vec<Value> = leaf
                .get_arguments()
                .filter_map(|arg| {
                    let long = arg.get_long()?;
                    Some(json!({
                        "name": long,
                        "required": arg.is_required_set(),
                        "takes_value": arg.get_action().takes_values(),
                        "default": arg.get_default_values().first().map(|value| value.to_string_lossy()),
                        "values": arg.get_possible_values().iter().map(|value| value.get_name().to_owned()).collect::<Vec<String>>(),
                        "doc": arg.get_help().map(ToString::to_string),
                    }))
                })
                .collect();
            let exits = EXITS
                .iter()
                .find(|(named, _)| *named == path)
                .map(|(_, exits)| {
                    exits
                        .iter()
                        .map(|(code, means)| (code.to_string(), json!(means)))
                        .collect::<serde_json::Map<String, Value>>()
                });
            let output = OUTPUTS
                .iter()
                .find(|(named, _)| *named == path)
                .map(|(_, schema)| schema);
            commands.push(json!({
                "path": path.split(' ').collect::<Vec<&str>>(),
                "doc": leaf.get_about().map(ToString::to_string),
                "options": options,
                "output": output,
                "exits": exits,
            }));
        }
    }
    json!({
        "schema": SCHEMA_SCHEMA,
        "commands": commands,
        "env": [
            {"name": CONTACT_VARIABLE, "doc": "the contact SEC's User-Agent carries"},
            {"name": CONTACT_FILE_VARIABLE, "doc": "a file holding it; set one of the two"},
        ],
        "error_codes": ErrorCode::ALL.map(|code| json!({
            "code": code.name(),
            "exit": code.exit(),
            "retryable": code.retryable(),
        })),
        "stdout_carries_the_result_for_exits": STDOUT_EXITS,
    })
}

/// The contact, refused unless it holds an address SEC accepts.
pub fn contact(env: impl Fn(&str) -> Option<OsString>) -> Result<String, FilingsError> {
    let contact = contact::read(CONTACT_VARIABLE, CONTACT_FILE_VARIABLE, env)?;
    if sec_edgar::contact_is_addressed(&contact) {
        Ok(contact)
    } else {
        Err(FilingsError::ContactUnaddressed)
    }
}

/// Every input is read before the store or the network: the contact, the rules, the pace root.
fn collect(args: &CollectArgs) -> Result<(Value, u8), FilingsError> {
    let contact = contact(|name| std::env::var_os(name))?;
    let rules = rules::read(&args.rules)?;
    let pace_root = args.pace.resolve(&args.store)?;
    let ask = collect::Ask {
        store: args.store.clone(),
        rules,
        deadline: args.deadline,
        dry_run: args.dry_run,
    };
    let wire = collect::wire_for(&contact, &pace_root);
    let report = collect::run(&ask, &wire, &SystemClock)?;
    Ok((report.value, report.exit))
}

fn check(args: &CheckArgs) -> Result<(Value, u8), FilingsError> {
    let asked = ledger::Asked {
        at: args.at,
        store: args.store.clone(),
        rules: rules::read(&args.rules)?,
    };
    let checked = ledger::check(&asked, SystemClock.now())?;
    Ok((checked.report, checked.exit))
}

/// Runs one command and writes its streams: the result on stdout for 0, 1 and 8, else one
/// error object on stderr and nothing on stdout. Returns the exit.
pub fn run(cli: Cli, argv: &[String], out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let (answered, format) = match &cli.command {
        Command::Filings {
            action: FilingsAction::Collect(args),
        } => (collect(args), args.format),
        Command::Ledger {
            action: LedgerAction::Check(args),
        } => (check(args), args.format),
        Command::Schema { format } => (Ok((schema(), 0)), *format),
    };
    let written = match &answered {
        Ok((value, _)) => match format {
            Format::Json => writeln!(out, "{value}"),
            Format::Human => render_human(value, out),
        },
        Err(why) => {
            let object = why.object(argv);
            match format {
                Format::Json => writeln!(err, "{object}"),
                Format::Human => writeln!(
                    err,
                    "error: {} (retryable {}): {}",
                    why.code().name(),
                    why.code().retryable(),
                    why
                ),
            }
        }
    };
    let exit = match answered {
        Ok((_, exit)) => exit,
        Err(why) => why.code().exit(),
    };
    if written.is_err() {
        return ErrorCode::IoFailed.exit();
    }
    exit
}

/// A refusal of the parser: help and the version are results on stdout, exit 0; anything else
/// is `invalid_argument`, an object on stderr when argv asks for `--format json`, else clap's text.
pub fn refused(why: &clap::Error, argv: &[String], out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    use clap::error::ErrorKind;
    let json = argv
        .windows(2)
        .any(|pair| pair[0] == "--format" && pair[1] == "json")
        || argv.iter().any(|arg| arg == "--format=json");
    let written = match why.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
            return match write!(out, "{}", why.render()) {
                Ok(()) => 0,
                Err(_) => ErrorCode::IoFailed.exit(),
            };
        }
        _ if json => writeln!(err, "{}", FilingsError::Usage(first_line(why)).object(argv)),
        _ => write!(err, "{}", why.render()),
    };
    match written {
        Ok(()) => ErrorCode::InvalidArgument.exit(),
        Err(_) => ErrorCode::IoFailed.exit(),
    }
}

/// clap's message without its `error: ` prefix and the usage lines after it.
fn first_line(why: &clap::Error) -> String {
    let text = why.to_string();
    let line = text.lines().next().unwrap_or_default();
    line.strip_prefix("error: ").unwrap_or(line).to_owned()
}

/// The human rendering of a result: its scalar fields one per line, then each list's length.
fn render_human(value: &Value, out: &mut dyn Write) -> std::io::Result<()> {
    let Some(fields) = value.as_object() else {
        return writeln!(out, "{value}");
    };
    for (name, field) in fields {
        match field {
            Value::Array(items) => writeln!(out, "{name:<20} {} entries", items.len())?,
            Value::Object(_) => writeln!(out, "{name:<20} {field}")?,
            other => writeln!(out, "{name:<20} {other}")?,
        }
    }
    Ok(())
}
