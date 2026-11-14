//! OWNER: WP1. Descriptor-relative backend for Unix (decision 0004).
//!
//! A [`Dir`] holds an open directory descriptor. Every relative path is
//! resolved one component at a time with `openat(O_NOFOLLOW|O_DIRECTORY)`
//! from that descriptor, and the final component is operated on with the
//! `*at` call relative to its parent's descriptor without following it. A
//! path is never re-joined into an ambient pathname for I/O, so swapping an
//! ancestor directory for a symlink after the anchor was opened cannot
//! redirect an operation: the swapped component is refused (`ELOOP`/
//! `ENOTDIR`). I/O stays within the directory identity opened as each root
//! ("descriptor-root identity"); a directory that is moved elsewhere while a
//! descriptor on it is held is still that same directory. This is not a
//! general sandbox (see decision 0004 for the threat model).

use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;

use super::{FileId, Kind, Meta};
use crate::Error;

/// This backend anchors every operation (decision 0004).
pub(crate) const HARDENING: super::Hardening = super::Hardening::Anchored;

pub(crate) struct Dir {
    fd: OwnedFd,
    path: PathBuf,
}

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Split `rel` into components, refusing empty, `.` and `..` components and
/// NUL bytes. The empty path is not a valid entry name.
fn components(rel: &[u8]) -> Result<Vec<&[u8]>, Error> {
    if rel.is_empty() || rel.contains(&0) {
        return Err(Error::InvalidPath(rel.to_vec()));
    }
    let parts: Vec<&[u8]> = rel.split(|&byte| byte == b'/').collect();
    if parts
        .iter()
        .any(|part| part.is_empty() || *part == b"." || *part == b"..")
    {
        return Err(Error::InvalidPath(rel.to_vec()));
    }
    Ok(parts)
}

/// Diagnostic path only (error messages); never used for I/O.
fn shown(base: &Path, rel: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    base.join(std::ffi::OsStr::from_bytes(rel))
}

fn io(error: Errno) -> Error {
    Error::Io(error.into())
}

/// The descriptor of an entry's parent directory plus its final name.
struct Parent<'a> {
    owned: Option<OwnedFd>,
    base: BorrowedFd<'a>,
    name: &'a [u8],
}

impl Parent<'_> {
    fn fd(&self) -> BorrowedFd<'_> {
        self.owned.as_ref().map_or(self.base, |fd| fd.as_fd())
    }
}

#[allow(clippy::unnecessary_cast)] // Field widths differ between Unix targets.
fn meta_of(stat: &rustix::fs::Stat) -> Meta {
    let mode = stat.st_mode as u32;
    let kind = match FileType::from_raw_mode(stat.st_mode as rustix::fs::RawMode) {
        FileType::RegularFile => Kind::File {
            executable: mode & 0o111 != 0,
        },
        FileType::Directory => Kind::Dir,
        FileType::Symlink => Kind::Symlink,
        _ => Kind::Other,
    };
    Meta {
        kind,
        len: stat.st_size as u64,
        id: FileId {
            volume: stat.st_dev as u64,
            index: u128::from(stat.st_ino as u64),
        },
        mtime: (
            stat.st_mtime as i64,
            (stat.st_mtime_nsec as i64).clamp(0, 999_999_999) as u32,
        ),
        ctime: (
            stat.st_ctime as i64,
            (stat.st_ctime_nsec as i64).clamp(0, 999_999_999) as u32,
        ),
    }
}

impl Dir {
    /// Open the policy root. The root path itself is resolved by the kernel
    /// (following symlinks, as C Git does); nothing below it is.
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io)?;
        Ok(Self {
            fd,
            path: path.to_owned(),
        })
    }

    pub(crate) fn display_path(&self) -> &Path {
        &self.path
    }

    /// Open each of `parts` as a directory, never following a symlink.
    fn descend(&self, parts: &[&[u8]]) -> Result<Option<OwnedFd>, Error> {
        let mut current: Option<OwnedFd> = None;
        for part in parts {
            let base = current.as_ref().map_or(self.fd.as_fd(), |fd| fd.as_fd());
            let next = rustix::fs::openat(base, *part, DIRECTORY, Mode::empty()).map_err(io)?;
            current = Some(next);
        }
        Ok(current)
    }

    fn parent<'a>(&'a self, rel: &'a [u8]) -> Result<Parent<'a>, Error> {
        let parts = components(rel)?;
        let (name, leading) = parts
            .split_last()
            .ok_or_else(|| Error::InvalidPath(rel.to_vec()))?;
        Ok(Parent {
            owned: self.descend(leading)?,
            base: self.fd.as_fd(),
            name,
        })
    }

    pub(crate) fn sub(&self, rel: &[u8]) -> Result<Self, Error> {
        let parts = components(rel)?;
        let fd = self
            .descend(&parts)?
            .ok_or_else(|| Error::InvalidPath(rel.to_vec()))?;
        Ok(Self {
            fd,
            path: shown(&self.path, rel),
        })
    }

    pub(crate) fn lstat(&self, rel: &[u8]) -> Result<Option<Meta>, Error> {
        let parent = match self.parent(rel) {
            Ok(parent) => parent,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        match rustix::fs::statat(parent.fd(), parent.name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(Some(meta_of(&stat))),
            Err(Errno::NOENT) => Ok(None),
            Err(error) => Err(io(error)),
        }
    }

    pub(crate) fn open_read(&self, rel: &[u8]) -> Result<(File, Meta), Error> {
        let parent = self.parent(rel)?;
        let fd = rustix::fs::openat(
            parent.fd(),
            parent.name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io)?;
        let meta = meta_of(&rustix::fs::fstat(&fd).map_err(io)?);
        if !matches!(meta.kind, Kind::File { .. }) {
            return Err(Error::UnsupportedCapability("not a regular file"));
        }
        Ok((File::from(fd), meta))
    }

    pub(crate) fn read_link(&self, rel: &[u8]) -> Result<Vec<u8>, Error> {
        let parent = self.parent(rel)?;
        let target = rustix::fs::readlinkat(parent.fd(), parent.name, Vec::new()).map_err(io)?;
        Ok(target.into_bytes())
    }

    pub(crate) fn read_dir(&self, rel: &[u8]) -> Result<Vec<(Vec<u8>, Kind)>, Error> {
        let held;
        let fd = if rel.is_empty() {
            self.fd.as_fd()
        } else {
            held = self.sub(rel)?;
            held.fd.as_fd()
        };
        let mut entries = Vec::new();
        for entry in rustix::fs::Dir::read_from(fd).map_err(io)? {
            let entry = entry.map_err(io)?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            // Always stat (no-follow): d_type may be unknown and lacks the
            // executable bit.
            let kind = match rustix::fs::statat(fd, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => meta_of(&stat).kind,
                // Removed since the directory was read.
                Err(Errno::NOENT) => continue,
                Err(error) => return Err(io(error)),
            };
            entries.push((name.to_vec(), kind));
        }
        Ok(entries)
    }

    pub(crate) fn create_new(&self, rel: &[u8], mode: u32) -> Result<File, Error> {
        let parent = self.parent(rel)?;
        match rustix::fs::openat(
            parent.fd(),
            parent.name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(mode as rustix::fs::RawMode),
        ) {
            Ok(fd) => Ok(File::from(fd)),
            Err(Errno::EXIST) => Err(Error::AlreadyExists(shown(&self.path, rel))),
            Err(error) => Err(io(error)),
        }
    }

    /// Open a regular file for appending, creating it with `mode` if absent;
    /// returns whether it was created. Never follows the final component,
    /// never blocks, and refuses anything that is not a regular file.
    pub(crate) fn open_append(&self, rel: &[u8], mode: u32) -> Result<(File, bool), Error> {
        let parent = self.parent(rel)?;
        let flags =
            OFlags::WRONLY | OFlags::APPEND | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let mut created = false;
        let fd = match rustix::fs::openat(parent.fd(), parent.name, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => {
                match rustix::fs::openat(
                    parent.fd(),
                    parent.name,
                    flags | OFlags::CREATE | OFlags::EXCL,
                    Mode::from_raw_mode(mode as rustix::fs::RawMode),
                ) {
                    Ok(fd) => {
                        created = true;
                        fd
                    }
                    // Created concurrently: append to it like any other.
                    Err(Errno::EXIST) => {
                        rustix::fs::openat(parent.fd(), parent.name, flags, Mode::empty())
                            .map_err(io)?
                    }
                    Err(error) => return Err(io(error)),
                }
            }
            Err(error) => return Err(io(error)),
        };
        let meta = meta_of(&rustix::fs::fstat(&fd).map_err(io)?);
        if !matches!(meta.kind, Kind::File { .. }) {
            return Err(Error::UnsupportedCapability("not a regular file"));
        }
        Ok((File::from(fd), created))
    }

    pub(crate) fn mkdir(&self, rel: &[u8]) -> Result<bool, Error> {
        let parent = self.parent(rel)?;
        match rustix::fs::mkdirat(parent.fd(), parent.name, Mode::from_raw_mode(0o777)) {
            Ok(()) => Ok(true),
            Err(Errno::EXIST) => {
                match rustix::fs::statat(parent.fd(), parent.name, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(stat) if meta_of(&stat).kind == Kind::Dir => Ok(false),
                    Ok(_) => Err(Error::AlreadyExists(shown(&self.path, rel))),
                    Err(error) => Err(io(error)),
                }
            }
            Err(error) => Err(io(error)),
        }
    }

    pub(crate) fn rmdir(&self, rel: &[u8]) -> Result<(), Error> {
        let parent = self.parent(rel)?;
        rustix::fs::unlinkat(parent.fd(), parent.name, AtFlags::REMOVEDIR).map_err(io)
    }

    pub(crate) fn symlink(&self, target: &[u8], rel: &[u8]) -> Result<(), Error> {
        let parent = self.parent(rel)?;
        match rustix::fs::symlinkat(target, parent.fd(), parent.name) {
            Ok(()) => Ok(()),
            Err(Errno::EXIST) => Err(Error::AlreadyExists(shown(&self.path, rel))),
            Err(error) => Err(io(error)),
        }
    }

    pub(crate) fn rename_noreplace(&self, from: &[u8], to: &[u8]) -> Result<(), Error> {
        let source = self.parent(from)?;
        let destination = self.parent(to)?;
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
        {
            use rustix::fs::{RenameFlags, renameat_with};
            match renameat_with(
                source.fd(),
                source.name,
                destination.fd(),
                destination.name,
                RenameFlags::NOREPLACE,
            ) {
                Ok(()) => Ok(()),
                Err(Errno::EXIST) => Err(Error::AlreadyExists(shown(&self.path, to))),
                // NOTSUP and OPNOTSUPP are the same value on some targets.
                Err(error)
                    if [Errno::INVAL, Errno::NOSYS, Errno::NOTSUP, Errno::OPNOTSUPP]
                        .contains(&error) =>
                {
                    Err(Error::UnsupportedCapability(
                        "atomic no-replace rename on this filesystem",
                    ))
                }
                Err(error) => Err(io(error)),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
        {
            let _ = (source, destination);
            Err(Error::UnsupportedCapability(
                "atomic no-replace rename on this platform",
            ))
        }
    }

    pub(crate) fn rename_replace(&self, from: &[u8], to: &[u8]) -> Result<(), Error> {
        let source = self.parent(from)?;
        let destination = self.parent(to)?;
        rustix::fs::renameat(source.fd(), source.name, destination.fd(), destination.name)
            .map_err(io)
    }

    pub(crate) fn hard_link(&self, from: &[u8], to: &[u8]) -> Result<(), Error> {
        let source = self.parent(from)?;
        let destination = self.parent(to)?;
        // Without AT_SYMLINK_FOLLOW a symlink at `from` is linked, not followed.
        match rustix::fs::linkat(
            source.fd(),
            source.name,
            destination.fd(),
            destination.name,
            AtFlags::empty(),
        ) {
            Ok(()) => Ok(()),
            Err(Errno::EXIST) => Err(Error::AlreadyExists(shown(&self.path, to))),
            Err(error) => Err(io(error)),
        }
    }

    pub(crate) fn unlink(&self, rel: &[u8]) -> Result<(), Error> {
        let parent = self.parent(rel)?;
        rustix::fs::unlinkat(parent.fd(), parent.name, AtFlags::empty()).map_err(io)
    }

    pub(crate) fn sync(&self) -> Result<(), Error> {
        rustix::fs::fsync(&self.fd).map_err(io)
    }

    /// Identity of the directory this descriptor holds, read with `fstat` so
    /// it does not depend on any path still naming it.
    pub(crate) fn id(&self) -> Result<FileId, Error> {
        Ok(meta_of(&rustix::fs::fstat(&self.fd).map_err(io)?).id)
    }
}

pub(crate) fn file_id(file: &File) -> Result<FileId, Error> {
    Ok(meta_of(&rustix::fs::fstat(file).map_err(io)?).id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> Result<PathBuf, Error> {
        let dir = std::env::temp_dir().join(format!("rgit-fsx-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    #[test]
    fn components_refuse_traversal_and_empty_parts() {
        for bad in [
            &b""[..],
            b".",
            b"..",
            b"a/../b",
            b"a//b",
            b"/a",
            b"a/",
            b"a\0b",
        ] {
            assert!(
                matches!(components(bad), Err(Error::InvalidPath(_))),
                "{bad:?}"
            );
        }
        assert!(components(b"a/b").is_ok());
    }

    #[test]
    fn symlinked_ancestors_are_never_followed() -> Result<(), Error> {
        let root = scratch("ancestors")?;
        let outside = scratch("ancestors-outside")?;
        std::fs::create_dir(root.join("real"))?;
        std::fs::write(outside.join("file"), b"outside")?;
        std::os::unix::fs::symlink(&outside, root.join("link"))?;
        let dir = Dir::open(&root)?;
        assert!(dir.sub(b"link").is_err());
        assert!(dir.lstat(b"link/file").is_err());
        assert!(dir.open_read(b"link/file").is_err());
        assert!(dir.create_new(b"link/new", 0o644).is_err());
        assert!(dir.mkdir(b"link/new").is_err());
        // A symlink to a directory is not "a directory already exists".
        assert!(matches!(dir.mkdir(b"link"), Err(Error::AlreadyExists(_))));
        assert!(dir.open_append(b"link/file", 0o644).is_err());
        assert!(matches!(
            dir.lstat(b"link")?.map(|meta| meta.kind),
            Some(Kind::Symlink)
        ));
        assert_eq!(std::fs::read(outside.join("file"))?, b"outside");
        assert!(!outside.join("new").exists());
        std::fs::remove_dir_all(&root)?;
        std::fs::remove_dir_all(&outside)?;
        Ok(())
    }

    #[test]
    fn a_held_directory_keeps_its_identity_after_a_swap() -> Result<(), Error> {
        let root = scratch("held")?;
        let outside = scratch("held-outside")?;
        std::fs::create_dir(root.join("sub"))?;
        let dir = Dir::open(&root)?;
        let sub = dir.sub(b"sub")?;
        std::fs::rename(root.join("sub"), root.join("moved"))?;
        std::os::unix::fs::symlink(&outside, root.join("sub"))?;
        drop(sub.create_new(b"file", 0o644)?);
        assert!(root.join("moved/file").exists());
        assert!(std::fs::read_dir(&outside)?.next().is_none());
        std::fs::remove_dir_all(&root)?;
        std::fs::remove_dir_all(&outside)?;
        Ok(())
    }

    #[test]
    fn a_held_directory_id_follows_the_descriptor_not_the_name() -> Result<(), Error> {
        let root = scratch("held-id")?;
        std::fs::create_dir(root.join("sub"))?;
        let dir = Dir::open(&root)?;
        let sub = dir.sub(b"sub")?;
        let before = sub.id()?;
        assert_eq!(dir.lstat(b"sub")?.map(|meta| meta.id), Some(before));
        std::fs::rename(root.join("sub"), root.join("moved"))?;
        std::fs::create_dir(root.join("sub"))?;
        assert_eq!(sub.id()?, before);
        assert_eq!(dir.lstat(b"moved")?.map(|meta| meta.id), Some(before));
        let replacement = dir.sub(b"sub")?.id()?;
        assert_ne!(replacement, before);
        assert_eq!(dir.lstat(b"sub")?.map(|meta| meta.id), Some(replacement));
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn leaves_are_not_followed_and_fifos_do_not_block() -> Result<(), Error> {
        let root = scratch("leaves")?;
        std::fs::write(root.join("file"), b"data")?;
        std::os::unix::fs::symlink(root.join("file"), root.join("link"))?;
        assert!(
            std::process::Command::new("mkfifo")
                .arg(root.join("fifo"))
                .status()?
                .success()
        );
        let dir = Dir::open(&root)?;
        let (_, meta) = dir.open_read(b"file")?;
        assert_eq!(meta.len, 4);
        assert!(dir.open_read(b"link").is_err());
        assert!(matches!(
            dir.open_read(b"fifo"),
            Err(Error::UnsupportedCapability(_))
        ));
        assert!(dir.open_append(b"fifo", 0o644).is_err());
        assert!(dir.open_append(b"link", 0o644).is_err());
        assert_eq!(
            dir.read_link(b"link")?,
            root.join("file").as_os_str().as_encoded_bytes()
        );
        let (_, created) = dir.open_append(b"log", 0o644)?;
        assert!(created);
        let (_, created) = dir.open_append(b"log", 0o644)?;
        assert!(!created);
        assert!(matches!(
            dir.create_new(b"link", 0o644),
            Err(Error::AlreadyExists(_))
        ));
        let mut names: Vec<_> = dir
            .read_dir(b"")?
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        names.sort();
        assert_eq!(names, [&b"fifo"[..], b"file", b"link", b"log"]);
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn publication_never_replaces() -> Result<(), Error> {
        let root = scratch("publish")?;
        std::fs::write(root.join("a"), b"a")?;
        std::fs::write(root.join("b"), b"b")?;
        std::fs::create_dir(root.join("empty"))?;
        std::fs::create_dir(root.join("full"))?;
        std::fs::write(root.join("full/x"), b"x")?;
        let dir = Dir::open(&root)?;
        assert!(matches!(
            dir.rename_noreplace(b"a", b"b"),
            Err(Error::AlreadyExists(_))
        ));
        assert!(matches!(
            dir.rename_noreplace(b"full", b"empty"),
            Err(Error::AlreadyExists(_))
        ));
        assert!(matches!(
            dir.hard_link(b"a", b"b"),
            Err(Error::AlreadyExists(_))
        ));
        assert_eq!(std::fs::read(root.join("b"))?, b"b");
        dir.hard_link(b"a", b"c")?;
        dir.rename_noreplace(b"c", b"d")?;
        dir.unlink(b"d")?;
        assert!(!dir.mkdir(b"empty")?);
        assert!(dir.mkdir(b"made")?);
        dir.rmdir(b"made")?;
        dir.symlink(b"target", b"sym")?;
        assert_eq!(dir.read_link(b"sym")?, b"target");
        dir.sync()?;
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }
}
