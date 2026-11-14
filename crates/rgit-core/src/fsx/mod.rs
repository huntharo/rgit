//! Anchored filesystem access for write operations (PWR-194).
//!
//! Every write-path filesystem operation is expressed relative to an
//! [`Anchor`]: an open handle on one of three policy roots (the worktree, the
//! per-worktree Git directory, the common Git directory). Relative paths are
//! `/`-separated native bytes; each component is resolved without following
//! symbolic links, so a parent directory swapped for a symlink after the
//! anchor was opened cannot redirect I/O outside its root.
//!
//! Ownership (decision 0004):
//! - this file and `portable.rs`: integration owner (frozen API, baseline);
//! - `unix.rs`: WP1 (descriptor-relative implementation via rustix);
//! - `windows.rs`: WP2 (handle-based identity and publication).
//!
//! The baseline `portable` backend is path-based and NOT anchored; it exists
//! so integration can proceed and is replaced per platform. A backend that
//! cannot provide an operation's guarantee returns
//! [`Error::UnsupportedCapability`] rather than a weaker fallback.

#![allow(dead_code)] // Removed once call sites adopt the API in this batch.

use std::fs::File;
use std::path::Path;

use crate::Error;

mod portable;
use portable as backend;

/// Which policy root an anchor represents (for diagnostics and policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Root {
    Worktree,
    GitDir,
    CommonDir,
}

/// Stable identity of a file: `(dev, ino)` on Unix, `(volume serial, file
/// index)` on Windows. Never size or timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FileId {
    pub(crate) volume: u64,
    pub(crate) index: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    File {
        executable: bool,
    },
    Symlink,
    Dir,
    /// FIFO, socket, device or anything else; never opened for content.
    Other,
}

/// Metadata of an entry, observed without following a final symlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Meta {
    pub(crate) kind: Kind,
    pub(crate) len: u64,
    pub(crate) id: FileId,
    /// `(seconds, nanoseconds)`; ctime is the status-change time on Unix and
    /// the creation time on Windows.
    pub(crate) mtime: (i64, u32),
    pub(crate) ctime: (i64, u32),
}

/// An open directory within one policy root.
pub(crate) struct Anchor {
    root: Root,
    inner: backend::Dir,
}

impl Anchor {
    /// Open `path` as the anchor for `root`. The root path itself is resolved
    /// once (it may legitimately be reached through a symlink, as C Git
    /// allows); everything below it is resolved through the handle.
    pub(crate) fn open(path: &Path, root: Root) -> Result<Self, Error> {
        Ok(Self {
            root,
            inner: backend::Dir::open(path)?,
        })
    }

    pub(crate) fn root(&self) -> Root {
        self.root
    }

    /// Diagnostic path of this anchor; never used for I/O.
    pub(crate) fn display_path(&self) -> &Path {
        self.inner.display_path()
    }

    /// Descend to a subdirectory without following symlinks. A missing,
    /// non-directory or symlinked component is an error.
    pub(crate) fn sub(&self, rel: &[u8]) -> Result<Self, Error> {
        Ok(Self {
            root: self.root,
            inner: self.inner.sub(rel)?,
        })
    }

    /// `lstat` relative to the anchor; `None` when absent.
    pub(crate) fn lstat(&self, rel: &[u8]) -> Result<Option<Meta>, Error> {
        self.inner.lstat(rel)
    }

    /// Open a regular file for reading: final component not followed, FIFOs
    /// never block, and the returned [`Meta`] describes the opened handle.
    /// Anything but a regular file is refused.
    pub(crate) fn open_read(&self, rel: &[u8]) -> Result<(File, Meta), Error> {
        self.inner.open_read(rel)
    }

    pub(crate) fn read_link(&self, rel: &[u8]) -> Result<Vec<u8>, Error> {
        self.inner.read_link(rel)
    }

    /// Entries of a directory (excluding `.`/`..`), not following symlinks.
    pub(crate) fn read_dir(&self, rel: &[u8]) -> Result<Vec<(Vec<u8>, Kind)>, Error> {
        self.inner.read_dir(rel)
    }

    /// Create a new file exclusively (fails if anything exists, including a
    /// dangling symlink) with Unix permission bits `mode`.
    pub(crate) fn create_new(&self, rel: &[u8], mode: u32) -> Result<File, Error> {
        self.inner.create_new(rel, mode)
    }

    /// Create a directory; `Ok(false)` if a directory already exists there.
    pub(crate) fn mkdir(&self, rel: &[u8]) -> Result<bool, Error> {
        self.inner.mkdir(rel)
    }

    /// Atomic rename that never replaces an existing entry
    /// ([`Error::AlreadyExists`]) or refuses ([`Error::UnsupportedCapability`]).
    pub(crate) fn rename_noreplace(&self, from: &[u8], to: &[u8]) -> Result<(), Error> {
        self.inner.rename_noreplace(from, to)
    }

    /// Atomic rename replacing `to` (lock publication only).
    pub(crate) fn rename_replace(&self, from: &[u8], to: &[u8]) -> Result<(), Error> {
        self.inner.rename_replace(from, to)
    }

    /// Hard link `to` to `from`; never replaces an existing `to`.
    pub(crate) fn hard_link(&self, from: &[u8], to: &[u8]) -> Result<(), Error> {
        self.inner.hard_link(from, to)
    }

    pub(crate) fn unlink(&self, rel: &[u8]) -> Result<(), Error> {
        self.inner.unlink(rel)
    }

    /// Flush the anchored directory's entries (no-op where unsupported; the
    /// limitation is documented per platform).
    pub(crate) fn sync(&self) -> Result<(), Error> {
        self.inner.sync()
    }
}

/// Identity of an open file handle.
pub(crate) fn file_id(file: &File) -> Result<FileId, Error> {
    backend::file_id(file)
}
