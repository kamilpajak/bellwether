//! `extract_version` (architecture.md section 2): the rules file's digest and this crate's
//! extraction source, so a change to either is a new version on every attempt it reads.

use canon::{CanonError, Digest, FileDigest};

/// Every file under `src/`, as compiled. A file missing here is caught by
/// `every_source_file_is_in_the_extraction_version`, which reads the directory.
const SOURCES: [(&str, &str); 7] = [
    ("depth.rs", include_str!("depth.rs")),
    ("hosts.rs", include_str!("hosts.rs")),
    ("lib.rs", include_str!("lib.rs")),
    ("parse.rs", include_str!("parse.rs")),
    ("rules.rs", include_str!("rules.rs")),
    ("strict.rs", include_str!("strict.rs")),
    ("version.rs", include_str!("version.rs")),
];

pub(crate) fn extract_version(rules: &FileDigest) -> Result<Digest, CanonError> {
    version_of(rules, &SOURCES)
}

fn version_of(rules: &FileDigest, sources: &[(&str, &str)]) -> Result<Digest, CanonError> {
    let digests: Vec<(&str, FileDigest)> = sources
        .iter()
        .map(|(name, text)| (*name, FileDigest::of(text.as_bytes())))
        .collect();
    let named: Vec<(&str, &str)> = digests
        .iter()
        .map(|(name, digest)| (*name, digest.hex()))
        .collect();
    canon::digest::extract_version(rules.hex(), &named)
}

#[cfg(test)]
mod tests {
    use super::{version_of, SOURCES};
    use canon::FileDigest;

    #[test]
    fn every_source_file_is_in_the_extraction_version() {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut listed: Vec<String> = std::fs::read_dir(&directory)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        listed.sort();
        let named: Vec<String> = SOURCES.iter().map(|(name, _)| (*name).to_owned()).collect();
        assert_eq!(listed, named);
    }

    /// One byte of one source file, or of the rules file, is a new version.
    #[test]
    fn one_byte_of_extraction_code_or_rules_changes_the_extraction_version() {
        let rules = FileDigest::of(b"{}");
        let edited = [("parse.rs", "fn a() {}"), ("lib.rs", "")];
        let versions = [
            version_of(&rules, &[("parse.rs", "fn a() {}"), ("lib.rs", "")]).ok(),
            version_of(&rules, &[("parse.rs", "fn b() {}"), ("lib.rs", "")]).ok(),
            version_of(&FileDigest::of(b"{ }"), &edited).ok(),
        ];
        assert!(versions.iter().all(Option::is_some));
        assert_ne!(versions[0], versions[1]);
        assert_ne!(versions[0], versions[2]);
    }
}
