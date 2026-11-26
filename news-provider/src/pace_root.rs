//! Where a deployable's pace files live: the directory a context names with `--pace-root`, under
//! which `budget` keeps `pace/`. What the checks close and leave open:
//! `docs/design/architecture.md` section 12.2 and section 8 row R19, hole 2.

use std::path::{Path, PathBuf};

use crate::budget::PACE_DIR;

/// Why no pace root was resolved. Each maps to one section 5.1 code through
/// [`PaceRootError::code`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum PaceRootError {
    /// A relative path is a bucket per working directory.
    #[error("--pace-root {} is not an absolute path", .0.display())]
    NotAbsolute(PathBuf),
    /// Never created here: a mistyped path would become a second bucket with no error.
    #[error("--pace-root {} does not exist", .0.display())]
    Absent(PathBuf),
    #[error("--pace-root {} is not a directory", .0.display())]
    NotADirectory(PathBuf),
    #[error("{} could not be read: {why}", .path.display())]
    Unreadable { path: PathBuf, why: String },
    /// The store's own pace directory still holds files while the pace root is elsewhere: the
    /// last send instants and any hold a server stated are in the files this run would not read.
    #[error(
        "{} still holds {} while --pace-root names {}: move each file into {}, or remove them",
        .left.display(), .files.join(", "), .root.display(), .root.join(PACE_DIR).display()
    )]
    Split {
        left: PathBuf,
        files: Vec<String>,
        root: PathBuf,
    },
    /// A directory below `pace/` is a second bucket no run reads: what `mv <store>/pace
    /// <root>/pace` leaves when the target exists, and what naming `pace/` itself as the root
    /// creates.
    #[error("{} holds the directory {}: a pace file below it is read by no run", .pace.display(), .files.join(", "))]
    Nested { pace: PathBuf, files: Vec<String> },
    #[error("--pace-root {} names a pace directory itself; name the directory that holds it", .0.display())]
    NamesThePaceDirectory(PathBuf),
}

impl PaceRootError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotAbsolute(_) | Self::NotADirectory(_) | Self::NamesThePaceDirectory(_) => {
                "invalid_argument"
            }
            Self::Absent(_) => "input_absent",
            Self::Unreadable { .. } => "io_failed",
            Self::Split { .. } | Self::Nested { .. } => "pace_root_split",
        }
    }

    /// Section 5.1's exit for [`PaceRootError::code`].
    #[must_use]
    pub const fn exit(&self) -> u8 {
        match self {
            Self::Absent(_) => 4,
            _ => 2,
        }
    }
}

/// The directory a `Wire` keeps `pace/` under. Built only by [`PaceRoot::resolve`], so no `Wire`
/// is handed a store path the option was never asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaceRoot(PathBuf);

impl PaceRoot {
    /// `given` when the caller named one, else the store, unchecked: the behaviour before the
    /// option existed. A named root is an existing absolute directory not itself named `pace`,
    /// whose `pace/` holds no directory; unless that is the store's own, the store's holds no file.
    /// Nothing is written, so a root the run cannot write is not seen here (section 12.2).
    pub fn resolve(given: Option<&Path>, store: &Path) -> Result<Self, PaceRootError> {
        let Some(root) = given else {
            return Ok(Self(store.to_path_buf()));
        };
        if !root.is_absolute() {
            return Err(PaceRootError::NotAbsolute(root.to_path_buf()));
        }
        match std::fs::metadata(root) {
            Ok(found) if found.is_dir() => {}
            Ok(_) => return Err(PaceRootError::NotADirectory(root.to_path_buf())),
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => {
                return Err(PaceRootError::Absent(root.to_path_buf()));
            }
            Err(why) => return Err(unreadable(root, &why)),
        }
        if root.file_name().is_some_and(|last| last == PACE_DIR) {
            return Err(PaceRootError::NamesThePaceDirectory(root.to_path_buf()));
        }
        let pace = root.join(PACE_DIR);
        let nested = directories_in(&pace)?;
        if !nested.is_empty() {
            return Err(PaceRootError::Nested {
                pace,
                files: nested,
            });
        }
        let left = store.join(PACE_DIR);
        let files = names_in(&left)?;
        if files.is_empty() || same_directory(&left, &pace) {
            return Ok(Self(root.to_path_buf()));
        }
        Err(PaceRootError::Split {
            left,
            files,
            root: root.to_path_buf(),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

fn unreadable(path: &Path, why: &std::io::Error) -> PaceRootError {
    PaceRootError::Unreadable {
        path: path.to_path_buf(),
        why: why.to_string(),
    }
}

/// Every name in `dir`, sorted; none when `dir` is absent.
fn names_in(dir: &Path) -> Result<Vec<String>, PaceRootError> {
    Ok(entries_in(dir)?.into_iter().map(|(name, _)| name).collect())
}

/// The names in `dir` that are directories, sorted. A `dir` that is a file is refused by name.
fn directories_in(dir: &Path) -> Result<Vec<String>, PaceRootError> {
    if dir.is_file() {
        return Err(PaceRootError::NotADirectory(dir.to_path_buf()));
    }
    let entries = entries_in(dir)?.into_iter();
    Ok(entries
        .filter_map(|(name, is_dir)| is_dir.then_some(name))
        .collect())
}

/// Every name in `dir` with whether it is a directory, sorted; none when `dir` is absent.
fn entries_in(dir: &Path) -> Result<Vec<(String, bool)>, PaceRootError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(why) => return Err(unreadable(dir, &why)),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|why| unreadable(dir, &why))?;
        let kind = entry.file_type().map_err(|why| unreadable(dir, &why))?;
        names.push((
            entry.file_name().to_string_lossy().into_owned(),
            kind.is_dir(),
        ));
    }
    names.sort();
    Ok(names)
}

fn same_directory(one: &Path, other: &Path) -> bool {
    match (std::fs::canonicalize(one), std::fs::canonicalize(other)) {
        (Ok(one), Ok(other)) => one == other,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{PaceRoot, PaceRootError};
    use crate::budget::PACE_DIR;
    use std::path::Path;

    type Checked = Result<(), Box<dyn std::error::Error>>;

    const RELATIVE: &str = "shared";
    const PER_HOST_FILE: &str = "host.example.test.per-host";
    const HOLD_FILE: &str = "host.example.test.hold";

    fn code(resolved: &Result<PaceRoot, PaceRootError>) -> Option<(&'static str, u8)> {
        resolved
            .as_ref()
            .err()
            .map(|refused| (refused.code(), refused.exit()))
    }

    /// No option is the store, absent or relative as it may be: nothing is read, so a run without
    /// the option does what it did before the option existed.
    #[test]
    fn no_pace_root_named_is_the_store_and_nothing_is_checked() -> Checked {
        let store = Path::new(RELATIVE);
        assert_eq!(PaceRoot::resolve(None, store)?.path(), store);
        Ok(())
    }

    #[test]
    fn a_relative_an_absent_and_a_file_pace_root_are_each_refused_by_name() -> Checked {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("a-file");
        std::fs::write(&file, "")?;
        let absent = dir.path().join("absent");
        let refused = |given: &Path| code(&PaceRoot::resolve(Some(given), dir.path()));
        assert_eq!(
            (
                refused(Path::new(RELATIVE)),
                refused(&absent),
                refused(&file),
                absent.exists()
            ),
            (
                Some(("invalid_argument", 2)),
                Some(("input_absent", 4)),
                Some(("invalid_argument", 2)),
                false
            )
        );
        Ok(())
    }

    /// The first run after the units name the shared root, with the store's files not moved.
    #[test]
    fn pace_files_left_in_the_store_refuse_a_pace_root_elsewhere_by_their_names() -> Checked {
        let (store, shared) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let left = store.path().join(PACE_DIR);
        std::fs::create_dir_all(&left)?;
        let emptied = PaceRoot::resolve(Some(shared.path()), store.path());
        for name in [PER_HOST_FILE, HOLD_FILE] {
            std::fs::write(left.join(name), "")?;
        }
        let split = PaceRoot::resolve(Some(shared.path()), store.path());
        assert_eq!(
            (emptied.map(|root| root.path().to_path_buf()), split),
            (
                Ok(shared.path().to_path_buf()),
                Err(PaceRootError::Split {
                    left,
                    files: vec![HOLD_FILE.to_owned(), PER_HOST_FILE.to_owned()],
                    root: shared.path().to_path_buf(),
                })
            )
        );
        Ok(())
    }

    /// `--pace-root` naming the store is the store's own bucket, not a split.
    #[test]
    fn a_pace_root_that_is_the_store_keeps_the_stores_pace_files() -> Checked {
        let store = tempfile::tempdir()?;
        let left = store.path().join(PACE_DIR);
        std::fs::create_dir_all(&left)?;
        std::fs::write(left.join(PER_HOST_FILE), "")?;
        let resolved = PaceRoot::resolve(Some(store.path()), store.path())?;
        assert_eq!(resolved.path(), store.path());
        Ok(())
    }

    /// The runbook's `mv` repeated onto an existing `pace/`, a `pace` that is a file, and the
    /// pace directory itself named as the root: each a bucket no other run would read.
    #[test]
    fn a_pace_directory_nested_a_file_or_named_as_the_root_is_refused() -> Checked {
        let (store, nested, filed) = (
            tempfile::tempdir()?,
            tempfile::tempdir()?,
            tempfile::tempdir()?,
        );
        let pace = nested.path().join(PACE_DIR);
        std::fs::create_dir_all(pace.join(PACE_DIR))?;
        std::fs::write(pace.join(PER_HOST_FILE), "")?;
        std::fs::write(filed.path().join(PACE_DIR), "")?;
        let refused = |given: &Path| PaceRoot::resolve(Some(given), store.path()).err();
        assert_eq!(
            (
                refused(nested.path()),
                refused(filed.path()).map(|refused| (refused.code(), refused.exit())),
                refused(&pace).map(|refused| (refused.code(), refused.exit())),
            ),
            (
                Some(PaceRootError::Nested {
                    pace: pace.clone(),
                    files: vec![PACE_DIR.to_owned()],
                }),
                Some(("invalid_argument", 2)),
                Some(("invalid_argument", 2)),
            )
        );
        Ok(())
    }

    /// A store `pace/` the run may not list is `io_failed`, not an empty directory. Not run by
    /// root, who lists it.
    #[cfg(unix)]
    #[test]
    fn a_store_pace_directory_that_cannot_be_listed_is_io_failed() -> Checked {
        use std::os::unix::fs::PermissionsExt;
        let (store, shared) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let left = store.path().join(PACE_DIR);
        std::fs::create_dir_all(&left)?;
        std::fs::set_permissions(&left, std::fs::Permissions::from_mode(0o000))?;
        let refused = PaceRoot::resolve(Some(shared.path()), store.path());
        std::fs::set_permissions(&left, std::fs::Permissions::from_mode(0o700))?;
        assert_eq!(code(&refused), Some(("io_failed", 2)));
        Ok(())
    }
}
