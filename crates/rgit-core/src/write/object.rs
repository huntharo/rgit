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
//!
//! Reads on write paths: a loose object is opened only through the objects
//! anchor (no-follow, non-blocking, regular files only), so a loose file
//! swapped for a FIFO or symlink is refused rather than waited on or
//! followed. Packed objects and alternates are still read by Gitoxide, by
//! path, after [`check_object_store`] verified through the anchor that every
//! pack, index, reverse index, mtimes and multi-pack-index entry and
//! `objects/info/alternates` is a regular file: packs and alternates are
//! opened by Gitoxide by path after an anchored kind check, and a swap after
//! that check is not covered.

use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::fsx::{Anchor, Kind, Root};
use crate::{Error, ObjectId, ObjectKind, Repository};

impl Repository {
    /// Store `data` as a blob, returning its ID in the repository's hash.
    /// The object file and its directory are synced before returning.
    pub fn write_blob(&self, data: &[u8]) -> Result<ObjectId, Error> {
        self.write_loose_object(ObjectKind::Blob, data)
    }

    /// Store an object of any kind durably and return its ID. An object that
    /// already exists is not rewritten; an existing loose copy is verified
    /// and synced so the caller may rely on it being durable.
    pub(crate) fn write_loose_object(
        &self,
        kind: ObjectKind,
        data: &[u8],
    ) -> Result<ObjectId, Error> {
        let common = self.common_anchor()?;
        check_object_store(&common)?;
        self.write_loose_object_at(&common, kind, data)
    }

    /// [`Self::write_loose_object`] relative to an already opened common
    /// Git directory anchor. Every path below `common` is resolved through
    /// descriptors without following symlinks.
    pub(crate) fn write_loose_object_at(
        &self,
        common: &Anchor,
        kind: ObjectKind,
        data: &[u8],
    ) -> Result<ObjectId, Error> {
        let id = gix::objs::compute_hash(self.inner.object_hash(), kind, data)?;
        let objects = common.sub(b"objects")?;
        let (fanout, name) = loose_names(&id);
        if existing_loose(&objects, &id)? {
            return Ok(id);
        }
        if !id.is_empty_tree() && self.inner.has_object(id) {
            // Packed (or alternate) objects are already durable.
            return Ok(id);
        }
        super::failpoint("object.before-fanout")?;
        if objects.mkdir(&fanout)? {
            objects.sync()?;
        }
        let fan = objects.sub(&fanout)?;
        let temporary = write_temporary(&fan, kind, data)?;
        let published = publish(&fan, &temporary, &name, &id);
        // Remove only our own temporary file (absent after a rename). A
        // competitor's entry at the object name is never touched.
        let removed = match fan.unlink(&temporary) {
            Err(Error::Io(error)) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(Error::Io(error))
            }
            _ => Ok(()),
        };
        published?;
        removed?;
        fan.sync()?;
        same_entry(common, &fan, &id)?;
        Ok(id)
    }

    /// Anchor on the common Git directory (objects, shared refs, info/).
    pub(crate) fn common_anchor(&self) -> Result<Anchor, Error> {
        Anchor::open(self.inner.common_dir(), Root::CommonDir)
    }

    /// Anchor on the per-worktree Git directory (HEAD, index, worktree refs).
    pub(crate) fn git_dir_anchor(&self) -> Result<Anchor, Error> {
        Anchor::open(self.inner.git_dir(), Root::GitDir)
    }

    /// Anchor on the worktree root; bare repositories have none.
    pub(crate) fn worktree_anchor(&self) -> Result<Anchor, Error> {
        let root = self
            .inner
            .workdir()
            .ok_or(Error::UnsupportedCapability("worktree required"))?;
        Anchor::open(root, Root::Worktree)
    }
}

/// `(xx, rest)` of a loose object's name.
fn loose_names(id: &ObjectId) -> (Vec<u8>, Vec<u8>) {
    let hex = id.to_string().into_bytes();
    (hex[..2].to_vec(), hex[2..].to_vec())
}

fn loose_rel(id: &ObjectId) -> Vec<u8> {
    let (fanout, name) = loose_names(id);
    [fanout, b"/".to_vec(), name].concat()
}

/// If a loose copy of `id` exists below `objects`, verify and sync it and
/// return `true`. A symlinked or special entry (or fan-out) is refused.
pub(super) fn existing_loose(objects: &Anchor, id: &ObjectId) -> Result<bool, Error> {
    let rel = loose_rel(id);
    match objects.lstat(&rel)? {
        None => Ok(false),
        Some(meta) if matches!(meta.kind, Kind::File { .. }) => {
            // Another writer may have published it without syncing yet.
            sync_read_only(&verify_loose_object(objects, &rel, id)?)?;
            objects.sub(&loose_names(id).0)?.sync()?;
            Ok(true)
        }
        Some(_) => Err(Error::UnsupportedCapability(
            "symlink or special loose object file",
        )),
    }
}

/// After publication, the object name reached by a fresh no-follow walk from
/// the common directory must be the entry under the held fan-out descriptor.
/// Otherwise `objects/` or `objects/xx` was replaced during the write and
/// the object may not be where readers look; refuse rather than report it.
fn same_entry(common: &Anchor, fan: &Anchor, id: &ObjectId) -> Result<(), Error> {
    let (_, name) = loose_names(id);
    let held = fan.lstat(&name)?.map(|meta| meta.id);
    let walked = [b"objects/".to_vec(), loose_rel(id)].concat();
    let fresh = common.lstat(&walked).ok().flatten().map(|meta| meta.id);
    if held.is_some() && held == fresh {
        Ok(())
    } else {
        Err(Error::UnsupportedCapability(
            "object directory was replaced while the object was written",
        ))
    }
}

/// Flush a file another writer created, through our read-only handle. On
/// Windows `FlushFileBuffers` needs a writable handle (loose objects are
/// read-only), so, as before this change, durability there relies on the
/// writer that created it.
fn sync_read_only(file: &File) -> Result<(), Error> {
    #[cfg(unix)]
    file.sync_all()?;
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

/// Largest loose object a write path reads fully into memory (commits,
/// trees); larger objects are refused rather than buffered.
const LOOSE_READ_LIMIT: u64 = 64 * 1024 * 1024;

/// Open the loose copy of `id` below `objects`, or `None` if there is none
/// (it may be packed). The entry must be a regular file, both on the no-follow
/// `lstat` and on the opened (non-blocking) handle.
fn open_loose(objects: &Anchor, id: &ObjectId) -> Result<Option<File>, Error> {
    let special = || Error::UnsupportedCapability("symlink or special loose object file");
    let rel = loose_rel(id);
    match objects.lstat(&rel)? {
        None => return Ok(None),
        Some(meta) if matches!(meta.kind, Kind::File { .. }) => {}
        Some(_) => return Err(special()),
    }
    super::failpoint("object.before-read")?;
    match objects.open_read(&rel) {
        Ok((file, _)) => Ok(Some(file)),
        Err(Error::UnsupportedCapability(_)) => Err(special()),
        // Swapped meanwhile: removed (for example packed by gc) is looked up
        // as packed; a symlink (ELOOP) or other non-file is refused.
        Err(error) => match objects.lstat(&rel)? {
            None => Ok(None),
            Some(meta) if !matches!(meta.kind, Kind::File { .. }) => Err(special()),
            Some(_) => Err(error),
        },
    }
}

fn corrupt_loose() -> Error {
    gix::error::corruption("a loose object file is not a valid object")
        .corrupted_error()
        .into()
}

/// Kind and size of the loose copy of `id`, from its header only (as
/// Gitoxide's `find_header` does), or `None` if it is not loose.
pub(super) fn loose_header(
    objects: &Anchor,
    id: &ObjectId,
) -> Result<Option<(ObjectKind, u64)>, Error> {
    let Some(file) = open_loose(objects, id)? else {
        return Ok(None);
    };
    let mut reader = std::io::BufReader::new(file);
    let mut state = gix::zlib::Decompress::new();
    let mut head = [0u8; 64];
    let read = gix::zlib::stream::inflate::read(&mut reader, &mut state, &mut head)
        .map_err(|_| corrupt_loose())?;
    let (kind, size, _) =
        gix::objs::decode::loose_header(&head[..read]).map_err(|_| corrupt_loose())?;
    Ok(Some((kind, size)))
}

/// The loose copy of `id` (kind and content), or `None` if it is not loose.
/// It is inflated in a streaming fashion up to [`LOOSE_READ_LIMIT`] and must
/// hash to `id`.
pub(super) fn read_loose(
    objects: &Anchor,
    id: &ObjectId,
) -> Result<Option<(ObjectKind, Vec<u8>)>, Error> {
    let Some(file) = open_loose(objects, id)? else {
        return Ok(None);
    };
    let mut reader = std::io::BufReader::new(file);
    let mut state = gix::zlib::Decompress::new();
    let mut hasher = gix::hash::hasher(id.kind());
    let mut chunk = vec![0u8; 64 * 1024];
    let mut inflated: Vec<u8> = Vec::new();
    let mut header: Option<(ObjectKind, u64, usize)> = None;
    loop {
        let written = gix::zlib::stream::inflate::read(&mut reader, &mut state, &mut chunk)
            .map_err(|_| corrupt_loose())?;
        if written == 0 {
            break;
        }
        hasher.update(&chunk[..written]);
        inflated.extend_from_slice(&chunk[..written]);
        if header.is_none() && (inflated.contains(&0) || inflated.len() >= 64) {
            let parsed = gix::objs::decode::loose_header(&inflated).map_err(|_| corrupt_loose())?;
            if parsed.1 > LOOSE_READ_LIMIT {
                return Err(Error::UnsupportedCapability(
                    "loose object larger than the write-path read limit",
                ));
            }
            header = Some(parsed);
        }
        if let Some((_, size, header_len)) = header
            && inflated.len() as u64 > header_len as u64 + size
        {
            return Err(corrupt_loose());
        }
    }
    let (kind, size, header_len) = header.ok_or_else(corrupt_loose)?;
    if inflated.len() as u64 != header_len as u64 + size
        || hasher.try_finalize().map_err(gix::Error::from_error)? != *id
    {
        return Err(corrupt_loose());
    }
    inflated.drain(..header_len);
    Ok(Some((kind, inflated)))
}

/// Before Gitoxide reads the object database by path in a write operation,
/// verify through the common-directory anchor that `objects/pack` is a real
/// directory whose pack-related entries are regular files and that
/// `objects/info/alternates` is a regular file if present. This narrows, but
/// does not close, the window in which Gitoxide could open a swapped file.
pub(crate) fn check_object_store(common: &Anchor) -> Result<(), Error> {
    let special = || Error::UnsupportedCapability("symlink or special pack or alternates file");
    match common.lstat(b"objects/pack")? {
        None => {}
        Some(meta) if meta.kind == Kind::Dir => {
            for (name, kind) in common.read_dir(b"objects/pack")? {
                let packed = [&b".idx"[..], b".pack", b".rev", b".mtimes"]
                    .iter()
                    .any(|suffix| name.ends_with(suffix))
                    || name.starts_with(b"multi-pack-index");
                if packed && !matches!(kind, Kind::File { .. }) {
                    return Err(special());
                }
            }
        }
        Some(_) => return Err(special()),
    }
    match common.lstat(b"objects/info/alternates")? {
        None => Ok(()),
        Some(meta) if matches!(meta.kind, Kind::File { .. }) => Ok(()),
        Some(_) => Err(special()),
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

/// Write the compressed object to a fresh temporary file in `fan`, synced
/// and read-only; returns its name. The file is removed on any failure.
fn write_temporary(fan: &Anchor, kind: ObjectKind, data: &[u8]) -> Result<Vec<u8>, Error> {
    let mut attempts = 0;
    let (name, file) = loop {
        let name = format!("tmp_obj_{}", unique_suffix()).into_bytes();
        match fan.create_new(&name, 0o644) {
            Ok(file) => break (name, file),
            Err(Error::AlreadyExists(_)) if attempts < 8 => attempts += 1,
            Err(error) => return Err(error),
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
        let _ = fan.unlink(&name);
        return Err(error);
    }
    Ok(name)
}

#[cfg(unix)]
fn set_read_only(file: &File) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o444))?;
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
fn publish(fan: &Anchor, temporary: &[u8], name: &[u8], id: &ObjectId) -> Result<(), Error> {
    super::failpoint("object.before-publish")?;
    let existing = || -> Result<(), Error> { sync_read_only(&verify_loose_object(fan, name, id)?) };
    match fan.hard_link(temporary, name) {
        Ok(()) => Ok(()),
        Err(Error::AlreadyExists(_)) => existing(),
        Err(_) => match fan.rename_noreplace(temporary, name) {
            Ok(()) => Ok(()),
            Err(Error::AlreadyExists(_)) => existing(),
            Err(Error::UnsupportedCapability(_)) => Err(Error::UnsupportedCapability(
                "publishing loose objects needs hard links or an atomic no-replace rename",
            )),
            Err(error) => Err(error),
        },
    }
}

/// Verify that the loose object file `rel` below `anchor` is a regular file
/// whose zlib stream inflates to a well-formed object hashing to `id`,
/// without following a symlink, blocking on a FIFO or buffering the inflated
/// content. Returns the verified handle (for syncing).
pub(super) fn verify_loose_object(
    anchor: &Anchor,
    rel: &[u8],
    id: &ObjectId,
) -> Result<File, Error> {
    use std::io::Read;
    let corrupt = || -> Error {
        gix::error::corruption("an existing loose object file does not hold the object it names")
            .corrupted_error()
            .into()
    };
    let (mut file, _) = anchor.open_read(rel)?;
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
    Ok(file)
}
