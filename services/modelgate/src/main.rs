//! `modelgate` - command line over the only exit to a language model.
//!
//! The repo's CLI contract: the result on stdout and nothing else, errors on stderr, exactly one
//! JSON value in JSON mode, and `schema` for a caller that is not human.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use chrono::Utc;
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{json, Value};

use modelgate::cmd_shadow::{
    self, Args, Env, ShadowError, ERROR_SCHEMA, EXIT_INCOMPLETE, EXIT_NOT_FOUND, EXIT_OK,
    EXIT_TRANSIENT, EXIT_USAGE,
};
use modelgate::wire::{ApiKey, KeyError, ModelWire, ENDPOINT, KEY_FILE_VARIABLE};

const STORE_VARIABLE: &str = "BELLWETHER_MODELGATE";
const DEFAULT_STORE: &str = ".bellwether/modelgate";

const TOP_HELP: &str = "\
{about}

USAGE:
  {usage}

COMMANDS:
{subcommands}

OPTIONS:
{options}

OUTPUT:
  One JSON value on stdout under --format json; see `modelgate schema`.

EXIT CODES:
  0 ok   2 usage, input, credential, io   4 not found   7 transient   8 incomplete

EXAMPLES:
  modelgate schema --format json
  modelgate schema shadow run
";

const RUN_HELP: &str = "\
{about}

USAGE:
  {usage}

OPTIONS:
{options}

OUTPUT:
  stdout: one modelgate.shadow_run/1 object for exits 0 and 8, nothing otherwise.
  stderr: one modelgate.error/1 object for exits 2, 4 and 7 under --format json.

EXIT CODES:
  0  every selected story has an answered request, or nothing was new, or a dry run
  2  invalid_argument, input_unparseable, input_schema_mismatch, credential_key_missing,
     credential_key_exposed, io_failed
  4  input_absent
  7  vendor_unreachable, lease_held (retryable)
  8  cost_cap_reached, cost_estimate_broken, partial_shadow; the envelope names which

EXAMPLES:
  modelgate shadow run --snapshot /cuts/3f2a --questions ripple-shadow-0.json \\
      --model typesafe/jev-1.14-20261014 --run-cap-usd 0.40 --day-cap-usd 1.50 \\
      --total-cap-usd 40 --input-price-per-mtok 0.042 --give-up-after-hours 24 \\
      --dry-run --format json
";

#[derive(Parser)]
#[command(
    name = "modelgate",
    about = "The only exit to a language model: decide calls behind a ledger and a cost cap.",
    version,
    help_template = TOP_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Record-only labelling of news stories. Nothing reads a price or builds a list.
    #[command(subcommand)]
    Shadow(Shadow),
    /// This binary's commands, options, outputs and exit codes, as JSON.
    Schema {
        /// A command path, such as `shadow run`. Without one, every command.
        command: Vec<String>,
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },
}

#[derive(Subcommand)]
enum Shadow {
    /// One decide request per new story of a `newsvault cut` snapshot. The only command that
    /// spends money.
    #[command(help_template = RUN_HELP)]
    Run(Box<RunArgs>),
}

#[derive(clap::Args)]
struct RunArgs {
    /// A snapshot directory written by `newsvault cut`.
    #[arg(long)]
    snapshot: PathBuf,
    /// A modelgate.questions/1 file.
    #[arg(long)]
    questions: PathBuf,
    /// A dated build, `<id>-YYYYMMDD`. An alias is refused.
    #[arg(long)]
    model: String,
    /// USD this run may spend at most. No default.
    #[arg(long)]
    run_cap_usd: String,
    /// USD the whole store may spend per UTC day, every build and document together.
    #[arg(long)]
    day_cap_usd: String,
    /// USD the whole store may spend over its lifetime.
    #[arg(long)]
    total_cap_usd: String,
    /// USD per million input tokens, for the worst-case bound.
    #[arg(long)]
    input_price_per_mtok: String,
    /// Hours after its observation at which a story whose request keeps failing is left.
    #[arg(long)]
    give_up_after_hours: u32,
    /// The gate's store. Defaults to $BELLWETHER_MODELGATE, then ~/.bellwether/modelgate.
    #[arg(long)]
    store: Option<PathBuf>,
    /// First run of a question version and build only: label rows above this seq. Defaults to
    /// the snapshot's seq_high_water, which labels nothing that is already collected.
    #[arg(long)]
    from_seq: Option<i64>,
    /// Requests this run sends at most.
    #[arg(long)]
    limit: Option<u64>,
    /// Only the 10% of stories a hash of the story id selects.
    #[arg(long)]
    sample_only: bool,
    /// 0 is the label. 1 and above are repeats of the same bytes, recorded as instrument noise.
    #[arg(long, default_value_t = 0)]
    repeat_index: u32,
    /// The run id whose cost_estimate_broken this run acknowledges.
    #[arg(long)]
    ack_cost_model: Option<String>,
    /// Validate, select and plan; send nothing and write nothing.
    #[arg(long)]
    dry_run: bool,
    /// Accepted and does nothing: no command here prompts or reads stdin.
    #[arg(long)]
    non_interactive: bool,
    #[arg(long, value_enum, default_value_t = Format::Human)]
    format: Format,
    /// Alias of --format json.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Human,
    Json,
}

/// The one description of the commands: `schema` prints it, and
/// `every_option_the_parser_takes_is_in_the_schema` holds the parser to it.
fn schema_doc() -> Value {
    json!({
        "schema": "modelgate.schema/1",
        "commands": [
            {
                "path": ["shadow", "run"],
                "summary": "One decide request per story of a newsvault cut snapshot whose first-sighted row is above the floor and has no answered request for this question version, build and repeat index. Records raw request and response bytes and an attempt ledger; reads no price and builds no list.",
                "options": [
                    {"name": "snapshot", "type": "path", "required": true},
                    {"name": "questions", "type": "path", "required": true},
                    {"name": "model", "type": "string", "required": true,
                     "doc": "a dated build, <id>-YYYYMMDD; an alias is invalid_argument"},
                    {"name": "run_cap_usd", "type": "usd", "required": true},
                    {"name": "day_cap_usd", "type": "usd", "required": true,
                     "doc": "over the whole store per UTC day"},
                    {"name": "total_cap_usd", "type": "usd", "required": true,
                     "doc": "over the whole store's lifetime"},
                    {"name": "input_price_per_mtok", "type": "usd", "required": true},
                    {"name": "give_up_after_hours", "type": "integer", "required": true},
                    {"name": "store", "type": "path"},
                    {"name": "from_seq", "type": "integer",
                     "doc": "the floor, fixed by the first run of a question version and build; a different value later is invalid_argument"},
                    {"name": "limit", "type": "integer"},
                    {"name": "sample_only", "type": "flag"},
                    {"name": "repeat_index", "type": "integer", "default": 0},
                    {"name": "ack_cost_model", "type": "string"},
                    {"name": "dry_run", "type": "flag",
                     "doc": "validates, selects and plans; opens no attempt, writes no row and no blob, sends nothing"},
                    {"name": "non_interactive", "type": "flag",
                     "doc": "accepted and does nothing: no command here prompts or reads stdin"},
                    {"name": "format", "type": "enum", "values": ["human", "json"]},
                    {"name": "json", "type": "flag", "doc": "alias of --format json"}
                ],
                "env": [
                    {"name": KEY_FILE_VARIABLE,
                     "doc": "names a file holding the API key, mode 0600; there is no variable for the value"},
                    {"name": STORE_VARIABLE,
                     "doc": "the store when --store is not given, then ~/.bellwether/modelgate"}
                ],
                "output": cmd_shadow::SCHEMA,
                "exits": {
                    "0": "every selected story has an answered request, or nothing was new, or a dry run",
                    "2": "invalid_argument, input_unparseable, input_schema_mismatch, credential_key_missing, credential_key_exposed, io_failed",
                    "4": "input_absent: the snapshot, the question file or the key file",
                    "7": "vendor_unreachable (nothing answered, every failure transient), lease_held",
                    "8": "cost_cap_reached, cost_estimate_broken or partial_shadow, named in the envelope's exit.error_code"
                },
                "mutates": true,
                "spends_money": true,
                "network": [ENDPOINT]
            },
            {"path": ["schema"], "summary": "This document, or one command of it.",
             "options": [
                 {"name": "command", "type": "words"},
                 {"name": "format", "type": "enum", "values": ["human", "json"]}
             ],
             "mutates": false}
        ],
        "exit_codes": {
            "0": "ok",
            "2": "usage, unparseable input, schema mismatch, credential, io",
            "4": "not found",
            "7": "transient (retryable)",
            "8": "the report is incomplete"
        },
        "error_codes": [
            {"code": "invalid_argument", "exit": EXIT_USAGE, "retryable": false},
            {"code": "input_absent", "exit": EXIT_NOT_FOUND, "retryable": false},
            {"code": "input_unparseable", "exit": EXIT_USAGE, "retryable": false},
            {"code": "input_schema_mismatch", "exit": EXIT_USAGE, "retryable": false},
            {"code": "io_failed", "exit": EXIT_USAGE, "retryable": false},
            {"code": "credential_key_missing", "exit": EXIT_USAGE, "retryable": false},
            {"code": "credential_key_exposed", "exit": EXIT_USAGE, "retryable": false},
            {"code": "lease_held", "exit": EXIT_TRANSIENT, "retryable": true},
            {"code": "vendor_unreachable", "exit": EXIT_TRANSIENT, "retryable": true},
            {"code": "cost_cap_reached", "exit": EXIT_INCOMPLETE, "retryable": false},
            {"code": "cost_estimate_broken", "exit": EXIT_INCOMPLETE, "retryable": false},
            {"code": "partial_shadow", "exit": EXIT_INCOMPLETE, "retryable": false}
        ],
        "stdout_carries_the_result_for_exits": [EXIT_OK, EXIT_INCOMPLETE]
    })
}

fn print(value: &Value, format: Format) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    match format {
        Format::Human => writeln!(out, "{value:#}"),
        Format::Json => writeln!(out, "{value}"),
    }
}

/// The error object on stderr, with the suggestion as an argv array.
fn fail(
    code: &str,
    exit: u8,
    retryable: bool,
    message: &str,
    details: &Value,
    format: Format,
) -> u8 {
    let object = json!({
        "schema": ERROR_SCHEMA, "code": code, "message": message, "retryable": retryable,
        "details": details,
        "suggestions": if retryable {
            json!([std::env::args().collect::<Vec<String>>()])
        } else {
            json!([])
        },
    });
    let mut err = std::io::stderr().lock();
    let written = match format {
        Format::Json => writeln!(err, "{object}"),
        Format::Human => writeln!(err, "error: {message} ({code})"),
    };
    // A closed stderr must not change the verdict.
    match written {
        Ok(()) | Err(_) => exit,
    }
}

fn key_exit(failure: &KeyError) -> u8 {
    match failure {
        KeyError::Absent(_) => EXIT_NOT_FOUND,
        _ => EXIT_USAGE,
    }
}

fn store_root(given: Option<PathBuf>) -> Option<PathBuf> {
    given
        .or_else(|| std::env::var_os(STORE_VARIABLE).map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(DEFAULT_STORE)))
}

fn shadow_run(run: RunArgs) -> u8 {
    let format = if run.json { Format::Json } else { run.format };
    let Some(store) = store_root(run.store) else {
        let message = format!("no --store, no {STORE_VARIABLE} and no HOME");
        return fail(
            "invalid_argument",
            EXIT_USAGE,
            false,
            &message,
            &json!({}),
            format,
        );
    };
    let key = match ApiKey::read(|name| std::env::var_os(name)) {
        Ok(key) => key,
        Err(failure) => {
            let exit = key_exit(&failure);
            return fail(
                failure.code(),
                exit,
                false,
                &failure.to_string(),
                &json!({}),
                format,
            );
        }
    };
    let wire = ModelWire::new(key);
    let args = Args {
        snapshot: run.snapshot,
        questions: run.questions,
        model: run.model,
        run_cap_usd: run.run_cap_usd,
        day_cap_usd: run.day_cap_usd,
        total_cap_usd: run.total_cap_usd,
        input_price_per_mtok: run.input_price_per_mtok,
        give_up_after_hours: run.give_up_after_hours,
        store,
        from_seq: run.from_seq,
        limit: run.limit,
        sample_only: run.sample_only,
        repeat_index: run.repeat_index,
        ack_cost_model: run.ack_cost_model,
        dry_run: run.dry_run,
    };
    let env = Env {
        exchange: &wire,
        now: &Utc::now,
        sleep: &std::thread::sleep,
    };
    match cmd_shadow::run(&args, &env) {
        Ok(done) => match print(&done.envelope, format) {
            Ok(()) => done.exit,
            Err(why) => fail(
                "io_failed",
                EXIT_USAGE,
                false,
                &why.to_string(),
                &json!({}),
                format,
            ),
        },
        Err(failure) => report(&failure, format),
    }
}

fn report(failure: &ShadowError, format: Format) -> u8 {
    fail(
        failure.code(),
        failure.exit(),
        failure.retryable(),
        &failure.to_string(),
        &failure.details(),
        format,
    )
}

fn schema(command: &[String], format: Format) -> u8 {
    let doc = schema_doc();
    let wanted = if command.is_empty() {
        Some(doc.clone())
    } else {
        doc["commands"].as_array().and_then(|commands| {
            commands
                .iter()
                .find(|entry| entry["path"] == json!(command))
                .cloned()
        })
    };
    let Some(wanted) = wanted else {
        let message = format!("no command `{}`", command.join(" "));
        return fail(
            "invalid_argument",
            EXIT_USAGE,
            false,
            &message,
            &json!({}),
            format,
        );
    };
    match print(&wanted, format) {
        Ok(()) => EXIT_OK,
        Err(_) => EXIT_USAGE,
    }
}

fn main() -> ExitCode {
    let exit = match Cli::parse().command {
        Command::Shadow(Shadow::Run(run)) => shadow_run(*run),
        Command::Schema { command, format } => schema(&command, format),
    };
    ExitCode::from(exit)
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::{schema_doc, Cli};

    /// Help and schema are two renderings; this holds them to one option list.
    #[test]
    fn every_option_the_parser_takes_is_in_the_schema_and_no_other() {
        let doc = schema_doc();
        let cli = Cli::command();
        let mut checked = 0;
        for entry in doc["commands"].as_array().into_iter().flatten() {
            let mut command = &cli;
            for word in entry["path"].as_array().into_iter().flatten() {
                let name = word.as_str().unwrap_or_default();
                command = match command.find_subcommand(name) {
                    Some(found) => found,
                    None => {
                        assert_eq!(name, "a subcommand of the parser");
                        return;
                    }
                };
            }
            let mut parsed: Vec<String> = command
                .get_arguments()
                .map(|argument| argument.get_id().to_string())
                .filter(|id| id != "help" && id != "version")
                .collect();
            let mut stated: Vec<String> = entry["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|option| option["name"].as_str().map(str::to_owned))
                .collect();
            parsed.sort();
            stated.sort();
            assert_eq!(parsed, stated, "{}", entry["path"]);
            checked += 1;
        }
        assert_eq!(checked, 2);
    }
}
