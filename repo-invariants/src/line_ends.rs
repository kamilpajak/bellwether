//! Files whose bytes feed a stored or committed digest hold no `\r`. `include_str!` embeds a
//! `.rs` file's working-tree bytes, `\r` included, while rustc compiles CRLF and LF alike, so a
//! CRLF checkout moves `extract_version` without changing what runs (rustc 1.99.1, 2026-11-16).
//! `.gitattributes` stores these files as LF; this gate reads the working-tree bytes, which an
//! editor saving CRLF changes before git normalises them on commit. Row: architecture.md §8 G2.

use std::path::{Path, PathBuf};

const ROW: &str = "docs/design/architecture.md section 8 row G2";

/// Each path and the digest it feeds. A directory stands for every non-hidden file under it.
pub const HASHED_INPUTS: [(&str, &str); 5] = [
    (
        "providers/open-web-feed/src",
        "open-web-feed `extract_version`, through `include_str!` in `version.rs`",
    ),
    (
        "providers/open-web-feed/rules/open-web-feed.rules.json",
        "open-web-feed `extract_rules_version` and `extract_version`",
    ),
    (
        "providers/open-web-feed/hosts",
        "open-web-feed `hosts_version`",
    ),
    (
        "providers/edgar-press/rules/edgar-press.rules.json",
        "edgar-press `extract_rules_version`",
    ),
    (
        "news-provider/policy/observation-lag.json",
        "`LagPolicy` version",
    ),
];

/// The files a listed path stands for, sorted. `Err` when the path is absent or a directory
/// holds no file, so a renamed input is a named failure and never an empty pass.
fn files_of(root: &Path, listed: &str) -> Result<Vec<PathBuf>, String> {
    let path = root.join(listed);
    if path.is_file() {
        return Ok(vec![path]);
    }
    if !path.is_dir() {
        return Err(format!(
            "{listed} is listed as a hashed input and does not exist"
        ));
    }
    let mut found: Vec<PathBuf> = Vec::new();
    let mut pending = vec![path];
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory)
            .map_err(|why| format!("{} could not be listed: {why}", directory.display()))?;
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let child = entry.path();
            if child.is_dir() {
                pending.push(child);
            } else {
                found.push(child);
            }
        }
    }
    if found.is_empty() {
        return Err(format!(
            "{listed} is listed as a hashed input and holds no file"
        ));
    }
    found.sort();
    Ok(found)
}

/// The check: `None` when every file `inputs` names exists and holds no `\r` byte.
pub fn carriage_return_failure(root: &Path, inputs: &[(&str, &str)]) -> Option<String> {
    let mut offenders: Vec<String> = Vec::new();
    for (listed, feeds) in inputs {
        let files = match files_of(root, listed) {
            Ok(files) => files,
            Err(why) => return Some(format!("{why}\n\nThe invariant is {ROW}.\n")),
        };
        for file in files {
            let bytes = match std::fs::read(&file) {
                Ok(bytes) => bytes,
                Err(why) => {
                    return Some(format!(
                        "{} could not be read: {why}\n\nThe invariant is {ROW}.\n",
                        file.display()
                    ))
                }
            };
            let returns = bytes.iter().filter(|byte| **byte == b'\r').count();
            if returns > 0 {
                let shown = file.strip_prefix(root).unwrap_or(&file).to_string_lossy();
                offenders.push(format!(
                    "    * {} holds {returns} `\\r` byte(s); it feeds {feeds}",
                    shown.replace('\\', "/")
                ));
            }
        }
    }
    if offenders.is_empty() {
        return None;
    }
    Some(format!(
        concat!(
            "{} hashed input(s) hold a carriage return:\n\n{}\n\n",
            "A CRLF copy of a file whose bytes are digested gives another version digest for the ",
            "same\ncontent. Save the file with LF line ends. The invariant is {}.\n",
        ),
        offenders.len(),
        offenders.join("\n"),
        ROW,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    type Outcome = Result<(), Box<dyn std::error::Error>>;

    fn workspace_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf()
    }

    /// Over this tree. Mutation 2026-11-16: the hosts file converted to CRLF turned this red.
    #[test]
    fn no_hashed_input_in_this_tree_holds_a_carriage_return() {
        let failure = carriage_return_failure(&workspace_root(), &HASHED_INPUTS);
        assert_eq!(failure, None);
    }

    #[test]
    fn a_crlf_file_under_a_listed_directory_is_named_with_the_digest_it_feeds() -> Outcome {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("crate/src"))?;
        std::fs::write(
            dir.path().join("crate/src/lib.rs"),
            "fn a() {}\r\nfn b() {}\r\n",
        )?;
        std::fs::write(dir.path().join("crate/src/ok.rs"), "fn c() {}\n")?;
        let failure = carriage_return_failure(dir.path(), &[("crate/src", "the test version")]);
        let text = failure.unwrap_or_default();
        assert!(
            text.contains("crate/src/lib.rs holds 2 `\\r` byte(s); it feeds the test version")
                && !text.contains("ok.rs"),
            "{text}"
        );
        Ok(())
    }

    /// A lone `\r` (old Mac line end) counts too: the digest sees it the same way.
    #[test]
    fn a_lone_carriage_return_in_a_listed_file_fails() -> Outcome {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("rules.json"), "{\r}\n")?;
        let failure = carriage_return_failure(dir.path(), &[("rules.json", "x")]);
        assert!(failure.is_some_and(|text| text.contains("rules.json holds 1")));
        Ok(())
    }

    #[test]
    fn a_listed_path_that_is_absent_or_an_empty_directory_fails_rather_than_passes() -> Outcome {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("empty"))?;
        let absent = carriage_return_failure(dir.path(), &[("gone.json", "x")]);
        let empty = carriage_return_failure(dir.path(), &[("empty", "x")]);
        assert!(absent.is_some_and(|text| text.contains("gone.json is listed")));
        assert!(empty.is_some_and(|text| text.contains("empty is listed")));
        Ok(())
    }

    /// The positive control: LF files, a hidden CRLF file skipped, and a nested directory read.
    #[test]
    fn lf_files_and_a_hidden_crlf_file_pass() -> Outcome {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("src/nested"))?;
        std::fs::write(dir.path().join("src/a.rs"), "fn a() {}\n")?;
        std::fs::write(dir.path().join("src/nested/b.rs"), "fn b() {}\n")?;
        std::fs::write(dir.path().join("src/.DS_Store"), "\r\n")?;
        assert_eq!(carriage_return_failure(dir.path(), &[("src", "x")]), None);
        Ok(())
    }

    /// A CRLF file nested under a listed directory is found, not only one at its top level.
    #[test]
    fn a_crlf_file_in_a_nested_directory_fails() -> Outcome {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("src/nested"))?;
        std::fs::write(dir.path().join("src/a.rs"), "fn a() {}\n")?;
        std::fs::write(dir.path().join("src/nested/b.rs"), "fn b() {}\r\n")?;
        let failure = carriage_return_failure(dir.path(), &[("src", "x")]);
        assert!(failure.is_some_and(|text| text.contains("src/nested/b.rs holds 1")));
        Ok(())
    }
}
