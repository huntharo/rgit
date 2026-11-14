//! OWNER: WP1 (init/add core). Frozen public signatures.
//!
//! The Git directory is assembled in a private sibling directory, every file
//! and directory in it is synced, and it is published with a single rename.
//! A crash before the rename leaves only an `*.rgit-init-*` temporary
//! directory and no repository; after the rename the repository is complete.
//! No hooks, sample hooks or templates are written.
//!
//! The only path resolved by name is the nearest existing directory at or
//! above the target (followed like C Git, decision 0004); it is opened as an
//! anchor before anything is created, and every directory and file below it
//! is created and published through anchored operations.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::fsx::{Anchor, Kind, Root};
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
/// reinitialize. An interruption never leaves a directory that C Git
/// recognizes as a partially initialized repository:
///
/// - A non-bare `.git`, or a bare repository at an absent path, is assembled
///   in a private sibling directory and published with one rename.
/// - An existing empty bare target (or a symlink to one) is populated in
///   place so its inode, mode, ownership, ACLs and extended attributes are
///   kept: `objects/`, `refs/` and `config` are written and synced first and
///   `HEAD`, without which no Git recognizes a repository, is published last
///   through a no-replace link.
///
/// A failure after publication is reported as [`InitPublishedError`]: the
/// repository exists even though an error is returned.
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
    let worktree = open_or_create(directory)?;
    // A non-bare target is a repository if `.git` exists in any form.
    if worktree.lstat(b".git")?.is_some() {
        return Err(Error::AlreadyExists(directory.join(".git")));
    }
    let temporary = format!(".git.rgit-init-{}", super::object::unique_suffix()).into_bytes();
    publish_git_dir(&worktree, &temporary, b".git", options, head_ref, false)?;
    finish(directory, &directory.join(".git"))
}

fn init_bare(
    directory: &Path,
    options: &InitOptions,
    head_ref: &[u8],
) -> Result<Repository, Error> {
    // Like C Git, an existing directory (or a symlink to one) is used as is.
    if std::fs::metadata(directory).is_ok_and(|metadata| metadata.is_dir()) {
        let target = Anchor::open(directory, Root::GitDir)?;
        if !target.read_dir(b"")?.is_empty() {
            return Err(Error::AlreadyExists(directory.to_owned()));
        }
        populate_in_place(&target, options, head_ref, true, None)?;
        return finish(directory, directory);
    }
    let name = directory
        .file_name()
        .ok_or_else(|| Error::InvalidPath(directory.as_os_str().as_encoded_bytes().to_vec()))?
        .as_encoded_bytes()
        .to_vec();
    let parent_path = match directory.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_owned(),
        _ => PathBuf::from("."),
    };
    let parent = open_or_create(&parent_path)?;
    match parent.lstat(&name)? {
        None => {}
        Some(meta) if meta.kind == Kind::Symlink => {
            return Err(Error::UnsupportedCapability(
                "bare repository target is a symbolic link that does not name a directory",
            ));
        }
        Some(_) => return Err(Error::AlreadyExists(directory.to_owned())),
    }
    let mut temporary = b".".to_vec();
    temporary.extend_from_slice(&name);
    temporary
        .extend_from_slice(format!(".rgit-init-{}", super::object::unique_suffix()).as_bytes());
    publish_git_dir(&parent, &temporary, &name, options, head_ref, true)?;
    finish(directory, directory)
}

/// Open `directory` as the policy root, creating it first if absent. Only
/// the nearest existing ancestor is resolved by path (following symlinks, as
/// C Git does) and opened as an anchor; the missing components below it are
/// created through that anchor, each made durable.
fn open_or_create(directory: &Path) -> Result<Anchor, Error> {
    let mut existing = directory.to_owned();
    let mut missing: Vec<Vec<u8>> = Vec::new();
    loop {
        match std::fs::metadata(&existing) {
            Ok(metadata) if metadata.is_dir() => break,
            Ok(_) => return Err(Error::AlreadyExists(existing)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let name = existing
            .file_name()
            .ok_or_else(|| Error::InvalidPath(existing.as_os_str().as_encoded_bytes().to_vec()))?;
        missing.push(name.as_encoded_bytes().to_vec());
        existing = match existing.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_owned(),
            _ => PathBuf::from("."),
        };
    }
    let root = Anchor::open(&existing, Root::Worktree)?;
    if missing.is_empty() {
        return Ok(root);
    }
    missing.reverse();
    let mut relative = Vec::new();
    for name in &missing {
        if !relative.is_empty() {
            relative.push(b'/');
        }
        relative.extend_from_slice(name);
        if root.mkdir(&relative)? {
            sync_parent_of(&root, &relative)?;
        }
    }
    root.sub(&relative)
}

/// Sync the directory containing `rel` below `anchor`.
fn sync_parent_of(anchor: &Anchor, rel: &[u8]) -> Result<(), Error> {
    match rel.iter().rposition(|byte| *byte == b'/') {
        Some(slash) => anchor.sub(&rel[..slash])?.sync(),
        None => anchor.sync(),
    }
}

/// Open the published repository; failure here is reported as published.
fn finish(directory: &Path, git_dir: &Path) -> Result<Repository, Error> {
    Repository::discover(directory).map_err(|source| published(git_dir, "opening it", source))
}

/// Populate an empty Git directory without replacing it: an existing empty
/// bare target, or a directory this call created exclusively (`created`
/// names it below its parent anchor). Everything except `HEAD` is written
/// and synced first; until `HEAD` exists the directory is not a repository
/// for C Git or rgit. On a failure before `HEAD` is published, the entries
/// this call created (and an exclusively created directory) are removed,
/// best effort, and the original error is returned.
fn populate_in_place(
    root: &Anchor,
    options: &InitOptions,
    head_ref: &[u8],
    bare: bool,
    created: Option<(&Anchor, &[u8])>,
) -> Result<(), Error> {
    let mut made: Vec<(Vec<u8>, bool)> = Vec::new();
    let mut head_temporary = None;
    let prepared = (|| -> Result<Vec<u8>, Error> {
        for directory in DIRECTORIES {
            mkdir_new(root, directory.as_bytes())?;
            made.push((directory.as_bytes().to_vec(), true));
        }
        let config = config_bytes(root, options, bare)?;
        write_synced(root, b"config", &config)?;
        made.push((b"config".to_vec(), false));
        sync_tree(root)?;
        super::failpoint("init.before-head")?;
        let temporary = format!("HEAD.rgit-init-{}", super::object::unique_suffix()).into_bytes();
        write_synced(root, &temporary, &head_bytes(head_ref))?;
        head_temporary = Some(temporary.clone());
        Ok(temporary)
    })();
    let published_head = prepared.and_then(|temporary| {
        let linked = publish_no_replace(root, &temporary, b"HEAD")?;
        Ok(linked.then_some(temporary))
    });
    let leftover = match published_head {
        Ok(leftover) => leftover,
        Err(error) => {
            // Undo only what this call created; the target stays a plain,
            // empty directory. Cleanup failures cannot hide the cause.
            if let Some(temporary) = head_temporary {
                let _ = root.unlink(&temporary);
            }
            for (path, directory) in made.iter().rev() {
                let _ = if *directory {
                    root.rmdir(path)
                } else {
                    root.unlink(path)
                };
            }
            if let Some((parent, name)) = created {
                // Only ever an empty directory this call created exclusively.
                let _ = parent.rmdir(name);
            }
            return Err(error);
        }
    };
    // HEAD is published: from here on the repository exists.
    let shown = root.display_path().to_owned();
    if let Some(temporary) = leftover {
        root.unlink(&temporary)
            .map_err(|source| published(&shown, "removing the temporary HEAD", source))?;
    }
    root.sync()
        .map_err(|source| published(&shown, "syncing the repository", source))
}

/// Publish the complete, synced `temporary` as `name` (both below `dir`)
/// without ever replacing an existing entry. Uses a hard link (which fails
/// if `name` exists) and returns `true`, leaving `temporary` for the caller
/// to remove. Where hard links fail it uses an atomic no-replace rename and
/// returns `false` (`temporary` is gone); where that is unsupported too, it
/// refuses.
fn publish_no_replace(dir: &Anchor, temporary: &[u8], name: &[u8]) -> Result<bool, Error> {
    match dir.hard_link(temporary, name) {
        Ok(()) => Ok(true),
        Err(Error::AlreadyExists(path)) => Err(Error::AlreadyExists(path)),
        Err(_) => {
            dir.rename_noreplace(temporary, name)?;
            Ok(false)
        }
    }
}

/// The repository was created (and is visible at [`Self::git_dir`]), but a
/// step after publication failed, such as syncing its parent directory or
/// opening it. Carried in [`Error::Other`]; never produced before publication.
#[derive(Debug)]
pub struct InitPublishedError {
    git_dir: PathBuf,
    step: &'static str,
    source: Error,
}

impl InitPublishedError {
    /// The Git directory that now exists.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }
}

impl std::fmt::Display for InitPublishedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the repository was created at {}, but {} failed afterwards: {}",
            self.git_dir.display(),
            self.step,
            self.source
        )
    }
}

impl std::error::Error for InitPublishedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn published(git_dir: &Path, step: &'static str, source: Error) -> Error {
    Error::Other(Box::new(InitPublishedError {
        git_dir: git_dir.to_owned(),
        step,
        source,
    }))
}

/// Assemble the Git directory in the private `temporary` (below `parent`)
/// and publish it as `target` with an atomic no-replace rename: an entry
/// that appears at `target` meanwhile, even an empty directory, makes it
/// fail with [`Error::AlreadyExists`] and is left untouched. Where no such
/// rename exists (for example on Windows), the private directory is
/// discarded and `target` is created exclusively and populated in place with
/// `HEAD` last; there is no check-then-rename.
fn publish_git_dir(
    parent: &Anchor,
    temporary: &[u8],
    target: &[u8],
    options: &InitOptions,
    head_ref: &[u8],
    bare: bool,
) -> Result<(), Error> {
    mkdir_new(parent, temporary)?;
    let result = (|| {
        let private = parent.sub(temporary)?;
        assemble(&private, options, head_ref, bare)?;
        drop(private);
        super::failpoint("init.before-publish")?;
        parent.rename_noreplace(temporary, target)
    })();
    let shown = || shown(parent, target);
    match result {
        Ok(()) => parent
            .sync()
            .map_err(|source| published(&shown(), "syncing its parent directory", source)),
        Err(Error::UnsupportedCapability(_)) => {
            remove_tree(parent, temporary)?;
            mkdir_new(parent, target)?;
            parent.sync()?;
            let created = parent.sub(target)?;
            populate_in_place(&created, options, head_ref, bare, Some((parent, target)))
        }
        Err(error) => {
            // Only our private directory; never the entry at `target`.
            let _ = remove_tree(parent, temporary);
            Err(error)
        }
    }
}

/// Diagnostic path for messages only; never used for I/O.
fn shown(anchor: &Anchor, rel: &[u8]) -> PathBuf {
    anchor
        .display_path()
        .join(String::from_utf8_lossy(rel).as_ref())
}

/// `mkdir` that fails if anything exists at `rel`, including a directory.
fn mkdir_new(anchor: &Anchor, rel: &[u8]) -> Result<(), Error> {
    if anchor.mkdir(rel)? {
        Ok(())
    } else {
        Err(Error::AlreadyExists(shown(anchor, rel)))
    }
}

/// Remove the private directory tree `rel` this call created, through the
/// anchor and without following symlinks.
fn remove_tree(anchor: &Anchor, rel: &[u8]) -> Result<(), Error> {
    for (name, kind) in anchor.read_dir(rel)? {
        let child = [rel, b"/", &name].concat();
        if kind == Kind::Dir {
            remove_tree(anchor, &child)?;
        } else {
            anchor.unlink(&child)?;
        }
    }
    anchor.rmdir(rel)
}

const DIRECTORIES: [&str; 6] = [
    "objects",
    "objects/info",
    "objects/pack",
    "refs",
    "refs/heads",
    "refs/tags",
];

fn head_bytes(head_ref: &[u8]) -> Vec<u8> {
    let mut head = b"ref: ".to_vec();
    head.extend_from_slice(head_ref);
    head.push(b'\n');
    head
}

fn assemble(
    root: &Anchor,
    options: &InitOptions,
    head_ref: &[u8],
    bare: bool,
) -> Result<(), Error> {
    for directory in DIRECTORIES {
        mkdir_new(root, directory.as_bytes())?;
    }
    write_synced(root, b"HEAD", &head_bytes(head_ref))?;
    let config = config_bytes(root, options, bare)?;
    write_synced(root, b"config", &config)?;
    sync_tree(root)
}

/// Sync the created directories bottom-up, then the root itself.
fn sync_tree(root: &Anchor) -> Result<(), Error> {
    for directory in DIRECTORIES.iter().rev() {
        root.sub(directory.as_bytes())?.sync()?;
    }
    root.sync()
}

/// Mirror the `[core]` settings C Git's `init` writes, including its
/// filesystem probes (executable bit, symlinks, case and, on macOS, Unicode
/// normalization), so C Git treats the repository identically.
fn config_bytes(root: &Anchor, options: &InitOptions, bare: bool) -> Result<Vec<u8>, Error> {
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
    if !probe_symlinks(root)? {
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

/// Git's probe: does toggling the executable bit stick? Done on the probe
/// file's own descriptor.
#[cfg(unix)]
fn probe_filemode(root: &Anchor) -> Result<bool, Error> {
    use std::os::unix::fs::PermissionsExt;
    let file = root.create_new(b"config", 0o666)?;
    let mode = file.metadata()?.permissions().mode();
    let toggled = mode ^ 0o100;
    let trusted = file
        .set_permissions(std::fs::Permissions::from_mode(toggled))
        .is_ok()
        && file.metadata()?.permissions().mode() & 0o100 == toggled & 0o100;
    drop(file);
    root.unlink(b"config")?;
    Ok(trusted)
}

#[cfg(not(unix))]
fn probe_filemode(_root: &Anchor) -> Result<bool, Error> {
    Ok(false)
}

/// Git's probe: can a symlink be created? A probe link that cannot be
/// removed is an error rather than a stray entry in the Git directory.
fn probe_symlinks(root: &Anchor) -> Result<bool, Error> {
    // Git for Windows records `core.symlinks=false` at init unless symlinks
    // were enabled through configuration rgit does not read; match it rather
    // than probing whether this process happens to hold symlink privileges.
    if !cfg!(unix) {
        return Ok(false);
    }
    let created = root.symlink(b"testing", b"tXXXXXX").is_ok();
    if created {
        root.unlink(b"tXXXXXX")?;
    }
    Ok(created)
}

/// Git's probe: after creating `config`, can it be found as `CoNfIg`?
fn probe_ignorecase(root: &Anchor) -> Result<bool, Error> {
    drop(root.create_new(b"config", 0o666)?);
    let folded = root.lstat(b"CoNfIg")?.is_some();
    root.unlink(b"config")?;
    Ok(folded)
}

/// Git's probe: is a precomposed (NFC) name reachable by its NFD spelling?
#[cfg(target_os = "macos")]
fn probe_precompose(root: &Anchor) -> Result<bool, Error> {
    drop(root.create_new(b"\xc3\xa4", 0o666)?);
    let reachable = root.lstat(b"a\xcc\x88")?.is_some();
    root.unlink(b"\xc3\xa4")?;
    Ok(reachable)
}

fn write_synced(root: &Anchor, rel: &[u8], bytes: &[u8]) -> Result<(), Error> {
    let mut file = root.create_new(rel, 0o666)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
