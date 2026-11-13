//! Git-compatible `<path>.lock` files with atomic publication.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::Error;

/// An exclusively created `<target>.lock`. Content is written to the lock,
/// flushed to stable storage, then renamed over the target. Dropping an
/// uncommitted lock removes it only if the path still names the file this
/// writer created, so cleanup never unlinks another writer's lock. A crash
/// leaves the lock (as with C Git), which refuses later writers until removed;
/// the target is never partially written.
pub(crate) struct LockFile {
    target: PathBuf,
    lock_path: PathBuf,
    file: Option<File>,
    identity: Identity,
    /// Set once the lock path has been renamed away (or must not be touched).
    released: bool,
}

impl LockFile {
    /// Create `<target>.lock` with `O_CREAT|O_EXCL`; an existing lock (or any
    /// entry, including a symlink, at that path) yields [`Error::Locked`].
    pub(crate) fn acquire(target: &Path) -> Result<Self, Error> {
        let mut lock_name = target
            .file_name()
            .ok_or_else(|| Error::InvalidPath(target.as_os_str().as_encoded_bytes().to_vec()))?
            .to_os_string();
        lock_name.push(".lock");
        let lock_path = target.with_file_name(lock_name);
        let file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(Error::Locked(lock_path));
            }
            Err(error) => return Err(error.into()),
        };
        let identity = Identity::of_file(&file)?;
        let lock = Self {
            target: target.to_owned(),
            lock_path,
            file: Some(file),
            identity,
            released: false,
        };
        super::failpoint("lock.acquired")?;
        Ok(lock)
    }

    pub(crate) fn lock_path(&self) -> &Path {
        &self.lock_path
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
        let mut file = self.file.take().ok_or(Error::Injected("lock reused"))?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        super::failpoint("lock.before-rename")?;
        if !self.still_ours() {
            // Someone removed or replaced our lock; never publish their file.
            self.released = true;
            return Err(Error::Locked(self.lock_path.clone()));
        }
        std::fs::rename(&self.lock_path, &self.target)?;
        self.released = true;
        super::failpoint("lock.after-rename")?;
        sync_parent(&self.target)
    }

    fn still_ours(&self) -> bool {
        Identity::of_path(&self.lock_path).is_ok_and(|identity| identity == self.identity)
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let ours = self.still_ours();
        drop(self.file.take());
        if ours {
            let _ = std::fs::remove_file(&self.lock_path);
        }
    }
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

    #[cfg(unix)]
    fn of_path(path: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path)?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    // Windows: the open handle (no FILE_SHARE_DELETE) prevents another process
    // from deleting or replacing the lock while held; std exposes no stable
    // file index, so presence of a regular file is the check.
    #[cfg(not(unix))]
    fn of_file(_file: &File) -> Result<Self, Error> {
        Ok(Self {})
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
}
