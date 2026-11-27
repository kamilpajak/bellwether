//! The HTTP allowlist: `docs/design/architecture.md` section 8 row R4's first hole. Only the two
//! conversation owners may DECLARE an HTTP or model client, asked of `cargo metadata` for the
//! same reason row G1 asks it there - `workspace.members` is not the member set. Beside it, the
//! isolation of the model's owner from the news side (row MG10) and of the store core from every
//! member (row FV1).
//!
//! What a manifest question cannot see, stated so the row does not claim it: a call site. A member
//! may still reach a socket through `std::net`, through `std::process::Command` running curl, or
//! through a client crate whose name is not in [`CLIENT_CRATES`]. The names are a list, and a list
//! is not a closure.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
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

/// The members allowed to declare one: one per kind of conversation, each sending only through
/// a permit its own ledger hands out. `news-provider::wire` talks to news vendors and
/// `modelgate::wire` to a model (decision D7 of section 3.5, 2026-11-23). A third name is an
/// edit to R4.
pub const CLIENT_ALLOWLIST: [&str; 2] = ["modelgate", "news-provider"];

/// The model's owner, and the crates it may not reach: the news seam, its adapters and the
/// vault. It reads news only as a snapshot file (section 3.5).
pub const MODEL_OWNER: &str = "modelgate";
const NEWS_SIDE: [&str; 2] = ["news-provider", "newsvault"];
const PROVIDER_DIR: &str = "providers/";
const ROW_MG10: &str = "docs/design/architecture.md section 8 row MG10";
/// The crate of store mechanics, which every context may link and which links none of them.
pub const STORE_CORE: &str = "vault-core";
const ROW_FV1: &str = "docs/design/architecture.md section 8 row FV1";
/// The members section 12.1 says hold no store, and the two crates a store is reached through.
pub const STORE_FREE: [&str; 2] = ["news-provider", "sec-edgar"];
const STORE_CRATES: [&str; 2] = ["rusqlite", STORE_CORE];
/// Each context, and what it may not reach (architecture.md section 12.1, "Dependency rules").
pub const CONTEXTS: [(&str, &[&str]); 2] = [
    (
        "filingvault",
        &["newsvault", "edgar-press", "open-web-feed"],
    ),
    ("newsvault", &["filingvault", "filing-contract"]),
];

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

/// Member name to the (name, is under `providers/`) of each dependency it declares, of any kind.
type Edges = BTreeMap<String, Vec<(String, bool)>>;

/// Every member's declared edges, read from `cargo metadata`.
fn member_edges(root_manifest: &Path) -> Result<Edges, String> {
    let json = cargo_metadata(root_manifest)?;
    let meta: serde_json::Value = serde_json::from_slice(&json)
        .map_err(|why| format!("`cargo metadata` printed something that is not JSON: {why}"))?;
    let mut edges = Edges::new();
    for package in array_at(&meta, "packages")? {
        let member = string_at(package, "name")?;
        let declared = edges.entry(member).or_default();
        for dependency in array_at(package, "dependencies")? {
            let from_providers = dependency
                .get("path")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|path| path.replace('\\', "/").contains(PROVIDER_DIR));
            declared.push((string_at(dependency, "name")?, from_providers));
        }
    }
    Ok(edges)
}

/// Breadth first from `start` over declared edges: each crate reached once, as (name, is under
/// `providers/`, the member that declares the edge). Rows MG10 and FV1 walk the graph through it.
fn reached_from(edges: &Edges, start: &str) -> Vec<(String, bool, String)> {
    let mut seen = BTreeSet::from([start.to_owned()]);
    let mut pending = VecDeque::from([start.to_owned()]);
    let mut reached = Vec::new();
    while let Some(via) = pending.pop_front() {
        for (name, from_providers) in edges.get(&via).into_iter().flatten() {
            if !seen.insert(name.clone()) {
                continue;
            }
            reached.push((name.clone(), *from_providers, via.clone()));
            pending.push_back(name.clone());
        }
    }
    reached
}

/// How a reached crate is named: directly, or through the member that declares the edge.
fn reach(subject: &str, name: &str, via: &str) -> String {
    if via == subject {
        format!("    * {subject} depends on `{name}`")
    } else {
        format!("    * {subject} reaches `{name}` through `{via}`")
    }
}

/// Row MG10: `None` when the model's owner reaches no crate of the news side, directly or through
/// another member, and no member declares a dependency on it. A manifest question, so a file read
/// through `std::fs` passes it.
pub fn model_owner_isolation_failure(root_manifest: &Path) -> Option<String> {
    let read = || -> Result<Vec<String>, String> {
        let edges = member_edges(root_manifest)?;
        let mut problems: Vec<String> = edges
            .iter()
            .flat_map(|(member, declared)| {
                declared
                    .iter()
                    .filter(|(name, _)| name == MODEL_OWNER)
                    .map(move |_| format!("    * {member} depends on `{MODEL_OWNER}`"))
            })
            .collect();
        for (name, from_providers, via) in reached_from(&edges, MODEL_OWNER) {
            if NEWS_SIDE.contains(&name.as_str()) || from_providers {
                problems.push(reach(MODEL_OWNER, &name, &via));
            }
        }
        problems.sort();
        Ok(problems)
    };
    match read() {
        Err(why) => Some(format!("{why}\n\nThe invariant is {ROW_MG10}.\n")),
        Ok(problems) if problems.is_empty() => None,
        Ok(problems) => Some(format!(
            "The model's owner is not isolated ({ROW_MG10}):\n\n{}\n\n`{MODEL_OWNER}` reads news \
             only as a snapshot file, and nothing links the crate that spends money.\n",
            problems.join("\n")
        )),
    }
}

/// Row FV1's context clause: `None` when each context is a member and reaches none of the
/// crates it may not, directly or through another member, by an edge of any kind. A context that
/// is no member is a failure, never a pass.
pub fn context_isolation_failure(root_manifest: &Path) -> Option<String> {
    let read = || -> Result<Vec<String>, String> {
        let edges = member_edges(root_manifest)?;
        let mut problems = Vec::new();
        for (subject, forbidden) in CONTEXTS {
            if !edges.contains_key(subject) {
                return Err(format!("no workspace member is named `{subject}`"));
            }
            for (name, _, via) in reached_from(&edges, subject) {
                if forbidden.contains(&name.as_str()) {
                    problems.push(reach(subject, &name, &via));
                }
            }
        }
        problems.sort();
        Ok(problems)
    };
    match read() {
        Err(why) => Some(format!("{why}\n\nThe invariant is {ROW_FV1}.\n")),
        Ok(problems) if problems.is_empty() => None,
        Ok(problems) => Some(format!(
            "A context reaches a sibling ({ROW_FV1}):\n\n{}\n\nThe shared mechanics are \
             `vault-core`, `news-provider` and `sec-edgar`; a context links neither the other \
             context nor its adapters.\n",
            problems.join("\n")
        )),
    }
}

/// Row FV1's first clause: `None` when [`STORE_CORE`] is a member and declares no dependency of
/// any kind on a workspace member. A crate absent from the member set is a failure, never a pass.
pub fn store_core_isolation_failure(root_manifest: &Path) -> Option<String> {
    let read = || -> Result<Vec<String>, String> {
        let json = cargo_metadata(root_manifest)?;
        let meta: serde_json::Value = serde_json::from_slice(&json)
            .map_err(|why| format!("`cargo metadata` printed something that is not JSON: {why}"))?;
        let packages = array_at(&meta, "packages")?;
        let members = packages
            .iter()
            .map(|package| string_at(package, "name"))
            .collect::<Result<BTreeSet<String>, String>>()?;
        let core = packages
            .iter()
            .find(|package| {
                package.get("name").and_then(serde_json::Value::as_str) == Some(STORE_CORE)
            })
            .ok_or_else(|| format!("no workspace member is named `{STORE_CORE}`"))?;
        let mut problems = Vec::new();
        for dependency in array_at(core, "dependencies")? {
            let name = string_at(dependency, "name")?;
            if members.contains(&name) {
                problems.push(format!("    * {STORE_CORE} depends on `{name}`"));
            }
        }
        problems.sort();
        Ok(problems)
    };
    match read() {
        Err(why) => Some(format!("{why}\n\nThe invariant is {ROW_FV1}.\n")),
        Ok(problems) if problems.is_empty() => None,
        Ok(problems) => Some(format!(
            "The store core is not a leaf ({ROW_FV1}):\n\n{}\n\n`{STORE_CORE}` holds what a store \
             on disk needs whatever it stores, so every context and `modelgate` can link it.\n",
            problems.join("\n")
        )),
    }
}

/// Row FV1's store clause: `None` when every member of [`STORE_FREE`] exists and declares no
/// dependency of any kind on a crate of [`STORE_CRATES`]. A manifest question, as the allowlist is.
pub fn store_free_failure(root_manifest: &Path) -> Option<String> {
    let read = || -> Result<Vec<String>, String> {
        let json = cargo_metadata(root_manifest)?;
        let meta: serde_json::Value = serde_json::from_slice(&json)
            .map_err(|why| format!("`cargo metadata` printed something that is not JSON: {why}"))?;
        let packages = array_at(&meta, "packages")?;
        let mut problems = Vec::new();
        for member in STORE_FREE {
            let package = packages
                .iter()
                .find(|package| {
                    package.get("name").and_then(serde_json::Value::as_str) == Some(member)
                })
                .ok_or_else(|| format!("no workspace member is named `{member}`"))?;
            for dependency in array_at(package, "dependencies")? {
                let name = string_at(dependency, "name")?;
                if STORE_CRATES.contains(&name.as_str()) {
                    problems.push(format!("    * {member} depends on `{name}`"));
                }
            }
        }
        problems.sort();
        Ok(problems)
    };
    match read() {
        Err(why) => Some(format!("{why}\n\nThe invariant is {ROW_FV1}.\n")),
        Ok(problems) if problems.is_empty() => None,
        Ok(problems) => Some(format!(
            "A crate that holds no store declares one ({ROW_FV1}):\n\n{}\n\nThese crates decide \
             what may be sent and what is true of a host. What is stored is a context's.\n",
            problems.join("\n")
        )),
    }
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
    /// `ureq` to `panel-contract/Cargo.toml` turns this red, measured 2026-11-13 and again
    /// 2026-11-23 with two names allowed.
    #[test]
    fn only_the_seam_crate_declares_an_http_client() {
        let failure = http_client_allowlist_failure(&root_manifest()).unwrap_or_default();
        assert!(failure.is_empty(), "\n\n{failure}\n");
    }

    /// Each allowed member is on the list because it holds a client, so the gate has to stay
    /// green over the two members that really do declare one, and the list holds no spare name.
    #[test]
    fn the_two_conversation_owners_declare_the_only_clients_this_workspace_holds() {
        let declared = declared_clients(&root_manifest()).unwrap();
        let edge = |member: &str| DeclaredClient {
            member: member.to_owned(),
            dependency: "ureq".to_owned(),
            kind: "normal".to_owned(),
        };
        assert_eq!(declared, vec![edge(MODEL_OWNER), edge(SEAM)]);
        assert_eq!(CLIENT_ALLOWLIST, [MODEL_OWNER, SEAM]);
    }

    /// Row MG10 over this workspace. Re-run 2026-11-27 after the walk became [`reached_from`]:
    /// the three manifest mutations of the test below, each red here.
    #[test]
    fn the_model_owner_depends_on_nothing_of_the_news_side_and_nothing_depends_on_it() {
        assert_eq!(model_owner_isolation_failure(&root_manifest()), None);
    }

    /// Row FV1 over this workspace. Mutations 2026-11-25, each red here: `news-provider` added to
    /// `vault-core/Cargo.toml`; `canon` added to its `[dev-dependencies]`; [`STORE_CORE`] naming
    /// a crate that does not exist.
    #[test]
    fn the_store_core_depends_on_no_workspace_member() {
        assert_eq!(store_core_isolation_failure(&root_manifest()), None);
    }

    /// Row FV1's store clause over this workspace. Mutations 2026-11-25, each red here and 0 red
    /// of 74 before this test: `rusqlite`, then `vault-core`, added to `news-provider/Cargo.toml`
    /// and to `providers/sec-edgar/Cargo.toml`.
    #[test]
    fn the_seam_and_the_sec_facts_crate_declare_no_store() {
        assert_eq!(store_free_failure(&root_manifest()), None);
    }

    /// Mutations 2026-11-23 on this workspace's own manifests, each red in the test above:
    /// `news-provider` added to `services/modelgate/Cargo.toml`; `modelgate` added to
    /// `services/newsvault/Cargo.toml`; `news-provider` added to `snapshot-contract/Cargo.toml`,
    /// which `modelgate` depends on.
    #[test]
    fn a_news_side_dependency_of_the_model_owner_and_a_dependant_of_it_are_each_named() {
        const TWO: &str =
            "[workspace]\nmembers = [\"services/modelgate\", \"services/newsvault\"]\nresolver = \"2\"\n";
        let reaches = Tree::new(TWO)
            .crate_at(
                "services/modelgate",
                &manifest(
                    "modelgate",
                    "\n[dependencies]\nnewsvault = { path = \"../newsvault\" }\n\n[lints]\nworkspace = true\n",
                ),
            )
            .crate_at("services/newsvault", &manifest("newsvault", INHERITS));
        let failure = model_owner_isolation_failure(&reaches.root_manifest()).unwrap_or_default();
        assert!(
            failure.contains("modelgate depends on `newsvault`"),
            "{failure}"
        );
        let reached = Tree::new(TWO)
            .crate_at("services/modelgate", &manifest("modelgate", INHERITS))
            .crate_at(
                "services/newsvault",
                &manifest(
                    "newsvault",
                    "\n[dependencies]\nmodelgate = { path = \"../modelgate\" }\n\n[lints]\nworkspace = true\n",
                ),
            );
        let failure = model_owner_isolation_failure(&reached.root_manifest()).unwrap_or_default();
        assert!(
            failure.contains("newsvault depends on `modelgate`"),
            "{failure}"
        );
        const THREE: &str = "[workspace]\nmembers = [\"services/modelgate\", \"services/newsvault\", \"between\"]\nresolver = \"2\"\n";
        let through = Tree::new(THREE)
            .crate_at(
                "services/modelgate",
                &manifest(
                    "modelgate",
                    "\n[dependencies]\nbetween = { path = \"../../between\" }\n\n[lints]\nworkspace = true\n",
                ),
            )
            .crate_at(
                "between",
                &manifest(
                    "between",
                    "\n[dependencies]\nnewsvault = { path = \"../services/newsvault\" }\n\n[lints]\nworkspace = true\n",
                ),
            )
            .crate_at("services/newsvault", &manifest("newsvault", INHERITS));
        let failure = model_owner_isolation_failure(&through.root_manifest()).unwrap_or_default();
        assert!(
            failure.contains("modelgate reaches `newsvault` through `between`"),
            "{failure}"
        );
    }

    /// Row FV1's context clause over this workspace. Mutations 2026-11-27, each red here:
    /// `newsvault`, then `edgar-press`, added to `services/filingvault/Cargo.toml`;
    /// `open-web-feed` added to its `[dev-dependencies]`; `filingvault` added to
    /// `services/newsvault/Cargo.toml`; the first context named `filingvaults`.
    #[test]
    fn the_filings_context_and_the_news_context_reach_each_other_through_no_member() {
        assert_eq!(context_isolation_failure(&root_manifest()), None);
    }

    /// The walk's control, on a tree of its own: a sibling reached through a third member is
    /// named from either side. Mutation 2026-11-27, the walk reading direct edges only: red.
    #[test]
    fn a_sibling_reached_through_another_member_is_named_from_either_side() {
        const THREE: &str = "[workspace]\nmembers = [\"services/filingvault\", \"services/newsvault\", \"between\"]\nresolver = \"2\"\n";
        let tree = Tree::new(THREE)
            .crate_at(
                "services/filingvault",
                &manifest(
                    "filingvault",
                    "\n[dependencies]\nbetween = { path = \"../../between\" }\n\n[lints]\nworkspace = true\n",
                ),
            )
            .crate_at(
                "between",
                &manifest(
                    "between",
                    "\n[dependencies]\nnewsvault = { path = \"../services/newsvault\" }\n\n[lints]\nworkspace = true\n",
                ),
            )
            .crate_at("services/newsvault", &manifest("newsvault", INHERITS));
        let failure = context_isolation_failure(&tree.root_manifest()).unwrap_or_default();
        assert!(
            failure.contains("filingvault reaches `newsvault` through `between`"),
            "{failure}"
        );
        let back = Tree::new(THREE)
            .crate_at("services/filingvault", &manifest("filingvault", INHERITS))
            .crate_at("between", &manifest("between", INHERITS))
            .crate_at(
                "services/newsvault",
                &manifest(
                    "newsvault",
                    "\n[dev-dependencies]\nfilingvault = { path = \"../filingvault\" }\n\n[lints]\nworkspace = true\n",
                ),
            );
        let failure = context_isolation_failure(&back.root_manifest()).unwrap_or_default();
        assert!(
            failure.contains("newsvault depends on `filingvault`"),
            "{failure}"
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
