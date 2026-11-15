//! Every member's library target keeps its doc tests on. `compile_fail` blocks are the only
//! enforcement point of §8 row R2, and `[lib] doctest = false` stops rustdoc from running any of
//! them: measured 2026-11-15 on `pit`, all tests stayed green with that one line added.
//!
//! Read from `cargo metadata`, whose target objects carry the effective `doctest` flag, so every
//! way of spelling the switch in a manifest is the same answer here.

use std::path::Path;

use crate::{array_at, cargo_metadata, string_at};

const ROW: &str = "docs/design/architecture.md section 8 row R2";

/// Member packages whose `lib` target has `doctest: false`, sorted. `Err` when cargo's answer
/// cannot be read, which the check reports rather than passing over.
pub fn members_without_doctests(root_manifest: &Path) -> Result<Vec<String>, String> {
    let json = cargo_metadata(root_manifest)?;
    let meta: serde_json::Value = serde_json::from_slice(&json)
        .map_err(|why| format!("`cargo metadata` printed something that is not JSON: {why}"))?;
    let ids = array_at(&meta, "workspace_members")?;
    let mut found = Vec::new();
    for package in array_at(&meta, "packages")? {
        if !package.get("id").is_some_and(|id| ids.contains(id)) {
            continue;
        }
        for target in array_at(package, "targets")? {
            let is_lib = array_at(target, "kind")?
                .iter()
                .any(|kind| kind.as_str() == Some("lib"));
            let doctest = target.get("doctest").and_then(serde_json::Value::as_bool);
            if is_lib && doctest != Some(true) {
                found.push(string_at(package, "name")?);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The check: `None` when every member library runs its doc tests.
pub fn doctests_switched_off_failure(root_manifest: &Path) -> Option<String> {
    let found = match members_without_doctests(root_manifest) {
        Ok(found) => found,
        Err(why) => return Some(format!("{why}\n\nThe invariant is {ROW}.\n")),
    };
    if found.is_empty() {
        return None;
    }
    Some(format!(
        "Doc tests are switched off for: {}.\n\nRemove `doctest = false` from the `[lib]` table: \
         the `compile_fail` blocks are the only enforcement point of {ROW}, and with doc tests \
         off every test stays green.\n",
        found.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::tests::{manifest, root_manifest, Tree, INHERITS};

    const ROOT: &str = "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n";

    #[test]
    fn every_member_library_in_this_workspace_runs_its_doc_tests() {
        assert_eq!(doctests_switched_off_failure(&root_manifest()), None);
    }

    #[test]
    fn a_member_library_with_doctest_false_is_named() {
        let tree = Tree::new(ROOT)
            .crate_at("a", &manifest("a", INHERITS))
            .crate_at(
                "b",
                &manifest("b", &format!("{INHERITS}\n[lib]\ndoctest = false\n")),
            );
        let failure = doctests_switched_off_failure(&tree.root_manifest()).expect("a failure");
        assert!(failure.contains("switched off for: b."), "{failure}");
    }

    #[test]
    fn member_libraries_with_the_default_are_silence() {
        let tree = Tree::new(ROOT)
            .crate_at("a", &manifest("a", INHERITS))
            .crate_at("b", &manifest("b", INHERITS));
        assert_eq!(doctests_switched_off_failure(&tree.root_manifest()), None);
    }
}
