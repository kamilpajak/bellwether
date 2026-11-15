//! The no-sink scan: `docs/design/architecture.md` section 8 row R4. No provider opens, holds or
//! writes an attempt row; the driver does. A text scan over CODE lines of every `providers/*`
//! member's `src`, because both providers' doc comments state "no `Sink`" and a scan reading
//! comments would flag them.

use std::path::Path;

use crate::clocks::rust_files;
use crate::{resolve_members, ROW_R4};

/// The trait every ledger writer implements; `RecordingSink` and any `…Sink` contain it.
pub const SINK: &str = "Sink";
const PROVIDER_DIR: &str = "providers/";

/// One code line naming a sink, as the scan found it.
#[derive(Debug, PartialEq, Eq)]
pub struct SinkNamed {
    /// Path relative to the workspace root, spelled with `/`.
    pub file: String,
    /// 1-indexed, as an editor counts.
    pub line: usize,
}

/// The line with any `//` comment cut off. A `/* */` block and a `//` inside a string literal
/// are not understood: the first hides a sink on its lines, the second cuts a line short.
fn code_of(line: &str) -> &str {
    line.split("//").next().unwrap_or("")
}

/// Every code line under a provider member's `src` naming [`SINK`] in an identifier.
pub fn sinks_named(root: &Path, members: &[crate::Member]) -> Result<Vec<SinkNamed>, String> {
    let mut found = Vec::new();
    for member in members
        .iter()
        .filter(|member| member.path.starts_with(PROVIDER_DIR))
    {
        let src = root.join(&member.path).join("src");
        if !src.is_dir() {
            continue;
        }
        for file in rust_files(&src)? {
            let text = std::fs::read_to_string(&file)
                .map_err(|why| format!("{} could not be read: {why}", file.display()))?;
            for (index, line) in text.lines().enumerate() {
                if code_of(line).contains(SINK) {
                    found.push(SinkNamed {
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

/// The check: `None` when no provider code line names a sink.
pub fn sink_failure(root_manifest: &Path) -> Option<String> {
    let root = root_manifest.parent().unwrap_or(Path::new("."));
    let found = resolve_members(root_manifest).and_then(|members| sinks_named(root, &members));
    match found {
        Err(why) => Some(format!("{why}\n\nThe invariant is {ROW_R4}.\n")),
        Ok(found) if found.is_empty() => None,
        Ok(found) => Some(format!(
            concat!(
                "A provider crate names `{}` on {} code line(s):\n\n{}\n\n",
                "Why this matters: the driver opens and closes every attempt row, so a provider ",
                "holding a sink\ncould write a ledger row the driver never opened. Comments are ",
                "not scanned. The invariant is {}.\n",
            ),
            SINK,
            found.len(),
            found
                .iter()
                .map(|named| format!("    * {}:{}", named.file, named.line))
                .collect::<Vec<String>>()
                .join("\n"),
            ROW_R4,
        )),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::tests::{manifest, root_manifest, Tree, INHERITS};

    /// Assembled so this file's own text holds no such line, should this crate ever be scanned.
    fn a_sink_use() -> String {
        format!("use news_provider::{};\n", "Sink")
    }

    /// §8 row R4(c) over this workspace: every `providers/*` member, code lines only.
    #[test]
    fn no_file_under_a_provider_crate_names_sink() {
        let failure = sink_failure(&root_manifest()).unwrap_or_default();
        assert!(failure.is_empty(), "\n\n{failure}\n");
    }

    #[test]
    fn a_sink_use_in_a_new_provider_file_turns_the_scan_red() {
        let tree = seam_and_provider();
        tree.write("providers/edgar-press/src/ledger/write.rs", &a_sink_use());
        let failure = sink_failure(&tree.root_manifest()).expect("expected a failure");
        assert!(
            failure.contains("providers/edgar-press/src/ledger/write.rs:1"),
            "{failure}"
        );
    }

    /// The skeleton's own doc comment says "no `Sink`", which is why this is not a plain grep.
    #[test]
    fn a_comment_naming_sink_is_not_a_finding() {
        let tree = seam_and_provider();
        tree.write(
            "providers/edgar-press/src/lib.rs",
            &format!("//! holds no `{SINK}`\npub fn f() {{}} // nor a {SINK}\n"),
        );
        assert_eq!(sink_failure(&tree.root_manifest()), None);
    }

    /// The seam crate defines the trait, so it is not a provider and not scanned.
    #[test]
    fn the_seam_crate_may_name_its_own_trait() {
        let tree = seam_and_provider();
        tree.write("news-provider/src/ledger.rs", &a_sink_use());
        assert_eq!(sink_failure(&tree.root_manifest()), None);
    }

    const TWO_MEMBERS: &str =
        "[workspace]\nmembers = [\"news-provider\", \"providers/edgar-press\"]\nresolver = \"2\"\n";

    fn seam_and_provider() -> Tree {
        Tree::new(TWO_MEMBERS)
            .crate_at("news-provider", &manifest("news-provider", INHERITS))
            .crate_at("providers/edgar-press", &manifest("edgar-press", INHERITS))
    }
}
