//! No file outside a `-text` or `binary` path of `.gitattributes` holds `\r`. `include_str!` embeds a
//! `.rs` file's working-tree bytes, `\r` included, while rustc compiles CRLF and LF alike, so a
//! CRLF checkout moves `extract_version` without changing what runs (rustc 1.99.1, 2026-11-16).
//! The scan covers the whole tree, so a digested file nobody listed is still checked; the
//! exemptions are read from `.gitattributes`, so deleting a `-text` line turns this red over the
//! CRLF fixture it covered. It reads working-tree bytes, which an editor saving CRLF changes
//! before git normalises them on commit. Row: architecture.md §8 G2.

use std::path::{Path, PathBuf};

const ROW: &str = "docs/design/architecture.md section 8 row G2";
const ATTRIBUTES: &str = ".gitattributes";
const BUILD_DIRECTORY: &str = "target";

/// Each path and the digest it feeds, for the failure message. A listed path must exist.
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

/// One `.gitattributes` line that sets `text`: its pattern split on `/`, and whether it marks
/// the path raw (`-text` or `binary`). An unanchored pattern holds one segment, matched on a name.
struct TextRule {
    segments: Vec<String>,
    anchored: bool,
    raw: bool,
}

fn text_rules(root: &Path) -> Result<Vec<TextRule>, String> {
    let text = std::fs::read_to_string(root.join(ATTRIBUTES))
        .map_err(|why| format!("{ATTRIBUTES} could not be read: {why}"))?;
    let mut rules = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(pattern) = fields.next().filter(|first| !first.starts_with('#')) else {
            continue;
        };
        let attributes: Vec<&str> = fields.collect();
        let raw = attributes.iter().any(|a| *a == "-text" || *a == "binary");
        let cooked = attributes
            .iter()
            .any(|a| *a == "text" || a.starts_with("text="));
        if !raw && !cooked {
            continue;
        }
        if pattern.contains(['[', '\\', '!']) || pattern.starts_with('"') {
            return Err(format!(
                "{ATTRIBUTES} pattern `{pattern}` uses syntax this check does not read"
            ));
        }
        let trimmed = pattern.trim_start_matches('/');
        rules.push(TextRule {
            segments: trimmed.split('/').map(str::to_owned).collect(),
            anchored: pattern.contains('/'),
            raw,
        });
    }
    Ok(rules)
}

/// `*` and `?` within one name, as in a gitattributes pattern.
fn name_matches(pattern: &[u8], name: &[u8]) -> bool {
    match (pattern.split_first(), name.split_first()) {
        (None, None) => true,
        (Some((b'*', rest)), _) => {
            name_matches(rest, name) || (!name.is_empty() && name_matches(pattern, &name[1..]))
        }
        (Some((b'?', rest)), Some((_, tail))) => name_matches(rest, tail),
        (Some((want, rest)), Some((got, tail))) => want == got && name_matches(rest, tail),
        _ => false,
    }
}

/// Segment by segment; `**` stands for zero or more whole segments.
fn path_matches(pattern: &[String], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((head, rest)) if head == "**" => {
            (0..=path.len()).any(|skip| path_matches(rest, &path[skip..]))
        }
        Some((head, rest)) => path.split_first().is_some_and(|(name, tail)| {
            name_matches(head.as_bytes(), name.as_bytes()) && path_matches(rest, tail)
        }),
    }
}

/// The last `text` line that matches decides, as git decides.
fn is_raw(rules: &[TextRule], relative: &str) -> bool {
    let path: Vec<&str> = relative.split('/').collect();
    let name = path.last().copied().unwrap_or_default();
    rules
        .iter()
        .rev()
        .find(|rule| {
            if rule.anchored {
                path_matches(&rule.segments, &path)
            } else {
                rule.segments
                    .first()
                    .is_some_and(|segment| name_matches(segment.as_bytes(), name.as_bytes()))
            }
        })
        .is_some_and(|rule| rule.raw)
}

/// Every non-hidden file under `root` outside `target/`, as `/`-separated relative paths, sorted.
pub(crate) fn tree_files(root: &Path) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    let mut pending: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory)
            .map_err(|why| format!("{} could not be listed: {why}", directory.display()))?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let child = entry.path();
            if child.is_dir() {
                if name != BUILD_DIRECTORY {
                    pending.push(child);
                }
            } else if let Ok(relative) = child.strip_prefix(root) {
                found.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    found.sort();
    Ok(found)
}

fn feeds_of<'a>(inputs: &[(&str, &'a str)], relative: &str) -> Option<&'a str> {
    inputs
        .iter()
        .find(|(listed, _)| relative == *listed || relative.starts_with(&format!("{listed}/")))
        .map(|(_, feeds)| *feeds)
}

/// The check: `None` when every listed input exists and no file outside a raw path holds `\r`.
pub fn carriage_return_failure(root: &Path, inputs: &[(&str, &str)]) -> Option<String> {
    let checked = || -> Result<Vec<String>, String> {
        let rules = text_rules(root)?;
        let files = tree_files(root)?;
        for (listed, _) in inputs {
            if !files
                .iter()
                .any(|file| feeds_of(&[(*listed, "")], file).is_some())
            {
                return Err(format!(
                    "{listed} is listed as a hashed input and holds no file"
                ));
            }
        }
        let mut offenders = Vec::new();
        for relative in files.iter().filter(|file| !is_raw(&rules, file)) {
            let bytes = std::fs::read(root.join(relative))
                .map_err(|why| format!("{relative} could not be read: {why}"))?;
            let returns = bytes.iter().filter(|byte| **byte == b'\r').count();
            if returns > 0 {
                let feeds = feeds_of(inputs, relative).unwrap_or("no listed digest");
                offenders.push(format!(
                    "    * {relative} holds {returns} `\\r` byte(s); it feeds {feeds}"
                ));
            }
        }
        Ok(offenders)
    };
    let offenders = match checked() {
        Ok(offenders) => offenders,
        Err(why) => return Some(format!("{why}\n\nThe invariant is {ROW}.\n")),
    };
    if offenders.is_empty() {
        return None;
    }
    Some(format!(
        concat!(
            "{} file(s) outside a `-text` path of .gitattributes hold a carriage return:\n\n",
            "{}\n\nA CRLF copy of a file whose bytes are digested gives another version digest ",
            "for the same\ncontent. Save the file with LF line ends, or mark a byte-exact ",
            "fixture `-text`. The invariant is {}.\n",
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

    const ATTRIBUTES_TEXT: &str = "* text=auto eol=lf\nfix/** -text\n*.bin binary\n";

    fn workspace_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf()
    }

    fn tree(files: &[(&str, &str)]) -> Result<tempfile::TempDir, std::io::Error> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join(ATTRIBUTES), ATTRIBUTES_TEXT)?;
        for (path, text) in files {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(full, text)?;
        }
        Ok(dir)
    }

    /// Over this tree. Mutations 2026-11-16: listed in architecture.md §8 row G2.
    #[test]
    fn no_hashed_input_in_this_tree_holds_a_carriage_return() {
        let failure = carriage_return_failure(&workspace_root(), &HASHED_INPUTS);
        assert_eq!(failure, None);
    }

    #[test]
    fn a_crlf_file_under_a_listed_directory_is_named_with_the_digest_it_feeds() -> Outcome {
        let dir = tree(&[
            ("crate/src/lib.rs", "fn a() {}\r\nfn b() {}\r\n"),
            ("crate/src/ok.rs", "fn c() {}\n"),
        ])?;
        let failure = carriage_return_failure(dir.path(), &[("crate/src", "the test version")]);
        let text = failure.unwrap_or_default();
        assert!(
            text.contains("crate/src/lib.rs holds 2 `\\r` byte(s); it feeds the test version")
                && !text.contains("ok.rs"),
            "{text}"
        );
        Ok(())
    }

    /// A file nobody listed is checked too: the list names digests, it does not bound the scan.
    #[test]
    fn a_crlf_file_outside_every_listed_input_fails() -> Outcome {
        let dir = tree(&[("src/a.rs", "fn a() {}\n"), ("other/new.json", "{}\r\n")])?;
        let failure = carriage_return_failure(dir.path(), &[("src", "x")]);
        assert!(failure.is_some_and(|text| text.contains("other/new.json holds 1")));
        Ok(())
    }

    /// A lone `\r` (old Mac line end) counts too: the digest sees it the same way.
    #[test]
    fn a_lone_carriage_return_in_a_listed_file_fails() -> Outcome {
        let dir = tree(&[("rules.json", "{\r}\n")])?;
        let failure = carriage_return_failure(dir.path(), &[("rules.json", "x")]);
        assert!(failure.is_some_and(|text| text.contains("rules.json holds 1")));
        Ok(())
    }

    #[test]
    fn a_listed_path_that_is_absent_or_an_empty_directory_fails_rather_than_passes() -> Outcome {
        let dir = tree(&[])?;
        std::fs::create_dir_all(dir.path().join("empty"))?;
        let absent = carriage_return_failure(dir.path(), &[("gone.json", "x")]);
        let empty = carriage_return_failure(dir.path(), &[("empty", "x")]);
        assert!(absent.is_some_and(|text| text.contains("gone.json is listed")));
        assert!(empty.is_some_and(|text| text.contains("empty is listed")));
        Ok(())
    }

    /// The positive control: LF files, CR under a `-text` path and a `binary` name, hidden and
    /// `target/` entries skipped, and a nested directory read.
    #[test]
    fn lf_files_raw_paths_hidden_and_build_entries_pass() -> Outcome {
        let dir = tree(&[
            ("src/a.rs", "fn a() {}\n"),
            ("src/nested/b.rs", "fn b() {}\n"),
            ("fix/deep/robots.txt", "a\r\n"),
            ("data/x.bin", "\r"),
            ("src/.DS_Store", "\r\n"),
            ("target/debug/out.txt", "\r\n"),
        ])?;
        assert_eq!(carriage_return_failure(dir.path(), &[("src", "x")]), None);
        Ok(())
    }

    /// The `-text` line is what exempts a fixture: without it the same tree fails.
    #[test]
    fn a_crlf_fixture_fails_once_its_raw_line_is_gone() -> Outcome {
        let dir = tree(&[("fix/deep/robots.txt", "a\r\n")])?;
        std::fs::write(dir.path().join(ATTRIBUTES), "* text=auto eol=lf\n")?;
        let failure = carriage_return_failure(dir.path(), &[]);
        assert!(failure.is_some_and(|text| text.contains("fix/deep/robots.txt holds 1")));
        Ok(())
    }

    /// A later `text` line overrides an earlier `-text` one, as in git.
    #[test]
    fn the_last_matching_line_decides() -> Outcome {
        let dir = tree(&[("fix/a.txt", "a\r\n")])?;
        std::fs::write(
            dir.path().join(ATTRIBUTES),
            "fix/** -text\nfix/*.txt text\n",
        )?;
        let failure = carriage_return_failure(dir.path(), &[]);
        assert!(failure.is_some_and(|text| text.contains("fix/a.txt holds 1")));
        Ok(())
    }

    #[test]
    fn a_pattern_this_check_cannot_read_fails_by_name() -> Outcome {
        let dir = tree(&[])?;
        std::fs::write(dir.path().join(ATTRIBUTES), "fix/[ab]/** -text\n")?;
        let failure = carriage_return_failure(dir.path(), &[]);
        assert!(failure.is_some_and(|text| text.contains("`fix/[ab]/**` uses syntax")));
        Ok(())
    }
}
