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

use modelgate::cmd_dedup::{self, SampleArgs, DISTRIBUTION_SCHEMA, SAMPLE_SCHEMA};
use modelgate::cmd_embed;
use modelgate::cmd_shadow::{
    self, Args, Env, ShadowError, ERROR_SCHEMA, EXIT_INCOMPLETE, EXIT_NOT_FOUND, EXIT_OK,
    EXIT_TRANSIENT, EXIT_USAGE,
};
use modelgate::ledger::SendPermit;
use modelgate::wire::{
    ApiKey, Exchanged, KeyError, ModelExchange, ModelWire, EMBED_ENDPOINT, ENDPOINT,
    KEY_FILE_VARIABLE,
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
";

const EMBED_HELP: &str = "\
{about}

USAGE:
  {usage}

OPTIONS:
{options}

OUTPUT:
  stdout: one modelgate.embed_run/1 object for exits 0 and 8, nothing otherwise.
  stderr: one modelgate.error/1 object for exits 2, 4 and 7 under --format json;
          otherwise `error: <text> (<code>)`, then the details when there are any.

EXIT CODES:
  0  the run's first canary closed ok and no story's request failed or closed unusable,
     or nothing was new, or a dry run, or the deadline or --limit stopped it
  2  invalid_argument, input_unparseable, input_schema_mismatch, credential_key_missing,
     credential_key_exposed, io_failed
  4  input_absent
  7  vendor_unreachable (the first canary met a retryable status or no answer), lease_held
  8  store_failed, cost_estimate_broken, cost_cap_reached, canary_not_ok, partial_embed,
     unusable_answer; the envelope names which

EXAMPLES:
  modelgate embed run --snapshot /cuts/3f2a --model example/embed-1 --provider example-host \\
      --quantization bf16 --epoch 1 --run-cap-usd 0.05 --day-cap-usd 0.50 \\
      --total-cap-usd 3 --input-price-per-mtok 0.01 --give-up-after-hours 24 --limit 200 \\
      --deadline 2026-11-24T10:40:00Z --dry-run --format json
";

const DEDUP_HELP: &str = "\
{about}

USAGE:
  {usage}

OPTIONS:
{options}

OUTPUT:
  stdout: one JSON object for exit 0, nothing otherwise; see `modelgate schema`.
  stderr: one modelgate.error/1 object under --format json, else `error: <text> (<code>)`.

EXIT CODES:
  0  printed
  2  invalid_argument, input_unparseable, input_schema_mismatch, io_failed
  4  input_absent: no embed.sqlite in the store, or no snapshot

EXAMPLES:
  modelgate dedup distribution --store /modelgate --format json
  modelgate dedup sample --seed 20261201 --per-stratum 8 --snapshot /cuts/3f2a --format json
  modelgate dedup sample --seed 20261201 --per-stratum 8 --snapshot /cuts/3f2a \\
      --compare-version 'dedup.compare/1(window_days=7)' --space 'modelgate.embed/1;epoch=1'
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

#[derive(Parser)]
#[command(
    name = "modelgate",
    about = "The only exit to a model: decide and embed calls behind a ledger and a cost cap.",
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
    /// Vectors of story texts and the comparison record of each story. Record only.
    #[command(subcommand)]
    Embed(Embed),
    /// Read-only reports over the comparison records.
    #[command(subcommand)]
    Dedup(Dedup),
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

#[derive(Subcommand)]
enum Embed {
    /// One embed request per story of a `newsvault cut` snapshot that has no vector, behind
    /// one canary call, then each settled story's nearest earlier stories. Spends money.
    #[command(help_template = EMBED_HELP)]
    Run(Box<EmbedArgs>),
}

#[derive(Subcommand)]
enum Dedup {
    /// Stories per similarity band of their best earlier candidate, and the canary series.
    #[command(help_template = DEDUP_HELP)]
    Distribution(DistributionArgs),
    /// Candidate pairs for a person to label, drawn by a seed. The output holds story text.
    #[command(help_template = DEDUP_HELP)]
    Sample(SampleCli),
}

#[derive(clap::Args)]
struct EmbedArgs {
    /// A snapshot directory written by `newsvault cut`.
    #[arg(long)]
    snapshot: PathBuf,
    /// The embedding model's id. No dated build exists, so the provider pins it.
    #[arg(long)]
    model: String,
    /// The one provider endpoint that may answer, by its slug. Fallbacks are off.
    #[arg(long)]
    provider: String,
    /// The precision that endpoint serves, by the vendor's name for it.
    #[arg(long)]
    quantization: String,
    /// Raise it when the provider's serving is seen to change: vectors of two epochs are
    /// never compared.
    #[arg(long)]
    epoch: u32,
    /// USD this run may spend at most. No default.
    #[arg(long)]
    run_cap_usd: String,
    /// USD the embed file may spend per UTC day.
    #[arg(long)]
    day_cap_usd: String,
    /// USD the embed file may spend over its lifetime.
    #[arg(long)]
    total_cap_usd: String,
    /// USD per million input tokens, for the worst-case bound.
    #[arg(long)]
    input_price_per_mtok: String,
    /// Hours after its first attempt at which a story whose request keeps failing is left.
    #[arg(long)]
    give_up_after_hours: u32,
    /// Stories this run embeds at most.
    #[arg(long)]
    limit: u64,
    /// No call starts later than 20 s before this instant: RFC 3339, or @<unix seconds>.
    #[arg(long)]
    deadline: String,
    /// The gate's store. Defaults to $BELLWETHER_MODELGATE, then ~/.bellwether/modelgate.
    #[arg(long)]
    store: Option<PathBuf>,
    /// The embed run id whose cost_estimate_broken this run acknowledges.
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

#[derive(clap::Args)]
struct DistributionArgs {
    /// The gate's store. Defaults to $BELLWETHER_MODELGATE, then ~/.bellwether/modelgate.
    #[arg(long)]
    store: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = Format::Human)]
    format: Format,
    /// Alias of --format json.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct SampleCli {
    /// Written down before the draw; the same seed over the same store draws the same pairs.
    #[arg(long)]
    seed: u64,
    /// Pairs drawn from each stratum: a similarity band x whether a visible host is shared.
    #[arg(long)]
    per_stratum: u64,
    /// A snapshot directory written by `newsvault cut`, for the two texts of each pair.
    #[arg(long)]
    snapshot: PathBuf,
    /// One comparison version's token, when the store holds comparisons of several.
    #[arg(long)]
    compare_version: Option<String>,
    /// One space's name, when the store holds comparisons of several.
    #[arg(long)]
    space: Option<String>,
    /// The gate's store. Defaults to $BELLWETHER_MODELGATE, then ~/.bellwether/modelgate.
    #[arg(long)]
    store: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = Format::Human)]
    format: Format,
    /// Alias of --format json.
    #[arg(long)]
    json: bool,
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
            {
                "path": ["embed", "run"],
                "summary": "One embed request per first-sighted story of a newsvault cut snapshot that has a body, no vector in this space and was observed no more than 7 days before the space's first run, stories never attempted first and each group oldest first, and none unless the run's first canary closed ok; then, for each embedded story with no earlier story still waiting for a vector, its nearest earlier stories within 7 days and what code reads in both texts. Records raw bytes, vectors and comparison records; writes no verdict, and shadow run reads none of it.",
                "options": [
                    {"name": "snapshot", "type": "path", "required": true},
                    {"name": "model", "type": "string", "required": true,
                     "doc": "an alias (leading ~) is invalid_argument; an undated id is accepted for this kind only"},
                    {"name": "provider", "type": "string", "required": true,
                     "doc": "a provider slug; sent as the only allowed provider with fallbacks off"},
                    {"name": "quantization", "type": "string", "required": true},
                    {"name": "epoch", "type": "integer", "required": true},
                    {"name": "run_cap_usd", "type": "usd", "required": true},
                    {"name": "day_cap_usd", "type": "usd", "required": true,
                     "doc": "over the embed file per UTC day; decide spend is not counted"},
                    {"name": "total_cap_usd", "type": "usd", "required": true,
                     "doc": "over the embed file's lifetime; decide spend is not counted"},
                    {"name": "input_price_per_mtok", "type": "usd", "required": true,
                     "doc": "above zero; 0 is invalid_argument"},
                    {"name": "give_up_after_hours", "type": "integer", "required": true,
                     "doc": "counted from a story's first attempt in the space, never from its observation"},
                    {"name": "limit", "type": "integer", "required": true},
                    {"name": "deadline", "type": "instant", "required": true,
                     "doc": "RFC 3339 or @<unix seconds>; no call starts later than 20 s before it and the comparison's scan stops 60 s past it"},
                    {"name": "store", "type": "path"},
                    {"name": "ack_cost_model", "type": "string",
                     "doc": "an embed run id with a cost_estimate_broken row, else invalid_argument"},
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
                     "doc": "the same key file as shadow run; not read under --dry-run"},
                    {"name": STORE_VARIABLE,
                     "doc": "the store when --store is not given, then ~/.bellwether/modelgate"}
                ],
                "output": cmd_embed::SCHEMA,
                "exits": {
                    "0": "the run's first canary closed ok and no story's request failed or closed unusable, or nothing was new, or a dry run, or the deadline or the limit stopped it; a text the provider refuses changes no exit",
                    "2": "invalid_argument, input_unparseable, input_schema_mismatch, credential_key_missing, credential_key_exposed, io_failed",
                    "4": "input_absent: the snapshot or the key file",
                    "7": "vendor_unreachable (the first canary met a retryable status or no answer; no story sent), lease_held",
                    "8": "store_failed, cost_estimate_broken, cost_cap_reached, canary_not_ok, partial_embed or unusable_answer, named in the envelope's exit.error_code"
                },
                "mutates": true,
                "spends_money": true,
                "network": [EMBED_ENDPOINT]
            },
            {
                "path": ["dedup", "distribution"],
                "summary": "Per comparison version and space: stories per similarity band of their best earlier candidate, with how many of those candidates share a visible host, a ticker tag or the source and how old they are; the hosts shared most often; the canary series.",
                "options": [
                    {"name": "store", "type": "path"},
                    {"name": "format", "type": "enum", "values": ["human", "json"],
                     "default": "human"},
                    {"name": "json", "type": "flag", "doc": "alias of --format json"}
                ],
                "output": DISTRIBUTION_SCHEMA,
                "exits": {"0": "printed", "2": "io_failed, input_schema_mismatch",
                          "4": "input_absent: no embed.sqlite in the store"},
                "mutates": false
            },
            {
                "path": ["dedup", "sample"],
                "summary": "Candidate pairs for a person to label duplicate, same_matter or different: per stratum (similarity band x a shared visible host) the first pairs by sha256 of the seed and the pair. The output states no similarity and holds story text, so it never enters a repository.",
                "options": [
                    {"name": "seed", "type": "integer", "required": true},
                    {"name": "per_stratum", "type": "integer", "required": true},
                    {"name": "snapshot", "type": "path", "required": true},
                    {"name": "compare_version", "type": "string",
                     "doc": "a comparison version's token as dedup distribution prints it"},
                    {"name": "space", "type": "string",
                     "doc": "a space's name as dedup distribution prints it"},
                    {"name": "store", "type": "path"},
                    {"name": "format", "type": "enum", "values": ["human", "json"],
                     "default": "human"},
                    {"name": "json", "type": "flag", "doc": "alias of --format json"}
                ],
                "output": SAMPLE_SCHEMA,
                "exits": {"0": "printed",
                          "2": "invalid_argument (the comparisons left by --compare-version and --space are of several versions or spaces; the message names each), input_unparseable, input_schema_mismatch, io_failed",
                          "4": "input_absent: no embed.sqlite in the store, or no snapshot"},
                "mutates": false
            },
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
            {"code": "partial_embed", "exit": EXIT_INCOMPLETE, "retryable": false},
            {"code": "canary_not_ok", "exit": EXIT_INCOMPLETE, "retryable": false},
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

/// The store, or the refusal already written to stderr.
fn store_or_refusal(given: Option<PathBuf>, format: Format) -> Result<PathBuf, u8> {
    store_root(given).ok_or_else(|| {
        let message = format!("no --store, no {STORE_VARIABLE} and no HOME");
        fail(
            "invalid_argument",
            EXIT_USAGE,
            false,
            &message,
            &json!({}),
            format,
        )
    })
}

/// A dry run's exchange with no key and no socket, or the wire `live` builds from the key file.
fn wire_or_refusal(
    dry_run: bool,
    live: fn(ApiKey) -> ModelWire,
    format: Format,
) -> Result<Box<dyn ModelExchange>, u8> {
    if dry_run {
        return Ok(Box::new(NoWire));
    }
    match ApiKey::read(|name| std::env::var_os(name)) {
        Ok(key) => Ok(Box::new(live(key))),
        Err(failure) => Err(fail(
            failure.code(),
            key_exit(&failure),
            false,
            &failure.to_string(),
            &json!({}),
            format,
        )),
    }
}

fn finished(done: Result<cmd_shadow::Finished, ShadowError>, format: Format) -> u8 {
    match done {
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

fn embed_run(run: EmbedArgs) -> u8 {
    let format = if run.json { Format::Json } else { run.format };
    let store = match store_or_refusal(run.store, format) {
        Ok(store) => store,
        Err(exit) => return exit,
    };
    let wire = match wire_or_refusal(run.dry_run, ModelWire::embed, format) {
        Ok(wire) => wire,
        Err(exit) => return exit,
    };
    let args = cmd_embed::Args {
        snapshot: run.snapshot,
        model: run.model,
        provider: run.provider,
        quantization: run.quantization,
        epoch: run.epoch,
        run_cap_usd: run.run_cap_usd,
        day_cap_usd: run.day_cap_usd,
        total_cap_usd: run.total_cap_usd,
        input_price_per_mtok: run.input_price_per_mtok,
        give_up_after_hours: run.give_up_after_hours,
        limit: run.limit,
        deadline: run.deadline,
        store,
        ack_cost_model: run.ack_cost_model,
        dry_run: run.dry_run,
    };
    let env = Env {
        exchange: wire.as_ref(),
        now: &Utc::now,
        sleep: &std::thread::sleep,
    };
    finished(cmd_embed::run(&args, &env), format)
}

/// A read-only report: the value on stdout at exit 0, or the refusal.
fn reported(found: Result<Value, ShadowError>, format: Format) -> u8 {
    finished(
        found.map(|envelope| cmd_shadow::Finished {
            envelope,
            exit: EXIT_OK,
        }),
        format,
    )
}

fn dedup(command: Dedup) -> u8 {
    match command {
        Dedup::Distribution(args) => {
            let format = if args.json { Format::Json } else { args.format };
            match store_or_refusal(args.store, format) {
                Ok(store) => reported(cmd_dedup::distribution(&store), format),
                Err(exit) => exit,
            }
        }
        Dedup::Sample(args) => {
            let format = if args.json { Format::Json } else { args.format };
            let store = match store_or_refusal(args.store, format) {
                Ok(store) => store,
                Err(exit) => return exit,
            };
            let sample = SampleArgs {
                seed: args.seed,
                per_stratum: args.per_stratum,
                snapshot: args.snapshot,
                store,
                compare_version: args.compare_version,
                space: args.space,
            };
            reported(cmd_dedup::sample(&sample), format)
        }
    }
}

fn shadow_run(run: RunArgs) -> u8 {
    let format = if run.json { Format::Json } else { run.format };
    let store = match store_or_refusal(run.store, format) {
        Ok(store) => store,
        Err(exit) => return exit,
    };
    let wire = match wire_or_refusal(run.dry_run, ModelWire::new, format) {
        Ok(wire) => wire,
        Err(exit) => return exit,
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
    finished(cmd_shadow::run(&args, &env), format)
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
        Command::Embed(Embed::Run(run)) => embed_run(*run),
        Command::Dedup(command) => dedup(command),
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
        assert_eq!(checked, 5);
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
