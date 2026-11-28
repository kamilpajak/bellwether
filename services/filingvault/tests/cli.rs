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

/// Row N1: the binary every test here spawns refuses any host but the loopback, so a test whose
/// lease, flag or gate is mutated away still sends nothing to SEC. Only the guard holds the text.
/// Mutation 2026-11-27, `loopback-only` dropped from this crate's dev-dependency: red.
#[test]
fn the_binary_the_tests_spawn_sends_to_loopback_only() -> TestResult {
    let binary = std::fs::read(env!("CARGO_BIN_EXE_filingvault"))?;
    let guard = news_provider::LOOPBACK_ONLY_REFUSAL.as_bytes();
    assert!(binary.windows(guard.len()).any(|bytes| bytes == guard));
    Ok(())
}

/// Mutations 2026-11-27, each red here: `lease_held` left out of `ErrorCode::ALL`; no code
/// retryable.
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
    let codes: Vec<(String, u64, bool)> = schema["error_codes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|code| {
            (
                code["code"].as_str().unwrap_or_default().to_owned(),
                code["exit"].as_u64().unwrap_or_default(),
                code["retryable"].as_bool().unwrap_or_default(),
            )
        })
        .collect();
    let expected: Vec<(String, u64, bool)> = [
        ("invalid_argument", 2, false),
        ("input_absent", 4, false),
        ("input_unparseable", 2, false),
        ("input_schema_mismatch", 2, false),
        ("holidays_uncovered", 2, false),
        ("credential_contact_missing", 2, false),
        ("io_failed", 2, false),
        ("pace_root_split", 2, false),
        ("lease_held", 7, true),
        ("robots_stopped", 7, true),
        ("traffic_stopped", 7, true),
        ("gate_refused", 7, true),
        ("partial_collect", 8, false),
    ]
    .map(|(code, exit, retryable)| (code.to_owned(), exit, retryable))
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

/// Everything but the mutation: the plan comes back, says the store is absent, and no store,
/// lease or log is made. Mutations 2026-11-27, each red here: the dry run opening the writer;
/// `store` left out of the report (review EF4).
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
            report["store"].clone(),
            report["planned"]["listings"]
                .as_array()
                .map(Vec::len)
                .unwrap_or_default()
                > 4,
            store.exists(),
            std::fs::read_dir(root.path())?.count(),
        ),
        (
            Some(0),
            serde_json::json!(true),
            serde_json::json!("absent"),
            true,
            false,
            0
        )
    );
    Ok(())
}

/// A rules file the plan cannot use is refused before any store or request: a form holding a
/// space (the index's form is its first word, so it never matches) and a `fill_days` past the
/// calendar's start. Before 2026-11-27 the first planned no row and the second panicked, exit 101.
#[test]
fn a_dry_run_refuses_a_spaced_form_and_a_fill_window_past_the_calendar() -> TestResult {
    let temp = TempDir::new()?;
    let root = TempDir::new()?;
    let mut refused = Vec::new();
    for (forms, fill_days) in [(r#"["DEF 14A"]"#, "456"), (r#"["10-K"]"#, "4294967295")] {
        let rules = temp.path().join("rules.json");
        std::fs::write(
            &rules,
            format!(
                r#"{{"schema": "filingvault.rules/1", "forms": {forms}, "fill_days": {fill_days}}}"#
            ),
        )?;
        let out = filingvault(&[
            "filings",
            "collect",
            "--store",
            text(&temp.path().join("store")),
            "--pace-root",
            text(root.path()),
            "--rules",
            text(&rules),
            "--dry-run",
            "--format",
            "json",
        ])?;
        let code =
            serde_json::from_slice::<Value>(&out.stderr).map(|object| object["code"].clone());
        refused.push((out.status.code(), out.stdout.len(), code.ok()));
    }
    let expected = (Some(2), 0, Some(serde_json::json!("input_unparseable")));
    assert_eq!(refused, vec![expected.clone(), expected]);
    Ok(())
}

/// A refusal of the parser is the same object as any other under `--format json`, and help is
/// still a result (review of 2026-11-27, F6). Before then clap wrote its own text, no `code`.
#[test]
fn a_parser_refusal_is_an_invalid_argument_object_and_help_is_a_result() -> TestResult {
    let refused = filingvault(&[
        "ledger", "check", "--at", "notadate", "--store", "s", "--rules", "r", "--format", "json",
    ])?;
    let help = filingvault(&["ledger", "check", "--help"])?;
    let object: Value = serde_json::from_slice(&refused.stderr)?;
    assert_eq!(
        (
            refused.status.code(),
            refused.stdout.len(),
            object["code"].clone(),
            object["retryable"].clone(),
            help.status.code(),
            help.stdout.is_empty(),
        ),
        (
            Some(2),
            0,
            serde_json::json!("invalid_argument"),
            serde_json::json!(false),
            Some(0),
            false
        )
    );
    Ok(())
}

/// `ledger check` reads its holiday list before the store: a missing list is `input_absent`
/// naming the list, never a list of no holiday (owner decision 1 of 2026-11-27, R6). Mutation
/// 2026-11-27, `--holidays` ignored and the committed list read: red here.
#[test]
fn ledger_check_refuses_a_missing_holiday_list_before_the_store() -> TestResult {
    let temp = TempDir::new()?;
    let absent = temp.path().join("no-holidays.json");
    let out = filingvault(&[
        "ledger",
        "check",
        "--at",
        "2026-11-20T00:00:00Z",
        "--store",
        text(temp.path()),
        "--rules",
        RULES,
        "--holidays",
        text(&absent),
        "--format",
        "json",
    ])?;
    let object: Value = serde_json::from_slice(&out.stderr)?;
    assert_eq!(
        (
            out.status.code(),
            object["code"].clone(),
            object["message"]
                .as_str()
                .is_some_and(|message| message.contains("no-holidays.json")),
        ),
        (Some(4), serde_json::json!("input_absent"), true)
    );
    Ok(())
}
