//! OWNER: WP1 (init/add core). Frozen public signatures.
//!
//! The Git directory is assembled in a private sibling directory, every file
//! and directory in it is synced, and it is published with a single rename.
//! A crash before the rename leaves only an `*.rgit-init-*` temporary
//! directory and no repository; after the rename the repository is complete.
//! No hooks, sample hooks or templates are written.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{BStr, Error, Repository};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitOptions {
    pub object_format: ObjectFormat,
    pub bare: bool,
    /// Branch name without `refs/heads/`; validated before any write.
    pub initial_branch: Vec<u8>,
}

impl Default for InitOptions {
    fn default() -> Self {
        Self {
            object_format: ObjectFormat::Sha1,
            bare: false,
            initial_branch: b"master".to_vec(),
        }
    }
}

/// Create a new repository at `directory` (created if absent). Refuses with
/// [`Error::AlreadyExists`] when a repository already exists there; does not
/// reinitialize. The Git directory is assembled privately and published with
/// one rename so an interruption never leaves a partially initialized one.
///
/// A bare repository is published by renaming over `directory`, which must be
/// absent or an empty directory. On Unix the rename atomically replaces an
/// empty directory (its permissions and ownership become those of the new
/// directory); a directory that gains entries concurrently makes the rename
/// fail and nothing is published.
pub fn init(directory: &Path, options: &InitOptions) -> Result<Repository, Error> {
    let head_ref = branch_reference(&options.initial_branch)?;
    if directory.as_os_str().is_empty() {
        return Err(Error::InvalidPath(Vec::new()));
    }
    if options.bare {
        init_bare(directory, options, &head_ref)
    } else {
        init_worktree(directory, options, &head_ref)
    }
}

/// `refs/heads/<name>` validated with `git check-ref-format` rules, as C Git
/// does for `--initial-branch` (it accepts e.g. `HEAD`, `@` and `-x`).
fn branch_reference(name: &[u8]) -> Result<Vec<u8>, Error> {
    let mut full = b"refs/heads/".to_vec();
    full.extend_from_slice(name);
    if name.is_empty() || gix::validate::reference::name(BStr::new(&full)).is_err() {
        return Err(Error::InvalidRefName(name.to_vec()));
    }
    Ok(full)
}

fn init_worktree(
    directory: &Path,
    options: &InitOptions,
    head_ref: &[u8],
) -> Result<Repository, Error> {
    let git_dir = directory.join(".git");
    // Like C Git, a symlink to a directory is followed for the worktree.
    match fs::metadata(directory) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(Error::AlreadyExists(directory.to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_directories(directory)?;
        }
        Err(error) => return Err(error.into()),
    }
    refuse_existing(&git_dir)?;
    let temporary = directory.join(format!(".git.rgit-init-{}", super::object::unique_suffix()));
    publish_git_dir(&temporary, &git_dir, options, head_ref, false)?;
    Repository::discover(directory)
}

fn init_bare(
    directory: &Path,
    options: &InitOptions,
    head_ref: &[u8],
) -> Result<Repository, Error> {
    let replace_empty = match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            // Publishing renames over the target, which would replace the
            // link itself rather than populate the directory it names.
            return Err(Error::UnsupportedCapability(
                "bare repository target is a symbolic link",
            ));
        }
        Ok(metadata) if metadata.is_dir() => {
            if fs::read_dir(directory)?.next().is_some() {
                return Err(Error::AlreadyExists(directory.to_owned()));
            }
            true
        }
        Ok(_) => return Err(Error::AlreadyExists(directory.to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let name = directory
        .file_name()
        .ok_or_else(|| Error::InvalidPath(directory.as_os_str().as_encoded_bytes().to_vec()))?;
    let parent = match directory.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_owned(),
        _ => PathBuf::from("."),
    };
    if !replace_empty {
        create_directories(&parent)?;
    }
    let mut temporary_name = std::ffi::OsString::from(".");
    temporary_name.push(name);
    temporary_name.push(format!(".rgit-init-{}", super::object::unique_suffix()));
    let temporary = parent.join(temporary_name);
    if replace_empty && !cfg!(unix) {
        // Windows cannot rename over a directory; the empty target is removed
        // first, leaving a short window in which it is absent.
        fs::remove_dir(directory)?;
    }
    publish_git_dir(&temporary, directory, options, head_ref, true)?;
    Repository::discover(directory)
}

/// A non-bare target is a repository if `.git` exists in any form.
fn refuse_existing(git_dir: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(git_dir) {
        Ok(_) => Err(Error::AlreadyExists(git_dir.to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// `create_dir_all`, then sync each newly created directory's parent.
fn create_directories(directory: &Path) -> Result<(), Error> {
    let mut missing = Vec::new();
    let mut cursor = Some(directory);
    while let Some(path) = cursor {
        if path.as_os_str().is_empty() || fs::symlink_metadata(path).is_ok() {
            break;
        }
        missing.push(path.to_owned());
        cursor = path.parent();
    }
    fs::create_dir_all(directory)?;
    for path in missing.iter().rev() {
        super::sync_parent(path)?;
    }
    Ok(())
}

fn publish_git_dir(
    temporary: &Path,
    target: &Path,
    options: &InitOptions,
    head_ref: &[u8],
    bare: bool,
) -> Result<(), Error> {
    fs::create_dir(temporary)?;
    let result = (|| {
        assemble(temporary, options, head_ref, bare)?;
        super::failpoint("init.before-publish")?;
        if !bare {
            // Narrow the window in which a concurrent writer created `.git`.
            refuse_existing(target)?;
        }
        rename_directory(temporary, target)?;
        Ok(())
    })();
    match result {
        Ok(()) => super::sync_parent(target),
        Err(error) => {
            let _ = fs::remove_dir_all(temporary);
            Err(error)
        }
    }
}

fn rename_directory(from: &Path, to: &Path) -> Result<(), Error> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        // A non-empty directory or other entry appeared at the destination.
        Err(error)
            if to.exists()
                && matches!(
                    error.kind(),
                    std::io::ErrorKind::DirectoryNotEmpty
                        | std::io::ErrorKind::AlreadyExists
                        | std::io::ErrorKind::NotADirectory
                        | std::io::ErrorKind::IsADirectory
                ) =>
        {
            Err(Error::AlreadyExists(to.to_owned()))
        }
        Err(error) => Err(error.into()),
    }
}

fn assemble(root: &Path, options: &InitOptions, head_ref: &[u8], bare: bool) -> Result<(), Error> {
    let directories = [
        "objects",
        "objects/info",
        "objects/pack",
        "refs",
        "refs/heads",
        "refs/tags",
    ];
    for directory in directories {
        fs::create_dir(root.join(directory))?;
    }
    let mut head = b"ref: ".to_vec();
    head.extend_from_slice(head_ref);
    head.push(b'\n');
    write_synced(&root.join("HEAD"), &head)?;
    let config = config_bytes(root, options, bare)?;
    write_synced(&root.join("config"), &config)?;
    for directory in directories.iter().rev() {
        sync_directory(&root.join(directory))?;
    }
    sync_directory(root)
}

/// Mirror the `[core]` settings C Git's `init` writes, including its
/// filesystem probes (executable bit, symlinks, case and, on macOS, Unicode
/// normalization), so C Git treats the repository identically.
fn config_bytes(root: &Path, options: &InitOptions, bare: bool) -> Result<Vec<u8>, Error> {
    let sha256 = options.object_format == ObjectFormat::Sha256;
    // C Git writes the extension section first for SHA-256 repositories.
    let mut config = String::new();
    if sha256 {
        config.push_str("[extensions]\n\tobjectformat = sha256\n");
    }
    config.push_str("[core]\n");
    config.push_str(&format!(
        "\trepositoryformatversion = {}\n",
        if sha256 { 1 } else { 0 }
    ));
    config.push_str(&format!("\tfilemode = {}\n", probe_filemode(root)?));
    config.push_str(&format!("\tbare = {bare}\n"));
    if !bare {
        config.push_str("\tlogallrefupdates = true\n");
    }
    if !probe_symlinks(root) {
        config.push_str("\tsymlinks = false\n");
    }
    if probe_ignorecase(root)? {
        config.push_str("\tignorecase = true\n");
    }
    #[cfg(target_os = "macos")]
    config.push_str(&format!(
        "\tprecomposeunicode = {}\n",
        probe_precompose(root)?
    ));
    Ok(config.into_bytes())
}

#[cfg(unix)]
fn probe_filemode(root: &Path) -> Result<bool, Error> {
    use std::os::unix::fs::PermissionsExt;
    let path = root.join("config");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    drop(file);
    let mode = fs::symlink_metadata(&path)?.permissions().mode();
    let toggled = mode ^ 0o100;
    let trusted = fs::set_permissions(&path, fs::Permissions::from_mode(toggled)).is_ok()
        && fs::symlink_metadata(&path)?.permissions().mode() & 0o100 == toggled & 0o100;
    fs::remove_file(&path)?;
    Ok(trusted)
}

#[cfg(not(unix))]
fn probe_filemode(_root: &Path) -> Result<bool, Error> {
    Ok(false)
}

fn probe_symlinks(root: &Path) -> bool {
    let link = root.join("tXXXXXX");
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink("testing", &link).is_ok();
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_file("testing", &link).is_ok();
    #[cfg(not(any(unix, windows)))]
    let created = false;
    if created {
        let _ = fs::remove_file(&link);
    }
    created
}

/// Git's probe: after creating `config`, can it be found as `CoNfIg`?
fn probe_ignorecase(root: &Path) -> Result<bool, Error> {
    let path = root.join("config");
    File::create(&path)?;
    let folded = fs::symlink_metadata(root.join("CoNfIg")).is_ok();
    fs::remove_file(&path)?;
    Ok(folded)
}

/// Git's probe: is a precomposed (NFC) name reachable by its NFD spelling?
#[cfg(target_os = "macos")]
fn probe_precompose(root: &Path) -> Result<bool, Error> {
    use std::os::unix::ffi::OsStrExt;
    let nfc = root.join(std::ffi::OsStr::from_bytes(b"\xc3\xa4"));
    let nfd = root.join(std::ffi::OsStr::from_bytes(b"a\xcc\x88"));
    File::create(&nfc)?;
    let reachable = fs::symlink_metadata(&nfd).is_ok();
    fs::remove_file(&nfc)?;
    Ok(reachable)
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
