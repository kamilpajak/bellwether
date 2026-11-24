//! The temp-store scan: `docs/design/architecture.md` section 8 row T1. A rusqlite constructor is
//! named outside a test module in two files only, once each: the `connect` of each crate that
//! opens SQLite, which sets `temp_store`. A text scan over every member's `src`.

use std::path::Path;

use crate::clocks::rust_files;
use crate::resolve_members;

/// Every rusqlite constructor starts with it: `open`, `open_with_flags`, `open_in_memory`.
/// Assembled so this line is no finding of its own scan.
pub const OPEN: &str = concat!("Connection", "::open");
/// The files that may name [`OPEN`] outside a test module, once each, in the scan's order.
pub const OPENERS: [&str; 2] = [
    "services/modelgate/src/store.rs",
    "services/newsvault/src/store.rs",
];
const TEST_MARKER: &str = "#[cfg(test)]";
const MODULE: &str = "mod";
const VISIBILITY: &str = "pub";
const ROW: &str = "docs/design/architecture.md section 8 row T1";

/// One code line outside a test module that names [`OPEN`], as the scan found it.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Open {
    /// Path relative to the workspace root, spelled with `/`.
    pub file: String,
    /// 1-indexed, as an editor counts.
    pub line: usize,
}

/// The line with any `//` comment cut off; `sinks::code_of` names what that misreads.
fn code_of(line: &str) -> &str {
    line.split("//").next().unwrap_or("")
}

/// The lines above the file's test module: a marker cuts only when the next line opens a `mod`.
fn above_the_test_module(text: &str) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    let opens_a_module = |index: usize| {
        lines.get(index + 1).is_some_and(|next| {
            let mut words = next.split_whitespace();
            words.find(|word| !word.starts_with(VISIBILITY)) == Some(MODULE)
        })
    };
    let end = (0..lines.len())
        .find(|&index| lines[index].trim() == TEST_MARKER && opens_a_module(index))
        .unwrap_or(lines.len());
    lines.into_iter().take(end).collect()
}

/// Every code line above a file's test module that names [`OPEN`], sorted by file then line.
pub fn opens(root: &Path, members: &[crate::Member]) -> Result<Vec<Open>, String> {
    let mut found = Vec::new();
    for member in members {
        let src = root.join(&member.path).join("src");
        if !src.is_dir() {
            continue;
        }
        for file in rust_files(&src)? {
            let text = std::fs::read_to_string(&file)
                .map_err(|why| format!("{} could not be read: {why}", file.display()))?;
            let shown = file
                .strip_prefix(root)
                .unwrap_or(&file)
                .to_string_lossy()
                .replace('\\', "/");
            for (index, line) in above_the_test_module(&text).iter().enumerate() {
                if code_of(line).contains(OPEN) {
                    found.push(Open {
                        file: shown.clone(),
                        line: index + 1,
                    });
                }
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The opens no opener accounts for: any outside [`OPENERS`], and any after a file's first.
pub fn bare(found: &[Open]) -> Vec<&Open> {
    found
        .iter()
        .enumerate()
        .filter(|(index, open)| {
            let repeated = found[..*index]
                .iter()
                .any(|earlier| earlier.file == open.file);
            repeated || !OPENERS.contains(&open.file.as_str())
        })
        .map(|(_, open)| open)
        .collect()
}

/// The check: `None` when no connection is opened outside the two `connect` functions.
pub fn bare_open_failure(root_manifest: &Path) -> Option<String> {
    let root = root_manifest.parent().unwrap_or(Path::new("."));
    let found = resolve_members(root_manifest).and_then(|members| opens(root, &members));
    let found = match found {
        Err(why) => return Some(format!("{why}\n\nThe invariant is {ROW}.\n")),
        Ok(found) => found,
    };
    let bare = bare(&found);
    if bare.is_empty() {
        return None;
    }
    Some(format!(
        concat!(
            "`{}` on {} code line(s) outside the one opener of {}:\n\n{}\n\n",
            "Why this matters: the image runs `--read-only`, and a sort that outgrows SQLite's ",
            "memory then\nfails with SQLITE_IOERR_GETTEMPPATH (6410) unless the connection keeps ",
            "its temporary data in\nmemory. Open through the crate's `connect` instead.\n",
            "A TEXT scan: it sees no constructor reached under another name. The invariant is ",
            "{}.\n",
        ),
        OPEN,
        bare.len(),
        OPENERS.join(" and "),
        bare.iter()
            .map(|open| format!("    * {}:{}", open.file, open.line))
            .collect::<Vec<String>>()
            .join("\n"),
        ROW,
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::tests::{manifest, root_manifest, Tree, INHERITS};

    const ONE_MEMBER: &str = "[workspace]\nmembers = [\"services/newsvault\"]\nresolver = \"2\"\n";
    const READER: &str = "services/newsvault/src/cmd_new.rs";
    const OPENER: &str = "services/newsvault/src/store.rs";
    /// Written out, never built from [`OPEN`]: a narrowed constant must not narrow the fixture.
    const THREE_CONSTRUCTORS: &str = "fn read() {\n    \
        let a = Connection::open(log)?;\n    \
        let b = rusqlite::Connection::open_with_flags(log, flags)?;\n    \
        let c = Connection::open_in_memory()?;\n    \
        // Connection::open(log) in a comment\n}\n";

    fn one_service() -> Tree {
        Tree::new(ONE_MEMBER).crate_at("services/newsvault", &manifest("newsvault", INHERITS))
    }

    fn seen(tree: &Tree) -> Vec<(String, usize)> {
        let manifest = tree.root_manifest();
        let members = resolve_members(&manifest).unwrap();
        opens(manifest.parent().unwrap(), &members)
            .unwrap()
            .into_iter()
            .map(|open| (open.file, open.line))
            .collect()
    }

    /// §8 row T1 over this workspace. The second assertion is the positive control: a scan that
    /// read no SQLite code would pass the first.
    #[test]
    fn a_sqlite_connection_is_opened_outside_a_test_module_in_the_two_openers_only() {
        let manifest = root_manifest();
        let failure = bare_open_failure(&manifest).unwrap_or_default();
        assert!(failure.is_empty(), "\n\n{failure}\n");
        let members = resolve_members(&manifest).unwrap();
        let files: Vec<String> = opens(manifest.parent().unwrap(), &members)
            .unwrap()
            .into_iter()
            .map(|open| open.file)
            .collect();
        assert_eq!(files, OPENERS);
    }

    #[test]
    fn each_of_the_three_constructors_is_seen_and_a_comment_or_a_test_module_is_not() {
        let tree = one_service();
        tree.write(
            READER,
            &format!(
                "{THREE_CONSTRUCTORS}{TEST_MARKER}\nmod tests {{\n    \
                 fn raw() {{ Connection::open_in_memory(); }}\n}}\n"
            ),
        );
        let reader = |line: usize| (READER.to_owned(), line);
        assert_eq!(seen(&tree), [reader(2), reader(3), reader(4)]);
    }

    #[test]
    fn an_open_outside_the_two_openers_turns_the_scan_red() {
        let tree = one_service();
        tree.write(READER, THREE_CONSTRUCTORS);
        let failure = bare_open_failure(&tree.root_manifest()).expect("expected a failure");
        let named = [2, 3, 4].map(|line| failure.contains(&format!("{READER}:{line}")));
        assert_eq!(named, [true; 3], "{failure}");
    }

    #[test]
    fn an_opener_holds_one_open_and_a_second_turns_the_scan_red() {
        let tree = one_service();
        let connect = "fn connect() {\n    Connection::open_with_flags(path, flags)\n}\n";
        tree.write(OPENER, connect);
        let one = bare_open_failure(&tree.root_manifest());
        tree.write(
            OPENER,
            &format!("{connect}fn other() {{\n    Connection::open(path)\n}}\n"),
        );
        let two = bare_open_failure(&tree.root_manifest()).unwrap_or_default();
        assert_eq!(
            (
                one,
                two.contains(&format!("{OPENER}:5")),
                two.contains(&format!("{OPENER}:2"))
            ),
            (None, true, false),
            "{two}"
        );
    }

    /// Mutation 2026-11-24: a `#[cfg(test)] const` above an open hid it from a scan that stopped
    /// at the first marker.
    #[test]
    fn a_test_marker_that_opens_no_module_hides_nothing_below_it() {
        let tree = one_service();
        tree.write(
            READER,
            &format!(
                "{TEST_MARKER}\nconst ONLY_IN_TESTS: u8 = 0;\n\
                 fn read() {{\n    Connection::open(log)\n}}\n\
                 {TEST_MARKER}\npub(crate) mod tests {{\n    \
                 fn raw() {{ Connection::open(log); }}\n}}\n"
            ),
        );
        assert_eq!(seen(&tree), [(READER.to_owned(), 4)]);
    }
}
