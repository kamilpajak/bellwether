//! The `modelgate` binary's contract: streams, exits and the error object. No test here opens a
//! socket: each is a dry run, or is refused at the key file, and the three key cases also name
//! an alias model, which is refused before any row if a key defect ever lets one through.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const KEY_FILE_VARIABLE: &str = "BELLWETHER_MODEL_KEY_FILE";
/// Dated before the fixture's observations of 2026-07-01 to 2026-07-03.
const MODEL: &str = "example/decide-1-20260101";

fn repo(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}

fn modelgate(args: &[&str], key_file: Option<&Path>) -> TestResult<Output> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_modelgate"));
    command.args(args).env_remove(KEY_FILE_VARIABLE);
    if let Some(path) = key_file {
        command.env(KEY_FILE_VARIABLE, path);
    }
    Ok(command.output()?)
}

fn key_file(dir: &TempDir, mode: u32) -> TestResult<PathBuf> {
    let path = dir.path().join("model-key");
    std::fs::write(&path, "not-a-real-key\n")?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
    Ok(path)
}

fn run_args<'a>(
    store: &'a str,
    snapshot: &'a str,
    questions: &'a str,
    model: &'a str,
) -> Vec<&'a str> {
    vec![
        "shadow",
        "run",
        "--snapshot",
        snapshot,
        "--questions",
        questions,
        "--model",
        model,
        "--run-cap-usd",
        "0.40",
        "--day-cap-usd",
        "1.50",
        "--total-cap-usd",
        "40",
        "--input-price-per-mtok",
        "0.042",
        "--give-up-after-hours",
        "24",
        "--store",
        store,
        "--from-seq",
        "0",
        "--dry-run",
        "--non-interactive",
        "--json",
    ]
}

fn error_code(output: &Output) -> TestResult<String> {
    let object: Value = serde_json::from_slice(&output.stderr)?;
    assert_eq!(object["schema"], "modelgate.error/1");
    assert!(object["suggestions"].is_array());
    Ok(object["code"].as_str().unwrap_or_default().to_owned())
}

#[test]
fn schema_prints_one_json_value_and_one_command_when_asked_for_it() -> TestResult {
    let whole = modelgate(&["schema"], None)?;
    let doc: Value = serde_json::from_slice(&whole.stdout)?;
    assert_eq!(whole.status.code(), Some(0));
    assert_eq!(doc["schema"], "modelgate.schema/1");
    let one = modelgate(&["schema", "shadow", "run"], None)?;
    let command: Value = serde_json::from_slice(&one.stdout)?;
    assert_eq!(command["output"], "modelgate.shadow_run/1");
    let none = modelgate(&["schema", "shadow", "stats"], None)?;
    assert_eq!((none.status.code(), none.stdout.len()), (Some(2), 0));
    Ok(())
}

#[test]
fn help_carries_the_fixed_headings_at_both_levels() -> TestResult {
    for args in [&["--help"][..], &["shadow", "run", "--help"][..]] {
        let help = String::from_utf8(modelgate(args, None)?.stdout)?;
        for heading in ["USAGE:", "OPTIONS:", "OUTPUT:", "EXIT CODES:", "EXAMPLES:"] {
            assert!(help.contains(heading), "{args:?} lacks {heading}\n{help}");
        }
    }
    Ok(())
}

/// The committed instrument over the committed fixture: seq 1 and 4 carry a body, seq 2 does not.
/// No key variable is set: mutation 2026-11-23, the key read before the `dry_run` branch: red.
#[test]
fn a_dry_run_over_the_committed_fixture_plans_two_requests_and_needs_no_key() -> TestResult {
    let dir = TempDir::new()?;
    let store = dir.path().join("gate");
    let (snapshot, questions) = (
        repo("snapshot-contract/fixtures/snapshot_v1"),
        repo("services/modelgate/questions/ripple-shadow-0.json"),
    );
    let (store_text, snapshot, questions) = (
        store.to_string_lossy(),
        snapshot.to_string_lossy(),
        questions.to_string_lossy(),
    );
    let output = modelgate(&run_args(&store_text, &snapshot, &questions, MODEL), None)?;
    let envelope: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(output.stderr.is_empty());
    assert_eq!(envelope["schema"], "modelgate.shadow_run/1");
    assert_eq!(envelope["dry_run"], true);
    assert_eq!(envelope["requests"]["planned"], 2);
    assert_eq!(envelope["stories"]["no_body"], 1);
    assert_eq!(envelope["question_document"], "ripple-shadow/0");
    assert!(!store.exists());
    Ok(())
}

#[test]
fn each_refusal_before_the_store_has_an_empty_stdout_its_code_and_its_exit() -> TestResult {
    let dir = TempDir::new()?;
    let (sound, exposed_dir) = (key_file(&dir, 0o600)?, TempDir::new()?);
    let exposed = key_file(&exposed_dir, 0o644)?;
    let store = dir.path().join("gate").to_string_lossy().into_owned();
    let snapshot = repo("snapshot-contract/fixtures/snapshot_v1");
    let snapshot = snapshot.to_string_lossy();
    let questions = repo("services/modelgate/questions/ripple-shadow-0.json");
    let questions = questions.to_string_lossy();
    let absent = dir.path().join("no-key");
    let sending = || {
        let mut args = run_args(&store, &snapshot, &questions, "~example/decide-latest");
        args.retain(|word| *word != "--dry-run");
        args
    };
    let cases: [(Vec<&str>, Option<&Path>, &str, i32); 5] = [
        (sending(), None, "credential_key_missing", 2),
        (sending(), Some(&exposed), "credential_key_exposed", 2),
        (sending(), Some(&absent), "input_absent", 4),
        (
            run_args(&store, &snapshot, &questions, "~example/decide-latest"),
            Some(&sound),
            "invalid_argument",
            2,
        ),
        (
            run_args(&store, "/nonexistent/cut", &questions, MODEL),
            Some(&sound),
            "input_absent",
            4,
        ),
    ];
    for (args, key, code, exit) in cases {
        let output = modelgate(&args, key)?;
        assert_eq!(output.status.code(), Some(exit), "{code}");
        assert!(output.stdout.is_empty(), "{code}");
        assert_eq!(error_code(&output)?, code);
    }
    assert!(!dir.path().join("gate").exists());
    Ok(())
}

/// C4: no cap, price or age limit has a default. Under `--json` the parser's refusal is the
/// error object. Mutations 2026-11-23, each red: a `default_value` on `--total-cap-usd`;
/// `Cli::parse` in place of `try_parse` and `usage`.
#[test]
fn leaving_out_a_required_option_is_a_usage_error_object_under_json() -> TestResult {
    let dir = TempDir::new()?;
    let store = dir.path().join("gate").to_string_lossy().into_owned();
    let snapshot = repo("snapshot-contract/fixtures/snapshot_v1");
    let snapshot = snapshot.to_string_lossy();
    let questions = repo("services/modelgate/questions/ripple-shadow-0.json");
    let questions = questions.to_string_lossy();
    for option in [
        "--run-cap-usd",
        "--day-cap-usd",
        "--total-cap-usd",
        "--input-price-per-mtok",
        "--give-up-after-hours",
    ] {
        let mut args = run_args(&store, &snapshot, &questions, MODEL);
        let Some(at) = args.iter().position(|word| *word == option) else {
            return Err(format!("{option} is not in the base arguments").into());
        };
        args.drain(at..at + 2);
        let output = modelgate(&args, None)?;
        assert_eq!(output.status.code(), Some(2), "{option}");
        assert!(output.stdout.is_empty(), "{option}");
        assert_eq!(error_code(&output)?, "invalid_argument", "{option}");
    }
    assert!(!dir.path().join("gate").exists());
    Ok(())
}
