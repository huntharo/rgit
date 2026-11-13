//! Git-compatible `<path>.lock` files with atomic publication.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::Error;

/// An exclusively created `<target>.lock`. Content is written to the lock,
/// flushed to stable storage, then renamed over the target. Dropping an
/// uncommitted lock removes it. A crash leaves the lock (as with C Git), which
/// refuses later writers until removed; the target is never partially written.
pub(crate) struct LockFile {
    target: PathBuf,
    lock_path: PathBuf,
    file: Option<File>,
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
        let lock = Self {
            target: target.to_owned(),
            lock_path,
            file: Some(file),
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
    pub(crate) fn commit(mut self) -> Result<(), Error> {
        let mut file = self.file.take().ok_or(Error::Injected("lock reused"))?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        super::failpoint("lock.before-rename")?;
        std::fs::rename(&self.lock_path, &self.target)?;
        // The lock path no longer exists; prevent Drop from touching it.
        self.lock_path = PathBuf::new();
        super::failpoint("lock.after-rename")?;
        sync_parent(&self.target)
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        if !self.lock_path.as_os_str().is_empty() {
            drop(self.file.take());
            let _ = std::fs::remove_file(&self.lock_path);
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

    #[test]
    fn lock_refuses_second_writer_and_publishes_atomically() -> Result<(), Error> {
        let dir = std::env::temp_dir().join(format!("rgit-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
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
}
