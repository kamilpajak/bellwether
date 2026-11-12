//! `panel-contract` - the panel manifest contract on the command line.
//!
//! It exists so the non-Rust side of the boundary can use the same validator the Rust side will.
//! Section 3.4 of `docs/design/architecture.md` keeps statistics in Python on the far side of a
//! file boundary; a contract that only Rust can check would be re-implemented there, and two
//! implementations of one rule set is how the rule set stops being one.
//!
//! Follows the repo's CLI contract: the result goes to stdout and nothing else does, errors go to
//! stderr with a non-zero exit, JSON mode emits exactly one value, and `schema` prints the
//! command tree and the field table for a caller that is not a human.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

use panel_contract::{validate, Finding, Need, FIELDS, SCHEMA};

/// Exit codes, kept small and documented. Domain detail belongs in the finding's `code`.
const EXIT_INVALID: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_NOT_FOUND: u8 = 4;

#[derive(Parser)]
#[command(
    name = "panel-contract",
    about = "The panel manifest contract: validate one manifest, or print the shape.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate one panel manifest. Exit 0 when it is admissible, 1 when it is not.
    Validate {
        /// Path to the manifest JSON that sits beside the panel's Arrow file.
        manifest: PathBuf,
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
    },
    /// Machine-readable description of this tool and of the manifest field table.
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
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(message) => {
            // The exit code carries the verdict, so a closed stderr must not turn an error path
            // into a panic or change it.
            #[allow(
                clippy::let_underscore_must_use,
                reason = "reporting an error must not fail on a closed stderr; the exit code is the verdict"
            )]
            let _ = writeln!(std::io::stderr(), "error: {}", message.text);
            ExitCode::from(message.code)
        }
    }
}

/// An error on the way to a verdict, as opposed to a verdict of "not admissible".
struct Failure {
    code: u8,
    text: String,
}

fn run(cli: Cli) -> Result<ExitCode, Failure> {
    match cli.command {
        Command::Schema { format } => {
            print_schema(format);
            Ok(ExitCode::SUCCESS)
        }
        Command::Validate { manifest, format } => {
            let text = std::fs::read_to_string(&manifest).map_err(|why| Failure {
                // A missing manifest is not an invalid manifest, and R6 says a missing input
                // aborts rather than becoming an empty result - which here would be a pass.
                code: if why.kind() == std::io::ErrorKind::NotFound {
                    EXIT_NOT_FOUND
                } else {
                    EXIT_USAGE
                },
                text: format!("{}: {why}", manifest.display()),
            })?;
            let parsed = serde_json::from_str(&text).map_err(|why| Failure {
                code: EXIT_USAGE,
                text: format!("{} is not JSON: {why}", manifest.display()),
            })?;
            let findings = validate(&parsed);
            report(&manifest, &findings, format).map_err(|why| Failure {
                code: EXIT_USAGE,
                text: format!("writing the result failed: {why}"),
            })?;
            Ok(if findings.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(EXIT_INVALID)
            })
        }
    }
}

fn report(manifest: &Path, findings: &[Finding], format: Format) -> std::io::Result<()> {
    let out = std::io::stdout();
    let mut out = out.lock();
    match format {
        Format::Json => {
            serde_json::to_writer_pretty(
                &mut out,
                &serde_json::json!({
                    "schema": "panel-contract.validate/1",
                    "manifest": manifest.display().to_string(),
                    "contract": SCHEMA,
                    "admissible": findings.is_empty(),
                    "finding_count": findings.len(),
                    "findings": findings.iter().map(|f| serde_json::json!({
                        "code": f.code,
                        "rule": f.rule,
                        "heuristic": f.heuristic,
                        "message": f.message,
                    })).collect::<Vec<_>>(),
                }),
            )?;
            writeln!(out)?;
        }
        Format::Human => {
            if findings.is_empty() {
                writeln!(out, "admissible  {}", manifest.display())?;
            } else {
                writeln!(
                    out,
                    "NOT admissible  {}  ({} finding(s))\n",
                    manifest.display(),
                    findings.len()
                )?;
                for (index, finding) in findings.iter().enumerate() {
                    writeln!(
                        out,
                        "{}. [{}] {}{}\n   {}\n",
                        index + 1,
                        finding.rule,
                        finding.code,
                        if finding.heuristic {
                            "  (heuristic)"
                        } else {
                            ""
                        },
                        finding.message
                    )?;
                }
                writeln!(
                    out,
                    "The invariant is docs/design/architecture.md section 8 row R8."
                )?;
            }
        }
    }
    Ok(())
}

fn print_schema(format: Format) {
    let doc = serde_json::json!({
        "schema": "panel-contract.schema/1",
        "tool": "panel-contract",
        "contract": SCHEMA,
        "commands": [
            {
                "path": ["validate"],
                "summary": "Validate one panel manifest.",
                "args": [{"name": "manifest", "type": "path", "required": true}],
                "options": [{"name": "format", "type": "enum", "values": ["human", "json"]}],
                "output": "panel-contract.validate/1",
                "mutates": false
            },
            {"path": ["schema"], "summary": "This document.", "mutates": false}
        ],
        "fields": FIELDS.iter().map(|field| serde_json::json!({
            "name": field.name,
            "shape": format!("{:?}", field.shape).to_lowercase(),
            "need": match field.need {
                Need::Always => "always",
                Need::WhenAttested => "when_attested",
                Need::DeferredToR9 => "deferred_to_r9",
            },
            "why": field.why,
        })).collect::<Vec<_>>(),
        "exit_codes": {"0": "admissible", "1": "not admissible", "2": "usage", "4": "not found"}
    });
    match format {
        Format::Human => println!("{doc:#}"),
        Format::Json => println!("{doc}"),
    }
}
