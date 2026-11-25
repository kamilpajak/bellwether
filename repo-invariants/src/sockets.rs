//! The no-socket scan: `docs/design/architecture.md` section 8 row FV1. The store core names the
//! standard library's socket module nowhere. A text scan over CODE lines of every `.rs` file below
//! `vault-core`, its build script and `tests/` included, because the module is reached with no
//! manifest line: rows FV1 and R4 read manifests only.

use std::path::Path;

use crate::clients::STORE_CORE;
use crate::clocks::rust_files;
use crate::resolve_members;

/// The path segment of `std::net` and of `std::os::unix::net`, matched as a whole word.
pub const SOCKET_MODULE: &str = "net";
const ROW: &str = "docs/design/architecture.md section 8 row FV1";

/// One code line naming [`SOCKET_MODULE`], as the scan found it.
#[derive(Debug, PartialEq, Eq)]
pub struct SocketNamed {
    /// Path relative to the workspace root, spelled with `/`.
    pub file: String,
    /// 1-indexed, as an editor counts.
    pub line: usize,
}

/// The line with any `//` comment cut off; `sinks::code_of` names what that misreads.
fn code_of(line: &str) -> &str {
    line.split("//").next().unwrap_or("")
}

fn names_the_module(line: &str) -> bool {
    code_of(line)
        .split(|letter: char| !(letter.is_alphanumeric() || letter == '_'))
        .any(|word| word == SOCKET_MODULE)
}

/// Every code line below the store core's directory naming [`SOCKET_MODULE`], test modules
/// included.
/// `Err` when the core is no member or holds no Rust file: a scan that read nothing found nothing.
pub fn sockets_named(root: &Path, members: &[crate::Member]) -> Result<Vec<SocketNamed>, String> {
    let core = members
        .iter()
        .find(|member| member.path == STORE_CORE)
        .ok_or_else(|| format!("no workspace member is at `{STORE_CORE}`"))?;
    let files = rust_files(&root.join(&core.path))?;
    if files.is_empty() {
        return Err(format!("`{STORE_CORE}` holds no Rust file"));
    }
    let mut found = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file)
            .map_err(|why| format!("{} could not be read: {why}", file.display()))?;
        for (index, line) in text.lines().enumerate() {
            if names_the_module(line) {
                found.push(SocketNamed {
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
    Ok(found)
}

/// The check: `None` when no code line of the store core names the socket module.
pub fn socket_failure(root_manifest: &Path) -> Option<String> {
    let root = root_manifest.parent().unwrap_or(Path::new("."));
    let found = resolve_members(root_manifest).and_then(|members| sockets_named(root, &members));
    match found {
        Err(why) => Some(format!("{why}\n\nThe invariant is {ROW}.\n")),
        Ok(found) if found.is_empty() => None,
        Ok(found) => Some(format!(
            concat!(
                "`{}` names the word `{}` on {} code line(s):\n\n{}\n\n",
                "Why this matters: the store core holds store mechanics only, so no socket is ",
                "opened from it;\ntransport is `news-provider`'s. A TEXT scan of whole words: a ",
                "local so named is a finding too,\nand a socket reached through a dependency is ",
                "not seen. The invariant is {}.\n",
            ),
            STORE_CORE,
            SOCKET_MODULE,
            found.len(),
            found
                .iter()
                .map(|named| format!("    * {}:{}", named.file, named.line))
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

    const CORE_AND_SEAM: &str =
        "[workspace]\nmembers = [\"vault-core\", \"news-provider\"]\nresolver = \"2\"\n";
    /// Written out, never built from [`SOCKET_MODULE`]: a changed constant must not change the
    /// fixture. Lines 1 to 3 are findings; 4 is a comment and 5 holds longer words only.
    const FIVE_LINES: &str = "use std::net::TcpStream;\n\
        use std::{fs, net};\n\
        use std::os::unix::net::UnixStream;\n\
        // std::net in a comment\n\
        fn f(network: u8) -> u8 { network } // and the internet\n";

    fn core_and_seam() -> Tree {
        Tree::new(CORE_AND_SEAM)
            .crate_at("vault-core", &manifest("vault-core", INHERITS))
            .crate_at("news-provider", &manifest("news-provider", INHERITS))
    }

    /// §8 row FV1's socket clause over this workspace. Mutations 2026-11-25, each red here:
    /// `use std::net::TcpStream;` in `vault-core/src/lib.rs`, in a new `vault-core/build.rs` and
    /// in a new `vault-core/tests/wire.rs`; `use std::{fs, net};` in `vault-core/src/lease.rs`;
    /// `std::os::unix::net::UnixStream` in a function of `sqlite.rs`.
    #[test]
    fn the_store_core_names_the_socket_module_on_no_code_line() {
        let failure = socket_failure(&root_manifest()).unwrap_or_default();
        assert!(failure.is_empty(), "\n\n{failure}\n");
    }

    #[test]
    fn each_spelling_of_the_socket_module_is_seen_and_a_comment_or_a_longer_word_is_not() {
        let tree = core_and_seam();
        tree.write("vault-core/src/wire.rs", FIVE_LINES);
        tree.write("news-provider/src/wire.rs", FIVE_LINES);
        tree.write("vault-core/build.rs", FIVE_LINES);
        tree.write("vault-core/tests/wire.rs", FIVE_LINES);
        let failure = socket_failure(&tree.root_manifest()).expect("expected a failure");
        let named = [1, 2, 3, 4, 5].map(|line| failure.contains(&format!("src/wire.rs:{line}")));
        let outside_src = ["vault-core/build.rs:1", "vault-core/tests/wire.rs:1"]
            .map(|place| failure.contains(place));
        assert_eq!(
            (named, outside_src, failure.contains("news-provider/src")),
            ([true, true, true, false, false], [true, true], false),
            "{failure}"
        );
    }

    #[test]
    fn a_workspace_whose_store_core_is_absent_is_a_failure_and_a_clean_one_passes() {
        let clean = core_and_seam();
        let absent = Tree::new("[workspace]\nmembers = [\"news-provider\"]\nresolver = \"2\"\n")
            .crate_at("news-provider", &manifest("news-provider", INHERITS));
        let failure = socket_failure(&absent.root_manifest()).unwrap_or_default();
        assert_eq!(
            (
                socket_failure(&clean.root_manifest()),
                failure.contains("no workspace member is at")
            ),
            (None, true),
            "{failure}"
        );
    }
}
