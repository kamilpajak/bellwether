//! `modelgate snapshot` and `modelgate verify`: the store's copy for the daily backup, and the
//! read that says a store or a restored copy holds every blob its rows name
//! (`docs/design/architecture.md` section 3.5, "The store's daily copy").

use std::path::Path;

use serde_json::{json, Value};

use crate::cmd_shadow::ShadowError;
use crate::store::{Store, BLOB_DIR, DB_FILE};

pub const SNAPSHOT_SCHEMA: &str = "modelgate.snapshot/1";
pub const VERIFY_SCHEMA: &str = "modelgate.store_verify/1";

fn io(path: &Path) -> impl Fn(std::io::Error) -> ShadowError + '_ {
    move |why| ShadowError::Io(format!("{}: {why}", path.display()))
}

fn size(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// Reads every blob a row names, each refused unless it hashes to its name, and hands it to
/// `each`. The object is the part of the envelope both commands share.
fn census(
    store: &Store,
    mut each: impl FnMut(&str, &[u8]) -> Result<(), ShadowError>,
) -> Result<Value, ShadowError> {
    let names = store.blob_names().map_err(ShadowError::Store)?;
    let mut bytes = 0_u64;
    for name in &names {
        let blob = store.read_blob(name).map_err(ShadowError::Store)?;
        bytes += size(blob.len());
        each(name, &blob)?;
    }
    let named = size(names.len());
    let files = store.blob_files().map_err(ShadowError::Store)?;
    Ok(json!({
        "rows": store.row_counts().map_err(ShadowError::Store)?,
        "blobs": {"named": named, "bytes": bytes, "unnamed_files": files.saturating_sub(named)},
    }))
}

fn envelope(schema: &str, store: &Path, out: Option<&Path>, census: &Value) -> Value {
    let mut object = json!({
        "schema": schema, "store": store.display().to_string(),
        "rows": census["rows"], "blobs": census["blobs"],
    });
    if let Some(out) = out {
        object["out"] = json!(out.display().to_string());
    }
    object
}

/// Copies the database and every blob a row of the copy's source names into `out`, a directory
/// that is absent or empty, under the store's `LOCK`. A blob no row names stays behind and is
/// counted as `blobs.unnamed_files`.
pub fn snapshot(store_root: &Path, out: &Path) -> Result<Value, ShadowError> {
    let Some(store) = Store::leased_read(store_root).map_err(ShadowError::Store)? else {
        return Err(ShadowError::InputAbsent(format!(
            "no store a run has written at {}",
            store_root.display()
        )));
    };
    let blobs = out.join(BLOB_DIR);
    std::fs::create_dir_all(out).map_err(io(out))?;
    if std::fs::read_dir(out).map_err(io(out))?.next().is_some() {
        return Err(ShadowError::InvalidArgument(format!(
            "--out {} is not empty",
            out.display()
        )));
    }
    std::fs::create_dir(&blobs).map_err(io(&blobs))?;
    store
        .copy_database(&out.join(DB_FILE))
        .map_err(ShadowError::Store)?;
    let census = census(&store, |name, bytes| {
        let path = blobs.join(name);
        std::fs::write(&path, bytes).map_err(io(&path))
    })?;
    Ok(envelope(SNAPSHOT_SCHEMA, store_root, Some(out), &census))
}

/// Reads a store without a lease and without writing: an absent database is `input_absent`, a
/// blob that is missing or altered is `io_failed` naming it.
pub fn verify(store_root: &Path) -> Result<Value, ShadowError> {
    let database = store_root.join(DB_FILE);
    if !database.is_file() {
        return Err(ShadowError::InputAbsent(format!(
            "no store at {}",
            store_root.display()
        )));
    }
    let store = Store::read_only(store_root).map_err(ShadowError::Store)?;
    let census = census(&store, |_, _| Ok(()))?;
    Ok(envelope(VERIFY_SCHEMA, store_root, None, &census))
}
