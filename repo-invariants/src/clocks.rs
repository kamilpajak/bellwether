//! The no-clock grep: `docs/design/architecture.md` section 8 row R4's clock clause, over the
//! provider crates and the seam crate outside its `wire` and `clock` modules. A text scan, which
//! is the gate shape this project otherwise refuses - it ships because it has a named mutation
//! that turns it red (`adding_a_clock_read_to_a_provider_crate_turns_the_grep_red`), and because
//! the thing it protects is a signature: no `Provider` method is handed an instant it did not
//! receive, so a provider reading a process clock is a fact outside the type system.

use std::path::{Path, PathBuf};

use crate::{resolve_members, Member, ROW_R4};

/// Every spelling of a process-clock read this gate knows. A list, so it sees no clock a crate
/// reaches through a name not written here.
pub const CLOCK_READS: [&str; 4] = ["Utc::now", "Local::now", "SystemTime::now", "Instant::now"];

/// The crate that owns the socket, and therefore the two instants either side of it.
const SEAM_CRATE: &str = "news-provider";
/// Where an adapter crate lives. A member under this prefix is scanned without being named here,
/// so a second provider is covered by existing on disk.
const PROVIDER_DIR: &str = "providers/";
/// The seam's two exempt modules: `wire` reads the instants either side of the socket and `clock`
/// is the injected trait. Exact file names, so a `wire/` directory is not exempt by prefix.
const EXEMPT_MODULES: [&str; 2] = ["clock.rs", "wire.rs"];
/// Cargo's own output, which holds generated sources no rule here is about.
const BUILD_DIR: &str = "target";

/// One clock read, as the scan found it.
#[derive(Debug, PartialEq, Eq)]
pub struct ClockRead {
    /// Path relative to the workspace root, spelled with `/`.
    pub file: String,
    /// 1-indexed, as an editor counts.
    pub line: usize,
    pub needle: &'static str,
}

/// Which member directories this gate reads, in member order.
fn scanned(members: &[Member]) -> Vec<&str> {
    members
        .iter()
        .map(|member| member.path.as_str())
        .filter(|path| *path == SEAM_CRATE || path.starts_with(PROVIDER_DIR))
        .collect()
}

fn exempt(member: &str, file: &Path, src: &Path) -> bool {
    member == SEAM_CRATE
        && file
            .strip_prefix(src)
            .ok()
            .and_then(|rest| rest.to_str())
            .is_some_and(|rest| EXEMPT_MODULES.contains(&rest))
}

/// `.rs` files under `dir`, recursively, in sorted order so the report is a function of the tree.
/// `target/` is skipped, so a caller walking a whole workspace reads the same set as the CI size
/// step's `find . -name '*.rs' -not -path './target/*'`.
pub(crate) fn rust_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut found: Vec<PathBuf> = Vec::new();
    let entries = std::fs::read_dir(dir)
        .map_err(|why| format!("{} could not be listed: {why}", dir.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|why| format!("{} could not be listed: {why}", dir.display()))?
            .path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == BUILD_DIR) {
                continue;
            }
            found.extend(rust_files(&path)?);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
    found.sort();
    Ok(found)
}

/// Every clock read in the scanned members' `src` trees, outside the exempt modules.
pub fn clock_reads(root: &Path, members: &[Member]) -> Result<Vec<ClockRead>, String> {
    let mut found: Vec<ClockRead> = Vec::new();
    for member in scanned(members) {
        let src = root.join(member).join("src");
        if !src.is_dir() {
            continue;
        }
        for file in rust_files(&src)? {
            if exempt(member, &file, &src) {
                continue;
            }
            let text = std::fs::read_to_string(&file)
                .map_err(|why| format!("{} could not be read: {why}", file.display()))?;
            for (index, line) in text.lines().enumerate() {
                for needle in CLOCK_READS {
                    if line.contains(needle) {
                        found.push(ClockRead {
                            file: file
                                .strip_prefix(root)
                                .unwrap_or(&file)
                                .to_string_lossy()
                                .replace('\\', "/"),
                            line: index + 1,
                            needle,
                        });
                    }
                }
            }
        }
    }
    Ok(found)
}

/// The check: `None` when no provider and no part of the seam outside its two exempt modules
/// names a process clock.
pub fn clock_read_failure(root_manifest: &Path) -> Option<String> {
    let members = match resolve_members(root_manifest) {
        Ok(members) => members,
        Err(why) => return Some(format!("{why}\n\nThe invariant is {ROW_R4}.\n")),
    };
    let root = root_manifest.parent().unwrap_or(Path::new("."));
    let reads = match clock_reads(root, &members) {
        Ok(reads) => reads,
        Err(why) => return Some(format!("{why}\n\nThe invariant is {ROW_R4}.\n")),
    };
    if reads.is_empty() {
        return None;
    }
    Some(format!(
        concat!(
            "A crate that may not read a clock names one. {} read(s):\n\n{}\n\n",
            "Why this matters: every instant a provider works from is handed to it - ",
            "`Provider::plan`\ntakes the run's `ObservationTime` and `Wire::send` reads the two ",
            "instants either side of\nthe socket through an injected `Clock`. A provider reading ",
            "the process clock makes its own\noutput a function of when it ran, which is what ",
            "makes a collection reproducible from the\nfiles alone. Exempt, in `{}` only: {}.\n\n",
            "Scanned: {}. This is a TEXT scan, so it also fires on the names in a comment and it ",
            "sees\nno clock reached under another name. The invariant is {}.\n",
        ),
        reads.len(),
        reads
            .iter()
            .map(|read| format!("    * {}:{} names `{}`", read.file, read.line, read.needle))
            .collect::<Vec<String>>()
            .join("\n"),
        SEAM_CRATE,
        EXEMPT_MODULES.join(", "),
        scanned(&members).join(", "),
        ROW_R4,
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::tests::{manifest, root_manifest, Tree, INHERITS};

    /// A line that reads the process clock, assembled so this file does not trip its own gate -
    /// repo-invariants is not a scanned member, and assembling it keeps that true if it becomes
    /// one.
    fn a_clock_read() -> String {
        format!("pub fn when() {{ let _ = chrono::{}::now(); }}\n", "Utc")
    }

    /// Row R4's clock clause over this workspace.
    #[test]
    fn no_provider_and_no_part_of_the_seam_outside_wire_and_clock_reads_a_clock() {
        let failure = clock_read_failure(&root_manifest()).unwrap_or_default();
        assert!(failure.is_empty(), "\n\n{failure}\n");
    }

    /// The gate's reach, asserted rather than assumed: the seam crate and every member under
    /// `providers/`, and nothing else. A second provider is covered by existing.
    #[test]
    fn the_scan_covers_the_seam_crate_and_every_provider_member() {
        let members = resolve_members(&root_manifest()).unwrap();
        assert_eq!(
            scanned(&members),
            vec!["news-provider", "providers/edgar-press"]
        );
    }

    #[test]
    fn adding_a_clock_read_to_a_provider_crate_turns_the_grep_red() {
        let tree = seam_and_provider();
        tree.write("providers/edgar-press/src/lib.rs", &a_clock_read());
        let failure = clock_read_failure(&tree.root_manifest()).expect("expected a failure");
        assert!(
            failure.contains("providers/edgar-press/src/lib.rs:1 names `Utc::now`"),
            "{failure}"
        );
        assert!(failure.contains("a function of when it ran"), "{failure}");
    }

    /// The same read in a module the seam is allowed to hold, which is why the gate is a scan
    /// with two exemptions rather than a ban.
    #[test]
    fn the_seams_wire_and_clock_modules_may_read_one() {
        let tree = seam_and_provider();
        tree.write("news-provider/src/clock.rs", &a_clock_read());
        tree.write("news-provider/src/wire.rs", &a_clock_read());
        assert_eq!(clock_read_failure(&tree.root_manifest()), None);
    }

    /// The exemption is two file names and not the crate: the seam's other modules are scanned.
    #[test]
    fn the_rest_of_the_seam_crate_is_scanned() {
        let tree = seam_and_provider();
        tree.write("news-provider/src/provider.rs", &a_clock_read());
        let failure = clock_read_failure(&tree.root_manifest()).expect("expected a failure");
        assert!(
            failure.contains("news-provider/src/provider.rs:1"),
            "{failure}"
        );
    }

    /// A nested module, because a provider's `src` is a tree and not a file.
    #[test]
    fn a_clock_read_in_a_nested_module_is_found_too() {
        let tree = seam_and_provider();
        tree.write("providers/edgar-press/src/listing/rows.rs", &a_clock_read());
        let failure = clock_read_failure(&tree.root_manifest()).expect("expected a failure");
        assert!(
            failure.contains("providers/edgar-press/src/listing/rows.rs:1"),
            "{failure}"
        );
    }

    /// A member that is neither the seam nor a provider holds the service code, which reads the
    /// clock once per run on purpose.
    #[test]
    fn a_service_crate_may_read_the_clock_and_is_not_scanned() {
        let tree = seam_and_provider();
        tree.write("services/newsvault/src/lib.rs", &a_clock_read());
        assert_eq!(clock_read_failure(&tree.root_manifest()), None);
    }

    const THREE_MEMBERS: &str = "[workspace]\nmembers = [\"news-provider\", \
         \"providers/edgar-press\", \"services/newsvault\"]\nresolver = \"2\"\n";

    /// The real tree's shape in miniature: the seam, one provider, one service.
    fn seam_and_provider() -> Tree {
        Tree::new(THREE_MEMBERS)
            .crate_at("news-provider", &manifest("news-provider", INHERITS))
            .crate_at("providers/edgar-press", &manifest("edgar-press", INHERITS))
            .crate_at("services/newsvault", &manifest("newsvault", INHERITS))
    }
}
