//! OWNER: WP2. Compare-and-swap reference publication.
//!
//! Loose references are replaced through `<ref>.lock` and a rename; the value
//! the expectation is compared with is read while that lock is held, from the
//! loose file or, if absent, from `packed-refs`. `packed-refs` is never
//! rewritten here: a new loose file shadows a packed entry, as in C Git.
//! As in C Git, reflog lines are appended (one write + fsync each) while the
//! locks are held, after the compare-and-swap check and before the rename,
//! so concurrent writers log in publication order. An update logged for a
//! ref other than `HEAD` also holds `HEAD.lock` (taken first) so `HEAD`
//! cannot be retargeted while deciding whether its log gets the line. A
//! crash between the append and the rename leaves a reflog line whose new
//! value never became the ref (see docs/decisions/0003).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{BStr, Error, ObjectId, ObjectKind, Repository};

use super::{LockFile, Signature, sync_parent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefExpectation {
    Any,
    MustNotExist,
    MustEqual(ObjectId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefLog {
    pub committer: Signature,
    pub message: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    /// Full name, e.g. `refs/heads/main`, or `HEAD` for a detached update.
    pub name: Vec<u8>,
    pub new: ObjectId,
    pub expected: RefExpectation,
    pub log: Option<RefLog>,
}

impl Repository {
    /// Verify `new` exists (and is a commit for `refs/heads/*`), then publish
    /// it only if the current value matches `expected`.
    ///
    /// `HEAD` may be updated only while detached; a symbolic `HEAD` is refused
    /// (update its branch instead). Symbolic or symlinked loose references are
    /// refused. When `log` is given, `HEAD.lock` is held as well (a held
    /// `HEAD.lock` refuses with [`Error::Locked`], as in C Git), and a reflog
    /// line is appended for the reference (and for this worktree's `HEAD`
    /// when, under `HEAD.lock`, it names the reference) as
    /// `core.logAllRefUpdates` dictates, before the rename publishes the
    /// value. See [`ReflogAppendError`], [`RefLoggedNotPublishedError`] and
    /// [`RefPublishedUnsyncedError`] for failures around publication.
    pub fn update_ref(&self, update: &RefUpdate) -> Result<(), Error> {
        self.publish_ref(
            update,
            PublishOptions {
                locked: "ref.locked",
                head_must_name_ref: false,
            },
        )
    }

    /// Shared CAS publication, in C Git's order:
    ///
    /// 1. Validate the name and the object (present, a commit for HEAD and
    ///    branches, loose copy synced) and preflight paths; nothing written.
    /// 2. When a reflog is requested for a ref other than `HEAD`, take
    ///    `HEAD.lock` first and decide, while holding it, whether this
    ///    worktree's `HEAD` names the ref (its log then gets a line too).
    ///    `HEAD.lock` is released without touching `HEAD`.
    /// 3. Take `<ref>.lock`, read the current value (loose, else packed) and
    ///    compare it with the expectation; failpoint `options.locked`.
    /// 4. Append every reflog line (one write + fsync each) while holding the
    ///    locks; failpoint `ref.logged`.
    /// 5. Rename the lock into place and sync the directory.
    ///
    /// A failure or crash after step 4 but before the rename leaves the ref
    /// unchanged and its reflog with a line whose new value never became the
    /// ref, as in C Git; the object exists, so fsck accepts it.
    pub(super) fn publish_ref(
        &self,
        update: &RefUpdate,
        options: PublishOptions,
    ) -> Result<(), Error> {
        let location = self.ref_location(&update.name)?;
        self.check_object_id(update.new)?;
        // Objects before names: refuse before taking any lock.
        if update.name == b"HEAD" || update.name.starts_with(b"refs/heads/") {
            self.require_kind(update.new, ObjectKind::Commit)?;
        } else {
            self.require_present(update.new)?;
        }
        // The object may have been written by someone who did not sync it.
        self.sync_loose_object(update.new)?;
        if let Some(log) = &update.log {
            super::commit::validate_signature(&log.committer)?;
        }
        // Pre-checks so no directory is created for a conflicting name; the
        // authoritative checks are repeated under the locks.
        self.preflight_location(&location, update.log.is_some())?;
        create_ref_dirs(&location.base, &location.path)?;

        let hash = self.inner.object_hash();
        let head_path = self.inner.git_dir().join("HEAD");
        let needs_head_lock =
            update.name != b"HEAD" && (update.log.is_some() || options.head_must_name_ref);
        let head_lock = if needs_head_lock {
            Some(LockFile::acquire(&head_path)?)
        } else {
            None
        };
        let head_names_ref = match &head_lock {
            Some(_) => {
                matches!(read_loose(&head_path, hash)?, Some(Loose::Symbolic(target)) if target == update.name)
            }
            None => false,
        };
        if options.head_must_name_ref && !head_names_ref {
            return Err(Error::RefConflict {
                name: b"HEAD".to_vec(),
                expected: match update.expected {
                    RefExpectation::MustEqual(id) => Some(id),
                    _ => None,
                },
                actual: match read_loose(&head_path, hash)? {
                    Some(Loose::Direct(id)) => Some(id),
                    _ => None,
                },
            });
        }

        let mut lock = LockFile::acquire(&location.path)?;
        let lock_identity = LockIdentity::capture(&location.path)?;
        let current = match read_loose(&location.path, hash)? {
            Some(Loose::Direct(id)) => Some(id),
            Some(Loose::Symbolic(_)) if update.name == b"HEAD" => {
                return Err(Error::UnsupportedCapability(
                    "updating a symbolic HEAD; update the branch it names",
                ));
            }
            Some(Loose::Symbolic(_)) => {
                return Err(Error::UnsupportedCapability(
                    "updating a symbolic reference",
                ));
            }
            None => {
                self.check_directory_conflicts(&location)?;
                if location.shared {
                    self.packed_refs()?.lookup(&update.name)
                } else {
                    None
                }
            }
        };
        let matches = match update.expected {
            RefExpectation::Any => true,
            RefExpectation::MustNotExist => current.is_none(),
            RefExpectation::MustEqual(id) => current == Some(id),
        };
        if !matches {
            return Err(Error::RefConflict {
                name: update.name.clone(),
                expected: match update.expected {
                    RefExpectation::MustEqual(id) => Some(id),
                    _ => None,
                },
                actual: current,
            });
        }
        if current == Some(update.new) {
            // Like C Git: an update to the current value writes nothing and
            // appends no reflog entry.
            return Ok(());
        }
        super::failpoint(options.locked)?;
        let content = format!("{}\n", update.new.to_hex());
        lock.write_all(content.as_bytes())?;

        // Reflogs are appended while the locks are held, so concurrent
        // writers append in publication order and HEAD's log follows the
        // HEAD target observed under HEAD.lock. A lock removed or replaced by
        // someone else is refused before anything is logged (the rename
        // re-checks it).
        if !lock_identity.still_held(&location.path) {
            return Err(Error::Locked(LockIdentity::lock_path(&location.path)));
        }
        let mut logged = false;
        if let Some(log) = &update.log {
            let old = current.unwrap_or_else(|| ObjectId::null(hash));
            let line = reflog_line(old, update.new, log);
            for (base, path) in self.reflog_targets(&location, head_names_ref)? {
                append_reflog(&base, &path, &line).map_err(|source| {
                    Error::Other(Box::new(ReflogAppendError {
                        reference: update.name.clone(),
                        source,
                        partially_logged: logged,
                    }))
                })?;
                logged = true;
            }
        }
        if let Err(source) = super::failpoint("ref.logged") {
            return Err(if logged {
                Error::Other(Box::new(RefLoggedNotPublishedError {
                    reference: update.name.clone(),
                    source,
                }))
            } else {
                source
            });
        }

        let result = match lock.commit() {
            Ok(()) => Ok(()),
            // Uncertain post-rename outcome (Windows legacy lock): the rename
            // returned success but the target is no longer our file. Passed
            // through as is, whether or not a reflog was appended; it is
            // neither "updated" nor "not updated".
            Err(source @ Error::Other(_))
                if matches!(&source, Error::Other(error)
                    if error.is::<super::RenameOutcomeUncertainError>()) =>
            {
                Err(source)
            }
            // Renamed: visible, durability unknown; reflogs already appended.
            Err(source) if lock_identity.renamed(&location.path, content.as_bytes()) => {
                Err(Error::Other(Box::new(RefPublishedUnsyncedError {
                    reference: update.name.clone(),
                    source,
                })))
            }
            // Not renamed: the ref is unchanged, but its reflog says otherwise.
            Err(source) if logged => Err(Error::Other(Box::new(RefLoggedNotPublishedError {
                reference: update.name.clone(),
                source,
            }))),
            Err(source) => Err(source),
        };
        drop(head_lock);
        result
    }

    /// Refusals for publishing `name` that need no object and no lock: name
    /// validity, D/F conflicts, symlinked/special/symbolic loose refs and
    /// unusable reflog paths. Commit runs this before writing any object.
    pub(super) fn preflight_ref(&self, name: &[u8], log: bool) -> Result<(), Error> {
        let location = self.ref_location(name)?;
        self.preflight_location(&location, log).map(|_| ())
    }

    fn preflight_location(
        &self,
        location: &RefLocation,
        log: bool,
    ) -> Result<Vec<(PathBuf, PathBuf)>, Error> {
        self.check_directory_conflicts(location)?;
        match read_loose(&location.path, self.inner.object_hash())? {
            Some(Loose::Symbolic(_)) if location.name == b"HEAD" => {
                return Err(Error::UnsupportedCapability(
                    "updating a symbolic HEAD; update the branch it names",
                ));
            }
            Some(Loose::Symbolic(_)) => {
                return Err(Error::UnsupportedCapability(
                    "updating a symbolic reference",
                ));
            }
            _ => {}
        }
        if log {
            // Path validation only; the HEAD decision is repeated under
            // HEAD.lock at publication.
            let head_names_ref = location.name != b"HEAD"
                && matches!(
                    read_loose(&self.inner.git_dir().join("HEAD"), self.inner.object_hash())?,
                    Some(Loose::Symbolic(target)) if target == location.name
                );
            self.reflog_targets(location, head_names_ref)
        } else {
            Ok(Vec::new())
        }
    }

    /// Sync the loose copy of `id`, if any, and its directory. Packed objects
    /// are durable already; a symlink or special file is refused.
    pub(super) fn sync_loose_object(&self, id: ObjectId) -> Result<(), Error> {
        let hex = id.to_hex().to_string();
        let path = self
            .inner
            .common_dir()
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..]);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => super::sync_existing_file(&path),
            Ok(_) => Err(Error::UnsupportedCapability(
                "symlink or special loose object file",
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn ref_location(&self, name: &[u8]) -> Result<RefLocation, Error> {
        let valid = name == b"HEAD"
            || (name.starts_with(b"refs/")
                && gix::validate::reference::name(BStr::new(name)).is_ok());
        if !valid {
            return Err(Error::InvalidRefName(name.to_vec()));
        }
        let relative =
            gix::path::from_byte_slice(name).map_err(|_| Error::InvalidRefName(name.to_vec()))?;
        let shared = !is_per_worktree(name);
        let base = if shared {
            self.inner.common_dir()
        } else {
            self.inner.git_dir()
        }
        .to_owned();
        Ok(RefLocation {
            name: name.to_vec(),
            path: base.join(relative),
            base,
            shared,
        })
    }

    /// Read a reference without locking (for decisions that are re-verified
    /// under a lock, or that only determine reflog destinations).
    pub(super) fn read_ref(&self, name: &[u8]) -> Result<Option<Loose>, Error> {
        let location = self.ref_location(name)?;
        match read_loose(&location.path, self.inner.object_hash())? {
            Some(value) => Ok(Some(value)),
            None if location.shared => Ok(self.packed_refs()?.lookup(name).map(Loose::Direct)),
            None => Ok(None),
        }
    }

    fn check_object_id(&self, id: ObjectId) -> Result<(), Error> {
        if id.kind() != self.inner.object_hash() {
            return Err(Error::UnsupportedCapability(
                "object ID of a different hash algorithm",
            ));
        }
        if id.is_null() {
            return Err(Error::MissingObject(id));
        }
        Ok(())
    }

    pub(super) fn require_present(&self, id: ObjectId) -> Result<(), Error> {
        use gix::objs::Exists;
        // Ask the object store directly: `Repository::has_object` reports the
        // empty tree as present even when it is not stored.
        if self.inner.objects.exists(&id) {
            Ok(())
        } else {
            Err(Error::MissingObject(id))
        }
    }

    pub(super) fn require_kind(&self, id: ObjectId, expected: ObjectKind) -> Result<(), Error> {
        self.require_present(id)?;
        let actual = self.inner.find_header(id)?.kind();
        if actual != expected {
            return Err(Error::UnexpectedKind { expected, actual });
        }
        Ok(())
    }

    /// Refuse a name that would be a directory of, or inside, another ref
    /// (`refs/heads/a` vs `refs/heads/a/b`), loose or packed.
    fn check_directory_conflicts(&self, location: &RefLocation) -> Result<(), Error> {
        if location.name == b"HEAD" {
            return Ok(());
        }
        let name = &location.name;
        let mut prefix_paths = Vec::new();
        for (offset, byte) in name.iter().enumerate().skip(b"refs/".len()) {
            if *byte == b'/' {
                prefix_paths.push(&name[..offset]);
            }
        }
        for prefix in &prefix_paths {
            let path = location.base.join(
                gix::path::from_byte_slice(prefix)
                    .map_err(|_| Error::InvalidRefName(name.clone()))?,
            );
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if !metadata.is_dir() => return Err(Error::AlreadyExists(path)),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => return Err(error.into()),
            }
        }
        match std::fs::symlink_metadata(&location.path) {
            Ok(metadata) if metadata.is_dir() => {
                return Err(Error::AlreadyExists(location.path.clone()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if location.shared {
            let packed = self.packed_refs()?;
            let mut child = name.clone();
            child.push(b'/');
            for packed_name in packed.names() {
                if prefix_paths.contains(&packed_name.as_slice()) || packed_name.starts_with(&child)
                {
                    return Err(Error::AlreadyExists(
                        location.base.join(
                            gix::path::from_byte_slice(packed_name)
                                .map_err(|_| Error::InvalidRefName(packed_name.clone()))?,
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    fn packed_refs(&self) -> Result<PackedRefs, Error> {
        let path = self.inner.common_dir().join("packed-refs");
        let bytes = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.is_file() => {
                return Err(Error::UnsupportedCapability(
                    "symlink or special packed-refs file",
                ));
            }
            Ok(_) => std::fs::read(&path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PackedRefs(Vec::new()));
            }
            Err(error) => return Err(error.into()),
        };
        let buffer = gix::refs::packed::Buffer::from_bytes(&bytes, self.inner.object_hash())?;
        let mut refs = Vec::new();
        for reference in buffer.iter()? {
            let reference = reference?;
            refs.push((reference.name.as_bstr().to_vec(), reference.target()));
        }
        Ok(PackedRefs(refs))
    }

    /// Reflog files to append for an update of `location`: the reference's
    /// own log and, when `head_names_ref`, this worktree's `HEAD` log.
    fn reflog_targets(
        &self,
        location: &RefLocation,
        head_names_ref: bool,
    ) -> Result<Vec<(PathBuf, PathBuf)>, Error> {
        let mode = self.log_all_ref_updates()?;
        let mut names = vec![location.name.clone()];
        if location.name != b"HEAD" && head_names_ref {
            names.push(b"HEAD".to_vec());
        }
        let mut paths = Vec::new();
        for name in names {
            let target = self.ref_location(&name)?;
            let relative = gix::path::from_byte_slice(&name)
                .map_err(|_| Error::InvalidRefName(name.clone()))?;
            let path = target.base.join("logs").join(relative);
            let exists = match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() => true,
                Ok(_) => {
                    return Err(Error::UnsupportedCapability(
                        "reflog path is not a regular file",
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                // A leftover log file where a directory is needed.
                Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => {
                    return Err(Error::AlreadyExists(path));
                }
                Err(error) => return Err(error.into()),
            };
            let create = match mode {
                LogMode::Always => true,
                LogMode::Normal => {
                    name == b"HEAD"
                        || name.starts_with(b"refs/heads/")
                        || name.starts_with(b"refs/remotes/")
                        || name.starts_with(b"refs/notes/")
                }
                LogMode::Never => false,
            };
            if exists || create {
                paths.push((target.base, path));
            }
        }
        Ok(paths)
    }

    fn log_all_ref_updates(&self) -> Result<LogMode, Error> {
        let snapshot = self.inner.config_snapshot();
        let config = snapshot.plumbing();
        match config.boolean("core.logAllRefUpdates") {
            Ok(Some(true)) => Ok(LogMode::Normal),
            Ok(Some(false)) => Ok(LogMode::Never),
            Ok(None) if self.inner.is_bare() => Ok(LogMode::Never),
            Ok(None) => Ok(LogMode::Normal),
            Err(error) => match config.string("core.logAllRefUpdates") {
                Some(value) if value.eq_ignore_ascii_case(b"always") => Ok(LogMode::Always),
                _ => Err(error.into()),
            },
        }
    }
}

/// How `publish_ref` is driven by its caller.
#[derive(Debug, Clone, Copy)]
pub(super) struct PublishOptions {
    /// Failpoint hit with the locks held and the expectation verified.
    pub(super) locked: &'static str,
    /// Commit on a branch: `HEAD` must still name the ref under `HEAD.lock`.
    pub(super) head_must_name_ref: bool,
}

#[derive(Debug, Clone, Copy)]
enum LogMode {
    Never,
    Normal,
    Always,
}

pub(super) struct RefLocation {
    name: Vec<u8>,
    /// The Git directory that holds this reference (common or per-worktree).
    base: PathBuf,
    path: PathBuf,
    shared: bool,
}

/// A loose reference's value as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Loose {
    Direct(ObjectId),
    Symbolic(Vec<u8>),
}

struct PackedRefs(Vec<(Vec<u8>, ObjectId)>);

impl PackedRefs {
    fn lookup(&self, name: &[u8]) -> Option<ObjectId> {
        self.0
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, id)| *id)
    }

    fn names(&self) -> impl Iterator<Item = &Vec<u8>> {
        self.0.iter().map(|(name, _)| name)
    }
}

/// Like C Git's `is_per_worktree_ref` for the names this module accepts.
fn is_per_worktree(name: &[u8]) -> bool {
    name == b"HEAD"
        || name.starts_with(b"refs/bisect/")
        || name.starts_with(b"refs/worktree/")
        || name.starts_with(b"refs/rewritten/")
}

/// Read a loose ref file. Absence is `None`; a directory is a D/F conflict;
/// symlinks and special files are refused.
pub(super) fn read_loose(path: &Path, hash: gix::hash::Kind) -> Result<Option<Loose>, Error> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_dir() {
        return Err(Error::AlreadyExists(path.to_owned()));
    }
    if !metadata.is_file() {
        return Err(Error::UnsupportedCapability(
            "symlink or special reference file",
        ));
    }
    let bytes = std::fs::read(path)?;
    parse_loose(&bytes, hash)
        .map(Some)
        .ok_or_else(|| corrupt("Malformed loose reference"))
}

fn parse_loose(bytes: &[u8], hash: gix::hash::Kind) -> Option<Loose> {
    if let Some(rest) = bytes.strip_prefix(b"ref:") {
        let target = rest.trim_ascii();
        let valid = target == b"HEAD"
            || (target.starts_with(b"refs/")
                && gix::validate::reference::name(BStr::new(target)).is_ok());
        return valid.then(|| Loose::Symbolic(target.to_vec()));
    }
    let hex_len = hash.len_in_hex();
    let hex = bytes.get(..hex_len)?;
    if !bytes[hex_len..]
        .first()
        .is_none_or(|byte| byte.is_ascii_whitespace())
    {
        return None;
    }
    ObjectId::from_hex(hex).ok().map(Loose::Direct)
}

pub(super) fn corrupt(message: &'static str) -> Error {
    gix::error::corruption(message).corrupted_error().into()
}

/// Create the missing directories between `base` and `path`'s parent, making
/// each new entry durable. A non-directory in the way is a D/F conflict.
pub(super) fn create_ref_dirs(base: &Path, path: &Path) -> Result<(), Error> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let relative = parent
        .strip_prefix(base)
        .map_err(|_| Error::InvalidPath(parent.as_os_str().as_encoded_bytes().to_vec()))?;
    let mut current = base.to_owned();
    for component in relative.components() {
        current.push(component);
        match std::fs::create_dir(&current) {
            Ok(()) => sync_parent(&current)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if !std::fs::symlink_metadata(&current)?.is_dir() {
                    return Err(Error::AlreadyExists(current));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// `<name> <<email>> <seconds> <±hhmm>`, as in commit headers and reflogs.
pub(super) fn format_signature(signature: &Signature) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&signature.name);
    out.extend_from_slice(b" <");
    out.extend_from_slice(&signature.email);
    out.extend_from_slice(b"> ");
    let sign = if signature.offset_minutes < 0 {
        '-'
    } else {
        '+'
    };
    let offset = signature.offset_minutes.unsigned_abs();
    out.extend_from_slice(
        format!(
            "{} {sign}{:02}{:02}",
            signature.seconds,
            offset / 60,
            offset % 60
        )
        .as_bytes(),
    );
    out
}

/// C Git's reflog line: the message is cut at NUL, whitespace runs collapse
/// to one space, and leading/trailing whitespace is dropped (`copy_reflog_msg`).
fn reflog_line(old: ObjectId, new: ObjectId, log: &RefLog) -> Vec<u8> {
    let mut line = format!("{} {} ", old.to_hex(), new.to_hex()).into_bytes();
    line.extend_from_slice(&format_signature(&log.committer));
    let mut message = Vec::new();
    let mut was_space = true;
    for &byte in log.message.iter().take_while(|&&byte| byte != 0) {
        let space = matches!(byte, b' ' | b'\t' | b'\n' | b'\r');
        if was_space && space {
            continue;
        }
        was_space = space;
        message.push(if space { b' ' } else { byte });
    }
    while message.last() == Some(&b' ') {
        message.pop();
    }
    if !message.is_empty() {
        line.push(b'\t');
        line.extend_from_slice(&message);
    }
    line.push(b'\n');
    line
}

/// Append to `path`, a reflog below the Git directory `base`.
fn append_reflog(base: &Path, path: &Path, line: &[u8]) -> std::io::Result<()> {
    create_ref_dirs(base, path).map_err(std::io::Error::other)?;
    let existed = std::fs::symlink_metadata(path).is_ok();
    let mut file: File = OpenOptions::new().append(true).create(true).open(path)?;
    // One write call so concurrent appenders do not interleave within a line.
    file.write_all(line)?;
    file.sync_all()?;
    if !existed {
        sync_parent(path).map_err(std::io::Error::other)?;
    }
    Ok(())
}

/// Identity of a held `<ref>.lock`, used after a failed `LockFile::commit`
/// to tell whether the rename (publication) already happened.
struct LockIdentity {
    #[cfg(unix)]
    identity: (u64, u64),
}

impl LockIdentity {
    fn lock_path(target: &Path) -> PathBuf {
        let mut name = target.file_name().unwrap_or_default().to_os_string();
        name.push(".lock");
        target.with_file_name(name)
    }

    #[cfg(unix)]
    fn capture(target: &Path) -> Result<Self, Error> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(Self::lock_path(target))?;
        Ok(Self {
            identity: (metadata.dev(), metadata.ino()),
        })
    }

    /// The lock path still names the lock we created (Unix: same inode).
    #[cfg(unix)]
    fn still_held(&self, target: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(Self::lock_path(target))
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.identity)
    }

    /// The target is now our lock file (Unix: same inode).
    #[cfg(unix)]
    fn renamed(&self, target: &Path, _content: &[u8]) -> bool {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(target)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.identity)
    }

    #[cfg(not(unix))]
    fn capture(_target: &Path) -> Result<Self, Error> {
        Ok(Self {})
    }

    /// Without inode numbers: our open handle denies deletion, so presence
    /// of a regular file is the check.
    #[cfg(not(unix))]
    fn still_held(&self, target: &Path) -> bool {
        std::fs::symlink_metadata(Self::lock_path(target)).is_ok_and(|metadata| metadata.is_file())
    }

    /// Without inode numbers: the lock is gone and the target holds our bytes.
    #[cfg(not(unix))]
    fn renamed(&self, target: &Path, content: &[u8]) -> bool {
        !Self::lock_path(target).exists()
            && std::fs::read(target).is_ok_and(|bytes| bytes == content)
    }
}

/// The reference WAS updated (renamed into place), but syncing its directory
/// failed, so its durability across a crash is unknown. Its reflogs had
/// already been appended. Returned inside [`Error::Other`].
#[derive(Debug)]
pub struct RefPublishedUnsyncedError {
    reference: Vec<u8>,
    source: Error,
}

impl RefPublishedUnsyncedError {
    /// The reference that now holds the new value.
    pub fn reference(&self) -> &[u8] {
        &self.reference
    }
}

impl std::fmt::Display for RefPublishedUnsyncedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "reference {:?} was updated, but syncing it failed ({}); its durability is unknown",
            BStr::new(&self.reference),
            self.source
        )
    }
}

impl std::error::Error for RefPublishedUnsyncedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Appending a reflog line failed before publication: the reference was NOT
/// updated. If `partially_logged()`, an earlier log of the same update (the
/// reference's own, before `HEAD`'s) already gained a line whose new value
/// never became the reference. Returned inside [`Error::Other`].
#[derive(Debug)]
pub struct ReflogAppendError {
    reference: Vec<u8>,
    source: std::io::Error,
    partially_logged: bool,
}

impl ReflogAppendError {
    /// The reference that was left unchanged.
    pub fn reference(&self) -> &[u8] {
        &self.reference
    }

    /// Whether another reflog of this update was already appended.
    pub fn partially_logged(&self) -> bool {
        self.partially_logged
    }
}

impl std::fmt::Display for ReflogAppendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "appending the reflog for {:?} failed ({}); the reference was not updated",
            BStr::new(&self.reference),
            self.source
        )
    }
}

impl std::error::Error for ReflogAppendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// The reflog lines were appended, but publishing the reference (the rename
/// of its lock) then failed: the reference was NOT updated and its reflog
/// has a line whose new value never became the reference (as after a crash
/// at the same point in C Git). Returned inside [`Error::Other`].
#[derive(Debug)]
pub struct RefLoggedNotPublishedError {
    reference: Vec<u8>,
    source: Error,
}

impl RefLoggedNotPublishedError {
    /// The reference that was left unchanged.
    pub fn reference(&self) -> &[u8] {
        &self.reference
    }
}

impl std::fmt::Display for RefLoggedNotPublishedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "reference {:?} was not updated ({}), but its reflog already records the update",
            BStr::new(&self.reference),
            self.source
        )
    }
}

impl std::error::Error for RefLoggedNotPublishedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}
