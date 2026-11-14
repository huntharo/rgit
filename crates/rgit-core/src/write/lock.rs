//! Git-compatible `<path>.lock` files with atomic publication.
//!
//! Two forms share one protocol (exclusive create, write, sync, verify the
//! lock is still ours while it is held, rename over the target, sync the
//! directory):
//!
//! - [`LockFile::acquire_at`] (decision 0004): on Unix, `<rel>.lock` is
//!   created, checked, renamed and removed through an [`Anchor`], never
//!   through an ambient path. Identity is the `(dev, ino)` of the held
//!   descriptor, compared with an anchored `lstat` while the descriptor is
//!   still open, so the inode cannot be reused by a replacement. Elsewhere
//!   (Windows) the legacy backend is selected deliberately and `acquire_at`
//!   is exactly the path form below; it makes no anchoring claim.
//! - [`LockFile::acquire`]: the path form, kept while non-anchored callers
//!   remain and as the Windows implementation.
//!
//! The ownership check before the rename (and before the unlink in `Drop`)
//! and the rename/unlink itself are check-then-act by name: a replacement in
//! that instant is not detected. That is inherent to rename and unlink by
//! name, as in C Git; the held descriptor only rules out the inode-reuse
//! variant (a replacement reusing our freed inode number).
//!
//! Errors from an anchored [`LockFile::commit`] are typed so callers can
//! tell them apart without string matching: an error after the rename is
//! always one of the post-publication types ([`LockPublishedUnsyncedError`],
//! [`LockPublishedUnreachableError`]) inside [`Error::Other`], and
//! [`after_publication`] says so; every other error happened before the
//! rename and published nothing.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::Error;
use crate::fsx::Anchor;

/// An exclusively created `<target>.lock`. Content is written to the lock,
/// flushed to stable storage, then renamed over the target. Dropping an
/// uncommitted lock removes it only if the path still names the file this
/// writer created, so cleanup never unlinks another writer's lock. A crash
/// leaves the lock (as with C Git), which refuses later writers until removed;
/// the target is never partially written.
pub(crate) struct LockFile<'a> {
    location: Location<'a>,
    file: Option<File>,
    identity: Identity,
    /// Set once the lock path has been renamed away (or must not be touched).
    released: bool,
}

enum Location<'a> {
    /// The path form. On Unix it is unused once every caller is anchored.
    #[cfg_attr(unix, allow(dead_code))]
    Path { target: PathBuf, lock_path: PathBuf },
    /// Constructed on Unix only (the anchored backend). The whole lifecycle
    /// (create, identity check, rename, remove, directory sync) runs on one
    /// held parent directory, by leaf name, so a parent swapped after acquire
    /// cannot split it across directories.
    ///
    /// `root` and the full names are kept to verify that the held parent is
    /// still the directory reached from the root anchor (descriptor-root
    /// identity) before and after the rename.
    #[cfg_attr(not(unix), allow(dead_code))]
    Anchored {
        root: &'a Anchor,
        dir: Held<'a>,
        /// Leaf names inside `dir`.
        target: Vec<u8>,
        lock: Vec<u8>,
        /// Full target name below `root`.
        target_rel: Vec<u8>,
        /// For a nested lock: the parent's name below `root` and the identity
        /// of the held parent directory itself (`Anchor::id`, read from its
        /// descriptor at acquire).
        parent: Option<(Vec<u8>, crate::fsx::FileId)>,
    },
}

/// The directory that holds a lock: the caller's anchor when the lock is
/// directly below it, otherwise an owned anchor on the parent directory.
#[cfg_attr(not(unix), allow(dead_code))]
enum Held<'a> {
    Root(&'a Anchor),
    Parent(Anchor),
}

impl Held<'_> {
    fn get(&self) -> &Anchor {
        match self {
            Self::Root(anchor) => anchor,
            Self::Parent(anchor) => anchor,
        }
    }
}

impl<'a> LockFile<'a> {
    /// Anchored form (decision 0004). On Unix: exclusively create
    /// `<rel>.lock` inside `anchor` without following symlinks (an existing
    /// entry of any kind yields [`Error::Locked`]); commit renames it over
    /// `rel` through the anchor. Elsewhere: the path form, unchanged.
    pub(crate) fn acquire_at(anchor: &'a Anchor, rel: &[u8]) -> Result<Self, Error> {
        if rel.is_empty() || rel.ends_with(b"/") {
            return Err(Error::InvalidPath(rel.to_vec()));
        }
        #[cfg(unix)]
        {
            // Hold the parent directory for the whole lifecycle.
            let (dir, leaf) = match rel.iter().rposition(|&byte| byte == b'/') {
                Some(slash) => (Held::Parent(anchor.sub(&rel[..slash])?), &rel[slash + 1..]),
                None => (Held::Root(anchor), rel),
            };
            let mut lock = leaf.to_vec();
            lock.extend_from_slice(b".lock");
            let file = match dir.get().create_new(&lock, 0o666) {
                Ok(file) => file,
                Err(Error::AlreadyExists(_)) => {
                    return Err(Error::Locked(display(dir.get(), &lock)));
                }
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(Error::Locked(display(dir.get(), &lock)));
                }
                Err(error) => return Err(error),
            };
            let identity = Identity::of_file(&file)?;
            // The held parent's own identity, compared before the rename with
            // what the root reaches at the parent's name.
            let parent = match (&dir, rel.iter().rposition(|&byte| byte == b'/')) {
                (Held::Parent(held), Some(slash)) => Some((rel[..slash].to_vec(), held.id()?)),
                _ => None,
            };
            let lock = Self {
                location: Location::Anchored {
                    root: anchor,
                    dir,
                    target: leaf.to_vec(),
                    lock,
                    target_rel: rel.to_vec(),
                    parent,
                },
                file: Some(file),
                identity,
                released: false,
            };
            super::failpoint("lock.acquired")?;
            Ok(lock)
        }
        #[cfg(not(unix))]
        {
            let rel_path = std::path::PathBuf::from(
                std::str::from_utf8(rel).map_err(|_| Error::InvalidPath(rel.to_vec()))?,
            );
            Self::acquire(&anchor.display_path().join(rel_path))
        }
    }

    /// Path form: create `<target>.lock` with `O_CREAT|O_EXCL`; an existing
    /// lock (or any entry, including a symlink, at that path) yields
    /// [`Error::Locked`]. On Unix it is unused once every caller is anchored.
    #[cfg_attr(unix, allow(dead_code))]
    pub(crate) fn acquire(target: &Path) -> Result<Self, Error> {
        let mut lock_name = target
            .file_name()
            .ok_or_else(|| Error::InvalidPath(target.as_os_str().as_encoded_bytes().to_vec()))?
            .to_os_string();
        lock_name.push(".lock");
        let lock_path = target.with_file_name(lock_name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        // Windows: deny delete/rename sharing while held so no other process
        // can remove or replace the lock under us (std shares DELETE by
        // default).
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_SHARE_READ: u32 = 0x1;
            const FILE_SHARE_WRITE: u32 = 0x2;
            options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
        }
        let file = match options.open(&lock_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(Error::Locked(lock_path));
            }
            Err(error) => return Err(error.into()),
        };
        let identity = Identity::of_file(&file)?;
        let lock = Self {
            location: Location::Path {
                target: target.to_owned(),
                lock_path,
            },
            file: Some(file),
            identity,
            released: false,
        };
        super::failpoint("lock.acquired")?;
        Ok(lock)
    }

    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let file = self.file.as_mut().ok_or(Error::Injected("lock reused"))?;
        file.write_all(bytes)?;
        Ok(())
    }

    /// Flush and sync the lock content, rename it over the target, then sync
    /// the containing directory where the platform supports it.
    ///
    /// Errors (anchored form): before the rename, any error (including
    /// [`Error::Locked`] when the lock or its parent is no longer the one
    /// reached from the root) means nothing was published. After the rename,
    /// the error is a post-publication type (see [`after_publication`]):
    /// [`LockPublishedUnsyncedError`] (visible, durability unknown) or
    /// [`LockPublishedUnreachableError`] (renamed inside the held directory,
    /// which the root no longer reaches at that name; state uncertain).
    /// Nothing is ever rolled back. The path form keeps its pre-hardening
    /// errors: an error after its rename is returned as is.
    pub(crate) fn commit(mut self) -> Result<(), Error> {
        let file = self.file.as_mut().ok_or(Error::Injected("lock reused"))?;
        file.flush()?;
        file.sync_all()?;
        super::failpoint("lock.before-rename")?;
        // Check identity while our descriptor is still open, so the inode
        // cannot have been freed and reused by a replacement lock.
        if !self.still_ours() {
            // Someone removed or replaced our lock; never publish their file.
            self.released = true;
            drop(self.file.take());
            return Err(Error::Locked(self.lock_display()));
        }
        // Descriptor-root identity: the held parent must still be the
        // directory a reader reaches from the root. Refused before the rename
        // (not released, so Drop removes our lock from the held directory).
        if !self.parent_reachable_from_root() {
            return Err(Error::Locked(self.lock_display()));
        }
        match &self.location {
            Location::Anchored {
                root,
                dir,
                target,
                lock,
                target_rel,
                ..
            } => {
                // The descriptor stays open across the rename; the rename and
                // the sync act on the held parent directory.
                let target_display = display(root, target_rel);
                dir.get().rename_replace(lock, target)?;
                self.released = true;
                drop(self.file.take());
                #[cfg(test)]
                tests::after_rename_hook();
                // Published inside the held directory; is it reachable at the
                // name from the root?
                let observed = root.lstat(target_rel).ok().flatten();
                if observed.and_then(|meta| Identity::of_id(meta.id)) != Some(self.identity) {
                    return Err(Error::Other(Box::new(LockPublishedUnreachableError {
                        target: target_display,
                        observed: observed.map(|meta| (meta.id.volume, meta.id.index)),
                    })));
                }
                let synced = super::failpoint("lock.after-rename").and_then(|()| dir.get().sync());
                synced.map_err(|source| unsynced(target_display, source))
            }
            Location::Path { target, lock_path } => {
                // Windows cannot rename a file this process holds open
                // without delete sharing; close immediately before the
                // rename. Unix keeps the descriptor open across the rename.
                #[cfg(windows)]
                drop(self.file.take());
                std::fs::rename(lock_path, target)?;
                self.released = true;
                drop(self.file.take());
                // The path form keeps its pre-hardening errors except for
                // this Windows check: the handle was closed before the
                // rename, so check what the rename published (detects some
                // replacement outcomes only; see
                // `RenameOutcomeUncertainError`).
                #[cfg(windows)]
                {
                    super::failpoint("lock.renamed-unverified")?;
                    observe_after_rename(target, self.identity)?;
                }
                super::failpoint("lock.after-rename")?;
                sync_parent(target)
            }
        }
    }

    /// Whether the root still reaches the held parent: the entry at the
    /// parent's name below the root is a directory with the held directory's
    /// own identity. (Our lock's presence in it is `still_ours`.)
    fn parent_reachable_from_root(&self) -> bool {
        match &self.location {
            Location::Anchored {
                root,
                parent: Some((parent_rel, parent_id)),
                ..
            } => root.lstat(parent_rel).is_ok_and(|meta| {
                meta.is_some_and(|meta| meta.kind == crate::fsx::Kind::Dir && meta.id == *parent_id)
            }),
            _ => true,
        }
    }

    /// Identity of the lock file this writer created, read (`fstat`) from its
    /// held descriptor. Used to re-validate a guard lock that is never renamed
    /// (`HEAD.lock` while a branch is updated) against an anchored `lstat`.
    pub(crate) fn held_id(&self) -> Result<crate::fsx::FileId, Error> {
        crate::fsx::file_id(self.file.as_ref().ok_or(Error::Injected("lock reused"))?)
    }

    fn still_ours(&self) -> bool {
        match &self.location {
            Location::Anchored { dir, lock, .. } => dir.get().lstat(lock).is_ok_and(|meta| {
                meta.is_some_and(|meta| {
                    matches!(meta.kind, crate::fsx::Kind::File { .. })
                        && Identity::of_id(meta.id) == Some(self.identity)
                })
            }),
            Location::Path { lock_path, .. } => {
                Identity::of_path(lock_path).is_ok_and(|identity| identity == self.identity)
            }
        }
    }

    fn lock_display(&self) -> PathBuf {
        match &self.location {
            Location::Anchored { dir, lock, .. } => display(dir.get(), lock),
            Location::Path { lock_path, .. } => lock_path.clone(),
        }
    }
}

impl Drop for LockFile<'_> {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        // Decided while our descriptor is open.
        let ours = self.still_ours();
        drop(self.file.take());
        if ours {
            let _ = match &self.location {
                Location::Anchored { dir, lock, .. } => dir.get().unlink(lock),
                Location::Path { lock_path, .. } => {
                    std::fs::remove_file(lock_path).map_err(Error::from)
                }
            };
        }
    }
}

fn unsynced(target: PathBuf, source: Error) -> Error {
    Error::Other(Box::new(LockPublishedUnsyncedError { target, source }))
}

/// Whether an error from [`LockFile::commit`] happened after the rename, so
/// the new content may be visible (never report it as "unchanged").
// Mapped by the anchored reference writer (WP1, refs.rs).
#[allow(dead_code)]
pub(crate) fn after_publication(error: &Error) -> bool {
    matches!(error, Error::Other(error)
        if error.is::<LockPublishedUnsyncedError>()
            || error.is::<LockPublishedUnreachableError>()
            || error.is::<RenameOutcomeUncertainError>())
}

/// The lock was renamed over its target, but a later step failed (the
/// directory sync, or an injected failure at `lock.after-rename`): the new
/// content is visible and its durability is unknown. Returned inside
/// [`Error::Other`].
#[derive(Debug)]
pub struct LockPublishedUnsyncedError {
    target: PathBuf,
    source: Error,
}

impl LockPublishedUnsyncedError {
    /// The target that now holds the new content (diagnostic path).
    pub fn target(&self) -> &Path {
        &self.target
    }
}

impl std::fmt::Display for LockPublishedUnsyncedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} was published, but a step after the rename failed ({}); its durability is unknown",
            self.target.display(),
            self.source
        )
    }
}

impl std::error::Error for LockPublishedUnsyncedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// The lock was renamed over its target inside the held parent directory,
/// but afterwards the root anchor does not reach our file at the target's
/// name: the parent was moved or replaced, or another writer replaced the
/// target after us. The content was published inside the original directory,
/// which may no longer be reachable at that name; the outcome is uncertain,
/// not "unchanged", and nothing is rolled back. Returned inside
/// [`Error::Other`].
#[derive(Debug)]
pub struct LockPublishedUnreachableError {
    target: PathBuf,
    /// `(volume, index)` of what the root reaches at the target's name.
    observed: Option<(u64, u128)>,
}

impl LockPublishedUnreachableError {
    /// The target's name below the root anchor (diagnostic path).
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Identity of the entry the root reaches at the target's name, if any.
    pub fn observed(&self) -> Option<(u64, u128)> {
        self.observed
    }
}

impl std::fmt::Display for LockPublishedUnreachableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "renamed the lock over {} inside its original directory, but that name no \
             longer reaches the published file (the directory was moved or replaced, or \
             another writer replaced the target); the outcome is uncertain",
            self.target.display()
        )
    }
}

impl std::error::Error for LockPublishedUnreachableError {}

/// Diagnostic path of `rel` below `anchor`; never used for I/O.
fn display(anchor: &Anchor, rel: &[u8]) -> PathBuf {
    anchor
        .display_path()
        .join(String::from_utf8_lossy(rel).as_ref())
}

/// File identity used to confirm the lock path still names our lock: never
/// size or timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    id: crate::fsx::FileId,
}

impl Identity {
    #[cfg(unix)]
    fn of_file(file: &File) -> Result<Self, Error> {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    /// Identity from an anchored `lstat`.
    #[cfg(unix)]
    fn of_id(id: crate::fsx::FileId) -> Option<Self> {
        Some(Self {
            device: id.volume,
            inode: u64::try_from(id.index).ok()?,
        })
    }

    #[cfg(unix)]
    fn of_path(path: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path)?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    #[cfg(unix)]
    fn parts(self) -> (u64, u128) {
        (self.device, u128::from(self.inode))
    }

    // Windows (legacy path backend): `(volume serial, file index)` of the
    // handle, through `fsx::file_id`. `acquire` opens the lock without
    // FILE_SHARE_DELETE, so no other process can delete or rename it while
    // our handle is open, and the pre-rename check compares real identities.
    // The handle must be closed before `MoveFileExW` (it needs DELETE access
    // to the source), so a replacement in that window is NOT prevented; the
    // post-rename observation only detects some of its outcomes.
    #[cfg(not(unix))]
    fn of_file(file: &File) -> Result<Self, Error> {
        Ok(Self {
            id: crate::fsx::file_id(file)?,
        })
    }

    #[cfg(not(unix))]
    fn of_id(id: crate::fsx::FileId) -> Option<Self> {
        Some(Self { id })
    }

    /// Identity of the entry at `path` itself: opened with no data access
    /// (exempt from share-mode conflicts, so it coexists with our deny-delete
    /// lock handle), without following a reparse point.
    #[cfg(not(unix))]
    fn of_path(path: &Path) -> std::io::Result<Self> {
        let mut options = OpenOptions::new();
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            options
                .access_mode(0)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
        }
        let probe = options.open(path)?;
        Ok(Self {
            id: crate::fsx::file_id(&probe).map_err(std::io::Error::other)?,
        })
    }

    #[cfg(not(unix))]
    fn parts(self) -> (u64, u128) {
        (self.id.volume, self.id.index)
    }
}

/// Upper bound on the target bytes captured for a report.
const OBSERVED_CONTENT_LIMIT: u64 = 64 * 1024;

/// After a path rename that returned success, check that `target` is the file
/// we created. A mismatch is reported as [`RenameOutcomeUncertainError`] with
/// the independently observed target state; the target is never rolled back
/// or rewritten.
#[cfg_attr(not(windows), allow(dead_code))]
fn observe_after_rename(target: &Path, ours: Identity) -> Result<(), Error> {
    let observed = Identity::of_path(target).ok();
    if observed == Some(ours) {
        return Ok(());
    }
    let content = (|| -> std::io::Result<Option<(Vec<u8>, u64)>> {
        use std::io::Read;
        let file = super::open_leaf_nofollow(target)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        file.take(OBSERVED_CONTENT_LIMIT).read_to_end(&mut bytes)?;
        Ok(Some((bytes, metadata.len())))
    })()
    .ok()
    .flatten();
    Err(Error::Other(Box::new(RenameOutcomeUncertainError {
        target: target.to_owned(),
        ours: ours.parts(),
        observed: observed.map(Identity::parts),
        content,
    })))
}

/// A path-form lock rename returned success, but the target no longer has the
/// identity of the lock file this writer created. The outcome is UNCERTAIN:
/// the value may have been published by us and then replaced by another
/// (possibly legitimate) writer, or the rename may have published a file
/// that replaced our lock in the close-before-rename window. It does not mean
/// "unchanged" and does not prove foreign publication. The observed target
/// state is reported; nothing is rolled back. This detects some replacement
/// outcomes only; it does not close the window (see the Windows handle-rename
/// proposal, PWR-197). Returned inside [`Error::Other`].
#[derive(Debug)]
pub struct RenameOutcomeUncertainError {
    target: PathBuf,
    /// `(volume, index)` of the lock file we created.
    ours: (u64, u128),
    /// `(volume, index)` of the target when observed, if it could be opened.
    observed: Option<(u64, u128)>,
    /// Up to 64 KiB of the observed target and its full length, if it was a
    /// regular file.
    content: Option<(Vec<u8>, u64)>,
}

impl RenameOutcomeUncertainError {
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Identity `(volume, index)` of the lock file this writer created.
    pub fn ours(&self) -> (u64, u128) {
        self.ours
    }

    /// Identity of the target as observed after the rename, if any.
    pub fn observed(&self) -> Option<(u64, u128)> {
        self.observed
    }

    /// Up to 64 KiB of the observed target content, if it was a regular file.
    pub fn observed_content(&self) -> Option<&[u8]> {
        self.content.as_ref().map(|(bytes, _)| bytes.as_slice())
    }

    /// Full length of the observed target, if it was a regular file.
    pub fn observed_len(&self) -> Option<u64> {
        self.content.as_ref().map(|(_, len)| *len)
    }
}

impl std::fmt::Display for RenameOutcomeUncertainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "rename to {} returned success, but the target no longer has our identity; \
             it may have been published by us and then replaced, or the rename published \
             another file (observed: ",
            self.target.display()
        )?;
        match (&self.observed, &self.content) {
            (None, _) => write!(f, "no entry)"),
            (Some((volume, index)), Some((_, len))) => {
                write!(f, "file {volume:x}:{index:x}, {len} bytes)")
            }
            (Some((volume, index)), None) => write!(f, "{volume:x}:{index:x}, not a regular file)"),
        }
    }
}

impl std::error::Error for RenameOutcomeUncertainError {}

/// Make a completed rename durable (Unix). Windows has no directory handle
/// sync through std; renames there are visibility-only until the OS flushes.
pub(crate) fn sync_parent(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// Runs once, between the anchored rename and the reachability check.
        static AFTER_RENAME: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            std::cell::RefCell::new(None);
    }

    pub(super) fn after_rename_hook() {
        if let Some(hook) = AFTER_RENAME.with(|cell| cell.borrow_mut().take()) {
            hook();
        }
    }

    fn scratch(name: &str) -> Result<PathBuf, Error> {
        let dir = std::env::temp_dir().join(format!("rgit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    #[test]
    fn lock_refuses_second_writer_and_publishes_atomically() -> Result<(), Error> {
        let dir = scratch("lock")?;
        let target = dir.join("index");
        std::fs::write(&target, b"old")?;
        let mut first = LockFile::acquire(&target)?;
        assert!(matches!(LockFile::acquire(&target), Err(Error::Locked(_))));
        first.write_all(b"new")?;
        assert_eq!(std::fs::read(&target)?, b"old");
        first.commit()?;
        assert_eq!(std::fs::read(&target)?, b"new");
        assert!(!dir.join("index.lock").exists());
        drop(LockFile::acquire(&target)?);
        assert!(!dir.join("index.lock").exists());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn replaced_lock_is_neither_unlinked_nor_published() -> Result<(), Error> {
        let dir = scratch("lock-replaced")?;
        let target = dir.join("HEAD");
        std::fs::write(&target, b"old")?;
        let lock_path = dir.join("HEAD.lock");

        // Another party removes our lock and a second writer takes the path.
        let abandoned = LockFile::acquire(&target)?;
        std::fs::remove_file(&lock_path)?;
        std::fs::write(&lock_path, b"other writer")?;
        drop(abandoned);
        assert_eq!(std::fs::read(&lock_path)?, b"other writer");

        std::fs::remove_file(&lock_path)?;
        let mut ours = LockFile::acquire(&target)?;
        ours.write_all(b"ours")?;
        std::fs::remove_file(&lock_path)?;
        std::fs::write(&lock_path, b"other writer")?;
        assert!(matches!(ours.commit(), Err(Error::Locked(_))));
        assert_eq!(std::fs::read(&target)?, b"old");
        assert_eq!(std::fs::read(&lock_path)?, b"other writer");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// Anchored form: same protocol with all I/O through the anchor; a
    /// replaced lock is neither published nor unlinked, and a symlink at the
    /// lock path is an existing entry that is never followed.
    #[cfg(unix)]
    #[test]
    fn anchored_lock_publishes_and_never_touches_a_replacement() -> Result<(), Error> {
        use crate::fsx::Root;
        let dir = scratch("lock-anchored")?;
        std::fs::create_dir(dir.join("refs"))?;
        std::fs::write(dir.join("refs/main"), b"old")?;
        let anchor = Anchor::open(&dir, Root::GitDir)?;
        let lock_path = dir.join("refs/main.lock");

        let mut first = LockFile::acquire_at(&anchor, b"refs/main")?;
        assert!(matches!(
            LockFile::acquire_at(&anchor, b"refs/main"),
            Err(Error::Locked(_))
        ));
        first.write_all(b"new")?;
        first.commit()?;
        assert_eq!(std::fs::read(dir.join("refs/main"))?, b"new");
        assert!(!lock_path.exists());

        // Abandoned without commit: removed only while it is still ours.
        drop(LockFile::acquire_at(&anchor, b"refs/main")?);
        assert!(!lock_path.exists());
        let abandoned = LockFile::acquire_at(&anchor, b"refs/main")?;
        std::fs::remove_file(&lock_path)?;
        std::fs::write(&lock_path, b"other writer")?;
        drop(abandoned);
        assert_eq!(std::fs::read(&lock_path)?, b"other writer");

        std::fs::remove_file(&lock_path)?;
        let mut ours = LockFile::acquire_at(&anchor, b"refs/main")?;
        ours.write_all(b"ours")?;
        std::fs::remove_file(&lock_path)?;
        std::fs::write(&lock_path, b"other writer")?;
        assert!(matches!(ours.commit(), Err(Error::Locked(_))));
        assert_eq!(std::fs::read(dir.join("refs/main"))?, b"new");
        assert_eq!(std::fs::read(&lock_path)?, b"other writer");

        std::fs::remove_file(&lock_path)?;
        std::os::unix::fs::symlink(dir.join("elsewhere"), &lock_path)?;
        assert!(matches!(
            LockFile::acquire_at(&anchor, b"refs/main"),
            Err(Error::Locked(_))
        ));
        assert!(!dir.join("elsewhere").exists());
        assert!(matches!(
            LockFile::acquire_at(&anchor, b""),
            Err(Error::InvalidPath(_))
        ));
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// Pre-rename mismatch: if the lock path does not name the file we hold,
    /// commit refuses with Locked and publishes nothing (on Windows the
    /// deny-delete handle prevents setting this up by replacement, so the
    /// held identity is substituted instead).
    #[test]
    fn mismatched_identity_refuses_before_rename() -> Result<(), Error> {
        let dir = scratch("lock-mismatch")?;
        let target = dir.join("HEAD");
        std::fs::write(&target, b"old")?;
        let mut lock = LockFile::acquire(&target)?;
        lock.write_all(b"new")?;
        lock.identity = Identity::of_path(&target)?;
        assert!(matches!(lock.commit(), Err(Error::Locked(_))));
        assert_eq!(std::fs::read(&target)?, b"old");
        assert_eq!(std::fs::read(dir.join("HEAD.lock"))?, b"new");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// Post-rename observation: our file is accepted; any other outcome is
    /// reported as uncertain with the observed state, and never rolled back.
    #[test]
    fn post_rename_observation_reports_uncertain_outcomes() -> Result<(), Error> {
        let dir = scratch("lock-observe")?;
        let ours_path = dir.join("ours");
        std::fs::write(&ours_path, b"ours")?;
        let ours = Identity::of_path(&ours_path)?;
        let target = dir.join("HEAD");

        std::fs::copy(&ours_path, dir.join("copy"))?;
        std::fs::rename(&ours_path, &target)?;
        observe_after_rename(&target, ours)?;

        // Replaced after publication (or published in place of ours). The
        // replacement is written first and renamed over the target, so it
        // never reuses our inode (ext4 can hand a freed inode straight back).
        std::fs::write(dir.join("replacement"), b"someone else")?;
        std::fs::rename(dir.join("replacement"), &target)?;
        let replaced = Identity::of_path(&target)?;
        match observe_after_rename(&target, ours) {
            Err(Error::Other(error)) => {
                let typed = error
                    .downcast_ref::<RenameOutcomeUncertainError>()
                    .ok_or(Error::Injected("type"))?;
                assert_eq!(typed.ours(), ours.parts());
                assert_eq!(typed.observed(), Some(replaced.parts()));
                assert_eq!(typed.observed_content(), Some(&b"someone else"[..]));
                assert_eq!(typed.observed_len(), Some(12));
                assert!(error.to_string().contains("no longer has our identity"));
            }
            other => panic!("expected an uncertain outcome, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(&target)?,
            b"someone else",
            "never rolled back"
        );

        // Nothing at the target.
        std::fs::remove_file(&target)?;
        match observe_after_rename(&target, ours) {
            Err(Error::Other(error)) => {
                let typed = error
                    .downcast_ref::<RenameOutcomeUncertainError>()
                    .ok_or(Error::Injected("type"))?;
                assert_eq!(typed.observed(), None);
                assert_eq!(typed.observed_content(), None);
            }
            other => panic!("expected an uncertain outcome, got {other:?}"),
        }
        assert!(!target.exists());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// Windows legacy lock: while held, no process (including us) can delete
    /// or rename it; its identity is the real (volume serial, file index);
    /// publication observes our file at the target.
    #[cfg(windows)]
    #[test]
    fn windows_held_lock_cannot_be_deleted_or_renamed() -> Result<(), Error> {
        let dir = scratch("lock-windows")?;
        let target = dir.join("HEAD");
        std::fs::write(&target, b"old")?;
        let lock_path = dir.join("HEAD.lock");
        let mut lock = LockFile::acquire(&target)?;
        assert!(std::fs::remove_file(&lock_path).is_err());
        assert!(std::fs::rename(&lock_path, dir.join("moved")).is_err());
        assert!(lock_path.exists());
        let held = lock.identity;
        assert_eq!(Identity::of_path(&lock_path)?, held);
        assert_ne!(Identity::of_path(&target)?, held);
        lock.write_all(b"new")?;
        lock.commit()?;
        assert_eq!(std::fs::read(&target)?, b"new");
        assert_eq!(Identity::of_path(&target)?, held);
        assert!(!lock_path.exists());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// Entries of `dir` with their bytes and inode, for "untouched" checks.
    #[cfg(unix)]
    fn tree(dir: &Path) -> Result<Vec<(std::ffi::OsString, Vec<u8>, u64)>, Error> {
        use std::os::unix::fs::MetadataExt;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let metadata = std::fs::symlink_metadata(entry.path())?;
            entries.push((
                entry.file_name(),
                std::fs::read(entry.path())?,
                metadata.ino(),
            ));
        }
        entries.sort();
        Ok(entries)
    }

    /// Retained parent anchor and descriptor-root identity: after acquire,
    /// the lock's parent `refs/heads` is renamed away and a decoy
    /// `refs/heads` (holding a foreign `main.lock`) takes its place. The decoy
    /// and the foreign lock are never touched and nothing is published. With
    /// the anchored backend, commit refuses because the root no longer
    /// reaches the held parent, and our lock is removed from the held
    /// (original) directory, so the writer leaves nothing of its own behind;
    /// drop also removes it there. The legacy path backend re-resolves by
    /// name, sees the foreign lock and refuses without touching it.
    #[cfg(unix)]
    #[test]
    fn anchored_lock_retains_its_parent_directory() -> Result<(), Error> {
        use crate::fsx::{HARDENING, Hardening, Root};
        let dir = scratch("lock-parent-swap")?;
        let heads = dir.join("refs/heads");
        let original = dir.join("refs/heads-original");
        std::fs::create_dir_all(&heads)?;
        std::fs::write(heads.join("main"), b"old")?;
        let anchor = Anchor::open(&dir, Root::GitDir)?;
        let anchored = HARDENING == Hardening::Anchored;
        let swap = || -> Result<Vec<(std::ffi::OsString, Vec<u8>, u64)>, Error> {
            std::fs::rename(&heads, &original)?;
            std::fs::create_dir(&heads)?;
            std::fs::write(heads.join("main.lock"), b"foreign")?;
            tree(&heads)
        };
        let restore = || -> Result<(), Error> {
            std::fs::remove_dir_all(&heads)?;
            let _ = std::fs::remove_file(original.join("main.lock"));
            std::fs::rename(&original, &heads)?;
            Ok(())
        };

        // Commit after the swap.
        let mut lock = LockFile::acquire_at(&anchor, b"refs/heads/main")?;
        lock.write_all(b"new")?;
        let decoy = swap()?;
        let result = lock.commit();
        assert_eq!(tree(&heads)?, decoy, "decoy parent touched by commit");
        assert!(matches!(result, Err(Error::Locked(_))), "{result:?}");
        assert_eq!(std::fs::read(original.join("main"))?, b"old");
        // The anchored writer removes its own lock from the held directory.
        assert_eq!(original.join("main.lock").exists(), !anchored);
        restore()?;

        // Drop after the swap.
        let lock = LockFile::acquire_at(&anchor, b"refs/heads/main")?;
        let decoy = swap()?;
        drop(lock);
        assert_eq!(tree(&heads)?, decoy, "decoy parent touched by drop");
        assert_eq!(original.join("main.lock").exists(), !anchored);
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// Post-rename descriptor-root check: the parent is swapped exactly
    /// between the rename and the check (test hook). The content was renamed
    /// inside the original directory; the root no longer reaches it at the
    /// name, so commit reports `LockPublishedUnreachableError` (uncertain,
    /// post-publication), the decoy is untouched and nothing is rolled back.
    #[cfg(unix)]
    #[test]
    fn anchored_lock_reports_publication_the_root_cannot_reach() -> Result<(), Error> {
        use crate::fsx::Root;
        let dir = scratch("lock-unreachable")?;
        let heads = dir.join("refs/heads");
        let original = dir.join("refs/heads-original");
        std::fs::create_dir_all(&heads)?;
        std::fs::write(heads.join("main"), b"old")?;
        let anchor = Anchor::open(&dir, Root::GitDir)?;

        let mut lock = LockFile::acquire_at(&anchor, b"refs/heads/main")?;
        lock.write_all(b"new")?;
        let (swap_heads, swap_original) = (heads.clone(), original.clone());
        AFTER_RENAME.with(|cell| {
            *cell.borrow_mut() = Some(Box::new(move || {
                let swapped = std::fs::rename(&swap_heads, &swap_original)
                    .and_then(|()| std::fs::create_dir(&swap_heads))
                    .and_then(|()| std::fs::write(swap_heads.join("main"), b"decoy"));
                assert!(swapped.is_ok(), "{swapped:?}");
            }));
        });
        let decoy_before = || tree(&heads);
        let result = lock.commit();
        let error = match result {
            Err(error) => error,
            Ok(()) => panic!("an unreachable publication was reported as success"),
        };
        assert!(after_publication(&error), "{error:?}");
        match &error {
            Error::Other(inner) => {
                let typed = inner
                    .downcast_ref::<LockPublishedUnreachableError>()
                    .ok_or(Error::Injected("type"))?;
                assert!(typed.observed().is_some());
                assert!(inner.to_string().contains("uncertain"));
            }
            other => panic!("expected a typed post-publication error, got {other:?}"),
        }
        // Published in the original directory; the decoy keeps its content.
        assert_eq!(std::fs::read(original.join("main"))?, b"new");
        assert!(!original.join("main.lock").exists());
        assert_eq!(std::fs::read(heads.join("main"))?, b"decoy");
        assert_eq!(decoy_before()?.len(), 1);
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// Pre-rename errors are never classified as post-publication.
    #[test]
    fn only_post_rename_errors_are_after_publication() {
        assert!(!after_publication(&Error::Locked(PathBuf::from("x.lock"))));
        assert!(!after_publication(&Error::Injected("lock.before-rename")));
        assert!(after_publication(&unsynced(
            PathBuf::from("x"),
            Error::Injected("lock.after-rename")
        )));
    }
}
