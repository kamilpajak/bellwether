//! What the CI size step assumes about a source file: the lines before its first `#[cfg(test)]`
//! are source and everything after is test. A file holding a SECOND marker breaks the assumption
//! silently, so rule 1 of `CONTRIBUTING.md` then reports a ratio nobody can read.
//!
//! The same count runs inside `.github/workflows/ci.yml`'s size step, which is the deliberate
//! second reader: the step is what fails a push, and this gate is what `cargo test` can show.

use std::path::Path;

use crate::clocks::rust_files;

/// The marker the size step stops counting source at, as that step's own `awk` matches it:
/// the attribute at the start of a line, with any leading indentation.
const MARKER: &str = "#[cfg(test)]";
const ROW: &str = "CONTRIBUTING.md rule 1, measured by the size step of .github/workflows/ci.yml";

/// A file and how many markers it holds. Only the ones above one are reported.
#[derive(Debug, PartialEq, Eq)]
pub struct Markers {
    /// Path relative to the workspace root, spelled with `/`.
    pub file: String,
    pub markers: usize,
}

/// Every `.rs` file under `root` holding more than one marker. `rust_files` skips `target/`, so
/// the file set is the size step's own; a `.rs` file outside every member is counted by both.
pub fn files_with_several_markers(root: &Path) -> Result<Vec<Markers>, String> {
    let mut found: Vec<Markers> = Vec::new();
    for file in rust_files(root)? {
        let text = std::fs::read_to_string(&file)
            .map_err(|why| format!("{} could not be read: {why}", file.display()))?;
        let markers = text
            .lines()
            .filter(|line| line.trim_start() == MARKER)
            .count();
        if markers > 1 {
            found.push(Markers {
                file: file
                    .strip_prefix(root)
                    .unwrap_or(&file)
                    .to_string_lossy()
                    .replace('\\', "/"),
                markers,
            });
        }
    }
    found.sort_by(|left, right| left.file.cmp(&right.file));
    Ok(found)
}

/// The check: `None` when every file holds at most one marker.
pub fn several_cfg_test_markers_failure(root: &Path) -> Option<String> {
    let found = match files_with_several_markers(root) {
        Ok(found) => found,
        Err(why) => return Some(format!("{why}\n\nThe invariant is {ROW}.\n")),
    };
    if found.is_empty() {
        return None;
    }
    Some(format!(
        concat!(
            "{} file(s) hold more than one `{}`:\n\n{}\n\n",
            "Why this matters: the size step counts the lines BEFORE a file's first marker as ",
            "source\nand the rest as test, so a second marker moves every source line below it ",
            "into the test\ncolumn and the ratio stops meaning what it says. Put the test items ",
            "in one\n`#[cfg(test)] mod` at the end of the file; a helper several modules need can ",
            "be\n`pub(crate)` inside it, which is what `news-provider/src/clock.rs` does. ",
            "The invariant is\n{}.\n",
        ),
        found.len(),
        MARKER,
        found
            .iter()
            .map(|at| format!("    * {} holds {}", at.file, at.markers))
            .collect::<Vec<String>>()
            .join("\n"),
        ROW,
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::path::PathBuf;

    /// Assembled, so this file does not count as an offender against its own gate.
    fn marker() -> String {
        format!("#[cfg{}]\n", "(test)")
    }

    fn workspace_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf()
    }

    /// Over this tree. `news-provider/src/clock.rs` held four markers until 2026-11-13 and was
    /// the only offender; its test items are one `pub(crate) mod testing` now.
    #[test]
    fn every_file_holds_at_most_one_cfg_test_marker_so_the_size_step_measures_right() {
        let failure = several_cfg_test_markers_failure(&workspace_root()).unwrap_or_default();
        assert!(failure.is_empty(), "\n\n{failure}\n");
    }

    #[test]
    fn a_file_holding_two_cfg_test_lines_fails_the_size_step() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("crate-a/src")).unwrap();
        std::fs::write(
            dir.path().join("crate-a/src/lib.rs"),
            format!(
                "pub fn a() {{}}\n{}struct T;\npub fn b() {{}}\n{}mod tests {{}}\n",
                marker(),
                marker()
            ),
        )
        .unwrap();
        let failure =
            several_cfg_test_markers_failure(dir.path()).expect("expected a failure, found none");
        assert!(failure.contains("crate-a/src/lib.rs holds 2"), "{failure}");
        assert!(failure.contains("into the test\ncolumn"), "{failure}");
    }

    #[test]
    fn one_marker_at_the_end_of_a_file_is_the_shape_the_step_measures_right() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("crate-a/src")).unwrap();
        std::fs::write(
            dir.path().join("crate-a/src/lib.rs"),
            format!("pub fn a() {{}}\n{}mod tests {{}}\n", marker()),
        )
        .unwrap();
        assert_eq!(several_cfg_test_markers_failure(dir.path()), None);
    }

    /// `target/` holds generated sources the ratio does not count, and scanning it would make
    /// this gate's answer depend on what was built.
    #[test]
    fn a_marker_under_target_is_not_an_offender() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("target/debug/build/x")).unwrap();
        std::fs::write(
            dir.path().join("target/debug/build/x/out.rs"),
            format!("{}mod a {{}}\n{}mod b {{}}\n", marker(), marker()),
        )
        .unwrap();
        assert_eq!(several_cfg_test_markers_failure(dir.path()), None);
    }
}
