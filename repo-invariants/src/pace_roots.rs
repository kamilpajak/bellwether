//! One pace root for the deployable: `docs/design/architecture.md` section 12.2 and section 8 row
//! R19's hole 2. Three text scans, each with a named mutation in that row: where a service builds
//! a `Wire`, what its pace root is resolved from, and what the unit that starts it mounts. Text,
//! so a `Wire` built through another name and a `Pace` literal written by hand are not seen.

use std::path::Path;

use crate::clocks::rust_files;
use crate::Member;

/// Where a context's crate lives; a second one is scanned by existing on disk.
const SERVICE_DIR: &str = "services/";
const WIRE_BUILT: &str = "Wire::new(";
const ROOT_RESOLVED: &str = "PaceRoot::resolve(";
/// The option's own field: what a resolve call in a service must start with.
const FROM_THE_OPTION: &str = "self.pace_root";
/// The `newsvault` commands that build a `Wire`, one per file [`wire_sites`] finds. A new site
/// adds its command here, and its unit then needs the mount.
pub const SENDING_COMMANDS: [&[&str]; 3] =
    [&["collect"], &["robots", "refresh"], &["roster", "fetch"]];
const UNIT_DIR: &str = "deploy/systemd";
/// The word naming the image in a unit's `ExecStart`; the words after it are the command.
const IMAGE: &str = "${NEWSVAULT_IMAGE}";
pub const MOUNT: [&str; 2] = ["--mount", "type=bind,src=%h/.bellwether/shared,dst=/shared"];
pub const OPTION: [&str; 2] = ["--pace-root", "/shared"];

/// One match of a needle in a service's `src`, with the text that follows it.
struct Found {
    file: String,
    line: usize,
    after: String,
}

/// Every code occurrence of `needle` not preceded by an identifier character, in the `src` tree
/// of each member under [`SERVICE_DIR`].
fn found(root: &Path, members: &[Member], needle: &str) -> Result<Vec<Found>, String> {
    let mut all = Vec::new();
    for member in members
        .iter()
        .filter(|member| member.path.starts_with(SERVICE_DIR))
    {
        let src = root.join(&member.path).join("src");
        if !src.is_dir() {
            continue;
        }
        for file in rust_files(&src)? {
            let text = std::fs::read_to_string(&file)
                .map_err(|why| format!("{} could not be read: {why}", file.display()))?;
            for (at, _) in text.match_indices(needle) {
                let before = &text[..at];
                let line_start = before.rfind('\n').map_or(0, |end| end + 1);
                let named_longer = before
                    .chars()
                    .next_back()
                    .is_some_and(|last| last.is_alphanumeric() || last == '_');
                if named_longer || before[line_start..].contains("//") {
                    continue;
                }
                all.push(Found {
                    file: file
                        .strip_prefix(root)
                        .unwrap_or(&file)
                        .to_string_lossy()
                        .replace('\\', "/"),
                    line: before.matches('\n').count() + 1,
                    after: text[at + needle.len()..].trim_start().to_owned(),
                });
            }
        }
    }
    Ok(all)
}

/// `file:line` of every `Wire::new(` a service writes.
pub fn wire_sites(root: &Path, members: &[Member]) -> Result<Vec<String>, String> {
    Ok(found(root, members, WIRE_BUILT)?
        .iter()
        .map(|site| site.file.clone())
        .collect())
}

/// The files in which a service resolves a pace root, and `file:line` of each call whose first
/// argument is not the option's field.
pub fn resolved_roots(
    root: &Path,
    members: &[Member],
) -> Result<(Vec<String>, Vec<String>), String> {
    let calls = found(root, members, ROOT_RESOLVED)?;
    let unasked = calls
        .iter()
        .filter(|call| !call.after.starts_with(FROM_THE_OPTION))
        .map(|call| format!("{}:{}", call.file, call.line))
        .collect();
    Ok((calls.into_iter().map(|call| call.file).collect(), unasked))
}

/// A unit's lines with comment lines dropped and `\` continuations joined, each as its words.
fn logical_lines(text: &str) -> Vec<Vec<String>> {
    let mut lines = Vec::new();
    let mut open = String::new();
    for line in text
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
    {
        match line.strip_suffix('\\') {
            Some(head) => open.push_str(head),
            None => {
                open.push_str(line);
                lines.push(std::mem::take(&mut open));
            }
        }
    }
    lines
        .iter()
        .map(|line| line.split_whitespace().map(str::to_owned).collect())
        .collect()
}

fn holds(words: &[String], run: &[&str]) -> bool {
    words.windows(run.len()).any(|window| window == run)
}

/// Each start of a sending command in one unit file: the command, and what its line lacks of
/// the mount before the image and the option after it.
pub fn senders(text: &str) -> Vec<(String, Vec<String>)> {
    let mut all = Vec::new();
    for words in logical_lines(text) {
        let Some(image) = words.iter().position(|word| word == IMAGE) else {
            continue;
        };
        let (container, command) = words.split_at(image);
        let Some(sending) = SENDING_COMMANDS
            .iter()
            .find(|sending| command[1..].starts_with_words(sending))
        else {
            continue;
        };
        let mut lacks = Vec::new();
        if !holds(container, &MOUNT) {
            lacks.push(MOUNT.join(" "));
        }
        if !holds(command, &OPTION) {
            lacks.push(OPTION.join(" "));
        }
        all.push((sending.join(" "), lacks));
    }
    all
}

trait StartsWithWords {
    fn starts_with_words(&self, run: &[&str]) -> bool;
}

impl StartsWithWords for [String] {
    fn starts_with_words(&self, run: &[&str]) -> bool {
        self.len() >= run.len() && self.iter().zip(run).all(|(word, wanted)| word == wanted)
    }
}

/// `(unit file, command, what it lacks)` for every sending start under [`UNIT_DIR`], by file name.
pub fn sending_units(root: &Path) -> Result<Vec<(String, String, Vec<String>)>, String> {
    let dir = root.join(UNIT_DIR);
    let listed = |why: std::io::Error| format!("{} could not be listed: {why}", dir.display());
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(listed)? {
        names.push(
            entry
                .map_err(listed)?
                .file_name()
                .to_string_lossy()
                .into_owned(),
        );
    }
    names.sort();
    let mut all = Vec::new();
    for name in names {
        let file = dir.join(&name);
        if !file.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&file)
            .map_err(|why| format!("{} could not be read: {why}", file.display()))?;
        for (command, lacks) in senders(&text) {
            all.push((name.clone(), command, lacks));
        }
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::resolve_members;
    use crate::tests::{manifest, root_manifest, Tree, INHERITS};

    const MAIN: &str = "services/newsvault/src/main.rs";
    /// The modules whose private field only `clap` fills, one per context.
    const OPTION_MODULES: [&str; 2] = [
        "services/filingvault/src/pace_option.rs",
        "services/newsvault/src/pace_option.rs",
    ];
    const ONE_SERVICE: &str = "[workspace]\nmembers = [\"services/newsvault\"]\nresolver = \"2\"\n";

    fn here() -> (std::path::PathBuf, Vec<Member>) {
        let manifest = root_manifest();
        let members = resolve_members(&manifest).unwrap();
        (manifest.parent().unwrap().to_path_buf(), members)
    }

    /// A service file holding `body`, scanned as the real tree is.
    fn planted(body: &str) -> (Vec<String>, (Vec<String>, Vec<String>)) {
        let tree =
            Tree::new(ONE_SERVICE).crate_at("services/newsvault", &manifest("newsvault", INHERITS));
        tree.write(MAIN, body);
        let manifest = tree.root_manifest();
        let members = resolve_members(&manifest).unwrap();
        let root = manifest.parent().unwrap();
        (
            wire_sites(root, &members).unwrap(),
            resolved_roots(root, &members).unwrap(),
        )
    }

    /// One file per sending command: the three of `SENDING_COMMANDS` and `filings collect`,
    /// whose unit arrives in step 17 (architecture.md section 12.8). A `Wire` built anywhere
    /// else is red here. Mutation 2026-11-27, a second `Wire::new(` in `filingvault`: red.
    #[test]
    fn a_service_builds_a_wire_in_exactly_the_sending_commands_of_both_contexts() {
        let (root, members) = here();
        assert_eq!(
            wire_sites(&root, &members).unwrap(),
            vec![
                "services/filingvault/src/collect.rs",
                "services/newsvault/src/cmd_collect.rs",
                "services/newsvault/src/cmd_robots.rs",
                "services/newsvault/src/cmd_roster.rs"
            ]
        );
    }

    /// `Wire::new` takes a `PaceRoot` and `PaceRoot::resolve` is its only constructor, so this
    /// is every pace root a service can hand a `Wire`. Mutation 2026-11-27,
    /// `PaceRoot::resolve(None, store)` in `filingvault`'s option module: red.
    #[test]
    fn every_pace_root_a_service_resolves_comes_from_the_option() {
        let (root, members) = here();
        assert_eq!(
            resolved_roots(&root, &members).unwrap(),
            (
                OPTION_MODULES.map(str::to_owned).to_vec(),
                Vec::<String>::new()
            )
        );
    }

    /// The scans' own controls: a root resolved from nothing is named by its line, a call broken
    /// over two lines by rustfmt is read, and a comment and a longer type name are not sites.
    #[test]
    fn a_pace_root_resolved_from_nothing_is_named_and_a_comment_is_no_site() {
        let body = concat!(
            "// Wire::new(a) and PaceRoot::resolve(None, b) in a comment\n",
            "fn f() { ModelWire::new(k); PaceRoot::resolve(\n    self.pace_root.as_deref(), s); }\n",
            "fn g() { Wire::new(p, c, r, s); PaceRoot::resolve(None, s); }\n",
        );
        assert_eq!(
            planted(body),
            (
                vec![MAIN.to_owned()],
                (vec![MAIN.to_owned(); 2], vec![format!("{MAIN}:4")])
            )
        );
    }

    #[test]
    fn every_unit_that_builds_a_wire_mounts_the_one_pace_root() {
        let (root, _) = here();
        let unit = |name: &str, command: &str| (name.to_owned(), command.to_owned(), Vec::new());
        assert_eq!(
            sending_units(&root).unwrap(),
            vec![
                unit("newsvault-collect.service", "collect"),
                unit("newsvault-robots-refresh.service", "robots refresh"),
                unit("newsvault-roster.service", "roster fetch"),
            ]
        );
    }

    /// The control: a start with neither is named for both, and a command that sends nothing and
    /// a commented start are not read.
    #[test]
    fn a_sending_start_without_the_mount_or_the_option_is_named_for_each() {
        let unit = concat!(
            "# ExecStart=docker run ${NEWSVAULT_IMAGE} collect\n",
            "ExecStart=docker run --mount type=bind,src=%h/.bellwether/vault,dst=/vault \\\n",
            "    ${NEWSVAULT_IMAGE} roster fetch --format json\n",
            "ExecStart=docker run ${NEWSVAULT_IMAGE} roster show --at now\n",
        );
        assert_eq!(
            senders(unit),
            vec![(
                "roster fetch".to_owned(),
                vec![MOUNT.join(" "), OPTION.join(" ")]
            )]
        );
    }
}
