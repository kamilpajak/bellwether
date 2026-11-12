//! Repo-wide invariant checks: row G1 of `docs/design/architecture.md` section 8, the lint floor
//! every workspace member must opt in to. Why such a gate is its own crate: `CONTRIBUTING.md`,
//! "Everything else". What it cannot see: section 8's third correction. Nothing depends on this
//! crate; its product is a failing test whose message says what to add and where.
//!
//! The member set comes from `cargo metadata`, never from `workspace.members`: measured
//! 2026-11-12 on cargo 1.99.1, a crate reached only by a `path` dependency from inside the
//! workspace directory is a member that `members` never lists, is compiled by
//! `cargo clippy --locked --all-targets`, inherits nothing, and leaves all three CI commands at
//! exit 0 over an `unwrap()` in it. That call also settles `exclude`, `default-members` and globs.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

const MANIFEST: &str = "Cargo.toml";
/// The two lines a crate needs in its own manifest to inherit the floor, indented for a message.
const STANZA: &str = "        [lints]\n        workspace = true";
const ROW: &str = "docs/design/architecture.md section 8 row G1";
/// The lints a crate loses by omitting the stanza, pre-wrapped for a terminal. Spelled out rather
/// than referenced so a reader need not open the root manifest to see what is switched off.
const FLOOR: &str = concat!(
    "        rust:   unsafe_code, unused_must_use\n",
    "        clippy: unwrap_used, expect_used, panic, todo, unimplemented, unreachable,\n",
    "                let_underscore_must_use",
);
/// The command that will not notice any of this.
const BLIND: &str = "cargo clippy --locked --all-targets -- -D warnings";

/// How one workspace member stands with respect to the lint floor.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// `[lints] workspace = true` is present. Nothing to do.
    Inherits,
    /// No `[lints]` table at all: the ordinary case this check exists for.
    NoLintsTable,
    /// A `[lints]` table that does not inherit. A different mistake from forgetting the stanza:
    /// the crate is stating a lint policy, and nothing compares it with the floor.
    LintsWithoutInheritance,
    /// Cargo named the member; there is no manifest at the path it gave.
    ManifestMissing,
    ManifestUnreadable(String),
    /// The manifest exists and is not valid TOML, so the question cannot be decided.
    ManifestUnparseable(String),
}

/// One workspace member, as cargo resolved it.
#[derive(Debug)]
pub struct Member {
    /// Directory relative to the workspace root, spelled with `/`.
    pub path: String,
    /// False when `workspace.default-members` excludes it, which means no bare `cargo test`,
    /// `cargo clippy` or `cargo build` at the root touches it.
    pub built_by_default: bool,
    pub verdict: Verdict,
}

/// The whole check: `None` when the floor is in force everywhere, otherwise the failure message.
/// `gate_crate_dir` is passed in rather than hard-coded so the check can run against another tree.
pub fn lint_floor_failure(root_manifest: &Path, gate_crate_dir: &str) -> Option<String> {
    let members = match resolve_members(root_manifest) {
        Ok(members) => members,
        Err(why) => return Some(format!("{why}\n\nThe invariant is {ROW}.\n")),
    };

    let mut problems: Vec<String> = Vec::new();
    if !members.iter().any(|member| member.path == gate_crate_dir) {
        problems.push(absent_from_the_workspace(gate_crate_dir));
    }
    for member in &members {
        if let Some(detail) = describe(member) {
            problems.push(format!("    * {}\n{detail}", member.path));
        }
    }
    if problems.is_empty() {
        return None;
    }

    Some(format!(
        concat!(
            "The workspace lint floor is not in force everywhere. {} workspace member(s) ",
            "checked, {} problem(s):\n\n",
            "{}\n\n",
            "Why this matters: `[workspace.lints]` in the root Cargo.toml is opt-in. A crate\n",
            "inherits it only by carrying\n\n",
            "{}\n\n",
            "in its own Cargo.toml. Without those two lines that crate gets none of the ",
            "floor -\n\n",
            "{}\n\n",
            "- and `{}` still exits 0,\n",
            "so nothing else in this repository will tell you. The invariant and the ",
            "measurement\nbehind it are {}.\n\n",
            "If a crate must genuinely be exempt, that is an edit to row G1 and to this test,\n",
            "not a quiet exception in one manifest.\n",
        ),
        members.len(),
        problems.len(),
        problems.join("\n"),
        STANZA,
        FLOOR,
        BLIND,
        ROW,
    ))
}

/// The verdict for a gate crate that is not a member of the workspace it is checking.
fn absent_from_the_workspace(gate_crate_dir: &str) -> String {
    format!(
        "    * {gate_crate_dir} (this crate)\n      \
         cargo does not count `{gate_crate_dir}` as a member of this workspace, so the crate \
         holding the\n      repo-wide checks is outside every CI command: neither `cargo clippy \
         --all-targets` nor\n      `cargo test` compiles it. Add \"{gate_crate_dir}\" to \
         `workspace.members` in the root\n      Cargo.toml. Note that in a tree where this is \
         really true, nothing compiles this file\n      either, so the line you are reading \
         cannot appear: what you would see instead is \
         `cargo test`\n      printing no `repo_invariants` test binary at all."
    )
}

/// The per-member half of the message. `None` for a member that is fine.
fn describe(member: &Member) -> Option<String> {
    let path = &member.path;
    if !member.built_by_default {
        return Some(format!(
            concat!(
                "      `workspace.default-members` in the root Cargo.toml leaves {} out, so a\n",
                "      bare `cargo clippy`, `cargo test` or `cargo build` at the root does not ",
                "touch it -\n      measured 2026-11-12: with the gate crate left out of that ",
                "list, all three CI\n      commands exit 0 and this test does not run at all. ",
                "Either add {} to\n      `default-members`, or delete the `default-members` key ",
                "so that every member is built.",
            ),
            path, path
        ));
    }
    let detail = match &member.verdict {
        Verdict::Inherits => return None,
        Verdict::NoLintsTable => format!(
            concat!(
                "      {}/{} has no `[lints]` table. Add these two lines to it,\n",
                "      as a table of their own; the end of the file is a fine place:\n\n",
                "{}\n\n",
                "      That is the whole fix - the lint set itself stays in the root ",
                "Cargo.toml. It\n      has to be a top-level `[lints]` table: a ",
                "`lints = {{ workspace = true }}` line written\n      inside `[package]` ",
                "becomes `package.lints`, which cargo ignores without even an\n",
                "      unused-key warning (measured 2026-11-12).",
            ),
            path, MANIFEST, STANZA
        ),
        Verdict::LintsWithoutInheritance => format!(
            concat!(
                "      {}/{} has a `[lints]` table of its own and does not\n",
                "      inherit: the table holds no `workspace = true`. This is not the same ",
                "mistake as a\n      missing stanza - the crate is stating a lint policy, and ",
                "nothing in this repository\n      compares that policy with the floor.\n\n",
                "      Cargo will not let the crate have both. Adding `workspace = true` ",
                "beside the\n      entries already there fails with \"cannot override ",
                "`workspace.lints` in `lints`\"\n      (measured 2026-11-12 on cargo 1.99.1). ",
                "So either replace that table with\n\n{}\n\n",
                "      and move whatever it added into `[workspace.lints]` in the root ",
                "Cargo.toml, or\n      record the exemption in {}.",
            ),
            path, MANIFEST, STANZA, ROW
        ),
        Verdict::ManifestMissing => format!(
            concat!(
                "      cargo named {} as a member and there is no manifest at\n",
                "      {}/{}, so nothing can inherit anything. Cargo refuses to load a ",
                "workspace in\n      this state, so reaching this line means the manifest went ",
                "away after cargo read it.",
            ),
            path, path, MANIFEST
        ),
        Verdict::ManifestUnreadable(why) => format!(
            "      {path}/{MANIFEST} could not be read, so the floor cannot be decided for this \
             crate: {why}."
        ),
        Verdict::ManifestUnparseable(why) => format!(
            concat!(
                "      {}/{} is not valid TOML, so whether it inherits the floor\n",
                "      cannot be decided - treat this as a broken manifest first and a lint ",
                "question\n      second:\n\n      {}\n\n",
                "      cargo refuses to load the workspace in this state too.",
            ),
            path,
            MANIFEST,
            // The parser's own report is several lines with its own caret diagram; indent the
            // continuation lines so the block stays inside this bullet.
            why.replace('\n', "\n      ")
        ),
    };
    Some(detail)
}

/// Ask cargo for the member set and read each member's manifest for its lint verdict. `Err` means
/// the member set is unknown; the check then fails rather than passing over a set it does not
/// trust, because a silently dropped member is a crate with no lint floor and a green CI.
pub fn resolve_members(root_manifest: &Path) -> Result<Vec<Member>, String> {
    let json = cargo_metadata(root_manifest)?;
    let meta: serde_json::Value = serde_json::from_slice(&json)
        .map_err(|why| format!("`cargo metadata` printed something that is not JSON: {why}"))?;
    let root = PathBuf::from(string_at(&meta, "workspace_root")?);
    let ids = array_at(&meta, "workspace_members")?;
    let default_ids = array_at(&meta, "workspace_default_members")?;
    let packages = array_at(&meta, "packages")?;

    let mut members: Vec<Member> = Vec::new();
    for id in ids {
        let package = packages
            .iter()
            .find(|package| package.get("id") == Some(id))
            .ok_or_else(|| {
                format!("`cargo metadata` lists the member {id} with no matching package object")
            })?;
        let manifest = PathBuf::from(string_at(package, "manifest_path")?);
        let dir = manifest.parent().unwrap_or(&root);
        members.push(Member {
            path: relative(&root, dir),
            built_by_default: default_ids.contains(id),
            verdict: read_verdict(dir),
        });
    }
    // cargo prints the order of `members`, which is stable but not sorted.
    members.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(members)
}

/// `cargo metadata --no-deps` over one workspace, as raw stdout. One subprocess per test run, and
/// `--no-deps` resolves no dependency graph, so it needs neither the network nor an up-to-date
/// lock file and does not deadlock on the package-cache lock the build already released.
fn cargo_metadata(root_manifest: &Path) -> Result<Vec<u8>, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(&cargo)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .arg("--manifest-path")
        .arg(root_manifest)
        .output()
        .map_err(|why| {
            format!(
                "`{cargo} metadata` could not be run, so the workspace member set is unknown: \
                 {why}"
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "`{cargo} metadata --manifest-path {}` failed, so the workspace member set is \
             unknown. Cargo refuses to load a workspace whose member list names a path with no \
             manifest, or a manifest it cannot parse, and that is the usual reason:\n\n{}",
            root_manifest.display(),
            String::from_utf8_lossy(&output.stderr).trim_end()
        ));
    }
    Ok(output.stdout)
}

fn string_at(value: &serde_json::Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("`cargo metadata` has no string `{key}`"))
}

fn array_at<'a>(
    value: &'a serde_json::Value,
    key: &str,
) -> Result<&'a Vec<serde_json::Value>, String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("`cargo metadata` has no array `{key}`"))
}

/// A member directory as a reader of the root manifest would spell it. A member outside the
/// workspace root cannot be written relative to it, so its absolute path is kept whole.
fn relative(root: &Path, dir: &Path) -> String {
    match dir.strip_prefix(root) {
        Ok(rest) => rest
            .components()
            .collect::<PathBuf>()
            .to_string_lossy()
            .replace('\\', "/"),
        Err(_) => dir.to_string_lossy().into_owned(),
    }
}

/// Read one member manifest and decide its verdict. A parse, not a line scan: a scan passes green
/// on `[lints.rust]`, on `workspace = true` under another table, and on a commented-out line. Not
/// `cargo metadata` either: measured 2026-11-12, a package object's 24 keys include no `lints`.
fn read_verdict(member_dir: &Path) -> Verdict {
    let text = match std::fs::read_to_string(member_dir.join(MANIFEST)) {
        Ok(text) => text,
        Err(why) if why.kind() == ErrorKind::NotFound => return Verdict::ManifestMissing,
        Err(why) => return Verdict::ManifestUnreadable(why.to_string()),
    };
    let doc: toml::Table = match text.parse() {
        Ok(doc) => doc,
        Err(why) => return Verdict::ManifestUnparseable(why.to_string()),
    };
    // A `[lints]` table with `workspace = true` and the inline `lints = { workspace = true }` are
    // the same manifest. Inside `[package]` it becomes `package.lints`, so `doc.get` is empty.
    match doc.get("lints").and_then(toml::Value::as_table) {
        None => Verdict::NoLintsTable,
        Some(lints) => match lints.get("workspace").and_then(toml::Value::as_bool) {
            Some(true) => Verdict::Inherits,
            // `workspace = false` is rejected by cargo itself ("`workspace` cannot be false",
            // measured 2026-11-12), so it needs no verdict of its own and lands here.
            _ => Verdict::LintsWithoutInheritance,
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// This crate's directory name, from `CARGO_MANIFEST_DIR` rather than from a search, which
    /// assumes the crate sits directly below the root. Move it deeper and `cargo metadata` is
    /// asked for a manifest that is not there, or for one with no `[workspace]` table, and says so.
    fn gate_crate_dir() -> String {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn root_manifest() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap_or(Path::new("."))
            .join(MANIFEST)
    }

    /// Row G1. Every crate cargo counts as a member here carries `[lints] workspace = true`, this
    /// one included, and every one of them is built by default.
    #[test]
    fn every_workspace_member_inherits_the_lint_floor() {
        // `clippy::panic` is denied here too, so the report arrives as an assertion message.
        let failure = lint_floor_failure(&root_manifest(), &gate_crate_dir()).unwrap_or_default();
        assert!(failure.is_empty(), "\n\n{failure}\n");
    }

    // ---- synthetic workspaces, resolved by the real cargo ----

    struct Tree(tempfile::TempDir);

    impl Tree {
        /// `workspace` is the `[workspace]` body; a `[workspace.lints]` table is appended
        /// because a member carrying the inherit stanza does not load without one - cargo
        /// refuses the whole workspace with "`workspace.lints` was not defined".
        fn new(workspace: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = format!("{workspace}\n[workspace.lints.clippy]\nunwrap_used = \"deny\"\n");
            std::fs::write(dir.path().join(MANIFEST), root).unwrap();
            Self(dir)
        }

        fn crate_at(self, path: &str, manifest: &str) -> Self {
            let dir = self.0.path().join(path);
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(dir.join(MANIFEST), manifest).unwrap();
            std::fs::write(dir.join("src/lib.rs"), "pub fn noop() {}\n").unwrap();
            self
        }

        fn failure(&self, gate_crate_dir: &str) -> Option<String> {
            lint_floor_failure(&self.0.path().join(MANIFEST), gate_crate_dir)
        }

        fn paths(&self) -> Vec<String> {
            resolve_members(&self.0.path().join(MANIFEST))
                .unwrap()
                .into_iter()
                .map(|member| member.path)
                .collect()
        }
    }

    /// A member manifest. `name` must be unique inside one synthetic workspace.
    fn manifest(name: &str, tail: &str) -> String {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{tail}")
    }

    const INHERITS: &str = "\n[lints]\nworkspace = true\n";
    const BARE: &str = "";
    const ROOT: &str =
        "[workspace]\nmembers = [\"gate\", \"services/newsvault\"]\nresolver = \"2\"\n";

    #[test]
    fn a_member_without_the_stanza_is_named_and_told_what_to_add() {
        let tree = Tree::new(ROOT)
            .crate_at("gate", &manifest("gate", INHERITS))
            .crate_at("services/newsvault", &manifest("newsvault", BARE));
        let failure = tree.failure("gate").expect("expected a failure");
        assert!(failure.contains("* services/newsvault"), "{failure}");
        assert!(failure.contains("has no `[lints]` table"), "{failure}");
        assert!(failure.contains("workspace = true"), "{failure}");
        assert!(!failure.contains("* gate\n"), "{failure}");
    }

    #[test]
    fn all_members_inheriting_is_silence() {
        let tree = Tree::new(ROOT)
            .crate_at("gate", &manifest("gate", INHERITS))
            .crate_at("services/newsvault", &manifest("newsvault", INHERITS));
        assert_eq!(tree.failure("gate"), None);
    }

    /// The hole the first version of this check had; the measurement is in the module header.
    #[test]
    fn a_path_dependency_is_a_member_even_though_members_does_not_name_it() {
        let tree = Tree::new(ROOT)
            .crate_at("gate", &manifest("gate", INHERITS))
            .crate_at(
                "services/newsvault",
                &manifest(
                    "newsvault",
                    "\n[dependencies]\nderive-rules = { path = \"../derive\" }\n\
                     \n[lints]\nworkspace = true\n",
                ),
            )
            .crate_at("services/derive", &manifest("derive-rules", BARE));
        assert_eq!(
            tree.paths(),
            vec!["gate", "services/derive", "services/newsvault"]
        );
        let failure = tree.failure("gate").expect("expected a failure");
        assert!(failure.contains("* services/derive"), "{failure}");
        assert!(
            failure.contains("3 workspace member(s) checked"),
            "{failure}"
        );
    }

    /// The other way a crate falls out of CI: it stays a member, and `default-members` stops
    /// every root-level cargo command from building it.
    #[test]
    fn default_members_leaving_a_member_out_is_its_own_verdict() {
        let tree = Tree::new(
            "[workspace]\nmembers = [\"gate\", \"services/newsvault\"]\n\
             default-members = [\"services/newsvault\"]\nresolver = \"2\"\n",
        )
        .crate_at("gate", &manifest("gate", INHERITS))
        .crate_at("services/newsvault", &manifest("newsvault", INHERITS));
        let failure = tree.failure("gate").expect("expected a failure");
        assert!(failure.contains("`workspace.default-members`"), "{failure}");
        assert!(failure.contains("leaves gate out"), "{failure}");
    }

    #[test]
    fn exclude_keeps_a_crate_out_of_the_member_set_and_out_of_this_check() {
        let tree = Tree::new(
            "[workspace]\nmembers = [\"gate\", \"services/*\"]\n\
             exclude = [\"services/derive\"]\nresolver = \"2\"\n",
        )
        .crate_at("gate", &manifest("gate", INHERITS))
        .crate_at("services/newsvault", &manifest("newsvault", INHERITS))
        .crate_at("services/derive", &manifest("derive-rules", BARE));
        assert_eq!(tree.paths(), vec!["gate", "services/newsvault"]);
        assert_eq!(tree.failure("gate"), None);
    }

    #[test]
    fn a_glob_entry_is_cargos_to_expand_and_every_match_is_checked() {
        let tree =
            Tree::new("[workspace]\nmembers = [\"gate\", \"services/*\"]\nresolver = \"2\"\n")
                .crate_at("gate", &manifest("gate", INHERITS))
                .crate_at("services/newsvault", &manifest("newsvault", INHERITS))
                .crate_at("services/derive", &manifest("derive-rules", BARE));
        assert_eq!(
            tree.paths(),
            vec!["gate", "services/derive", "services/newsvault"]
        );
        let failure = tree.failure("gate").expect("expected a failure");
        assert!(failure.contains("* services/derive"), "{failure}");
    }

    #[test]
    fn the_inline_lints_table_counts_as_inheritance() {
        let tree = Tree::new(ROOT)
            .crate_at("gate", &manifest("gate", INHERITS))
            .crate_at(
                "services/newsvault",
                &format!(
                    "lints = {{ workspace = true }}\n{}",
                    manifest("newsvault", BARE)
                ),
            );
        assert_eq!(tree.failure("gate"), None);
    }

    /// Measured 2026-11-12: a crate whose `lints = { workspace = true }` sits inside `[package]`,
    /// with an `unwrap()` in it, passes `cargo clippy -- -D warnings` with no output at all. The
    /// mechanism is at `read_verdict`.
    #[test]
    fn an_inline_lints_key_inside_the_package_table_is_not_inheritance() {
        let tree = Tree::new(ROOT)
            .crate_at("gate", &manifest("gate", INHERITS))
            .crate_at(
                "services/newsvault",
                &manifest("newsvault", "lints = { workspace = true }\n"),
            );
        let failure = tree.failure("gate").expect("expected a failure");
        assert!(failure.contains("has no `[lints]` table"), "{failure}");
    }

    #[test]
    fn the_gate_crate_policing_itself_is_the_same_check() {
        let tree = Tree::new(ROOT)
            .crate_at("gate", &manifest("gate", BARE))
            .crate_at("services/newsvault", &manifest("newsvault", INHERITS));
        let failure = tree.failure("gate").expect("expected a failure");
        assert!(failure.contains("* gate\n"), "{failure}");
        assert!(failure.contains("has no `[lints]` table"), "{failure}");
    }

    #[test]
    fn a_gate_crate_outside_the_workspace_is_a_different_message() {
        let tree = Tree::new("[workspace]\nmembers = [\"services/newsvault\"]\nresolver = \"2\"\n")
            .crate_at("gate", &manifest("gate", INHERITS))
            .crate_at("services/newsvault", &manifest("newsvault", INHERITS));
        let failure = tree.failure("gate").expect("expected a failure");
        assert!(
            failure.contains("does not count `gate` as a member"),
            "{failure}"
        );
        assert!(failure.contains("outside every CI command"), "{failure}");
    }

    #[test]
    fn its_own_lints_table_reads_differently_from_a_missing_stanza() {
        let tree = Tree::new(ROOT)
            .crate_at("gate", &manifest("gate", INHERITS))
            .crate_at(
                "services/newsvault",
                &manifest("newsvault", "\n[lints.clippy]\nunwrap_used = \"warn\"\n"),
            );
        let failure = tree.failure("gate").expect("expected a failure");
        assert!(failure.contains("table of its own"), "{failure}");
        assert!(
            failure.contains("cannot override `workspace.lints`"),
            "{failure}"
        );
        assert!(!failure.contains("has no `[lints]` table"), "{failure}");
    }

    /// Cargo refuses to load a workspace whose `members` names a path with no manifest, so the
    /// member set is unknown and the check says that rather than reporting a smaller set.
    #[test]
    fn a_member_path_that_does_not_exist_is_a_refusal_not_a_smaller_member_set() {
        let tree = Tree::new(ROOT).crate_at("gate", &manifest("gate", INHERITS));
        let failure = tree.failure("gate").expect("expected a failure");
        assert!(
            failure.contains("the workspace member set is unknown"),
            "{failure}"
        );
        assert!(failure.contains("services/newsvault"), "{failure}");
    }

    #[test]
    fn a_member_manifest_that_is_not_toml_is_also_a_refusal() {
        let tree = Tree::new(ROOT).crate_at("gate", &manifest("gate", INHERITS));
        let bad = tree.0.path().join("services/newsvault");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join(MANIFEST), "[package\nname = \"newsvault\"\n").unwrap();
        let failure = tree.failure("gate").expect("expected a failure");
        assert!(
            failure.contains("the workspace member set is unknown"),
            "{failure}"
        );
    }

    /// The verdicts for a manifest that goes missing or stops parsing after cargo has read it:
    /// unreachable through `resolve_members` by construction, so they are checked directly.
    #[test]
    fn a_manifest_that_disappears_or_stops_parsing_has_its_own_verdict() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_verdict(dir.path()), Verdict::ManifestMissing);
        std::fs::write(dir.path().join(MANIFEST), "[package\n").unwrap();
        // `clippy::panic` is denied here too, so the shape is asserted rather than destructured
        // with a panicking `else` arm.
        let verdict = read_verdict(dir.path());
        assert!(
            matches!(&verdict, Verdict::ManifestUnparseable(why) if why.contains("unclosed table")),
            "{verdict:?}"
        );
    }

    #[test]
    fn a_root_manifest_with_no_workspace_table_is_a_refusal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(MANIFEST),
            "[package]\nname = \"c\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let failure =
            lint_floor_failure(&dir.path().join(MANIFEST), "gate").expect("expected a failure");
        assert!(
            failure.contains("the workspace member set is unknown"),
            "{failure}"
        );
    }
}
