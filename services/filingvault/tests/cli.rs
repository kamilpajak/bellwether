//! The binary: one JSON value on stdout for 0, 1 and 8, one error object on stderr otherwise, and
//! a `schema` generated from the parser. Nothing here reaches the network: each command below
//! stops before the first request.

use std::path::Path;
use std::process::{Command, Output};

use filingvault::error::ErrorCode;
use filingvault::store::LOCK;
use serde_json::Value;
use tempfile::TempDir;
use vault_core::lease::Lease;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const CONTACT: &str = "bellwether collector contact@example.test";
const RULES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/rules/filingvault.rules.json");

fn filingvault(args: &[&str]) -> Result<Output, std::io::Error> {
    Command::new(env!("CARGO_BIN_EXE_filingvault"))
        .args(args)
        .env("BELLWETHER_FEED_CONTACT", CONTACT)
        .env_remove("BELLWETHER_FEED_CONTACT_FILE")
        .output()
}

fn text(path: &Path) -> &str {
    path.to_str().unwrap_or_default()
}

/// Mutation 2026-11-27, `lease_held` left out of `ErrorCode::ALL`: red.
#[test]
fn the_schema_names_every_command_option_exit_and_error_code() -> TestResult {
    let out = filingvault(&["schema"])?;
    let schema: Value = serde_json::from_slice(&out.stdout)?;
    let commands = schema["commands"].as_array().cloned().unwrap_or_default();
    let paths: Vec<Value> = commands
        .iter()
        .map(|command| command["path"].clone())
        .collect();
    let collect = commands
        .iter()
        .find(|command| command["path"] == serde_json::json!(["filings", "collect"]))
        .cloned()
        .unwrap_or_default();
    let pace_root = collect["options"]
        .as_array()
        .and_then(|options| options.iter().find(|option| option["name"] == "pace-root"))
        .map(|option| option["required"].clone());
    let codes: Vec<(String, u64)> = schema["error_codes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|code| {
            (
                code["code"].as_str().unwrap_or_default().to_owned(),
                code["exit"].as_u64().unwrap_or_default(),
            )
        })
        .collect();
    let expected: Vec<(String, u64)> = [
        ("invalid_argument", 2),
        ("input_absent", 4),
        ("input_unparseable", 2),
        ("input_schema_mismatch", 2),
        ("credential_contact_missing", 2),
        ("io_failed", 2),
        ("pace_root_split", 2),
        ("lease_held", 7),
        ("robots_stopped", 7),
        ("gate_refused", 7),
        ("partial_collect", 8),
    ]
    .map(|(code, exit)| (code.to_owned(), exit))
    .to_vec();
    assert_eq!(
        (
            out.status.code(),
            paths,
            pace_root,
            collect["exits"]
                .as_object()
                .map(|exits| exits.keys().cloned().collect::<Vec<String>>()),
            codes.len(),
            codes,
        ),
        (
            Some(0),
            vec![
                serde_json::json!(["filings", "collect"]),
                serde_json::json!(["ledger", "check"]),
                serde_json::json!(["schema"]),
            ],
            Some(serde_json::json!(true)),
            Some(
                vec!["0", "2", "4", "7", "8"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect()
            ),
            ErrorCode::ALL.len(),
            expected,
        )
    );
    Ok(())
}

/// Section 5.1's iff rule. Mutation 2026-11-27, the error object written to stdout: red.
#[test]
fn an_error_writes_one_object_to_stderr_and_nothing_to_stdout() -> TestResult {
    let store = TempDir::new()?;
    let root = TempDir::new()?;
    let absent = store.path().join("no-rules.json");
    let missing = filingvault(&[
        "filings",
        "collect",
        "--store",
        text(store.path()),
        "--pace-root",
        text(root.path()),
        "--rules",
        text(&absent),
        "--format",
        "json",
    ])?;
    let held = Lease::take(&store.path().join(LOCK))?;
    let leased = filingvault(&[
        "filings",
        "collect",
        "--store",
        text(store.path()),
        "--pace-root",
        text(root.path()),
        "--rules",
        RULES,
        "--format",
        "json",
    ])?;
    drop(held);
    let object = |out: &Output| -> Result<(Option<i32>, usize, Value), serde_json::Error> {
        Ok((
            out.status.code(),
            out.stdout.len(),
            serde_json::from_slice::<Value>(&out.stderr)?["code"].clone(),
        ))
    };
    assert_eq!(
        (object(&missing)?, object(&leased)?),
        (
            (Some(4), 0, serde_json::json!("input_absent")),
            (Some(7), 0, serde_json::json!("lease_held"))
        )
    );
    Ok(())
}

/// Everything but the mutation: the plan comes back, and no store, lease or log is made.
/// Mutation 2026-11-27, the dry run opening the writer: red.
#[test]
fn a_dry_run_plans_from_the_store_and_creates_nothing() -> TestResult {
    let temp = TempDir::new()?;
    let root = TempDir::new()?;
    let store = temp.path().join("store");
    let out = filingvault(&[
        "filings",
        "collect",
        "--store",
        text(&store),
        "--pace-root",
        text(root.path()),
        "--rules",
        RULES,
        "--dry-run",
        "--format",
        "json",
    ])?;
    let report: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(
        (
            out.status.code(),
            report["dry_run"].clone(),
            report["planned"]["listings"]
                .as_array()
                .map(Vec::len)
                .unwrap_or_default()
                > 4,
            store.exists(),
            std::fs::read_dir(root.path())?.count(),
        ),
        (Some(0), serde_json::json!(true), true, false, 0)
    );
    Ok(())
}
