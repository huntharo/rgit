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
//! rename and published nothing. The Windows path form adds one typed
//! post-rename outcome, [`RenameOutcomeUncertainError`], which
//! [`after_publication`] also recognises; its other post-rename errors stay
//! untyped.

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
    /// errors (an error after its rename is returned as is), except that on
    /// Windows a target that cannot be confirmed to still have our identity
    /// after the rename is [`RenameOutcomeUncertainError`], also a
    /// post-publication type.
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
    match observe(target, ours) {
        None => Ok(()),
        Some(observed) => Err(Error::Other(Box::new(RenameOutcomeUncertainError {
            target: target.to_owned(),
            ours: ours.parts(),
            observed,
        }))),
    }
}

/// One observation of `target`: a single no-follow open with read access
/// (sharing read, write and delete), whose handle supplies the identity, the
/// kind and the content. `None` means the target is our file.
///
/// If that open fails with anything but `NotFound`, a second, separate probe
/// without data access (which no share mode denies) reads the identity alone,
/// so a sharing conflict on our own published file is not misreported. That
/// probe never contributes content.
#[cfg_attr(not(windows), allow(dead_code))]
fn observe(target: &Path, ours: Identity) -> Option<ObservedTarget> {
    let file = match open_for_observation(target) {
        Ok(file) => file,
        Err(error) => return classify_open_failure(error, || Identity::of_path(target), ours),
    };
    let id = match Identity::of_file(&file) {
        Ok(id) => id,
        Err(error) => {
            return classify_open_failure(into_io(error), || Identity::of_path(target), ours);
        }
    };
    if id == ours {
        return None;
    }
    #[cfg(test)]
    tests::between_open_and_read_hook();
    Some(read_observed(&file, id))
}

/// Kind and content of an opened target whose identity is `id`, all from the
/// same handle.
#[cfg_attr(not(windows), allow(dead_code))]
fn read_observed(file: &File, id: Identity) -> ObservedTarget {
    use std::io::Read;
    let unavailable = |error: std::io::Error| ObservedTarget::unavailable(Some(id.parts()), &error);
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => return unavailable(error),
    };
    if !metadata.is_file() {
        return ObservedTarget::NotRegular { id: id.parts() };
    }
    let mut content = Vec::new();
    match file.take(OBSERVED_CONTENT_LIMIT).read_to_end(&mut content) {
        Ok(_) => ObservedTarget::File {
            id: id.parts(),
            content,
            len: metadata.len(),
        },
        Err(error) => unavailable(error),
    }
}

/// The observation open failed. Only `NotFound` is a confirmed absence;
/// anything else (sharing, permission, a symlink refused by no-follow) is
/// unavailable with its error kind kept. The identity, if any, comes from a
/// separate probe; a probe that finds our own file means "ours".
#[cfg_attr(not(windows), allow(dead_code))]
fn classify_open_failure(
    error: std::io::Error,
    probe: impl FnOnce() -> std::io::Result<Identity>,
    ours: Identity,
) -> Option<ObservedTarget> {
    if error.kind() == std::io::ErrorKind::NotFound {
        return Some(ObservedTarget::Absent);
    }
    let probed = probe().ok();
    if probed == Some(ours) {
        return None;
    }
    Some(ObservedTarget::unavailable(
        probed.map(Identity::parts),
        &error,
    ))
}

#[cfg_attr(not(windows), allow(dead_code))]
fn into_io(error: Error) -> std::io::Error {
    match error {
        Error::Io(error) => error,
        other => std::io::Error::other(other),
    }
}

/// Open the entry at `path` itself for reading: no symlink following (Unix:
/// `O_NOFOLLOW`, non-blocking so a FIFO cannot hang; Windows: the reparse
/// point itself, directories allowed), sharing read, write and delete.
#[cfg_attr(not(windows), allow(dead_code))]
fn open_for_observation(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use rustix::fs::OFlags;
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
    }
    options.open(path)
}

/// What the post-rename observation found at the target. Identities are
/// `(volume, index)`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObservedTarget {
    /// Confirmed absent: the open reported `NotFound`.
    Absent,
    /// An entry that is not a regular file (a directory, a symlink or other
    /// reparse point, a FIFO), with the identity of the opened handle.
    NotRegular { id: (u64, u128) },
    /// A regular file. Identity, content (at most 64 KiB) and full length all
    /// come from the same opened handle.
    File {
        id: (u64, u128),
        content: Vec<u8>,
        len: u64,
    },
    /// The target could not be observed (sharing violation, permission, a
    /// symlink refused by no-follow, a read error). This is NOT a claim of
    /// absence. `id` is the identity if one was obtained: from the opened
    /// handle when the read failed, or from a separate probe without data
    /// access when the open failed. No content is reported.
    Unavailable {
        id: Option<(u64, u128)>,
        kind: std::io::ErrorKind,
        raw_os_error: Option<i32>,
        message: String,
    },
}

impl ObservedTarget {
    fn unavailable(id: Option<(u64, u128)>, error: &std::io::Error) -> Self {
        Self::Unavailable {
            id,
            kind: error.kind(),
            raw_os_error: error.raw_os_error(),
            message: error.to_string(),
        }
    }

    /// The observed identity, if one was obtained.
    pub fn id(&self) -> Option<(u64, u128)> {
        match self {
            Self::Absent => None,
            Self::NotRegular { id } | Self::File { id, .. } => Some(*id),
            Self::Unavailable { id, .. } => *id,
        }
    }
}

impl std::fmt::Display for ObservedTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Absent => f.write_str("no entry (not found)"),
            Self::NotRegular {
                id: (volume, index),
            } => {
                write!(f, "{volume:x}:{index:x}, not a regular file")
            }
            Self::File {
                id: (volume, index),
                len,
                ..
            } => write!(f, "file {volume:x}:{index:x}, {len} bytes"),
            Self::Unavailable {
                id, kind, message, ..
            } => {
                write!(f, "unavailable, {kind:?}: {message}")?;
                match id {
                    Some((volume, index)) => write!(f, "; probed identity {volume:x}:{index:x}"),
                    None => f.write_str("; identity unknown"),
                }
            }
        }
    }
}

/// A path-form lock rename returned success, but the target could not be
/// confirmed to still have the identity of the lock file this writer created:
/// either a different identity was observed there, or the target was absent,
/// or it could not be observed at all ([`ObservedTarget::Unavailable`], where
/// the identity may be unknown). The outcome is UNCERTAIN:
/// the value may have been published by us and then replaced by another
/// (possibly legitimate) writer, or the rename may have published a file
/// that replaced our lock in the close-before-rename window. It does not mean
/// "unchanged" and does not prove foreign publication. The observed target
/// state ([`ObservedTarget`]) is reported; nothing is rolled back. This
/// detects some replacement outcomes only; it does not close the window (see
/// the Windows handle-rename proposal, PWR-197). Returned inside
/// [`Error::Other`].
#[derive(Debug)]
pub struct RenameOutcomeUncertainError {
    target: PathBuf,
    /// `(volume, index)` of the lock file we created.
    ours: (u64, u128),
    observed: ObservedTarget,
}

impl RenameOutcomeUncertainError {
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Identity `(volume, index)` of the lock file this writer created.
    pub fn ours(&self) -> (u64, u128) {
        self.ours
    }

    /// What was observed at the target after the rename.
    pub fn observed(&self) -> &ObservedTarget {
        &self.observed
    }

    /// Up to 64 KiB of the observed target content, read from the same
    /// handle as its identity; only for [`ObservedTarget::File`].
    pub fn observed_content(&self) -> Option<&[u8]> {
        match &self.observed {
            ObservedTarget::File { content, .. } => Some(content),
            _ => None,
        }
    }

    /// Full length of the observed target; only for [`ObservedTarget::File`].
    pub fn observed_len(&self) -> Option<u64> {
        match &self.observed {
            ObservedTarget::File { len, .. } => Some(*len),
            _ => None,
        }
    }
}

impl std::fmt::Display for RenameOutcomeUncertainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A mismatch is only asserted when it was observed: a different
        // identity, or a confirmed absence. Otherwise our identity simply
        // could not be confirmed.
        let finding = match &self.observed {
            ObservedTarget::Unavailable { id: None, .. } => {
                "could not confirm that the target still has our identity"
            }
            _ => "the target no longer has our identity",
        };
        write!(
            f,
            "rename to {} returned success, but {finding}; \
             it may have been published by us and then replaced, or the rename published \
             another file (observed: {})",
            self.target.display(),
            self.observed
        )
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
                assert_eq!(
                    typed.observed(),
                    &ObservedTarget::File {
                        id: replaced.parts(),
                        content: b"someone else".to_vec(),
                        len: 12,
                    }
                );
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
                assert_eq!(typed.observed(), &ObservedTarget::Absent);
                assert_eq!(typed.observed_content(), None);
                assert!(error.to_string().contains("no entry (not found)"));
            }
            other => panic!("expected an uncertain outcome, got {other:?}"),
        }
        assert!(!target.exists());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// Classification of a failed observation open, independent of the
    /// filesystem: only `NotFound` is absent; every other error is
    /// unavailable with its kind and OS code kept, with the separately
    /// probed identity when there is one; a probe that finds our file is
    /// "ours", not uncertain.
    #[test]
    fn failed_observation_open_is_classified_by_error_kind() -> Result<(), Error> {
        use std::io::{Error as IoError, ErrorKind};
        let dir = scratch("lock-classify")?;
        std::fs::write(dir.join("ours"), b"ours")?;
        std::fs::write(dir.join("other"), b"other")?;
        let ours = Identity::of_path(&dir.join("ours"))?;
        let other = Identity::of_path(&dir.join("other"))?;
        let probe_fails = || Err(IoError::from(ErrorKind::PermissionDenied));

        assert_eq!(
            classify_open_failure(IoError::from(ErrorKind::NotFound), || Ok(other), ours),
            Some(ObservedTarget::Absent)
        );
        let denied = classify_open_failure(
            IoError::new(ErrorKind::PermissionDenied, "denied"),
            probe_fails,
            ours,
        );
        assert!(
            matches!(
                &denied,
                Some(ObservedTarget::Unavailable {
                    id: None,
                    kind: ErrorKind::PermissionDenied,
                    raw_os_error: None,
                    message,
                }) if message == "denied"
            ),
            "{denied:?}"
        );
        // An OS error (Windows ERROR_SHARING_VIOLATION is 32) keeps its code
        // and the separately probed identity.
        let sharing = classify_open_failure(IoError::from_raw_os_error(32), || Ok(other), ours);
        assert!(
            matches!(
                &sharing,
                Some(ObservedTarget::Unavailable { id: Some(id), raw_os_error: Some(32), .. })
                    if *id == other.parts()
            ),
            "{sharing:?}"
        );
        // Our own file behind a sharing conflict is ours, not uncertain.
        assert_eq!(
            classify_open_failure(IoError::from_raw_os_error(32), || Ok(ours), ours),
            None
        );
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// Each observation variant on the real filesystem, with a truthful
    /// message: ours, a regular file, not a regular file, absent and (Unix:
    /// a symlink refused by no-follow) unavailable. Windows sharing denial is
    /// `windows_sharing_denial_is_unavailable_not_absent`.
    #[test]
    fn observation_reports_each_variant() -> Result<(), Error> {
        let dir = scratch("lock-variants")?;
        let target = dir.join("HEAD");
        std::fs::write(dir.join("ours"), b"ours")?;
        let ours = Identity::of_path(&dir.join("ours"))?;
        std::fs::rename(dir.join("ours"), &target)?;
        assert_eq!(observe(&target, ours), None);

        std::fs::write(dir.join("file"), b"file")?;
        std::fs::rename(dir.join("file"), &target)?;
        let file = Identity::of_path(&target)?;
        let observed = observe(&target, ours).ok_or(Error::Injected("file"))?;
        assert_eq!(
            observed,
            ObservedTarget::File {
                id: file.parts(),
                content: b"file".to_vec(),
                len: 4,
            }
        );
        assert!(observed.to_string().ends_with(", 4 bytes"), "{observed}");

        std::fs::remove_file(&target)?;
        std::fs::create_dir(&target)?;
        let directory = Identity::of_path(&target)?;
        let observed = observe(&target, ours).ok_or(Error::Injected("dir"))?;
        assert_eq!(
            observed,
            ObservedTarget::NotRegular {
                id: directory.parts()
            }
        );
        assert!(
            observed.to_string().contains("not a regular file"),
            "{observed}"
        );

        std::fs::remove_dir(&target)?;
        let observed = observe(&target, ours).ok_or(Error::Injected("absent"))?;
        assert_eq!(observed, ObservedTarget::Absent);
        assert_eq!(observed.to_string(), "no entry (not found)");

        #[cfg(unix)]
        {
            // No-follow refuses the symlink (ELOOP): it exists, so it is
            // unavailable with the separately probed identity, never absent.
            std::os::unix::fs::symlink("elsewhere", &target)?;
            let link = Identity::of_path(&target)?;
            let observed = observe(&target, ours).ok_or(Error::Injected("link"))?;
            match &observed {
                ObservedTarget::Unavailable {
                    id, raw_os_error, ..
                } => {
                    assert_eq!(*id, Some(link.parts()));
                    assert_eq!(*raw_os_error, Some(rustix::io::Errno::LOOP.raw_os_error()));
                }
                other => panic!("expected unavailable, got {other:?}"),
            }
            assert!(
                observed.to_string().starts_with("unavailable, "),
                "{observed}"
            );
            assert!(!observed.to_string().contains("not found"), "{observed}");
        }
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    thread_local! {
        /// Runs once, between the observation open and its read.
        static BETWEEN_OPEN_AND_READ: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            std::cell::RefCell::new(None);
    }

    pub(super) fn between_open_and_read_hook() {
        if let Some(hook) = BETWEEN_OPEN_AND_READ.with(|cell| cell.borrow_mut().take()) {
            hook();
        }
    }

    /// One observation: the target is swapped between the open and the read,
    /// and the reported identity and bytes still both describe the file that
    /// was opened, never the opened identity with the newcomer's bytes.
    #[test]
    fn observation_takes_identity_and_content_from_one_handle() -> Result<(), Error> {
        let dir = scratch("lock-one-handle")?;
        let target = dir.join("HEAD");
        std::fs::write(dir.join("ours"), b"ours")?;
        let ours = Identity::of_path(&dir.join("ours"))?;
        std::fs::write(dir.join("first"), b"first writer")?;
        std::fs::rename(dir.join("first"), &target)?;
        let first = Identity::of_path(&target)?;
        std::fs::write(dir.join("second"), b"second")?;
        let second = Identity::of_path(&dir.join("second"))?;
        let (swap_target, swap_dir) = (target.clone(), dir.clone());
        BETWEEN_OPEN_AND_READ.with(|cell| {
            *cell.borrow_mut() = Some(Box::new(move || {
                // Move the opened file away (allowed: the observation shares
                // delete), then put another file at the name.
                let swapped = std::fs::rename(&swap_target, swap_dir.join("moved"))
                    .and_then(|()| std::fs::rename(swap_dir.join("second"), &swap_target));
                assert!(swapped.is_ok(), "{swapped:?}");
            }));
        });
        let observed = observe(&target, ours);
        assert_eq!(Identity::of_path(&target)?, second, "the hook swapped");
        assert_eq!(
            observed,
            Some(ObservedTarget::File {
                id: first.parts(),
                content: b"first writer".to_vec(),
                len: 12,
            })
        );
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// Windows: a target held open without FILE_SHARE_READ cannot be read.
    /// The observation reports it as unavailable with the sharing-violation
    /// code (and the identity from the no-data-access probe), never absent
    /// or not regular; our own file held that way is still accepted.
    #[cfg(windows)]
    #[test]
    fn windows_sharing_denial_is_unavailable_not_absent() -> Result<(), Error> {
        use std::os::windows::fs::OpenOptionsExt;
        const ERROR_SHARING_VIOLATION: i32 = 32;
        let dir = scratch("lock-sharing")?;
        let target = dir.join("HEAD");
        std::fs::write(dir.join("ours"), b"ours")?;
        let ours = Identity::of_path(&dir.join("ours"))?;
        std::fs::write(&target, b"held")?;
        let held = Identity::of_path(&target)?;
        let holder = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&target)?;
        let observed = observe(&target, ours).ok_or(Error::Injected("held"))?;
        match &observed {
            ObservedTarget::Unavailable {
                id, raw_os_error, ..
            } => {
                assert_eq!(*raw_os_error, Some(ERROR_SHARING_VIOLATION), "{observed}");
                assert_eq!(*id, Some(held.parts()), "{observed}");
            }
            other => panic!("expected unavailable, got {other:?}"),
        }
        assert!(
            observed.to_string().starts_with("unavailable, "),
            "{observed}"
        );
        drop(holder);

        // Our own published file behind the same denial is accepted.
        let ours_path = dir.join("ours");
        let ours_holder = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&ours_path)?;
        assert_eq!(observe(&ours_path, ours), None);
        drop(ours_holder);
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

    /// The outer message asserts a mismatch only when one was observed; with
    /// an unavailable observation and no identity it says the identity could
    /// not be confirmed.
    #[test]
    fn uncertain_display_asserts_mismatch_only_when_observed() {
        let error = |observed| RenameOutcomeUncertainError {
            target: PathBuf::from("HEAD"),
            ours: (1, 2),
            observed,
        };
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);

        let unknown = error(ObservedTarget::unavailable(None, &denied)).to_string();
        assert!(
            unknown.contains("could not confirm that the target still has our identity"),
            "{unknown}"
        );
        assert!(!unknown.contains("no longer has our identity"), "{unknown}");

        let probed = error(ObservedTarget::unavailable(Some((1, 3)), &denied)).to_string();
        assert!(probed.contains("no longer has our identity"), "{probed}");

        let absent = error(ObservedTarget::Absent).to_string();
        assert!(absent.contains("no longer has our identity"), "{absent}");
    }
}
