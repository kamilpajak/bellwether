//! The temp-store scan: `docs/design/architecture.md` section 8 row T1. A SQLite connection opened
//! outside a test sets `temp_store` on the next line, so no command of an image run `--read-only`
//! needs a temporary file. A text scan over every member's `src`, lines before `#[cfg(test)]`.

use std::path::Path;

use crate::clocks::rust_files;
use crate::resolve_members;

/// Every rusqlite constructor starts with it: `open`, `open_with_flags`, `open_in_memory`.
/// Assembled so this file's own text holds no such line.
pub const OPEN: &str = concat!("Connection", "::open");
/// The pragma's name as `pragma_update` is given it.
pub const PRAGMA: &str = "\"temp_store\"";
const TEST_MARKER: &str = "#[cfg(test)]";
const ROW: &str = "docs/design/architecture.md section 8 row T1";

/// One connection opened with no `temp_store` line after it, as the scan found it.
#[derive(Debug, PartialEq, Eq)]
pub struct BareOpen {
    /// Path relative to the workspace root, spelled with `/`.
    pub file: String,
    /// 1-indexed, as an editor counts.
    pub line: usize,
}

/// The line with any `//` comment cut off; `sinks::code_of` names what that misreads.
fn code_of(line: &str) -> &str {
    line.split("//").next().unwrap_or("")
}

/// Every code line before a file's test marker that names [`OPEN`] and is not followed by a line
/// naming [`PRAGMA`].
pub fn bare_opens(root: &Path, members: &[crate::Member]) -> Result<Vec<BareOpen>, String> {
    let mut found = Vec::new();
    for member in members {
        let src = root.join(&member.path).join("src");
        if !src.is_dir() {
            continue;
        }
        for file in rust_files(&src)? {
            let text = std::fs::read_to_string(&file)
                .map_err(|why| format!("{} could not be read: {why}", file.display()))?;
            let source: Vec<&str> = text
                .lines()
                .take_while(|line| line.trim_start() != TEST_MARKER)
                .collect();
            for (index, line) in source.iter().enumerate() {
                let set_next = source
                    .get(index + 1)
                    .is_some_and(|next| code_of(next).contains(PRAGMA));
                if code_of(line).contains(OPEN) && !set_next {
                    found.push(BareOpen {
                        file: file
                            .strip_prefix(root)
                            .unwrap_or(&file)
                            .to_string_lossy()
                            .replace('\\', "/"),
                        line: index + 1,
                    });
                }
            }
        }
    }
    Ok(found)
}

/// The check: `None` when every connection opened outside a test sets `temp_store` at once.
pub fn bare_open_failure(root_manifest: &Path) -> Option<String> {
    let root = root_manifest.parent().unwrap_or(Path::new("."));
    let found = resolve_members(root_manifest).and_then(|members| bare_opens(root, &members));
    match found {
        Err(why) => Some(format!("{why}\n\nThe invariant is {ROW}.\n")),
        Ok(found) if found.is_empty() => None,
        Ok(found) => Some(format!(
            concat!(
                "`{}` on {} code line(s) with no {} line after it:\n\n{}\n\n",
                "Why this matters: the image runs `--read-only`, and a sort that outgrows SQLite's ",
                "memory then\nfails with SQLITE_IOERR_GETTEMPPATH (6410). Open through the crate's ",
                "`connect` instead.\nA TEXT scan: it sees no constructor reached under another ",
                "name. The invariant is {}.\n",
            ),
            OPEN,
            found.len(),
            PRAGMA,
            found
                .iter()
                .map(|open| format!("    * {}:{}", open.file, open.line))
                .collect::<Vec<String>>()
                .join("\n"),
            ROW,
        )),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::tests::{manifest, root_manifest, Tree, INHERITS};

    const ONE_MEMBER: &str = "[workspace]\nmembers = [\"services/newsvault\"]\nresolver = \"2\"\n";
    const READER: &str = "services/newsvault/src/cmd_new.rs";

    fn one_service() -> Tree {
        Tree::new(ONE_MEMBER).crate_at("services/newsvault", &manifest("newsvault", INHERITS))
    }

    /// §8 row T1 over this workspace.
    #[test]
    fn every_sqlite_connection_opened_outside_a_test_sets_temp_store_on_the_next_line() {
        let failure = bare_open_failure(&root_manifest()).unwrap_or_default();
        assert!(failure.is_empty(), "\n\n{failure}\n");
    }

    #[test]
    fn a_connection_opened_with_no_pragma_after_it_turns_the_scan_red() {
        let tree = one_service();
        tree.write(
            READER,
            &format!(
                "fn read() {{\n    let db = {OPEN}_with_flags(log, flags)?;\n    db.query()\n}}\n"
            ),
        );
        let failure = bare_open_failure(&tree.root_manifest()).expect("expected a failure");
        assert!(failure.contains(&format!("{READER}:2")), "{failure}");
    }

    #[test]
    fn an_open_followed_by_the_pragma_or_written_in_a_test_module_is_not_a_finding() {
        let tree = one_service();
        tree.write(
            READER,
            &format!(
                "fn connect() {{\n    let db = {OPEN}(path)?;\n    \
                 db.pragma_update(None, {PRAGMA}, \"MEMORY\")?;\n}}\n\
                 {TEST_MARKER}\nmod tests {{\n    fn raw() {{ {OPEN}_in_memory(); }}\n}}\n"
            ),
        );
        assert_eq!(bare_open_failure(&tree.root_manifest()), None);
    }
}
