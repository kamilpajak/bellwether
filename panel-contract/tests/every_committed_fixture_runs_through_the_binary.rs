//! Row R8's gate, run the way the other side of the file boundary runs it: the compiled
//! `panel-contract` binary over every manifest committed under `fixtures/`, on every `cargo test`.
//!
//! Why this and not a CI step. Until 2026-11-12 `panel-contract validate` was invoked by nothing -
//! not by CI, not by a probe, not by a test - while row R8's status read "enforced"; the library
//! function behind it was tested and the command was not. A test runs in the same three commands
//! `README.md` names, runs locally before a push as well as on it, and needs no decision about
//! where a produced panel's manifest sits on disk (section 3.4 names no layout, and the two
//! consumers take a file and a directory). A CI step would have bought one thing more - validating
//! a panel that `probes/panel_recount.py` actually produced - and that one needs the layout
//! decided and the probe emitting a `panel.manifest/1` first, which is requirement R9's work.
//!
//! What it pins beyond "the binary runs": the exit status **is** the verdict, the binary and the
//! library agree on every committed fixture, and a fixture cannot be added without being run.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Every committed fixture and whether the contract admits it. The fixture's name says which it is
/// meant to be; this table is what makes a fixture edited into the other verdict turn red.
const FIXTURES: [(&str, bool); 7] = [
    ("attrition_does_not_reconcile.json", false),
    ("corrected_panel_78460.json", true),
    ("full_rectangle_attested.json", true),
    ("historical_failure_78584.json", false),
    ("units_equal_rows_attested.json", true),
    ("units_equal_rows_unattested.json", false),
    ("units_exceed_rows.json", false),
];

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn validate(manifest: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_panel-contract"))
        .arg("validate")
        .arg(manifest)
        .arg("--format=json")
        .output()
        .expect("the validator binary runs")
}

/// A fixture nobody validates is a fixture that stops meaning anything, so the table above is the
/// count of the directory and not a selection from it.
#[test]
fn the_table_below_names_every_file_in_the_fixtures_directory() {
    let mut on_disk: Vec<String> = std::fs::read_dir(fixtures())
        .expect("the fixtures directory is readable")
        .map(|entry| entry.expect("a directory entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    let mut listed: Vec<String> = FIXTURES
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect();
    listed.sort();
    assert_eq!(on_disk, listed);
}

#[test]
fn the_binary_states_each_fixtures_verdict_in_its_exit_status() {
    for (name, admissible) in FIXTURES {
        let path = fixtures().join(name);
        let run = validate(&path);
        let stdout = String::from_utf8(run.stdout).expect("stdout is UTF-8");
        let report: serde_json::Value =
            serde_json::from_str(&stdout).expect("stdout is exactly one JSON value");
        assert_eq!(
            run.status.code(),
            Some(i32::from(!admissible)),
            "{name}: exit status is the verdict - 0 admissible, 1 not\n{stdout}"
        );
        assert_eq!(
            report["admissible"].as_bool(),
            Some(admissible),
            "{name}: the body and the exit status say the same thing"
        );
        assert!(
            run.stderr.is_empty(),
            "{name}: a verdict is a result on stdout, not a message on stderr: {}",
            String::from_utf8_lossy(&run.stderr)
        );
    }
}

/// The binary is the library plus an exit status, and nothing else: the findings a caller reads are
/// the findings the 21 library tests pin.
#[test]
fn the_binary_reports_the_findings_the_library_reports() {
    for (name, _) in FIXTURES {
        let path = fixtures().join(name);
        let text = std::fs::read_to_string(&path).expect("fixture is readable");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("fixture is JSON");
        let expected: Vec<&str> = panel_contract::validate(&parsed)
            .iter()
            .map(|finding| finding.code)
            .collect();

        let run = validate(&path);
        let report: serde_json::Value =
            serde_json::from_str(&String::from_utf8_lossy(&run.stdout)).expect("one JSON value");
        let got: Vec<&str> = report["findings"]
            .as_array()
            .expect("findings is an array")
            .iter()
            .map(|finding| finding["code"].as_str().expect("a code is a string"))
            .collect();
        assert_eq!(got, expected, "{name}");
    }
}

/// A manifest that is not there is not a manifest that passes: R6 at the command-line boundary,
/// and the `input_absent` exit of section 5.1's registry.
#[test]
fn a_manifest_that_is_not_there_exits_4_with_an_empty_stdout() {
    let run = validate(&fixtures().join("no_such_manifest.json"));
    assert_eq!(run.status.code(), Some(4));
    assert!(run.stdout.is_empty());
    assert!(!run.stderr.is_empty());
}
