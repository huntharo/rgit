//! Legacy backend: path-based and NOT anchored. Selected at compile time on
//! platforms without an anchored backend (Windows today); it preserves the
//! pre-hardening behaviour exactly and makes no anchoring claim.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use super::{FileId, Kind, Meta};
use crate::Error;

pub(crate) const HARDENING: super::Hardening = super::Hardening::LegacyPath;

pub(crate) struct Dir {
    path: PathBuf,
}

fn join(base: &Path, rel: &[u8]) -> Result<PathBuf, Error> {
    let mut path = base.to_owned();
    for component in rel.split(|&byte| byte == b'/') {
        if component.is_empty() || component == b"." || component == b".." {
            return Err(Error::InvalidPath(rel.to_vec()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            path.push(std::ffi::OsStr::from_bytes(component));
        }
        #[cfg(not(unix))]
        path.push(std::str::from_utf8(component).map_err(|_| Error::InvalidPath(rel.to_vec()))?);
    }
    Ok(path)
}

fn kind_of(metadata: &fs::Metadata) -> Kind {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        Kind::Symlink
    } else if file_type.is_dir() {
        Kind::Dir
    } else if file_type.is_file() {
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        Kind::File { executable }
    } else {
        Kind::Other
    }
}

/// Metadata plus identity. On Unix the identity comes from the metadata; on
/// Windows from the handle (`GetFileInformationByHandle` via winapi-util:
/// volume serial number and file index), never from size or timestamps.
fn meta_of(metadata: &fs::Metadata, id: FileId) -> Meta {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Meta {
            kind: kind_of(metadata),
            len: metadata.len(),
            id,
            mtime: (
                metadata.mtime(),
                metadata.mtime_nsec().clamp(0, 999_999_999) as u32,
            ),
            ctime: (
                metadata.ctime(),
                metadata.ctime_nsec().clamp(0, 999_999_999) as u32,
            ),
        }
    }
    #[cfg(not(unix))]
    {
        let stamp = |time: std::io::Result<std::time::SystemTime>| {
            time.ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or((0, 0), |since| {
                    (
                        i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
                        since.subsec_nanos(),
                    )
                })
        };
        Meta {
            kind: kind_of(metadata),
            len: metadata.len(),
            id,
            mtime: stamp(metadata.modified()),
            // Creation time, as the index stat data uses on Windows.
            ctime: stamp(metadata.created()),
        }
    }
}

#[cfg(unix)]
fn id_of_metadata(metadata: &fs::Metadata) -> FileId {
    use std::os::unix::fs::MetadataExt;
    FileId {
        volume: metadata.dev(),
        index: u128::from(metadata.ino()),
    }
}

/// Identity of an open handle.
fn id_of_file(file: &File) -> Result<FileId, Error> {
    #[cfg(unix)]
    {
        Ok(id_of_metadata(&file.metadata()?))
    }
    #[cfg(windows)]
    {
        let information = winapi_util::file::information(file)?;
        Ok(FileId {
            volume: information.volume_serial_number(),
            index: u128::from(information.file_index()),
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(Error::UnsupportedCapability(
            "stable file identity on this platform",
        ))
    }
}

/// `lstat` with identity, not following a final symlink or junction.
fn lstat_path(path: &Path) -> std::io::Result<Meta> {
    let metadata = fs::symlink_metadata(path)?;
    #[cfg(unix)]
    {
        Ok(meta_of(&metadata, id_of_metadata(&metadata)))
    }
    #[cfg(not(unix))]
    {
        // Open the entry itself (a reparse point is not followed; directories
        // need backup semantics) with no data access, only to read identity.
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
        let handle = options.open(path)?;
        let id = id_of_file(&handle).map_err(std::io::Error::other)?;
        Ok(meta_of(&metadata, id))
    }
}

impl Dir {
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        if !fs::metadata(path)?.is_dir() {
            return Err(Error::InvalidPath(
                path.as_os_str().as_encoded_bytes().to_vec(),
            ));
        }
        Ok(Self {
            path: path.to_owned(),
        })
    }

    pub(crate) fn display_path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn sub(&self, rel: &[u8]) -> Result<Self, Error> {
        let path = join(&self.path, rel)?;
        if !fs::symlink_metadata(&path)?.is_dir() {
            return Err(Error::InvalidPath(rel.to_vec()));
        }
        Ok(Self { path })
    }

    pub(crate) fn lstat(&self, rel: &[u8]) -> Result<Option<Meta>, Error> {
        match lstat_path(&join(&self.path, rel)?) {
            Ok(meta) => Ok(Some(meta)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn open_read(&self, rel: &[u8]) -> Result<(File, Meta), Error> {
        let file = crate::write::open_leaf_nofollow(&join(&self.path, rel)?)?;
        let meta = meta_of(&file.metadata()?, id_of_file(&file)?);
        if !matches!(meta.kind, Kind::File { .. }) {
            return Err(Error::UnsupportedCapability("not a regular file"));
        }
        Ok((file, meta))
    }

    pub(crate) fn read_link(&self, rel: &[u8]) -> Result<Vec<u8>, Error> {
        let target = fs::read_link(join(&self.path, rel)?)?;
        Ok(target.into_os_string().as_encoded_bytes().to_vec())
    }

    pub(crate) fn read_dir(&self, rel: &[u8]) -> Result<Vec<(Vec<u8>, Kind)>, Error> {
        let path = if rel.is_empty() {
            self.path.clone()
        } else {
            join(&self.path, rel)?
        };
        let mut entries = Vec::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let kind = kind_of(&fs::symlink_metadata(entry.path())?);
            entries.push((entry.file_name().as_encoded_bytes().to_vec(), kind));
        }
        Ok(entries)
    }

    pub(crate) fn create_new(&self, rel: &[u8], mode: u32) -> Result<File, Error> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        Ok(options.open(join(&self.path, rel)?)?)
    }

    pub(crate) fn mkdir(&self, rel: &[u8]) -> Result<bool, Error> {
        let path = join(&self.path, rel)?;
        match fs::create_dir(&path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if fs::symlink_metadata(&path)?.is_dir() {
                    Ok(false)
                } else {
                    Err(Error::AlreadyExists(path))
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn rename_noreplace(&self, from: &[u8], to: &[u8]) -> Result<(), Error> {
        crate::write::rename_noreplace(&join(&self.path, from)?, &join(&self.path, to)?)
    }

    pub(crate) fn rename_replace(&self, from: &[u8], to: &[u8]) -> Result<(), Error> {
        Ok(fs::rename(join(&self.path, from)?, join(&self.path, to)?)?)
    }

    pub(crate) fn hard_link(&self, from: &[u8], to: &[u8]) -> Result<(), Error> {
        let to_path = join(&self.path, to)?;
        match fs::hard_link(join(&self.path, from)?, &to_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(Error::AlreadyExists(to_path))
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn unlink(&self, rel: &[u8]) -> Result<(), Error> {
        Ok(fs::remove_file(join(&self.path, rel)?)?)
    }

    pub(crate) fn open_append(&self, rel: &[u8], mode: u32) -> Result<(File, bool), Error> {
        let path = join(&self.path, rel)?;
        let existed = fs::symlink_metadata(&path).is_ok();
        let mut options = OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        let file = options.open(&path)?;
        if !file.metadata()?.is_file() {
            return Err(Error::UnsupportedCapability("not a regular file"));
        }
        Ok((file, !existed))
    }

    pub(crate) fn rmdir(&self, rel: &[u8]) -> Result<(), Error> {
        Ok(fs::remove_dir(join(&self.path, rel)?)?)
    }

    pub(crate) fn symlink(&self, target: &[u8], rel: &[u8]) -> Result<(), Error> {
        let link = join(&self.path, rel)?;
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), link)?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (target, link);
            Err(Error::UnsupportedCapability(
                "symbolic links on this platform",
            ))
        }
    }

    pub(crate) fn sync(&self) -> Result<(), Error> {
        #[cfg(unix)]
        File::open(&self.path)?.sync_all()?;
        Ok(())
    }
}

pub(crate) fn file_id(file: &File) -> Result<FileId, Error> {
    id_of_file(file)
}
