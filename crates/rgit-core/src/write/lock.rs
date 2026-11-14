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
    Path {
        target: PathBuf,
        lock_path: PathBuf,
    },
    /// Constructed on Unix only (the anchored backend).
    #[cfg_attr(not(unix), allow(dead_code))]
    Anchored {
        anchor: &'a Anchor,
        target: Vec<u8>,
        lock: Vec<u8>,
    },
}

impl<'a> LockFile<'a> {
    /// Anchored form (decision 0004). On Unix: exclusively create
    /// `<rel>.lock` inside `anchor` without following symlinks (an existing
    /// entry of any kind yields [`Error::Locked`]); commit renames it over
    /// `rel` through the anchor. Elsewhere: the path form, unchanged.
    // Adopted by WP1's call sites in this batch.
    #[allow(dead_code)]
    pub(crate) fn acquire_at(anchor: &'a Anchor, rel: &[u8]) -> Result<Self, Error> {
        if rel.is_empty() || rel.ends_with(b"/") {
            return Err(Error::InvalidPath(rel.to_vec()));
        }
        #[cfg(unix)]
        {
            let mut lock = rel.to_vec();
            lock.extend_from_slice(b".lock");
            let file = match anchor.create_new(&lock, 0o666) {
                Ok(file) => file,
                Err(Error::AlreadyExists(_)) => {
                    return Err(Error::Locked(display(anchor, &lock)));
                }
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(Error::Locked(display(anchor, &lock)));
                }
                Err(error) => return Err(error),
            };
            let identity = Identity::of_file(&file)?;
            let lock = Self {
                location: Location::Anchored {
                    anchor,
                    target: rel.to_vec(),
                    lock,
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
    /// [`Error::Locked`].
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
    /// An error returned after the rename (directory sync) means the new
    /// content is already visible but its durability is unknown; callers must
    /// not report that as "unchanged".
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
        match &self.location {
            Location::Anchored {
                anchor,
                target,
                lock,
            } => {
                // The descriptor stays open across the rename.
                anchor.rename_replace(lock, target)?;
                self.released = true;
                drop(self.file.take());
                super::failpoint("lock.after-rename")?;
                anchor.sync()
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
                super::failpoint("lock.after-rename")?;
                sync_parent(target)
            }
        }
    }

    fn still_ours(&self) -> bool {
        match &self.location {
            Location::Anchored { anchor, lock, .. } => anchor.lstat(lock).is_ok_and(|meta| {
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
            Location::Anchored { anchor, lock, .. } => display(anchor, lock),
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
                Location::Anchored { anchor, lock, .. } => anchor.unlink(lock),
                Location::Path { lock_path, .. } => {
                    std::fs::remove_file(lock_path).map_err(Error::from)
                }
            };
        }
    }
}

/// Diagnostic path of `rel` below `anchor`; never used for I/O.
fn display(anchor: &Anchor, rel: &[u8]) -> PathBuf {
    anchor
        .display_path()
        .join(String::from_utf8_lossy(rel).as_ref())
}

/// File identity used to confirm the lock path still names our lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
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

    // Windows: `acquire` opens the lock without FILE_SHARE_DELETE, so no other
    // process can delete or replace it while the handle is open; std exposes
    // no stable file index, so presence of a regular file is the check. The
    // handle is closed immediately before the rename (Windows requires it),
    // leaving a short window that this check cannot cover.
    #[cfg(not(unix))]
    fn of_file(_file: &File) -> Result<Self, Error> {
        Ok(Self {})
    }

    #[cfg(not(unix))]
    fn of_id(_id: crate::fsx::FileId) -> Option<Self> {
        None
    }

    #[cfg(not(unix))]
    fn of_path(path: &Path) -> std::io::Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.is_file() {
            Ok(Self {})
        } else {
            Err(std::io::ErrorKind::NotFound.into())
        }
    }
}

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
}
