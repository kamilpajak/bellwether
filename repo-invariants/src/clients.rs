//! The one-crate HTTP allowlist: `docs/design/architecture.md` section 8 row R4's first hole.
//! Only the seam crate may DECLARE an HTTP or model client, asked of `cargo metadata` for the
//! same reason row G1 asks it there - `workspace.members` is not the member set.
//!
//! What a manifest question cannot see, stated so the row does not claim it: a call site. A member
//! may still reach a socket through `std::net`, through `std::process::Command` running curl, or
//! through a client crate whose name is not in [`CLIENT_CRATES`]. The names are a list, and a list
//! is not a closure.

use std::path::Path;

use crate::{array_at, cargo_metadata, string_at, ROW_R4};

/// Crate names that open a socket or carry a conversation with a model. The list is the gate's
/// whole reach, so it names the clients in use plus the ones a change here would plausibly pull
/// in; `ureq` is the only one this workspace holds (2026-11-13).
pub const CLIENT_CRATES: [&str; 20] = [
    "anthropic-sdk",
    "async-anthropic",
    "async-openai",
    "attohttpc",
    "awc",
    "aws-sdk-bedrockruntime",
    "clust",
    "curl",
    "hyper",
    "hyper-util",
    "isahc",
    "minreq",
    "misanthropic",
    "openai",
    "openai-api-rs",
    "reqwest",
    "rig-core",
    "surf",
    "tonic",
    "ureq",
];

/// The members allowed to declare one. One name, which is what makes the check complete rather
/// than a convention: `news-provider::wire` is the only module in the workspace holding a client.
pub const CLIENT_ALLOWLIST: [&str; 1] = ["news-provider"];

/// One declared edge from a workspace member to a client crate.
#[derive(Debug, PartialEq, Eq)]
pub struct DeclaredClient {
    pub member: String,
    pub dependency: String,
    /// `normal`, `dev` or `build`, as cargo spells the kind. A dev edge counts: a test that opens
    /// a socket is a test that talks to a vendor, which the provider fixtures exist to avoid.
    pub kind: String,
}

/// Every member-to-client edge the manifests declare, allowlisted or not. `Err` means the member
/// set is unknown, which is a different answer from "no member declares one".
pub fn declared_clients(root_manifest: &Path) -> Result<Vec<DeclaredClient>, String> {
    let json = cargo_metadata(root_manifest)?;
    let meta: serde_json::Value = serde_json::from_slice(&json)
        .map_err(|why| format!("`cargo metadata` printed something that is not JSON: {why}"))?;
    let mut found: Vec<DeclaredClient> = Vec::new();
    for package in array_at(&meta, "packages")? {
        let member = string_at(package, "name")?;
        for dependency in array_at(package, "dependencies")? {
            let name = string_at(dependency, "name")?;
            if !CLIENT_CRATES.contains(&name.as_str()) {
                continue;
            }
            found.push(DeclaredClient {
                member: member.clone(),
                dependency: name,
                kind: dependency
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("normal")
                    .to_owned(),
            });
        }
    }
    found.sort_by(|left, right| {
        (&left.member, &left.dependency).cmp(&(&right.member, &right.dependency))
    });
    Ok(found)
}

/// The check: `None` when the only member declaring a client is the one allowed to.
pub fn http_client_allowlist_failure(root_manifest: &Path) -> Option<String> {
    let declared = match declared_clients(root_manifest) {
        Ok(declared) => declared,
        Err(why) => return Some(format!("{why}\n\nThe invariant is {ROW_R4}.\n")),
    };
    let problems: Vec<String> = declared
        .iter()
        .filter(|edge| !CLIENT_ALLOWLIST.contains(&edge.member.as_str()))
        .map(|edge| {
            format!(
                "    * {} declares `{}` as a {} dependency",
                edge.member, edge.dependency, edge.kind
            )
        })
        .collect();
    if problems.is_empty() {
        return None;
    }
    Some(format!(
        concat!(
            "A workspace member outside the allowlist declares an HTTP or model client. {} ",
            "problem(s):\n\n{}\n\n",
            "Why this matters: the capability check of {} is a type - `Wire::send` consumes a\n",
            "`SendPermit` only an open attempt row can hand out - and field privacy is ",
            "module-scoped.\nA second crate holding its own client writes a permitless fetch that ",
            "compiles clean, with no\nledger row and nothing in the gate mentioning it. The ",
            "allowed member(s): {}.\n\n",
            "If another crate must genuinely hold a client, that is an edit to row R4 and to this\n",
            "list, not a line in one manifest. What this check cannot see is a CALL SITE: ",
            "`std::net`,\n`std::process::Command` running curl, and any client crate not named in ",
            "`CLIENT_CRATES`\nall pass it.\n",
        ),
        problems.len(),
        problems.join("\n"),
        ROW_R4,
        CLIENT_ALLOWLIST.join(", "),
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::tests::{manifest, root_manifest, Tree, INHERITS};

    const A_CLIENT: &str = "\n[dependencies]\nureq = \"3.5.2\"\n";
    const A_CLIENT_IN_TESTS: &str = "\n[dev-dependencies]\nreqwest = \"0.12\"\n";
    const SEAM: &str = "news-provider";

    /// Row R4 over this workspace. The mutation is in the architecture's own cell: adding
    /// `ureq` to `panel-contract/Cargo.toml` turns this red, measured 2026-11-13.
    #[test]
    fn only_the_seam_crate_declares_an_http_client() {
        let failure = http_client_allowlist_failure(&root_manifest()).unwrap_or_default();
        assert!(failure.is_empty(), "\n\n{failure}\n");
    }

    /// The seam crate is on the allowlist because it holds the client, so the gate has to stay
    /// green over the one member that really does declare one.
    #[test]
    fn the_seam_crate_declares_the_only_client_this_workspace_holds() {
        let declared = declared_clients(&root_manifest()).unwrap();
        assert_eq!(
            declared,
            vec![DeclaredClient {
                member: SEAM.to_owned(),
                dependency: "ureq".to_owned(),
                kind: "normal".to_owned(),
            }]
        );
    }

    #[test]
    fn adding_an_http_client_to_a_member_other_than_the_seam_crate_exits_one_hundred_and_one() {
        let tree = Tree::new(TWO_MEMBERS)
            .crate_at("gate", &manifest("gate", INHERITS))
            .crate_at(
                "services/newsvault",
                &manifest("newsvault", &format!("{INHERITS}{A_CLIENT}")),
            );
        let failure =
            http_client_allowlist_failure(&tree.root_manifest()).expect("expected a failure");
        assert!(
            failure.contains("newsvault declares `ureq` as a normal dependency"),
            "{failure}"
        );
        assert!(failure.contains("permitless fetch"), "{failure}");
    }

    #[test]
    fn removing_it_again_leaves_the_gate_green() {
        let tree = Tree::new(TWO_MEMBERS)
            .crate_at("gate", &manifest("gate", INHERITS))
            .crate_at("services/newsvault", &manifest("newsvault", INHERITS));
        assert_eq!(http_client_allowlist_failure(&tree.root_manifest()), None);
    }

    /// The same hole row G1 measured: a crate reached only by a `path` dependency is a member
    /// `workspace.members` never lists, and a manifest check that read that list would miss it.
    #[test]
    fn the_allowlist_is_read_from_cargo_metadata_and_not_from_the_members_list() {
        let tree = Tree::new(TWO_MEMBERS)
            .crate_at("gate", &manifest("gate", INHERITS))
            .crate_at(
                "services/newsvault",
                &manifest(
                    "newsvault",
                    "\n[dependencies]\nderive-rules = { path = \"../derive\" }\n\
                     \n[lints]\nworkspace = true\n",
                ),
            )
            .crate_at(
                "services/derive",
                &manifest("derive-rules", &format!("{INHERITS}{A_CLIENT}")),
            );
        let root = tree.root_manifest();
        assert!(
            !std::fs::read_to_string(&root)
                .unwrap()
                .contains("services/derive"),
            "the point of this test is that `members` does not name the crate"
        );
        let failure = http_client_allowlist_failure(&root).expect("expected a failure");
        assert!(
            failure.contains("derive-rules declares `ureq`"),
            "{failure}"
        );
    }

    /// A dev edge is an edge. The provider fixtures exist so no test opens a socket, and a
    /// `dev-dependencies` client is how that returns.
    #[test]
    fn a_client_declared_only_for_tests_is_still_a_declared_client() {
        let tree = Tree::new(TWO_MEMBERS)
            .crate_at("gate", &manifest("gate", INHERITS))
            .crate_at(
                "services/newsvault",
                &manifest("newsvault", &format!("{INHERITS}{A_CLIENT_IN_TESTS}")),
            );
        let failure =
            http_client_allowlist_failure(&tree.root_manifest()).expect("expected a failure");
        assert!(
            failure.contains("newsvault declares `reqwest` as a dev dependency"),
            "{failure}"
        );
    }

    const TWO_MEMBERS: &str =
        "[workspace]\nmembers = [\"gate\", \"services/newsvault\"]\nresolver = \"2\"\n";
}
