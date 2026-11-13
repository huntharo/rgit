//! Filesystem primitives for publication and worktree reads that never
//! replace an existing entry and never follow or block on a swapped leaf.
//! Only safe APIs are used (std and `rustix`); this crate forbids `unsafe`.

use std::fs::{File, OpenOptions};
use std::path::Path;

use crate::Error;

/// Rename `from` to `to` atomically, failing with [`Error::AlreadyExists`] if
/// `to` exists. There is no check-then-rename fallback: where the platform or
/// filesystem offers no atomic no-replace rename, this refuses with
/// [`Error::UnsupportedCapability`] and the caller must use another scheme
/// (for files, `hard_link`; for directories, exclusive creation).
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> Result<(), Error> {
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    {
        use rustix::fs::{CWD, RenameFlags, renameat_with};
        use rustix::io::Errno;
        match renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
            Ok(()) => Ok(()),
            Err(Errno::EXIST) => Err(Error::AlreadyExists(to.to_owned())),
            // ENOTSUP and EOPNOTSUPP are the same value on Linux.
            Err(error)
                if error == Errno::INVAL
                    || error == Errno::NOSYS
                    || error == Errno::NOTSUP
                    || error == Errno::OPNOTSUPP =>
            {
                Err(Error::UnsupportedCapability(
                    "atomic no-replace rename on this filesystem",
                ))
            }
            Err(error) => Err(std::io::Error::from(error).into()),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    {
        let _ = (from, to);
        Err(Error::UnsupportedCapability(
            "atomic no-replace rename on this platform",
        ))
    }
}

/// Open an existing worktree leaf for reading without following a final
/// symlink and without blocking on a FIFO. The caller must still validate the
/// opened descriptor (`File::metadata`) against its earlier `lstat`; parent
/// directories are resolved normally (no directory anchoring).
pub(crate) fn open_leaf_nofollow(path: &Path) -> std::io::Result<File> {
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
        // Open a reparse point (symlink/junction) itself instead of its target.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

/// Flush an existing file that another writer (or an earlier run) created,
/// then its directory. Unix syncs through a read-only descriptor. Windows
/// cannot: `FlushFileBuffers` requires write access, and loose objects are
/// read-only, so there this is a no-op and durability relies on the writer.
pub(crate) fn sync_existing_file(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    super::sync_parent(path)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn scratch(name: &str) -> Result<std::path::PathBuf, Error> {
        let dir = std::env::temp_dir().join(format!("rgit-sys-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn rename_noreplace_never_replaces_files_or_empty_directories() -> Result<(), Error> {
        let dir = scratch("rename")?;
        std::fs::write(dir.join("a"), b"ours")?;
        std::fs::write(dir.join("b"), b"theirs")?;
        assert!(matches!(
            rename_noreplace(&dir.join("a"), &dir.join("b")),
            Err(Error::AlreadyExists(_))
        ));
        assert_eq!(std::fs::read(dir.join("b"))?, b"theirs");
        assert_eq!(std::fs::read(dir.join("a"))?, b"ours");

        std::fs::create_dir(dir.join("src"))?;
        std::fs::write(dir.join("src/HEAD"), b"x")?;
        std::fs::create_dir(dir.join("empty"))?;
        assert!(matches!(
            rename_noreplace(&dir.join("src"), &dir.join("empty")),
            Err(Error::AlreadyExists(_))
        ));
        assert!(dir.join("src/HEAD").exists());
        assert!(dir.join("empty").is_dir());

        rename_noreplace(&dir.join("src"), &dir.join("dest"))?;
        assert_eq!(std::fs::read(dir.join("dest/HEAD"))?, b"x");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn leaf_open_refuses_symlinks_and_does_not_block_on_fifos() -> Result<(), Error> {
        let dir = scratch("open")?;
        std::fs::write(dir.join("file"), b"data")?;
        std::os::unix::fs::symlink(dir.join("file"), dir.join("link"))?;
        assert!(open_leaf_nofollow(&dir.join("link")).is_err());
        let file = open_leaf_nofollow(&dir.join("file"))?;
        assert!(file.metadata()?.is_file());
        // A FIFO opens immediately (nonblocking) and is then rejected by the
        // caller's descriptor validation, never by a hang.
        let status = std::process::Command::new("mkfifo")
            .arg(dir.join("fifo"))
            .status()?;
        assert!(status.success());
        let fifo = open_leaf_nofollow(&dir.join("fifo"))?;
        assert!(!fifo.metadata()?.is_file());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
