//! `newsvault blobs pack`: the blobs and dictionaries a commit log names that no pack of the
//! backup holds, as one tar of store-relative paths and its manifest (architecture.md section
//! 10, N1, "Packs"). The caller states what the backup folder holds; nothing here reads it.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::blobs::BlobStore;
use crate::store::{self, StoreError};

pub const SCHEMA: &str = "newsvault.blobs.pack/1";
/// A manifest is named by its pack's file name plus this.
pub const MANIFEST_SUFFIX: &str = ".manifest";
const PACK: &str = "pack";
const DICTIONARY: &str = "dictionary";
const BLOB: &str = "blob";
const END: &str = "end";
const MEMBER_MODE: u32 = 0o644;

#[derive(Debug, Clone)]
pub struct Ask {
    pub store: PathBuf,
    /// The commit log whose rows are packed: the backup's snapshot, or a restored copy.
    pub log: PathBuf,
    /// `<pack file name> <bytes>` per line: every pack the folder holds.
    pub have: PathBuf,
    /// Every manifest of the folder, one after another.
    pub manifests: PathBuf,
    /// The tar to write; `None` plans and writes nothing.
    pub out: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub store: String,
    pub log: String,
    /// The blobs and dictionaries the log names.
    pub named: u64,
    /// Of `named`, those in a pack the folder holds at its manifest's length.
    pub held: u64,
    /// Members written, or the ones a dry run would write: `named - held`, plus a dictionary a
    /// packed blob names that the log does not.
    pub packed: u64,
    /// The file written; `null` when nothing was left to pack, and in a dry run.
    pub pack: Option<String>,
    pub pack_bytes: u64,
    /// Manifests whose pack is absent, of another length, or whose `end` line is missing.
    pub lost: Vec<String>,
    /// Packs no manifest names.
    pub unrecorded: Vec<String>,
    pub dry_run: bool,
    pub changed: bool,
}

impl Report {
    /// Every manifest has its pack and every pack its manifest.
    #[must_use]
    pub fn is_whole(&self) -> bool {
        self.lost.is_empty() && self.unrecorded.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("no {what} at {}", .path.display())]
    InputAbsent { what: &'static str, path: PathBuf },
    #[error("{} exists, and a pack never replaces a file", .0.display())]
    OutputExists(PathBuf),
    #[error("line {line} of {} is not a {expected} line: {text:?}", .file.display())]
    Unreadable {
        file: PathBuf,
        line: usize,
        expected: &'static str,
        text: String,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl PackError {
    /// The section 5.1 registry code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InputAbsent { .. }
            | Self::Store(StoreError::BlobAbsent { .. } | StoreError::DictionaryAbsent { .. }) => {
                "input_absent"
            }
            Self::OutputExists(_) => "invalid_argument",
            Self::Unreadable { .. }
            | Self::Store(
                StoreError::BlobAltered { .. }
                | StoreError::BlobUndecodable { .. }
                | StoreError::DictionaryAltered { .. },
            ) => "input_unparseable",
            Self::Store(_) | Self::Sqlite(_) | Self::Io(_) => "io_failed",
        }
    }

    #[must_use]
    pub const fn exit(&self) -> u8 {
        match self.code().as_bytes() {
            b"input_absent" => 4,
            _ => 2,
        }
    }
}

/// One manifest of the stream: `pack <name> <bytes>`, its entry lines, `end <entries>`.
struct Manifest {
    pack: String,
    bytes: u64,
    entries: Vec<String>,
    ended: bool,
}

fn text_of(path: &Path, what: &'static str) -> Result<String, PackError> {
    match fs::read_to_string(path) {
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => {
            Err(PackError::InputAbsent {
                what,
                path: path.to_path_buf(),
            })
        }
        read => Ok(read?),
    }
}

fn packs_held(path: &Path) -> Result<BTreeMap<String, u64>, PackError> {
    let mut held = BTreeMap::new();
    for (index, line) in text_of(path, "pack listing")?.lines().enumerate() {
        let mut words = line.split_whitespace();
        let bytes = words.clone().nth(1).and_then(|bytes| bytes.parse().ok());
        match (words.next(), bytes, words.nth(1)) {
            (Some(name), Some(bytes), None) => held.insert(name.to_owned(), bytes),
            _ => {
                return Err(PackError::Unreadable {
                    file: path.to_path_buf(),
                    line: index + 1,
                    expected: "`<pack> <bytes>`",
                    text: line.to_owned(),
                })
            }
        };
    }
    Ok(held)
}

fn manifests_in(path: &Path) -> Result<Vec<Manifest>, PackError> {
    let mut found: Vec<Manifest> = Vec::new();
    for (index, line) in text_of(path, "manifest stream")?.lines().enumerate() {
        let words: Vec<&str> = line.split(' ').collect();
        let open = found.last_mut().filter(|manifest| !manifest.ended);
        match (words.as_slice(), open) {
            ([PACK, name, bytes], _) if bytes.parse::<u64>().is_ok() => found.push(Manifest {
                pack: (*name).to_owned(),
                bytes: bytes.parse().unwrap_or_default(),
                entries: Vec::new(),
                ended: false,
            }),
            ([DICTIONARY | BLOB, _], Some(open)) => open.entries.push(line.to_owned()),
            ([END, count], Some(open)) if count.parse() == Ok(open.entries.len()) => {
                open.ended = true;
            }
            _ => {
                return Err(PackError::Unreadable {
                    file: path.to_path_buf(),
                    line: index + 1,
                    expected: "manifest",
                    text: line.to_owned(),
                })
            }
        }
    }
    Ok(found)
}

/// `dictionary <digest>` then `blob <digest>` lines, each kind in digest order. A log from
/// before user_version 16 has no `dictionaries` table; one with no `blobs` table is refused.
fn named_by(log: &Path) -> Result<Vec<String>, PackError> {
    if !log.is_file() {
        return Err(PackError::InputAbsent {
            what: "commit log",
            path: log.to_path_buf(),
        });
    }
    let db = store::read_only(log)?;
    let mut lines = Vec::new();
    for (kind, table) in [(DICTIONARY, "dictionaries"), (BLOB, "blobs")] {
        if kind == DICTIONARY && !store::holds_table(&db, table)? {
            continue;
        }
        let mut rows = db.prepare(&format!("SELECT digest FROM {table} ORDER BY digest"))?;
        let mut rows = rows.query([])?;
        while let Some(row) = rows.next()? {
            lines.push(format!("{kind} {}", row.get::<_, String>(0)?));
        }
    }
    Ok(lines)
}

/// Plans from the log, the listing and the manifests; with `out`, writes the tar and then its
/// manifest beside it. A blob the log names that is not in the store, or does not read back to
/// its digest, ends the run by name with neither file left.
pub fn run(ask: &Ask) -> Result<Report, PackError> {
    let have = packs_held(&ask.have)?;
    let manifests = manifests_in(&ask.manifests)?;
    let named = named_by(&ask.log)?;
    let mut held: HashSet<&str> = HashSet::new();
    let mut lost = Vec::new();
    for manifest in &manifests {
        if manifest.ended && have.get(&manifest.pack) == Some(&manifest.bytes) {
            held.extend(manifest.entries.iter().map(String::as_str));
        } else {
            lost.push(manifest.pack.clone());
        }
    }
    let recorded: BTreeSet<&str> = manifests.iter().map(|m| m.pack.as_str()).collect();
    let unrecorded = have
        .keys()
        .filter(|pack| !recorded.contains(pack.as_str()))
        .cloned()
        .collect();
    let pending: Vec<&str> = named
        .iter()
        .map(String::as_str)
        .filter(|line| !held.contains(line))
        .collect();
    let count = |lines: usize| u64::try_from(lines).unwrap_or(u64::MAX);
    let mut report = Report {
        schema: SCHEMA,
        store: ask.store.display().to_string(),
        log: ask.log.display().to_string(),
        named: count(named.len()),
        held: count(named.len() - pending.len()),
        packed: count(pending.len()),
        pack: None,
        pack_bytes: 0,
        lost,
        unrecorded,
        dry_run: ask.out.is_none(),
        changed: false,
    };
    let Some(out) = ask.out.as_ref().filter(|_| !pending.is_empty()) else {
        return Ok(report);
    };
    let manifest = manifest_of(out);
    for path in [out, &manifest] {
        if fs::symlink_metadata(path).is_ok() {
            return Err(PackError::OutputExists(path.clone()));
        }
    }
    match write(ask, out, &manifest, &pending, &held) {
        Ok((bytes, members)) => {
            report.pack = out.file_name().map(|name| name.to_string_lossy().into());
            report.pack_bytes = bytes;
            report.packed = count(members);
            report.changed = true;
            Ok(report)
        }
        Err(failure) => {
            for path in [out, &manifest] {
                #[allow(
                    clippy::let_underscore_must_use,
                    reason = "the write's own failure is the one reported, and the caller's work \
                              directory is removed with whatever is left in it"
                )]
                let _ = fs::remove_file(path);
            }
            Err(failure)
        }
    }
}

fn manifest_of(pack: &Path) -> PathBuf {
    let mut name = pack.as_os_str().to_owned();
    name.push(MANIFEST_SUFFIX);
    PathBuf::from(name)
}

/// The tar, fsynced, then the manifest that states its length: `(bytes, members)`.
fn write(
    ask: &Ask,
    out: &Path,
    manifest: &Path,
    pending: &[&str],
    held: &HashSet<&str>,
) -> Result<(u64, usize), PackError> {
    let blobs = store::blob_store(&ask.store);
    let mut tar = tar::Builder::new(File::create_new(out)?);
    let mut entries: Vec<String> = Vec::new();
    let mut dictionaries: BTreeSet<String> = BTreeSet::new();
    for line in pending {
        let (kind, hex) = line.split_once(' ').unwrap_or_default();
        if kind == DICTIONARY {
            dictionaries.insert((*line).to_owned());
            append_dictionary(&mut tar, ask, &blobs, hex)?;
        } else {
            let absent = || StoreError::BlobAbsent {
                digest: hex.to_owned(),
            };
            let stored = blobs.stored_file(&blobs.named(hex).ok_or_else(absent)?)?;
            if let Some(needed) = &stored.dictionary {
                let needed_line = format!("{DICTIONARY} {needed}");
                if !held.contains(needed_line.as_str()) && dictionaries.insert(needed_line.clone())
                {
                    append_dictionary(&mut tar, ask, &blobs, needed)?;
                    entries.push(needed_line);
                }
            }
            append(&mut tar, ask, &stored.path, &stored.bytes)?;
        }
        entries.push((*line).to_owned());
    }
    let file = tar.into_inner()?;
    file.sync_all()?;
    let bytes = file.metadata()?.len();
    let name = out.file_name().unwrap_or_default().to_string_lossy();
    let mut text = format!("{PACK} {name} {bytes}\n");
    for entry in &entries {
        text.push_str(entry);
        text.push('\n');
    }
    text.push_str(&format!("{END} {}\n", entries.len()));
    let mut written = File::create_new(manifest)?;
    written.write_all(text.as_bytes())?;
    written.sync_all()?;
    Ok((bytes, entries.len()))
}

fn append_dictionary(
    tar: &mut tar::Builder<File>,
    ask: &Ask,
    blobs: &BlobStore,
    hex: &str,
) -> Result<(), PackError> {
    let dictionary = blobs.load_dictionary(hex)?;
    append(tar, ask, &blobs.dictionary_path(hex), dictionary.bytes())
}

/// One member under its path relative to the store, so a pack unpacks into a store directory.
fn append(
    tar: &mut tar::Builder<File>,
    ask: &Ask,
    path: &Path,
    bytes: &[u8],
) -> Result<(), PackError> {
    let member = path
        .strip_prefix(&ask.store)
        .map_err(std::io::Error::other)?;
    let mut header = tar::Header::new_ustar();
    header.set_size(u64::try_from(bytes.len()).map_err(std::io::Error::other)?);
    header.set_mode(MEMBER_MODE);
    tar.append_data(&mut header, member, bytes)?;
    Ok(())
}
