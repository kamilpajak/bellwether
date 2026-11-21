//! The content-addressed payload store: architecture.md section 2's "Content-addressed blobs"
//! and section 3.9's L2 layer. A blob is written, fsynced and renamed BEFORE any transaction
//! opens, so a crash leaves an unreferenced file and never an index row about a missing one.
//! The digest is over the payload as received; the file holds it raw (written before
//! compression existed) or as one zstd frame under `<digest>.zst`. The `.zst` layout, the level
//! and how a dictionary is named: architecture.md section 3.9, row "compression".

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{ErrorKind, Read as _, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use zstd::dict::{DecoderDictionary, EncoderDictionary};
use zstd::zstd_safe::{self, CCtx, DCtx};

use crate::store::StoreError;

/// A sha256 over a payload's bytes, lower-case hex. The field is private and [`BlobStore::put`]
/// is the only producer, so a value of this type names bytes this process hashed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlobDigest(String);

impl BlobDigest {
    #[must_use]
    pub fn hex(&self) -> &str {
        &self.0
    }
}

/// The zstd level every blob is written at, chosen by architecture.md section 3.9's level table.
pub const LEVEL: i32 = 9;
/// What marks a compressed file is its name, never its bytes: a payload a server sent as zstd
/// starts with zstd's own magic number and is stored raw all the same.
pub const COMPRESSED_SUFFIX: &str = ".zst";
/// The largest dictionary `dictionary train` writes: the zstd 1.5.7 CLI's `--maxdict` default.
pub const DICTIONARY_BYTES: usize = 112_640;
/// A zstd skippable frame, magic nibble `E`, holding the dictionary's sha256 or nothing.
const HEADER_MAGIC: u32 = 0x184D_2A5E;
const HEADER_FIELDS: usize = 8;
const SHA256_BYTES: usize = 32;
/// `ZSTD_FRAMEHEADERSIZE_MAX`: enough of a frame to read the content size it records.
const FRAME_HEADER_MAX: usize = 18;
/// Hex characters per directory level, two levels deep: the 65,536-directory fan-out
/// architecture.md section 3.9 counts files against.
const FAN_OUT: usize = 2;
const DIGEST_HEX: usize = 64;
const SPACE_DIR: &str = "sha256";
const DICTIONARY_DIR: &str = "dictionaries";
const STAGING_PREFIX: &str = ".tmp-";

/// Which file a blob is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Encoding {
    /// `<digest>`, the payload's own bytes.
    Raw,
    /// `<digest>.zst`, one zstd frame behind the dictionary header.
    Zstd,
}

impl Encoding {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Zstd => "zstd",
        }
    }
}

/// A dictionary as stored: its bytes, re-hashed to the name it was asked by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dictionary {
    digest: String,
    bytes: Vec<u8>,
}

impl Dictionary {
    #[must_use]
    pub fn hex(&self) -> &str {
        &self.digest
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// One digest the store holds, and in which forms. Both is the window `blobs compress` leaves
/// between renaming the `.zst` copy and removing the raw one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub digest: BlobDigest,
    pub raw: bool,
    pub zstd: bool,
}

impl Listed {
    /// The copy a read takes: the raw one while it exists.
    #[must_use]
    pub const fn read_from(&self) -> Encoding {
        if self.raw {
            Encoding::Raw
        } else {
            Encoding::Zstd
        }
    }
}

/// A copy read back and checked against its digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadBack {
    pub encoding: Encoding,
    /// The dictionary the `.zst` header names; `None` for a raw copy and a dictionary-less frame.
    pub dictionary: Option<String>,
    pub stored_bytes: u64,
    pub payload: Vec<u8>,
}

/// One digest of [`BlobStore::stream`]: what is on disk, and its preferred copy read back.
#[derive(Debug)]
pub struct StreamedBlob {
    pub listed: Listed,
    pub read: Result<ReadBack, StoreError>,
}

/// What `blobs compress` did to one raw file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rewritten {
    pub logical_bytes: u64,
    pub stored_bytes: u64,
    /// The `.zst` copy was already there, from a run that stopped before removing the raw one.
    pub resumed: bool,
}

struct Writer {
    context: Option<CCtx<'static>>,
    dictionary: Option<(String, EncoderDictionary<'static>)>,
}

struct Reader {
    context: Option<DCtx<'static>>,
    /// Loaded once per process and kept: a dictionary is never re-read per blob.
    dictionaries: BTreeMap<String, DecoderDictionary<'static>>,
}

pub struct BlobStore {
    /// `<store>/blobs`.
    root: PathBuf,
    writer: RefCell<Writer>,
    reader: RefCell<Reader>,
}

impl std::fmt::Debug for BlobStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobStore")
            .field("root", &self.root)
            .field("dictionary", &self.active_dictionary())
            .finish_non_exhaustive()
    }
}

impl BlobStore {
    /// The blob tree under `blobs`, `<store>/blobs`. Writes no dictionary until one is activated.
    #[must_use]
    pub fn new(blobs: impl Into<PathBuf>) -> Self {
        Self {
            root: blobs.into(),
            writer: RefCell::new(Writer {
                context: None,
                dictionary: None,
            }),
            reader: RefCell::new(Reader {
                context: None,
                dictionaries: BTreeMap::new(),
            }),
        }
    }

    fn space(&self) -> PathBuf {
        self.root.join(SPACE_DIR)
    }

    /// The raw copy's path.
    #[must_use]
    pub fn path_of(&self, digest: &BlobDigest) -> PathBuf {
        let hex = digest.hex();
        self.space()
            .join(&hex[..FAN_OUT])
            .join(&hex[FAN_OUT..FAN_OUT * 2])
            .join(hex)
    }

    #[must_use]
    pub fn compressed_path_of(&self, digest: &BlobDigest) -> PathBuf {
        let mut path = self.path_of(digest).into_os_string();
        path.push(COMPRESSED_SUFFIX);
        PathBuf::from(path)
    }

    fn path_in(&self, digest: &BlobDigest, encoding: Encoding) -> PathBuf {
        match encoding {
            Encoding::Raw => self.path_of(digest),
            Encoding::Zstd => self.compressed_path_of(digest),
        }
    }

    #[must_use]
    pub fn dictionary_path(&self, hex: &str) -> PathBuf {
        self.root.join(DICTIONARY_DIR).join(hex)
    }

    /// Where the bytes land before the rename. Named by the run as well as the digest: a crash
    /// leaves this path behind and [`BlobStore::put`] refuses to reopen it.
    pub(crate) fn staging_path(&self, run: &str, digest: &BlobDigest) -> PathBuf {
        self.space()
            .join(format!("{STAGING_PREFIX}{run}-{}", digest.hex()))
    }

    fn listed(&self, digest: &BlobDigest) -> Listed {
        Listed {
            digest: digest.clone(),
            raw: self.path_of(digest).exists(),
            zstd: self.compressed_path_of(digest).exists(),
        }
    }

    /// Stores a payload under its own digest and returns it. A digest already held in either
    /// form is not written again.
    pub fn put(&self, run: &str, payload: &[u8]) -> Result<BlobDigest, StoreError> {
        let digest = BlobDigest(const_hex::encode(Sha256::digest(payload)));
        let held = self.listed(&digest);
        if held.raw || held.zstd {
            return Ok(digest);
        }
        let encoded = self.encode(payload)?;
        write_durably(
            &self.staging_path(run, &digest),
            &self.compressed_path_of(&digest),
            &encoded,
        )?;
        Ok(digest)
    }

    /// A digest this store ALREADY holds, by its hex, in either form. The file has to be there,
    /// so a value of this type still names bytes this process wrote rather than a string a
    /// caller assembled.
    pub fn named(&self, hex: &str) -> Option<BlobDigest> {
        if !is_digest(hex) {
            return None;
        }
        let digest = BlobDigest(hex.to_owned());
        let held = self.listed(&digest);
        (held.raw || held.zstd).then_some(digest)
    }

    /// The payload stored under `digest`, decoded and re-hashed: bytes that hash to another
    /// digest are `BlobAltered`.
    pub fn read_verified(&self, digest: &BlobDigest) -> Result<Vec<u8>, StoreError> {
        let held = self.listed(digest);
        if !held.raw && !held.zstd {
            return Err(StoreError::BlobAbsent {
                digest: digest.hex().to_owned(),
            });
        }
        Ok(self.read_copy(digest, held.read_from())?.payload)
    }

    /// One copy read back: the raw file as it is, or the `.zst` frame decoded with the dictionary
    /// its header names. Either way the result is re-hashed against `digest`.
    pub fn read_copy(
        &self,
        digest: &BlobDigest,
        encoding: Encoding,
    ) -> Result<ReadBack, StoreError> {
        let file = fs::read(self.path_in(digest, encoding))?;
        let stored_bytes = u64::try_from(file.len()).unwrap_or(u64::MAX);
        let (dictionary, payload) = match encoding {
            Encoding::Raw => (None, file),
            Encoding::Zstd => self.decode(digest, &file)?,
        };
        let found = const_hex::encode(Sha256::digest(&payload));
        if found != digest.hex() {
            return Err(StoreError::BlobAltered {
                digest: digest.hex().to_owned(),
                found,
            });
        }
        Ok(ReadBack {
            encoding,
            dictionary,
            stored_bytes,
            payload,
        })
    }

    /// The payload's length, uncompressed, which is what `blobs.bytes` records and §11.1's held
    /// read compares with the body limit; and the check that no index row is written about a
    /// missing file. A `.zst` copy answers from the content size its frame header records.
    pub fn len_of(&self, digest: &BlobDigest) -> Result<u64, StoreError> {
        let absent = || StoreError::BlobAbsent {
            digest: digest.hex().to_owned(),
        };
        let held = self.listed(digest);
        if held.raw {
            return fs::metadata(self.path_of(digest))
                .map(|stored| stored.len())
                .map_err(|_| absent());
        }
        if !held.zstd {
            return Err(absent());
        }
        let mut prefix = Vec::new();
        File::open(self.compressed_path_of(digest))
            .map_err(|_| absent())?
            .take(u64::try_from(HEADER_FIELDS + SHA256_BYTES + FRAME_HEADER_MAX).unwrap_or(0))
            .read_to_end(&mut prefix)?;
        let undecodable = |why: String| StoreError::BlobUndecodable {
            digest: digest.hex().to_owned(),
            why,
        };
        let (_, frame) = split_header(&prefix).map_err(undecodable)?;
        content_size(frame).map_err(undecodable)
    }

    /// The dictionary `put` compresses with from now on, held prepared for the whole process.
    pub fn activate(&self, dictionary: &Dictionary) -> Result<(), StoreError> {
        let prepared = EncoderDictionary::try_copy(dictionary.bytes(), LEVEL)?;
        self.writer.borrow_mut().dictionary = Some((dictionary.hex().to_owned(), prepared));
        Ok(())
    }

    /// The dictionary `put` compresses with, `None` before one is activated.
    #[must_use]
    pub fn active_dictionary(&self) -> Option<String> {
        self.writer
            .borrow()
            .dictionary
            .as_ref()
            .map(|(hex, _)| hex.clone())
    }

    /// A stored dictionary, re-hashed. Absent is `DictionaryAbsent`: no blob is ever read or
    /// written as though a dictionary it names were not needed.
    pub fn load_dictionary(&self, hex: &str) -> Result<Dictionary, StoreError> {
        let bytes = match fs::read(self.dictionary_path(hex)) {
            Ok(bytes) => bytes,
            Err(failure) if failure.kind() == ErrorKind::NotFound || !is_digest(hex) => {
                return Err(StoreError::DictionaryAbsent {
                    dictionary: hex.to_owned(),
                })
            }
            Err(failure) => return Err(failure.into()),
        };
        let found = const_hex::encode(Sha256::digest(&bytes));
        if found != hex {
            return Err(StoreError::DictionaryAltered {
                dictionary: hex.to_owned(),
                found,
            });
        }
        Ok(Dictionary {
            digest: found,
            bytes,
        })
    }

    /// Stores a dictionary under its own digest, with `put`'s crash order. Nothing deletes one.
    pub fn store_dictionary(&self, run: &str, bytes: &[u8]) -> Result<Dictionary, StoreError> {
        let hex = const_hex::encode(Sha256::digest(bytes));
        let path = self.dictionary_path(&hex);
        if path.exists() {
            return self.load_dictionary(&hex);
        }
        let staging = self
            .root
            .join(DICTIONARY_DIR)
            .join(format!("{STAGING_PREFIX}{run}-{hex}"));
        write_durably(&staging, &path, bytes)?;
        Ok(Dictionary {
            digest: hex,
            bytes: bytes.to_vec(),
        })
    }

    /// Rewrites a raw copy as `.zst` and removes the raw one only after the `.zst` file is
    /// renamed, fsynced and read back to the same digest, so no step leaves zero readable copies.
    pub fn compress_raw(&self, run: &str, digest: &BlobDigest) -> Result<Rewritten, StoreError> {
        let original = self.read_copy(digest, Encoding::Raw)?;
        let compressed = self.compressed_path_of(digest);
        let resumed = compressed.exists();
        if !resumed {
            let encoded = self.encode(&original.payload)?;
            write_durably(&self.staging_path(run, digest), &compressed, &encoded)?;
        }
        let back = self.read_copy(digest, Encoding::Zstd)?;
        let raw = self.path_of(digest);
        fs::remove_file(&raw)?;
        sync_directory(raw.parent())?;
        Ok(Rewritten {
            logical_bytes: original.stored_bytes,
            stored_bytes: back.stored_bytes,
            resumed,
        })
    }

    /// Every digest on disk, in hex order, one leaf directory in memory at a time.
    #[must_use]
    pub fn listing(&self) -> Listing {
        Listing {
            pending: vec![Level::Top(self.space())],
            leaf: BTreeMap::new().into_iter(),
        }
    }

    /// Every digest on disk read back through [`BlobStore::read_listed`], one payload in memory at
    /// a time, each dictionary loaded once: the bulk read a re-extraction pass makes (§3.9, P11).
    pub fn stream(&self) -> impl Iterator<Item = Result<StreamedBlob, StoreError>> + '_ {
        self.listing().map(move |listed| {
            listed.map(|listed| StreamedBlob {
                read: self.read_listed(&listed),
                listed,
            })
        })
    }

    /// The copy `listed` prefers, or, when that file is gone since the listing, the one left.
    pub fn read_listed(&self, listed: &Listed) -> Result<ReadBack, StoreError> {
        match self.read_copy(&listed.digest, listed.read_from()) {
            Err(StoreError::Io(gone)) if gone.kind() == ErrorKind::NotFound => {
                let now = self.listed(&listed.digest);
                self.read_copy(&now.digest, now.read_from())
            }
            read => read,
        }
    }

    /// Staging files a crash left beside the tree; `put` never reopens one.
    pub fn staging_leftovers(&self) -> Result<usize, StoreError> {
        let mut count = 0;
        for directory in [self.space(), self.root.join(DICTIONARY_DIR)] {
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(failure) if failure.kind() == ErrorKind::NotFound => continue,
                Err(failure) => return Err(failure.into()),
            };
            for entry in entries {
                if entry?
                    .file_name()
                    .to_string_lossy()
                    .starts_with(STAGING_PREFIX)
                {
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    /// `[skippable frame: dictionary sha256 or nothing][one zstd frame recording its size]`.
    fn encode(&self, payload: &[u8]) -> Result<Vec<u8>, StoreError> {
        let mut writer = self.writer.borrow_mut();
        let Writer {
            context,
            dictionary,
        } = &mut *writer;
        let context = match context {
            Some(context) => context,
            None => context.insert(CCtx::try_create().ok_or_else(|| {
                std::io::Error::other("zstd could not allocate a compression context")
            })?),
        };
        let mut frame = Vec::with_capacity(zstd_safe::compress_bound(payload.len()));
        let named = match dictionary {
            Some((hex, prepared)) => {
                context
                    .compress_using_cdict(&mut frame, payload, prepared.as_cdict())
                    .map_err(zstd_failure)?;
                Some(hex.as_str())
            }
            None => {
                context
                    .compress(&mut frame, payload, LEVEL)
                    .map_err(zstd_failure)?;
                None
            }
        };
        let mut encoded = header(named)?;
        encoded.extend_from_slice(&frame);
        Ok(encoded)
    }

    fn decode(
        &self,
        digest: &BlobDigest,
        file: &[u8],
    ) -> Result<(Option<String>, Vec<u8>), StoreError> {
        let undecodable = |why: String| StoreError::BlobUndecodable {
            digest: digest.hex().to_owned(),
            why,
        };
        let (dictionary, frame) = split_header(file).map_err(undecodable)?;
        let whole = zstd_safe::find_frame_compressed_size(frame)
            .map_err(|code| undecodable(zstd_safe::get_error_name(code).to_owned()))?;
        if whole != frame.len() {
            return Err(undecodable(format!(
                "{} bytes follow the header and its one frame is {whole}",
                frame.len()
            )));
        }
        let size = content_size(frame).map_err(undecodable)?;
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(usize::try_from(size).unwrap_or(usize::MAX))
            .map_err(|failure| undecodable(failure.to_string()))?;
        if let Some(hex) = &dictionary {
            self.ensure_decoder(hex)?;
        }
        let mut reader = self.reader.borrow_mut();
        let Reader {
            context,
            dictionaries,
        } = &mut *reader;
        let context = match context {
            Some(context) => context,
            None => context.insert(DCtx::try_create().ok_or_else(|| {
                std::io::Error::other("zstd could not allocate a decompression context")
            })?),
        };
        let decoded = match dictionary.as_ref().and_then(|hex| dictionaries.get(hex)) {
            Some(prepared) => {
                context.decompress_using_ddict(&mut payload, frame, prepared.as_ddict())
            }
            None => context.decompress(&mut payload, frame),
        };
        decoded.map_err(|code| undecodable(zstd_safe::get_error_name(code).to_owned()))?;
        if u64::try_from(payload.len()).ok() != Some(size) {
            return Err(undecodable(format!(
                "the frame records {size} bytes and decodes to {}",
                payload.len()
            )));
        }
        Ok((dictionary, payload))
    }

    fn ensure_decoder(&self, hex: &str) -> Result<(), StoreError> {
        if self.reader.borrow().dictionaries.contains_key(hex) {
            return Ok(());
        }
        let dictionary = self.load_dictionary(hex)?;
        let prepared = DecoderDictionary::try_copy(dictionary.bytes())?;
        self.reader
            .borrow_mut()
            .dictionaries
            .insert(hex.to_owned(), prepared);
        Ok(())
    }
}

fn is_digest(hex: &str) -> bool {
    hex.len() == DIGEST_HEX
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn zstd_failure(code: usize) -> StoreError {
    StoreError::Io(std::io::Error::other(zstd_safe::get_error_name(code)))
}

fn header(dictionary: Option<&str>) -> Result<Vec<u8>, StoreError> {
    let named = match dictionary {
        Some(hex) => const_hex::decode(hex).map_err(std::io::Error::other)?,
        None => Vec::new(),
    };
    let mut header = HEADER_MAGIC.to_le_bytes().to_vec();
    header.extend_from_slice(
        &u32::try_from(named.len())
            .map_err(std::io::Error::other)?
            .to_le_bytes(),
    );
    header.extend_from_slice(&named);
    Ok(header)
}

/// The dictionary a `.zst` file names, and the frame after its header.
fn split_header(file: &[u8]) -> Result<(Option<String>, &[u8]), String> {
    let field = |at: usize| {
        file.get(at..at + 4)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map(u32::from_le_bytes)
    };
    if field(0) != Some(HEADER_MAGIC) {
        return Err("the file does not open with the dictionary header".to_owned());
    }
    let named = field(4).and_then(|len| usize::try_from(len).ok());
    match named {
        Some(0) => Ok((None, file.get(HEADER_FIELDS..).unwrap_or_default())),
        Some(SHA256_BYTES) => match file.get(HEADER_FIELDS..HEADER_FIELDS + SHA256_BYTES) {
            Some(raw) => Ok((
                Some(const_hex::encode(raw)),
                file.get(HEADER_FIELDS + SHA256_BYTES..).unwrap_or_default(),
            )),
            None => Err("the header is cut short".to_owned()),
        },
        other => Err(format!("the header names a dictionary of {other:?} bytes")),
    }
}

fn content_size(frame: &[u8]) -> Result<u64, String> {
    match zstd_safe::get_frame_content_size(frame) {
        Ok(Some(size)) => Ok(size),
        Ok(None) => Err("the frame records no content size".to_owned()),
        Err(_) => Err("the frame header is unreadable".to_owned()),
    }
}

/// `staging`, fsynced, renamed onto `target`, the directory fsynced: durable before any
/// transaction names it.
fn write_durably(staging: &Path, target: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    // `File::create` TRUNCATES an existing path: measured 2026-11-13, a 10-byte file rewritten
    // through it is 2 bytes afterwards, while `create_new` answers `AlreadyExists`.
    let mut file = File::create_new(staging)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(staging, target)?;
    sync_directory(target.parent())
}

/// A rename is durable only once the directory holding the new name is fsynced.
fn sync_directory(directory: Option<&Path>) -> Result<(), StoreError> {
    if let Some(directory) = directory {
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}

enum Level {
    Top(PathBuf),
    Middle(PathBuf),
    Leaf(PathBuf),
}

/// [`BlobStore::listing`]'s walk. A name in a leaf that is neither `<64 hex>` nor
/// `<64 hex>.zst` is a `BlobStray` item, and the walk goes on.
pub struct Listing {
    pending: Vec<Level>,
    leaf: std::collections::btree_map::IntoIter<String, Result<Listed, StoreError>>,
}

/// The subdirectories of `directory`, in reverse name order so `pop` takes the first.
fn subdirectories(directory: &Path) -> Result<Vec<PathBuf>, StoreError> {
    let mut found = Vec::new();
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(failure) if failure.kind() == ErrorKind::NotFound => return Ok(found),
        Err(failure) => return Err(failure.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            found.push(entry.path());
        }
    }
    found.sort();
    found.reverse();
    Ok(found)
}

fn leaf_of(directory: &Path) -> Result<BTreeMap<String, Result<Listed, StoreError>>, StoreError> {
    let mut found: BTreeMap<String, Result<Listed, StoreError>> = BTreeMap::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let (hex, encoding) = match name.strip_suffix(COMPRESSED_SUFFIX) {
            Some(hex) => (hex.to_owned(), Encoding::Zstd),
            None => (name.clone(), Encoding::Raw),
        };
        if !is_digest(&hex) || !entry.file_type()?.is_file() {
            found.insert(name, Err(StoreError::BlobStray { path: entry.path() }));
            continue;
        }
        let slot = found.entry(hex.clone()).or_insert_with(|| {
            Ok(Listed {
                digest: BlobDigest(hex),
                raw: false,
                zstd: false,
            })
        });
        if let Ok(listed) = slot {
            match encoding {
                Encoding::Raw => listed.raw = true,
                Encoding::Zstd => listed.zstd = true,
            }
        }
    }
    Ok(found)
}

impl Iterator for Listing {
    type Item = Result<Listed, StoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some((_, listed)) = self.leaf.next() {
                return Some(listed);
            }
            let walked = match self.pending.pop()? {
                Level::Top(directory) => subdirectories(&directory).map(|found| {
                    self.pending.extend(found.into_iter().map(Level::Middle));
                }),
                Level::Middle(directory) => subdirectories(&directory).map(|found| {
                    self.pending.extend(found.into_iter().map(Level::Leaf));
                }),
                Level::Leaf(directory) => leaf_of(&directory).map(|found| {
                    self.leaf = found.into_iter();
                }),
            };
            if let Err(failure) = walked {
                return Some(Err(failure));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use tempfile::TempDir;

    use super::*;

    type TestResult = Result<(), Box<dyn Error>>;

    const RUN: &str = "a-run";
    const PAYLOAD: &[u8] =
        b"<html><body><p>One press release, as the server sent it.</p></body></html>";
    /// `ZSTD_MAGICNUMBER`, as it opens every zstd frame on disk.
    const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
    /// Small enough for the few hundred kilobytes of fixtures a test trains on.
    const TEST_DICTIONARY_BYTES: usize = 16_384;

    /// Every file of one `open-web-feed` fixture directory, in name order.
    fn fixtures(directory: &str) -> Result<Vec<Vec<u8>>, Box<dyn Error>> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../providers/open-web-feed/tests/fixtures")
            .join(directory);
        let mut paths: Vec<PathBuf> = fs::read_dir(root)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<_, _>>()?;
        paths.sort();
        Ok(paths.iter().map(fs::read).collect::<Result<_, _>>()?)
    }

    fn trained(directories: &[&str]) -> Result<Vec<u8>, Box<dyn Error>> {
        let mut samples = Vec::new();
        for directory in directories {
            samples.extend(fixtures(directory)?);
        }
        Ok(zstd::dict::from_samples(&samples, TEST_DICTIONARY_BYTES)?)
    }

    fn sha256(bytes: &[u8]) -> String {
        const_hex::encode(Sha256::digest(bytes))
    }

    /// A raw file as the store wrote every blob before compression: the payload at `path_of`.
    fn write_raw(store: &BlobStore, payload: &[u8]) -> Result<BlobDigest, Box<dyn Error>> {
        let digest = BlobDigest(sha256(payload));
        let path = store.path_of(&digest);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, payload)?;
        Ok(digest)
    }

    /// `payload` compressed as `put` writes it, copied over the `.zst` file at `path`.
    fn overwrite_compressed(path: &Path, payload: &[u8]) -> TestResult {
        let scratch = TempDir::new()?;
        let other = BlobStore::new(scratch.path());
        let digest = other.put(RUN, payload)?;
        fs::copy(other.compressed_path_of(&digest), path)?;
        Ok(())
    }

    /// Invariant 1 of the compression row: the name is the sha256 of the payload, not of the
    /// file. Mutation 2026-11-21, the digest taken over the encoded bytes: red.
    #[test]
    fn the_digest_is_the_payload_s_sha256_and_the_file_is_one_zstd_frame_under_the_suffix(
    ) -> TestResult {
        let temp = TempDir::new()?;
        let store = BlobStore::new(temp.path());
        let digest = store.put(RUN, PAYLOAD)?;
        let file = fs::read(store.compressed_path_of(&digest))?;
        assert_eq!(
            (
                digest.hex(),
                store.path_of(&digest).exists(),
                file.get(HEADER_FIELDS..HEADER_FIELDS + 4),
                store.read_verified(&digest)?,
            ),
            (
                sha256(PAYLOAD).as_str(),
                false,
                Some(&ZSTD_MAGIC[..]),
                PAYLOAD.to_vec()
            )
        );
        Ok(())
    }

    /// Mutation 2026-11-21, the hash comparison in `read_copy` removed: red.
    #[test]
    fn a_compressed_file_decoding_to_other_bytes_is_altered() -> TestResult {
        let temp = TempDir::new()?;
        let store = BlobStore::new(temp.path());
        let digest = store.put(RUN, PAYLOAD)?;
        overwrite_compressed(&store.compressed_path_of(&digest), b"other bytes")?;
        let read = store.read_verified(&digest);
        assert!(
            matches!(&read, Err(StoreError::BlobAltered { found, .. }) if *found == sha256(b"other bytes")),
            "{read:?}"
        );
        Ok(())
    }

    /// `blobs.bytes` is the payload's length in either form, which §11.1's held read compares
    /// with the body limit. Mutation 2026-11-21, `len_of` answering the `.zst` file's size: red.
    #[test]
    fn the_length_is_the_payload_s_for_a_compressed_and_for_a_raw_copy() -> TestResult {
        let temp = TempDir::new()?;
        let store = BlobStore::new(temp.path());
        let long = [b'a'; 10_000];
        let compressed = store.put(RUN, &long)?;
        let raw = write_raw(&store, PAYLOAD)?;
        assert_eq!(
            (
                store.len_of(&compressed)?,
                fs::metadata(store.compressed_path_of(&compressed))?.len() < 100,
                store.len_of(&raw)?,
            ),
            (10_000, true, u64::try_from(PAYLOAD.len())?)
        );
        Ok(())
    }

    /// A blob written before compression is the same blob: `put` keeps it and writes no second
    /// copy, and `named` finds either form. Mutation 2026-11-21, `put` checking the `.zst` path
    /// only: red.
    #[test]
    fn a_payload_held_raw_is_not_written_again_and_either_form_is_named() -> TestResult {
        let temp = TempDir::new()?;
        let store = BlobStore::new(temp.path());
        let raw = write_raw(&store, PAYLOAD)?;
        let put = store.put(RUN, PAYLOAD)?;
        let compressed = store.put(RUN, b"a later payload")?;
        assert_eq!(
            (
                &put,
                store.compressed_path_of(&raw).exists(),
                store.named(raw.hex()),
                store.named(compressed.hex()),
                store.named("not a digest"),
            ),
            (
                &raw,
                false,
                Some(raw.clone()),
                Some(compressed.clone()),
                None
            )
        );
        Ok(())
    }

    /// Invariant 6: the form is the file NAME. A raw payload that is itself a zstd file (a
    /// server's zstd body; here exactly the bytes a `.zst` file holds) reads back as stored.
    /// Mutation 2026-11-21, a raw copy decoded when it opens with either magic number: red.
    #[test]
    fn a_raw_payload_that_is_itself_a_zstd_file_is_read_as_the_bytes_it_holds() -> TestResult {
        let temp = TempDir::new()?;
        let store = BlobStore::new(temp.path());
        let looks_compressed = store.encode(PAYLOAD)?;
        let raw = write_raw(&store, &looks_compressed)?;
        let put = store.put(RUN, &[&ZSTD_MAGIC[..], PAYLOAD].concat())?;
        assert_eq!(
            (store.read_verified(&raw)?, store.read_verified(&put)?),
            (looks_compressed, [&ZSTD_MAGIC[..], PAYLOAD].concat())
        );
        Ok(())
    }

    /// The header names the dictionary by its sha256, and a handle that never activated it
    /// loads it from the store by that name.
    #[test]
    fn a_blob_names_its_dictionary_and_another_handle_reads_it_back() -> TestResult {
        let temp = TempDir::new()?;
        let writer = BlobStore::new(temp.path());
        let dictionary = writer.store_dictionary(RUN, &trained(&["prn", "gnw"])?)?;
        writer.activate(&dictionary)?;
        let page = fixtures("fed")?.concat();
        let digest = writer.put(RUN, &page)?;
        let reader = BlobStore::new(temp.path());
        let back = reader.read_copy(&digest, Encoding::Zstd)?;
        assert_eq!(
            (back.dictionary.as_deref(), back.payload == page),
            (Some(dictionary.hex()), true)
        );
        Ok(())
    }

    /// A missing dictionary is named, never read as "no dictionary". Mutation 2026-11-21, an
    /// absent dictionary file decoding the frame with none: red.
    #[test]
    fn a_blob_whose_dictionary_file_is_gone_is_dictionary_absent() -> TestResult {
        let temp = TempDir::new()?;
        let writer = BlobStore::new(temp.path());
        let dictionary = writer.store_dictionary(RUN, &trained(&["prn", "gnw"])?)?;
        writer.activate(&dictionary)?;
        let digest = writer.put(RUN, PAYLOAD)?;
        fs::remove_file(writer.dictionary_path(dictionary.hex()))?;
        let read = BlobStore::new(temp.path()).read_verified(&digest);
        assert!(
            matches!(&read, Err(StoreError::DictionaryAbsent { dictionary: named }) if named == dictionary.hex()),
            "{read:?}"
        );
        Ok(())
    }

    /// Mutation 2026-11-21, the hash comparison in `load_dictionary` removed: red.
    #[test]
    fn a_dictionary_whose_bytes_changed_is_refused_by_its_digest() -> TestResult {
        let temp = TempDir::new()?;
        let store = BlobStore::new(temp.path());
        let dictionary = store.store_dictionary(RUN, &trained(&["prn", "gnw"])?)?;
        fs::write(store.dictionary_path(dictionary.hex()), b"changed")?;
        let loaded = store.load_dictionary(dictionary.hex());
        assert!(
            matches!(loaded, Err(StoreError::DictionaryAltered { .. })),
            "{loaded:?}"
        );
        Ok(())
    }

    /// The re-extraction read: one pass over a raw blob, a dictionary-less frame and frames
    /// under two dictionary versions returns every payload exactly, each dictionary read from
    /// disk once: the second pass runs with both files deleted.
    #[test]
    fn one_pass_reads_every_form_and_dictionary_version_exactly() -> TestResult {
        let temp = TempDir::new()?;
        let store = BlobStore::new(temp.path());
        let pages = fixtures("cnw")?;
        let raw = write_raw(&store, &pages[0])?;
        let plain = store.put(RUN, &pages[1])?;
        let first = store.store_dictionary(RUN, &trained(&["prn", "gnw"])?)?;
        store.activate(&first)?;
        let under_first = store.put(RUN, &pages[2])?;
        let second = store.store_dictionary(RUN, &trained(&["fed", "yahoo", "gnw"])?)?;
        store.activate(&second)?;
        let under_second = store.put(RUN, &pages[3])?;
        let mut expected = vec![
            (raw, Encoding::Raw, None, pages[0].clone()),
            (plain, Encoding::Zstd, None, pages[1].clone()),
            (
                under_first,
                Encoding::Zstd,
                Some(first.hex().to_owned()),
                pages[2].clone(),
            ),
            (
                under_second,
                Encoding::Zstd,
                Some(second.hex().to_owned()),
                pages[3].clone(),
            ),
        ];
        expected.sort();
        let pass = || -> Result<Vec<_>, StoreError> {
            store
                .stream()
                .map(|streamed| {
                    let streamed = streamed?;
                    let read = streamed.read?;
                    Ok((
                        streamed.listed.digest,
                        read.encoding,
                        read.dictionary,
                        read.payload,
                    ))
                })
                .collect()
        };
        let before = pass()?;
        fs::remove_file(store.dictionary_path(first.hex()))?;
        fs::remove_file(store.dictionary_path(second.hex()))?;
        assert_eq!(
            (before, pass()?, first == second),
            (expected.clone(), expected, false)
        );
        Ok(())
    }

    #[test]
    fn compress_raw_replaces_the_raw_copy_with_one_that_reads_back() -> TestResult {
        let temp = TempDir::new()?;
        let store = BlobStore::new(temp.path());
        let digest = write_raw(&store, PAYLOAD)?;
        let rewritten = store.compress_raw(RUN, &digest)?;
        assert_eq!(
            (
                rewritten.resumed,
                rewritten.logical_bytes,
                store.path_of(&digest).exists(),
                store.read_verified(&digest)?,
            ),
            (
                false,
                u64::try_from(PAYLOAD.len())?,
                false,
                PAYLOAD.to_vec()
            )
        );
        Ok(())
    }

    /// No step leaves zero readable copies: a `.zst` copy that does not read back to the digest
    /// keeps the raw one. Mutation 2026-11-21, the read-back in `compress_raw` removed: red.
    #[test]
    fn a_compressed_copy_that_does_not_read_back_keeps_the_raw_one() -> TestResult {
        let temp = TempDir::new()?;
        let store = BlobStore::new(temp.path());
        let digest = write_raw(&store, PAYLOAD)?;
        overwrite_compressed(&store.compressed_path_of(&digest), b"other bytes")?;
        let refused = store.compress_raw(RUN, &digest);
        assert!(
            matches!(refused, Err(StoreError::BlobAltered { .. })),
            "{refused:?}"
        );
        assert_eq!(store.read_copy(&digest, Encoding::Raw)?.payload, PAYLOAD);
        Ok(())
    }

    /// A reader that listed a raw copy which `blobs compress` then removed reads the `.zst` one.
    /// Mutation 2026-11-21, `read_listed` without the second look: red.
    #[test]
    fn a_raw_copy_removed_after_the_listing_is_read_from_the_compressed_one() -> TestResult {
        let temp = TempDir::new()?;
        let store = BlobStore::new(temp.path());
        let digest = write_raw(&store, PAYLOAD)?;
        let listed = store.listed(&digest);
        store.compress_raw(RUN, &digest)?;
        let read = store.read_listed(&listed)?;
        assert_eq!(
            (listed.raw, read.encoding, read.payload),
            (true, Encoding::Zstd, PAYLOAD.to_vec())
        );
        Ok(())
    }

    /// A run that stopped after the rename and before the removal leaves both copies; the next
    /// run keeps the `.zst` one it reads back and removes the raw one.
    #[test]
    fn a_rewrite_stopped_before_the_removal_is_resumed() -> TestResult {
        let temp = TempDir::new()?;
        let store = BlobStore::new(temp.path());
        let digest = write_raw(&store, PAYLOAD)?;
        fs::write(store.compressed_path_of(&digest), store.encode(PAYLOAD)?)?;
        let listed = store.listing().collect::<Result<Vec<_>, _>>()?;
        let rewritten = store.compress_raw(RUN, &digest)?;
        assert_eq!(
            (
                listed,
                rewritten.resumed,
                store.path_of(&digest).exists(),
                store.read_verified(&digest)?
            ),
            (
                vec![Listed {
                    digest: digest.clone(),
                    raw: true,
                    zstd: true
                }],
                true,
                false,
                PAYLOAD.to_vec()
            )
        );
        Ok(())
    }

    #[test]
    fn a_file_that_names_no_digest_is_listed_as_a_stray_and_the_walk_goes_on() -> TestResult {
        let temp = TempDir::new()?;
        let store = BlobStore::new(temp.path());
        let digest = store.put(RUN, PAYLOAD)?;
        let stray = store.space().join("00").join("00").join("notes.txt");
        if let Some(parent) = stray.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&stray, b"x")?;
        let listed: Vec<String> = store
            .listing()
            .map(|listed| match listed {
                Ok(listed) => listed.digest.hex().to_owned(),
                Err(failure) => failure.to_string(),
            })
            .collect();
        assert_eq!(
            listed,
            vec![
                format!(
                    "{} is in the blob tree and names no digest",
                    stray.display()
                ),
                digest.hex().to_owned()
            ]
        );
        Ok(())
    }
}
