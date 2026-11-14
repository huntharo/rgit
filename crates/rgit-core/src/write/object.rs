//! OWNER: WP1. Durable loose object storage.
//!
//! Objects are written like C Git's loose object store: zlib-compressed
//! `<kind> <size>\0<data>` under `objects/<xx>/<rest>`. Content goes to an
//! exclusively created temporary file in the destination fan-out directory,
//! is synced, made read-only, then published with a hard link that never
//! replaces an existing object (or, where links are unsupported, an atomic
//! no-replace rename; with neither, the write is refused). The fan-out
//! directory is synced before returning, so an object is durable before any
//! index or reference names it. An entry already present under the object's
//! name counts as the object only after its content is inflated and hashed.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Error, ObjectId, ObjectKind, Repository};

impl Repository {
    /// Store `data` as a blob, returning its ID in the repository's hash.
    /// The object file and its directory are synced before returning.
    pub fn write_blob(&self, data: &[u8]) -> Result<ObjectId, Error> {
        self.write_loose_object(ObjectKind::Blob, data)
    }

    /// Store an object of any kind durably and return its ID. An object that
    /// already exists is not rewritten; an existing loose copy is synced so
    /// the caller may rely on it being durable.
    pub(crate) fn write_loose_object(
        &self,
        kind: ObjectKind,
        data: &[u8],
    ) -> Result<ObjectId, Error> {
        let id = gix::objs::compute_hash(self.inner.object_hash(), kind, data)?;
        let objects = self.inner.common_dir().join("objects");
        let (fanout, path) = loose_path(&objects, &id);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                // Another writer may have published it without syncing yet.
                verify_loose_object(&path, &id)?;
                sync_existing(&path)?;
                return Ok(id);
            }
            Ok(_) => {
                return Err(Error::UnsupportedCapability(
                    "symlink or special loose object file",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if !id.is_empty_tree() && self.inner.has_object(id) {
            // Packed (or alternate) objects are already durable.
            return Ok(id);
        }
        ensure_directory(&objects, &fanout)?;
        let temporary = write_temporary(&fanout, kind, data)?;
        let published = publish(&temporary, &path, &id);
        // Remove only our own temporary file (absent after a rename). A
        // competitor's entry at `path` is never touched.
        let removed: Result<(), Error> = match fs::remove_file(&temporary) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        };
        published?;
        removed?;
        super::sync_parent(&path)?;
        Ok(id)
    }
}

fn loose_path(objects: &Path, id: &ObjectId) -> (PathBuf, PathBuf) {
    let hex = id.to_string();
    let fanout = objects.join(&hex[..2]);
    let path = fanout.join(&hex[2..]);
    (fanout, path)
}

fn sync_existing(path: &Path) -> Result<(), Error> {
    super::sync_existing_file(path)
}

/// Create the fan-out directory if needed, syncing `objects/` when it was.
fn ensure_directory(objects: &Path, fanout: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(objects) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(Error::UnsupportedCapability(
                "symlink or special objects directory",
            ));
        }
        Err(error) => return Err(error.into()),
    }
    match fs::create_dir(fanout) {
        Ok(()) => super::sync_parent(fanout),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if fs::symlink_metadata(fanout)?.is_dir() {
                Ok(())
            } else {
                Err(Error::UnsupportedCapability(
                    "symlink or special object fan-out directory",
                ))
            }
        }
        Err(error) => Err(error.into()),
    }
}

/// Unique-per-process temporary names; `create_new` guarantees exclusivity.
pub(super) fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    format!(
        "{}-{}-{:x}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
        nanos
    )
}

/// Write the compressed object to a fresh temporary file, synced and
/// read-only. The file is removed on any failure.
fn write_temporary(fanout: &Path, kind: ObjectKind, data: &[u8]) -> Result<PathBuf, Error> {
    let mut attempts = 0;
    let (path, file) = loop {
        let path = fanout.join(format!("tmp_obj_{}", unique_suffix()));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => break (path, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempts < 8 => {
                attempts += 1;
            }
            Err(error) => return Err(error.into()),
        }
    };
    let result = (|| -> Result<(), Error> {
        let mut writer =
            gix::zlib::stream::deflate::Write::new(file, gix::zlib::Compression::BEST_SPEED);
        writer.write_all(&gix::objs::encode::loose_header(kind, data.len() as u64))?;
        writer.write_all(data)?;
        writer.flush()?;
        let file = writer.into_inner();
        file.sync_all()?;
        set_read_only(&file)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&path);
        return Err(error);
    }
    Ok(path)
}

#[cfg(unix)]
fn set_read_only(file: &File) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o444))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_read_only(file: &File) -> Result<(), Error> {
    let mut permissions = file.metadata()?.permissions();
    permissions.set_readonly(true);
    file.set_permissions(permissions)?;
    Ok(())
}

/// Publish without ever replacing an existing object. Like C Git, prefer a
/// hard link (fails if the name exists). Where links fail, use an atomic
/// no-replace rename; where that is unsupported too, refuse. An entry that
/// already holds the name is accepted only if it inflates to this object.
fn publish(temporary: &Path, path: &Path, id: &ObjectId) -> Result<(), Error> {
    super::failpoint("object.before-publish")?;
    match fs::hard_link(temporary, path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            verify_loose_object(path, id)?;
            sync_existing(path)
        }
        Err(_) => match super::rename_noreplace(temporary, path) {
            Ok(()) => Ok(()),
            Err(Error::AlreadyExists(_)) => {
                verify_loose_object(path, id)?;
                sync_existing(path)
            }
            Err(Error::UnsupportedCapability(_)) => Err(Error::UnsupportedCapability(
                "publishing loose objects needs hard links or an atomic no-replace rename",
            )),
            Err(error) => Err(error),
        },
    }
}

/// Verify that the loose object file at `path` is a regular file whose zlib
/// stream inflates to a well-formed object hashing to `id`, without following
/// a symlink, blocking on a FIFO or buffering the inflated content.
pub(super) fn verify_loose_object(path: &Path, id: &ObjectId) -> Result<(), Error> {
    use std::io::Read;
    let corrupt = || -> Error {
        gix::error::corruption("an existing loose object file does not hold the object it names")
            .corrupted_error()
            .into()
    };
    let mut file = super::open_leaf_nofollow(path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::UnsupportedCapability(
            "symlink or special loose object file",
        ));
    }
    let mut compressed = Vec::new();
    file.read_to_end(&mut compressed)?;
    let mut input = compressed.as_slice();
    let mut state = gix::zlib::Decompress::new();
    let mut hasher = gix::hash::hasher(id.kind());
    let mut chunk = vec![0u8; 64 * 1024];
    let mut header = Vec::new();
    let mut total = 0u64;
    loop {
        let written = gix::zlib::stream::inflate::read(&mut input, &mut state, &mut chunk)
            .map_err(|_| corrupt())?;
        if written == 0 {
            break;
        }
        hasher.update(&chunk[..written]);
        if header.len() < 64 {
            let take = (64 - header.len()).min(written);
            header.extend_from_slice(&chunk[..take]);
        }
        total += written as u64;
    }
    let (_, size, header_len) = gix::objs::decode::loose_header(&header).map_err(|_| corrupt())?;
    if !input.is_empty() || total != header_len as u64 + size {
        return Err(corrupt());
    }
    if hasher.try_finalize().map_err(gix::Error::from_error)? != *id {
        return Err(corrupt());
    }
    Ok(())
}
