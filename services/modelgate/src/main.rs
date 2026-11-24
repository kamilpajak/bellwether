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
use modelgate::cmd_snapshot::{self, SNAPSHOT_SCHEMA, VERIFY_SCHEMA};
use modelgate::ledger::SendPermit;
use modelgate::wire::{
    ApiKey, Exchanged, KeyError, ModelExchange, ModelWire, ENDPOINT, KEY_FILE_VARIABLE,
};

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
  modelgate verify --store /tmp/restored --format json
";

const RUN_HELP: &str = "\
{about}

USAGE:
  {usage}

OPTIONS:
{options}

OUTPUT:
  stdout: one modelgate.shadow_run/1 object for exits 0 and 8, nothing otherwise.
  stderr: one modelgate.error/1 object for exits 2, 4 and 7 under --format json;
          otherwise `error: <text> (<code>)`, then the details when there are any.

EXIT CODES:
  0  no attempt of this run failed or closed unusable, or nothing was new, or a dry run
  2  invalid_argument, input_unparseable, input_schema_mismatch, credential_key_missing,
     credential_key_exposed, io_failed
  4  input_absent
  7  vendor_unreachable, lease_held (retryable)
  8  store_failed, cost_estimate_broken, cost_cap_reached, partial_shadow, unusable_answer;
     the envelope names which

EXAMPLES:
  modelgate shadow run --snapshot /cuts/3f2a --questions ripple-shadow-1.json \\
      --model typesafe/jev-1.14-20261014 --run-cap-usd 0.40 --day-cap-usd 1.50 \\
      --total-cap-usd 40 --input-price-per-mtok 0.042 --give-up-after-hours 24 \\
      --dry-run --format json
";

const SNAPSHOT_HELP: &str = "\
{about}

USAGE:
  {usage}

OPTIONS:
{options}

OUTPUT:
  stdout: one modelgate.snapshot/1 object for exit 0, nothing otherwise.
  stderr: one modelgate.error/1 object under --format json; otherwise `error: <text> (<code>)`.

EXIT CODES:
  0  the database and every blob a row names are in --out
  2  invalid_argument (--out is not empty), input_schema_mismatch, io_failed (a named blob is
     missing or does not hash to its name; the copy fails quick_check)
  4  input_absent: no gate.sqlite or no LOCK in the store
  7  lease_held: a run holds the store's LOCK (retryable)

EXAMPLES:
  modelgate snapshot --store ~/.bellwether/modelgate --out /tmp/gate-copy --format json
";

const VERIFY_HELP: &str = "\
{about}

USAGE:
  {usage}

OPTIONS:
{options}

OUTPUT:
  stdout: one modelgate.store_verify/1 object for exit 0, nothing otherwise.
  stderr: one modelgate.error/1 object under --format json; otherwise `error: <text> (<code>)`.

EXIT CODES:
  0  the database passes quick_check and every blob a row names hashes to its name
  2  input_schema_mismatch, io_failed (the database fails quick_check; the error names a blob
     that is missing or altered)
  4  input_absent: no gate.sqlite in the store

EXAMPLES:
  modelgate verify --store /tmp/restored --format json
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
    /// Copy the database and every blob a row names into an empty directory, under the store's
    /// LOCK. The daily backup's input.
    #[command(help_template = SNAPSHOT_HELP)]
    Snapshot {
        /// An absent or empty directory; it receives gate.sqlite and blobs/.
        #[arg(long)]
        out: PathBuf,
        #[command(flatten)]
        read: ReadArgs,
    },
    /// Check a store's database, count its rows and read every blob they name. Writes nothing
    /// and takes no lock.
    #[command(help_template = VERIFY_HELP)]
    Verify {
        #[command(flatten)]
        read: ReadArgs,
    },
    /// This binary's commands, options, outputs and exit codes, as JSON.
    Schema {
        /// A command path, such as `shadow run`. Without one, every command.
        command: Vec<String>,
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },
}

#[derive(clap::Args)]
struct ReadArgs {
    /// The gate's store. Defaults to $BELLWETHER_MODELGATE, then ~/.bellwether/modelgate.
    #[arg(long)]
    store: Option<PathBuf>,
    /// Accepted and does nothing: no command here prompts or reads stdin.
    #[arg(long)]
    non_interactive: bool,
    #[arg(long, value_enum, default_value_t = Format::Human)]
    format: Format,
    /// Alias of --format json.
    #[arg(long)]
    json: bool,
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
    /// Validate, select and plan; send nothing, write nothing and read no key.
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
                    {"name": "input_price_per_mtok", "type": "usd", "required": true,
                     "doc": "above zero; 0 is invalid_argument"},
                    {"name": "give_up_after_hours", "type": "integer", "required": true},
                    {"name": "store", "type": "path"},
                    {"name": "from_seq", "type": "integer",
                     "doc": "the floor, fixed by the first run of a question version and build; a different value later is invalid_argument"},
                    {"name": "limit", "type": "integer"},
                    {"name": "sample_only", "type": "flag"},
                    {"name": "repeat_index", "type": "integer", "default": 0},
                    {"name": "ack_cost_model", "type": "string",
                     "doc": "a run id with a cost_estimate_broken row, else invalid_argument; one already acknowledged writes nothing"},
                    {"name": "dry_run", "type": "flag",
                     "doc": "validates, selects and plans; opens no attempt, writes no row and no blob, sends nothing, and neither needs nor reads the key file"},
                    {"name": "non_interactive", "type": "flag",
                     "doc": "accepted and does nothing: no command here prompts or reads stdin"},
                    {"name": "format", "type": "enum", "values": ["human", "json"],
                     "default": "human"},
                    {"name": "json", "type": "flag", "doc": "alias of --format json"}
                ],
                "env": [
                    {"name": KEY_FILE_VARIABLE,
                     "doc": "names a file holding the API key, mode 0600; there is no variable for the value; not read under --dry-run"},
                    {"name": STORE_VARIABLE,
                     "doc": "the store when --store is not given, then ~/.bellwether/modelgate"}
                ],
                "output": cmd_shadow::SCHEMA,
                "exits": {
                    "0": "no attempt of this run failed or closed unusable, or nothing was new, or a dry run",
                    "2": "invalid_argument, input_unparseable, input_schema_mismatch, credential_key_missing, credential_key_exposed, io_failed",
                    "4": "input_absent: the snapshot, the question file or the key file",
                    "7": "vendor_unreachable (nothing answered, every failure transient), lease_held",
                    "8": "store_failed, cost_estimate_broken, cost_cap_reached, partial_shadow or unusable_answer, named in the envelope's exit.error_code"
                },
                "mutates": true,
                "spends_money": true,
                "network": [ENDPOINT]
            },
            {"path": ["snapshot"],
             "summary": "Copies gate.sqlite by SQLite's online backup and every blob a row of that copy names into --out, under the store's LOCK opened without write access. rows are the copy's. A blob no row names is left behind and counted.",
             "options": [
                 {"name": "out", "type": "path", "required": true,
                  "doc": "absent or empty, else invalid_argument"},
                 {"name": "store", "type": "path"},
                 {"name": "non_interactive", "type": "flag",
                  "doc": "accepted and does nothing: no command here prompts or reads stdin"},
                 {"name": "format", "type": "enum", "values": ["human", "json"],
                  "default": "human"},
                 {"name": "json", "type": "flag", "doc": "alias of --format json"}
             ],
             "env": [{"name": STORE_VARIABLE,
                      "doc": "the store when --store is not given, then ~/.bellwether/modelgate"}],
             "output": SNAPSHOT_SCHEMA,
             "exits": {
                 "0": "the database and every named blob are in --out",
                 "2": "invalid_argument, input_schema_mismatch, io_failed: a named blob is missing or altered, or the copy fails quick_check",
                 "4": "input_absent: no gate.sqlite or no LOCK",
                 "7": "lease_held"
             },
             "mutates": true, "spends_money": false, "network": []},
            {"path": ["verify"],
             "summary": "Runs PRAGMA quick_check, counts the rows of every table and reads every blob a row names, each refused unless it hashes to its name. Read-only, no lock.",
             "options": [
                 {"name": "store", "type": "path"},
                 {"name": "non_interactive", "type": "flag",
                  "doc": "accepted and does nothing: no command here prompts or reads stdin"},
                 {"name": "format", "type": "enum", "values": ["human", "json"],
                  "default": "human"},
                 {"name": "json", "type": "flag", "doc": "alias of --format json"}
             ],
             "env": [{"name": STORE_VARIABLE,
                      "doc": "the store when --store is not given, then ~/.bellwether/modelgate"}],
             "output": VERIFY_SCHEMA,
             "exits": {
                 "0": "the database passes quick_check and every named blob hashes to its name",
                 "2": "input_schema_mismatch, io_failed: the database fails quick_check, or a named blob is missing or altered",
                 "4": "input_absent: no gate.sqlite"
             },
             "mutates": false, "spends_money": false, "network": []},
            {"path": ["schema"], "summary": "This document, or one command of it.",
             "options": [
                 {"name": "command", "type": "words"},
                 {"name": "format", "type": "enum", "values": ["human", "json"],
                  "default": "json"}
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
            {"code": "partial_shadow", "exit": EXIT_INCOMPLETE, "retryable": false},
            {"code": "unusable_answer", "exit": EXIT_INCOMPLETE, "retryable": false},
            {"code": "store_failed", "exit": EXIT_INCOMPLETE, "retryable": false}
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
    let written = writeln!(std::io::stderr().lock(), "{}", error_text(&object, format));
    // A closed stderr must not change the verdict.
    match written {
        Ok(()) | Err(_) => exit,
    }
}

/// The human form carries the details too: on exit 7 they are the run's envelope, which no
/// other stream holds.
fn error_text(object: &Value, format: Format) -> String {
    let text = |key: &str| object[key].as_str().unwrap_or_default();
    let details = &object["details"];
    match format {
        Format::Json => object.to_string(),
        Format::Human if details.as_object().is_some_and(|map| !map.is_empty()) => {
            format!("error: {} ({})\n{details:#}", text("message"), text("code"))
        }
        Format::Human => format!("error: {} ({})", text("message"), text("code")),
    }
}

/// What a dry run is handed in place of the wire: no key and no socket. `cmd_shadow` returns
/// before a send under `--dry-run`, so a call here could only be a defect, and it answers as one.
struct NoWire;

impl ModelExchange for NoWire {
    fn send(&self, permit: SendPermit, _body: &[u8]) -> Exchanged {
        let now = Utc::now();
        Exchanged::new(permit, now, now, Err("no_wire"))
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
    let wire: Box<dyn ModelExchange> = if run.dry_run {
        Box::new(NoWire)
    } else {
        match ApiKey::read(|name| std::env::var_os(name)) {
            Ok(key) => Box::new(ModelWire::new(key)),
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
        }
    };
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
        exchange: wire.as_ref(),
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

/// `snapshot` with `out`, `verify` without.
fn store_read(read: ReadArgs, out: Option<&PathBuf>) -> u8 {
    let format = if read.json { Format::Json } else { read.format };
    let Some(store) = store_root(read.store) else {
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
    let done = match out {
        Some(out) => cmd_snapshot::snapshot(&store, out),
        None => cmd_snapshot::verify(&store),
    };
    match done.map(|envelope| print(&envelope, format)) {
        Ok(Ok(())) => EXIT_OK,
        Ok(Err(why)) => fail(
            "io_failed",
            EXIT_USAGE,
            false,
            &why.to_string(),
            &json!({}),
            format,
        ),
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

fn wants_json(argv: &[String]) -> bool {
    let named = |pair: &[String]| pair[0] == "--format" && pair[1] == "json";
    argv.iter()
        .any(|word| word == "--json" || word == "--format=json")
        || argv.windows(2).any(named)
}

/// A refusal by the parser. Under `--json` it is the same error object as any other refusal;
/// help, version and the human form stay clap's own.
fn usage(refusal: &clap::Error) -> u8 {
    let argv: Vec<String> = std::env::args_os()
        .map(|word| word.to_string_lossy().into_owned())
        .collect();
    if refusal.use_stderr() && wants_json(&argv) {
        let message = refusal.to_string();
        return fail(
            "invalid_argument",
            EXIT_USAGE,
            false,
            message.trim_end(),
            &json!({}),
            Format::Json,
        );
    }
    refusal.exit()
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(refusal) => return ExitCode::from(usage(&refusal)),
    };
    let exit = match cli.command {
        Command::Shadow(Shadow::Run(run)) => shadow_run(*run),
        Command::Snapshot { out, read } => store_read(read, Some(&out)),
        Command::Verify { read } => store_read(read, None),
        Command::Schema { command, format } => schema(&command, format),
    };
    ExitCode::from(exit)
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use serde_json::{json, Value};

    use super::{error_text, schema_doc, wants_json, Cli, Format};

    /// `(name, required, default)` of an option as the schema states it.
    fn stated(option: &Value) -> (String, bool, Option<String>) {
        let default = match &option["default"] {
            Value::Null => None,
            Value::String(text) => Some(text.clone()),
            other => Some(other.to_string()),
        };
        (
            option["name"].as_str().unwrap_or_default().to_owned(),
            option["required"] == true,
            default,
        )
    }

    /// Help and schema are two renderings; this holds them to one option list, with what is
    /// required and what has a default. Mutation 2026-11-23, a `default_value` on
    /// `--total-cap-usd`: red.
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
            let mut parsed: Vec<(String, bool, Option<String>)> = command
                .get_arguments()
                .filter(|argument| !["help", "version"].contains(&argument.get_id().as_str()))
                .map(|argument| {
                    let default = argument
                        .get_default_values()
                        .first()
                        .filter(|_| argument.get_action().takes_values())
                        .map(|value| value.to_string_lossy().into_owned());
                    (
                        argument.get_id().to_string(),
                        argument.is_required_set(),
                        default,
                    )
                })
                .collect();
            let mut in_schema: Vec<(String, bool, Option<String>)> = entry["options"]
                .as_array()
                .into_iter()
                .flatten()
                .map(stated)
                .collect();
            parsed.sort();
            in_schema.sort();
            assert_eq!(parsed, in_schema, "{}", entry["path"]);
            checked += 1;
        }
        assert_eq!(checked, 4);
    }

    /// Exit 7 carries the run's envelope under `details`, and no binary test reaches a send.
    /// Mutation 2026-11-23, the details arm of `error_text` removed: red.
    #[test]
    fn the_human_error_text_carries_the_details_and_json_is_asked_for_in_three_spellings() {
        let object = json!({"code": "vendor_unreachable", "message": "nothing answered",
                            "details": {"requests": {"sent": 9}}});
        let human = error_text(&object, Format::Human);
        assert!(human.starts_with("error: nothing answered (vendor_unreachable)\n{"));
        assert!(human.contains("\"sent\": 9"), "{human}");
        let bare = json!({"code": "lease_held", "message": "held", "details": {}});
        assert_eq!(error_text(&bare, Format::Human), "error: held (lease_held)");
        assert_eq!(error_text(&bare, Format::Json), bare.to_string());
        let argv = |words: &[&str]| {
            words
                .iter()
                .map(|word| (*word).to_owned())
                .collect::<Vec<_>>()
        };
        assert!(wants_json(&argv(&["shadow", "run", "--json"])));
        assert!(wants_json(&argv(&["shadow", "run", "--format=json"])));
        assert!(wants_json(&argv(&["shadow", "run", "--format", "json"])));
        assert!(!wants_json(&argv(&["shadow", "run", "--format", "human"])));
    }
}
