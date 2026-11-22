//! `snapshot-contract` - the Snapshot contract on the command line, so a reader outside Rust runs
//! this validator instead of a second copy of its checks (`docs/design/architecture.md` section
//! 3.0). Same CLI contract as `panel-contract`: the result on stdout, errors on stderr.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

use snapshot_contract::{
    data_schema, validate, Finding, Rule, ValidateError, DATA_FILE, FINDING_CODES, MANIFEST_FILE,
    MANIFEST_SCHEMA, PROVENANCES, ROW_GROUP_BYTES,
};

const EXIT_INVALID: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_NOT_FOUND: u8 = 4;

#[derive(Parser)]
#[command(
    name = "snapshot-contract",
    about = "The Snapshot contract: validate one snapshot directory, or print the shape.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate one snapshot directory. Exit 0 when it is admissible, 1 when it is not.
    Validate {
        /// The directory holding manifest.json and snapshot.parquet.
        dir: PathBuf,
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
    },
    /// Machine-readable description of this tool and of the contract.
    Schema {
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Human,
    Json,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Schema { format } => {
            print_schema(format);
            ExitCode::SUCCESS
        }
        Command::Validate { dir, format } => match validate(&dir) {
            Ok(findings) => match report(&dir, &findings, format) {
                Ok(()) if findings.is_empty() => ExitCode::SUCCESS,
                Ok(()) => ExitCode::from(EXIT_INVALID),
                Err(why) => fail(EXIT_USAGE, &format!("writing the result failed: {why}")),
            },
            Err(failure) => fail(
                match failure {
                    ValidateError::Absent(_) => EXIT_NOT_FOUND,
                    ValidateError::NotADirectory(_) | ValidateError::Io { .. } => EXIT_USAGE,
                },
                &failure.to_string(),
            ),
        },
    }
}

fn fail(code: u8, text: &str) -> ExitCode {
    #[allow(
        clippy::let_underscore_must_use,
        reason = "reporting an error must not fail on a closed stderr; the exit code is the verdict"
    )]
    let _ = writeln!(std::io::stderr(), "error: {text}");
    ExitCode::from(code)
}

fn report(dir: &Path, findings: &[Finding], format: Format) -> std::io::Result<()> {
    let out = std::io::stdout();
    let mut out = out.lock();
    match format {
        Format::Json => {
            serde_json::to_writer_pretty(
                &mut out,
                &serde_json::json!({
                    "schema": "snapshot-contract.validate/1",
                    "dir": dir.display().to_string(),
                    "contract": MANIFEST_SCHEMA,
                    "admissible": findings.is_empty(),
                    "finding_count": findings.len(),
                    "findings": findings,
                }),
            )?;
            writeln!(out)?;
        }
        Format::Human if findings.is_empty() => writeln!(out, "admissible  {}", dir.display())?,
        Format::Human => {
            writeln!(
                out,
                "NOT admissible  {}  ({} finding(s))",
                dir.display(),
                findings.len()
            )?;
            for finding in findings {
                writeln!(out, "  {}  {}", finding.code, finding.message)?;
            }
        }
    }
    Ok(())
}

fn print_schema(format: Format) {
    let doc = serde_json::json!({
        "schema": "snapshot-contract.schema/1",
        "tool": "snapshot-contract",
        "contract": MANIFEST_SCHEMA,
        "commands": [
            {
                "path": ["validate"],
                "summary": "Validate one snapshot directory.",
                "args": [{"name": "dir", "type": "path", "required": true}],
                "options": [{"name": "format", "type": "enum", "values": ["human", "json"]}],
                "output": "snapshot-contract.validate/1",
                "mutates": false
            },
            {"path": ["schema"], "summary": "This document.", "mutates": false}
        ],
        "files": [MANIFEST_FILE, DATA_FILE],
        "manifest_fields": [
            {"name": "schema", "type": "string", "value": MANIFEST_SCHEMA},
            {"name": "snapshot_id", "type": "string", "doc": "64 lower-case hex, canon's digest text form"},
            {"name": "cutoff", "type": "string", "doc": "RFC 3339 UTC, exactly six fractional digits"},
            {"name": "seq_high_water", "type": "integer", "doc": "at least 1; the seq bound the rows were selected under"},
            {"name": "items", "type": "integer", "doc": "at least 1; the row count"},
            {"name": "provenance_counts", "type": "object", "keys": PROVENANCES, "doc": "summing to items"},
            {"name": "data_sha256", "type": "string", "doc": "sha256 of the data file's bytes"}
        ],
        "data_columns": data_schema().fields().iter().map(|field| serde_json::json!({
            "name": field.name(),
            "type": field.data_type().to_string(),
            "nullable": field.is_nullable(),
        })).collect::<Vec<_>>(),
        "data_rules": Rule::ALL.map(|rule| serde_json::json!({
            "data_schema": rule.data_schema(),
            "tzdb": rule.tzdb(),
            "current": rule == Rule::CURRENT,
        })),
        "compression": "ZSTD on every column chunk",
        "row_group_bytes": ROW_GROUP_BYTES,
        "finding_codes": FINDING_CODES,
        "exit_codes": {"0": "admissible", "1": "not admissible", "2": "usage, io", "4": "no such directory"}
    });
    match format {
        Format::Human => println!("{doc:#}"),
        Format::Json => println!("{doc}"),
    }
}
