//! Every member's library target keeps its doc tests and unit tests on, at `src/lib.rs`, and no
//! committed config filters rustdoc. `compile_fail` blocks are the only enforcement point of §8
//! row R2. Measured 2026-11-15 on `pit`, each of these left every test green with the block not
//! run: `doctest = false` (also behind `crate-type = ["rlib"]`), `path` to a header-less copy,
//! `test = false` with `compile_fail,ignore`, and `--test-args=--skip` in `rustdocflags`.

use std::path::Path;

use crate::{array_at, cargo_metadata, string_at};

const ROW: &str = "docs/design/architecture.md section 8 row R2";
/// Target kinds that are not the library; every other kind (`lib`, `rlib`, `dylib`, `cdylib`,
/// `staticlib`, `proc-macro`) is.
const NOT_LIBRARY: [&str; 5] = ["bin", "test", "bench", "example", "custom-build"];
const LIB_PATH: &str = "src/lib.rs";
/// Where a committed setting can pass `--test-args` to rustdoc, and how each spells it.
const FLAG_SOURCES: [(&str, &str); 3] = [
    (".cargo/config.toml", "rustdocflags"),
    (".cargo/config", "rustdocflags"),
    (".github/workflows", "RUSTDOCFLAGS"),
];

/// One way a member library's doc tests stop running, as a line for the failure message.
pub fn offences(root_manifest: &Path) -> Result<Vec<String>, String> {
    let json = cargo_metadata(root_manifest)?;
    let meta: serde_json::Value = serde_json::from_slice(&json)
        .map_err(|why| format!("`cargo metadata` printed something that is not JSON: {why}"))?;
    let ids = array_at(&meta, "workspace_members")?;
    let mut found = Vec::new();
    for package in array_at(&meta, "packages")? {
        if !package.get("id").is_some_and(|id| ids.contains(id)) {
            continue;
        }
        let name = string_at(package, "name")?;
        for target in array_at(package, "targets")? {
            let is_library = array_at(target, "kind")?.iter().any(|kind| {
                kind.as_str()
                    .is_some_and(|kind| !NOT_LIBRARY.contains(&kind))
            });
            if !is_library {
                continue;
            }
            for flag in ["doctest", "test"] {
                if target.get(flag).and_then(serde_json::Value::as_bool) != Some(true) {
                    found.push(format!("{name}: `{flag}` is not true on the library"));
                }
            }
            let src = string_at(target, "src_path")?.replace('\\', "/");
            if !src.ends_with(&format!("/{LIB_PATH}")) {
                found.push(format!("{name}: the library is {src}, not {LIB_PATH}"));
            }
        }
    }
    let root = root_manifest.parent().unwrap_or(Path::new("."));
    for (place, key) in FLAG_SOURCES {
        for file in files_at(&root.join(place))? {
            let text = std::fs::read_to_string(&file)
                .map_err(|why| format!("{} could not be read: {why}", file.display()))?;
            if text.contains(key) {
                found.push(format!("{} sets `{key}`", file.display()));
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The file itself, or the files directly inside a directory; nothing when the path is absent.
fn files_at(path: &Path) -> Result<Vec<std::path::PathBuf>, String> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if !path.is_dir() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(path)
        .map_err(|why| format!("{} could not be listed: {why}", path.display()))?;
    let mut files = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|why| format!("{} could not be listed: {why}", path.display()))?;
        if entry.path().is_file() {
            files.push(entry.path());
        }
    }
    Ok(files)
}

/// The check: `None` when every member library runs its doc tests.
pub fn doctests_switched_off_failure(root_manifest: &Path) -> Option<String> {
    let found = match offences(root_manifest) {
        Ok(found) => found,
        Err(why) => return Some(format!("{why}\n\nThe invariant is {ROW}.\n")),
    };
    if found.is_empty() {
        return None;
    }
    Some(format!(
        "Doc tests can stop running:\n\n    * {}\n\nThe `compile_fail` blocks are the only \
         enforcement point of {ROW}; with any of these, every test stays green.\n",
        found.join("\n    * ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{manifest, root_manifest, Tree, INHERITS};

    const ROOT: &str = "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n";

    fn failure_with(b_tail: &str) -> String {
        let tree = Tree::new(ROOT)
            .crate_at("a", &manifest("a", INHERITS))
            .crate_at("b", &manifest("b", &format!("{INHERITS}{b_tail}")));
        doctests_switched_off_failure(&tree.root_manifest()).unwrap_or_default()
    }

    #[test]
    fn every_member_library_in_this_workspace_runs_its_doc_tests() {
        assert_eq!(doctests_switched_off_failure(&root_manifest()), None);
    }

    #[test]
    fn a_member_library_with_doctest_false_is_named() {
        let failure = failure_with("\n[lib]\ndoctest = false\n");
        assert!(failure.contains("b: `doctest` is not true"), "{failure}");
    }

    #[test]
    fn an_explicit_crate_type_does_not_hide_doctest_false() {
        let failure = failure_with("\n[lib]\ncrate-type = [\"rlib\"]\ndoctest = false\n");
        assert!(failure.contains("b: `doctest` is not true"), "{failure}");
    }

    #[test]
    fn a_member_library_with_test_false_is_named() {
        let failure = failure_with("\n[lib]\ntest = false\n");
        assert!(failure.contains("b: `test` is not true"), "{failure}");
    }

    #[test]
    fn a_library_moved_off_src_lib_rs_is_named() {
        let tree = Tree::new(ROOT)
            .crate_at("a", &manifest("a", INHERITS))
            .crate_at(
                "b",
                &manifest("b", &format!("{INHERITS}\n[lib]\npath = \"src/real.rs\"\n")),
            );
        tree.write("b/src/real.rs", "pub fn noop() {}\n");
        let failure = doctests_switched_off_failure(&tree.root_manifest()).unwrap_or_default();
        assert!(failure.contains("not src/lib.rs"), "{failure}");
    }

    #[test]
    fn rustdocflags_in_a_committed_cargo_config_are_named() {
        let tree = Tree::new(ROOT)
            .crate_at("a", &manifest("a", INHERITS))
            .crate_at("b", &manifest("b", INHERITS));
        tree.write(
            ".cargo/config.toml",
            "[build]\nrustdocflags = [\"--test-args=--skip=x\"]\n",
        );
        let failure = doctests_switched_off_failure(&tree.root_manifest()).unwrap_or_default();
        assert!(failure.contains("sets `rustdocflags`"), "{failure}");
    }

    #[test]
    fn rustdocflags_in_a_workflow_are_named() {
        let tree = Tree::new(ROOT)
            .crate_at("a", &manifest("a", INHERITS))
            .crate_at("b", &manifest("b", INHERITS));
        tree.write(
            ".github/workflows/ci.yml",
            "env:\n  RUSTDOCFLAGS: --test-args=--skip=x\n",
        );
        let failure = doctests_switched_off_failure(&tree.root_manifest()).unwrap_or_default();
        assert!(failure.contains("sets `RUSTDOCFLAGS`"), "{failure}");
    }

    #[test]
    fn member_libraries_with_the_defaults_are_silence() {
        assert_eq!(failure_with(""), "");
    }
}
