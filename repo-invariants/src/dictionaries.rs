//! No zstd dictionary is committed: one trained on the VPS store is built from vendor pages, and
//! this repository is public. A dictionary is found by zstd's dictionary magic number at offset 0
//! (`ZSTD_MAGIC_DICTIONARY`, which every trained dictionary opens with) or by the store directory
//! it lives in. Hidden files are read too. Unguarded: a raw-content dictionary, which carries no
//! magic number, and a file under a hidden directory (`.github/` too), which the walk never enters.
//! Row: architecture.md §8 B2.

use std::path::Path;

use crate::line_ends::{walk_files, HiddenFiles};

const ROW: &str = "docs/design/architecture.md section 8 row B2";
/// `ZSTD_MAGIC_DICTIONARY`, 0xEC30A437, as it is written on disk.
pub const DICTIONARY_MAGIC: [u8; 4] = [0x37, 0xA4, 0x30, 0xEC];
/// Where `newsvault` keeps dictionaries inside a store.
const STORE_DIRECTORY: &str = "blobs/dictionaries/";

/// The check: `None` when no file in the tree is a zstd dictionary.
pub fn committed_dictionary_failure(root: &Path) -> Option<String> {
    let checked = || -> Result<Vec<String>, String> {
        let mut offenders = Vec::new();
        for relative in walk_files(root, HiddenFiles::Listed)? {
            let bytes = std::fs::read(root.join(&relative))
                .map_err(|why| format!("{relative} could not be read: {why}"))?;
            if bytes.starts_with(&DICTIONARY_MAGIC) {
                offenders.push(format!(
                    "    * {relative} opens with zstd's dictionary magic"
                ));
            } else if relative.contains(STORE_DIRECTORY) {
                offenders.push(format!(
                    "    * {relative} is under a store's {STORE_DIRECTORY}"
                ));
            }
        }
        Ok(offenders)
    };
    match checked() {
        Ok(offenders) if offenders.is_empty() => None,
        Ok(offenders) => Some(format!(
            concat!(
                "{} file(s) in the tree are zstd dictionaries:\n\n{}\n\nA dictionary is trained ",
                "on the VPS from stored vendor pages and stays in that store; tests train one from ",
                "repo fixtures at run time. The invariant is {}.\n",
            ),
            offenders.len(),
            offenders.join("\n"),
            ROW,
        )),
        Err(why) => Some(format!("{why}\n\nThe invariant is {ROW}.\n")),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    type Outcome = Result<(), Box<dyn std::error::Error>>;

    fn workspace_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf()
    }

    fn tree(files: &[(&str, &[u8])]) -> Result<tempfile::TempDir, std::io::Error> {
        let dir = tempfile::tempdir()?;
        for (path, bytes) in files {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(full, bytes)?;
        }
        Ok(dir)
    }

    /// Over this tree. Mutation 2026-11-21: listed in architecture.md §8 row B2.
    #[test]
    fn no_file_in_this_tree_is_a_zstd_dictionary() {
        assert_eq!(committed_dictionary_failure(&workspace_root()), None);
    }

    #[test]
    fn a_file_opening_with_the_dictionary_magic_or_under_a_store_s_dictionaries_fails() -> Outcome {
        let dir = tree(&[
            ("fixtures/page.html", b"<html></html>"),
            ("fixtures/trained.dict", &[0x37, 0xA4, 0x30, 0xEC, 1, 2, 3]),
            ("docs/.hidden.dict", &[0x37, 0xA4, 0x30, 0xEC, 1, 2]),
            ("vault/blobs/dictionaries/ab12", b"raw content"),
        ])?;
        let failure = committed_dictionary_failure(dir.path()).unwrap_or_default();
        assert_eq!(
            (
                failure.contains("fixtures/trained.dict opens"),
                failure.contains("docs/.hidden.dict opens"),
                failure.contains("vault/blobs/dictionaries/ab12 is under"),
                failure.contains("page.html"),
            ),
            (true, true, true, false)
        );
        Ok(())
    }
}
