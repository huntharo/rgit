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

use std::io::{Read, Write};

use crate::fsx::{Anchor, FileId, Kind};
use crate::{BStr, Error, ObjectId, ObjectKind, Repository};

use super::{LockFile, Signature};

/// The Git directory roots a reference operation works in: the per-worktree
/// Git directory (`HEAD`, per-worktree refs and logs) and the common one
/// (shared refs, logs, `packed-refs`, objects). Opened before the first
/// mutation; every path below them is resolved through the handles.
pub(super) struct RefAnchors {
    pub(super) git: Anchor,
    pub(super) common: Anchor,
}

impl RefAnchors {
    pub(super) fn open(repo: &Repository) -> Result<Self, Error> {
        Ok(Self {
            git: repo.git_dir_anchor()?,
            common: repo.common_anchor()?,
        })
    }

    fn of(&self, location: &RefLocation) -> &Anchor {
        if location.shared {
            &self.common
        } else {
            &self.git
        }
    }
}

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
    /// value. See [`ReflogAppendError`], [`RefLoggedNotPublishedError`],
    /// [`RefPublishedUnsyncedError`] and [`RefPublishedUnreachableError`] for
    /// failures around publication.
    pub fn update_ref(&self, update: &RefUpdate) -> Result<(), Error> {
        let anchors = RefAnchors::open(self)?;
        super::object::check_object_store(&anchors.common)?;
        self.publish_ref(
            &anchors,
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
    /// 5. Rename the lock into place and sync the directory. An error after
    ///    the rename is post-publication: [`RefPublishedUnsyncedError`] or
    ///    [`RefPublishedUnreachableError`], never "not updated".
    ///
    /// A failure or crash after step 4 but before the rename leaves the ref
    /// unchanged and its reflog with a line whose new value never became the
    /// ref, as in C Git; the object exists, so fsck accepts it.
    pub(super) fn publish_ref(
        &self,
        anchors: &RefAnchors,
        update: &RefUpdate,
        options: PublishOptions,
    ) -> Result<(), Error> {
        let location = self.ref_location(&update.name)?;
        self.check_object_id(update.new)?;
        // Objects before names: refuse before taking any lock.
        if update.name == b"HEAD" || update.name.starts_with(b"refs/heads/") {
            self.require_kind(anchors, update.new, ObjectKind::Commit)?;
        } else {
            self.require_present(anchors, update.new)?;
        }
        // The object may have been written by someone who did not sync it.
        self.sync_loose_object(anchors, update.new)?;
        if let Some(log) = &update.log {
            super::commit::validate_signature(&log.committer)?;
        }
        // Pre-checks so no directory is created for a conflicting name; the
        // authoritative checks are repeated under the locks.
        self.preflight_location(anchors, &location, update.log.is_some())?;
        let anchor = anchors.of(&location);
        create_ref_dirs(anchor, &location.name)?;

        let hash = self.inner.object_hash();
        let needs_head_lock =
            update.name != b"HEAD" && (update.log.is_some() || options.head_must_name_ref);
        let head_lock = if needs_head_lock {
            let lock = LockFile::acquire_at(&anchors.git, b"HEAD")?;
            // The identity of the descriptor we hold, not a fresh lookup.
            let identity = LockIdentity {
                identity: lock.held_id()?,
            };
            Some((lock, identity))
        } else {
            None
        };
        // HEAD.lock is a guard that is never renamed, so nothing else would
        // notice it being removed and recreated by another writer: re-verify
        // it before any reflog line is appended and before the rename.
        let head_lock_held = || match &head_lock {
            Some((_, identity)) if !identity.still_held(&anchors.git, b"HEAD") => Err(
                Error::Locked(LockIdentity::lock_path(&anchors.git, b"HEAD")),
            ),
            _ => Ok(()),
        };
        let head_names_ref = match &head_lock {
            Some(_) => {
                matches!(read_loose(&anchors.git, b"HEAD", hash)?, Some(Loose::Symbolic(target)) if target == update.name)
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
                actual: match read_loose(&anchors.git, b"HEAD", hash)? {
                    Some(Loose::Direct(id)) => Some(id),
                    _ => None,
                },
            });
        }

        let mut lock = LockFile::acquire_at(anchor, &location.name)?;
        let lock_identity = LockIdentity {
            identity: lock.held_id()?,
        };
        let current = match read_loose(anchor, &location.name, hash)? {
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
                self.check_directory_conflicts(anchors, &location)?;
                if location.shared {
                    packed_refs(&anchors.common, hash)?.lookup(&update.name)
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
        if !lock_identity.still_held(anchor, &location.name) {
            return Err(Error::Locked(LockIdentity::lock_path(
                anchor,
                &location.name,
            )));
        }
        head_lock_held()?;
        let mut logged = false;
        if let Some(log) = &update.log {
            let old = current.unwrap_or_else(|| ObjectId::null(hash));
            let line = reflog_line(old, update.new, log);
            for (shared, log) in self.reflog_targets(anchors, &location, head_names_ref)? {
                let base = if shared {
                    &anchors.common
                } else {
                    &anchors.git
                };
                append_reflog(base, &log, &line).map_err(|source| {
                    Error::Other(Box::new(ReflogAppendError {
                        reference: update.name.clone(),
                        source,
                        partially_logged: logged,
                    }))
                })?;
                logged = true;
            }
        }
        if let Err(source) = super::failpoint("ref.logged").and_then(|()| head_lock_held()) {
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
            // After the rename (LockFile's post-publication errors): the new
            // value was published, so never report it as not updated.
            Err(source) if super::lock::after_publication(&source) => {
                Err(published_after_error(update.name.clone(), logged, source))
            }
            // The path form (non-Unix) keeps untyped post-rename errors;
            // there, the target holding our lock file tells that it was
            // renamed.
            Err(source) if !cfg!(unix) && lock_identity.renamed(anchor, &location.name) => {
                Err(Error::Other(Box::new(RefPublishedUnsyncedError {
                    reference: update.name.clone(),
                    source,
                })))
            }
            // Before the rename: the ref is unchanged, but its reflog says
            // otherwise.
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
    pub(super) fn preflight_ref(
        &self,
        anchors: &RefAnchors,
        name: &[u8],
        log: bool,
    ) -> Result<(), Error> {
        let location = self.ref_location(name)?;
        self.preflight_location(anchors, &location, log).map(|_| ())
    }

    fn preflight_location(
        &self,
        anchors: &RefAnchors,
        location: &RefLocation,
        log: bool,
    ) -> Result<Vec<(bool, Vec<u8>)>, Error> {
        self.check_directory_conflicts(anchors, location)?;
        match read_loose(
            anchors.of(location),
            &location.name,
            self.inner.object_hash(),
        )? {
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
                    read_loose(&anchors.git, b"HEAD", self.inner.object_hash())?,
                    Some(Loose::Symbolic(target)) if target == location.name
                );
            self.reflog_targets(anchors, location, head_names_ref)
        } else {
            Ok(Vec::new())
        }
    }

    /// Verify and sync the loose copy of `id`, if any, and its directory.
    /// Packed objects are durable already; a symlink or special file is
    /// refused.
    pub(super) fn sync_loose_object(
        &self,
        anchors: &RefAnchors,
        id: ObjectId,
    ) -> Result<(), Error> {
        super::object::existing_loose(&anchors.common.sub(b"objects")?, &id).map(drop)
    }

    pub(super) fn ref_location(&self, name: &[u8]) -> Result<RefLocation, Error> {
        let valid = name == b"HEAD"
            || (name.starts_with(b"refs/")
                && gix::validate::reference::name(BStr::new(name)).is_ok());
        if !valid {
            return Err(Error::InvalidRefName(name.to_vec()));
        }
        Ok(RefLocation {
            name: name.to_vec(),
            shared: !is_per_worktree(name),
        })
    }

    /// Read a reference without locking (for decisions that are re-verified
    /// under a lock, or that only determine reflog destinations).
    pub(super) fn read_ref(
        &self,
        anchors: &RefAnchors,
        name: &[u8],
    ) -> Result<Option<Loose>, Error> {
        let location = self.ref_location(name)?;
        let hash = self.inner.object_hash();
        match read_loose(anchors.of(&location), name, hash)? {
            Some(value) => Ok(Some(value)),
            None if location.shared => Ok(packed_refs(&anchors.common, hash)?
                .lookup(name)
                .map(Loose::Direct)),
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

    /// `id` exists: a loose copy is found through the objects anchor (no
    /// path lookup); otherwise the packed store is asked, after the anchored
    /// pack check.
    pub(super) fn require_present(&self, anchors: &RefAnchors, id: ObjectId) -> Result<(), Error> {
        use gix::objs::Exists;
        let objects = anchors.common.sub(b"objects")?;
        let hex = id.to_hex().to_string();
        let loose = format!("{}/{}", &hex[..2], &hex[2..]);
        if matches!(objects.lstat(loose.as_bytes())?, Some(meta) if matches!(meta.kind, Kind::File { .. }))
        {
            return Ok(());
        }
        super::object::check_object_store(&anchors.common)?;
        // Ask the object store directly: `Repository::has_object` reports the
        // empty tree as present even when it is not stored.
        if self.inner.objects.exists(&id) {
            Ok(())
        } else {
            Err(Error::MissingObject(id))
        }
    }

    /// The kind of `id` is `expected`. A loose object's header is read only
    /// through the objects anchor (no-follow, non-blocking); a packed one is
    /// read by Gitoxide after the anchored pack check.
    pub(super) fn require_kind(
        &self,
        anchors: &RefAnchors,
        id: ObjectId,
        expected: ObjectKind,
    ) -> Result<(), Error> {
        let objects = anchors.common.sub(b"objects")?;
        let actual = match super::object::loose_header(&objects, &id)? {
            Some((kind, _)) => kind,
            None => {
                self.require_present(anchors, id)?;
                self.inner.find_header(id)?.kind()
            }
        };
        if actual != expected {
            return Err(Error::UnexpectedKind { expected, actual });
        }
        Ok(())
    }

    /// Refuse a name that would be a directory of, or inside, another ref
    /// (`refs/heads/a` vs `refs/heads/a/b`), loose or packed.
    fn check_directory_conflicts(
        &self,
        anchors: &RefAnchors,
        location: &RefLocation,
    ) -> Result<(), Error> {
        if location.name == b"HEAD" {
            return Ok(());
        }
        let anchor = anchors.of(location);
        let name = &location.name;
        let mut prefix_paths = Vec::new();
        for (offset, byte) in name.iter().enumerate().skip(b"refs/".len()) {
            if *byte == b'/' {
                prefix_paths.push(&name[..offset]);
            }
        }
        for prefix in &prefix_paths {
            match anchor.lstat(prefix)? {
                Some(meta) if meta.kind != Kind::Dir => {
                    return Err(Error::AlreadyExists(shown(anchor, prefix)));
                }
                Some(_) => {}
                None => break,
            }
        }
        if let Some(meta) = anchor.lstat(name)?
            && meta.kind == Kind::Dir
        {
            return Err(Error::AlreadyExists(shown(anchor, name)));
        }
        if location.shared {
            let packed = packed_refs(&anchors.common, self.inner.object_hash())?;
            let mut child = name.clone();
            child.push(b'/');
            for packed_name in packed.names() {
                if prefix_paths.contains(&packed_name.as_slice()) || packed_name.starts_with(&child)
                {
                    return Err(Error::AlreadyExists(shown(anchor, packed_name)));
                }
            }
        }
        Ok(())
    }

    /// Reflog files to append for an update of `location`, as
    /// `(in the common directory, path below it)`: the reference's own log
    /// and, when `head_names_ref`, this worktree's `HEAD` log.
    fn reflog_targets(
        &self,
        anchors: &RefAnchors,
        location: &RefLocation,
        head_names_ref: bool,
    ) -> Result<Vec<(bool, Vec<u8>)>, Error> {
        let mode = self.log_all_ref_updates()?;
        let mut names = vec![location.name.clone()];
        if location.name != b"HEAD" && head_names_ref {
            names.push(b"HEAD".to_vec());
        }
        let mut paths = Vec::new();
        for name in names {
            let target = self.ref_location(&name)?;
            let anchor = anchors.of(&target);
            let log = [&b"logs/"[..], &name].concat();
            let exists = match anchor.lstat(&log) {
                Ok(Some(meta)) if matches!(meta.kind, Kind::File { .. }) => true,
                Ok(Some(_)) => {
                    return Err(Error::UnsupportedCapability(
                        "reflog path is not a regular file",
                    ));
                }
                Ok(None) => false,
                // A leftover log file where a directory is needed.
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotADirectory => {
                    return Err(Error::AlreadyExists(shown(anchor, &log)));
                }
                Err(error) => return Err(error),
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
                paths.push((target.shared, log));
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
    /// Full name, which is also its path below its Git directory.
    name: Vec<u8>,
    /// In the common Git directory (otherwise the per-worktree one).
    shared: bool,
}

/// Diagnostic path for errors only; never used for I/O.
fn shown(anchor: &Anchor, rel: &[u8]) -> std::path::PathBuf {
    anchor
        .display_path()
        .join(String::from_utf8_lossy(rel).as_ref())
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

/// Read the loose ref `rel` below `anchor`. Absence is `None`; a directory is
/// a D/F conflict; symlinks and special files are refused.
pub(super) fn read_loose(
    anchor: &Anchor,
    rel: &[u8],
    hash: gix::hash::Kind,
) -> Result<Option<Loose>, Error> {
    match anchor.lstat(rel)? {
        None => return Ok(None),
        Some(meta) if meta.kind == Kind::Dir => {
            return Err(Error::AlreadyExists(shown(anchor, rel)));
        }
        Some(meta) if !matches!(meta.kind, Kind::File { .. }) => {
            return Err(Error::UnsupportedCapability(
                "symlink or special reference file",
            ));
        }
        Some(_) => {}
    }
    let (mut file, _) = anchor.open_read(rel)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    parse_loose(&bytes, hash)
        .map(Some)
        .ok_or_else(|| corrupt("Malformed loose reference"))
}

/// `packed-refs` of the common directory, read through its anchor.
fn packed_refs(common: &Anchor, hash: gix::hash::Kind) -> Result<PackedRefs, Error> {
    match common.lstat(b"packed-refs")? {
        None => return Ok(PackedRefs(Vec::new())),
        Some(meta) if !matches!(meta.kind, Kind::File { .. }) => {
            return Err(Error::UnsupportedCapability(
                "symlink or special packed-refs file",
            ));
        }
        Some(_) => {}
    }
    let (mut file, _) = common.open_read(b"packed-refs")?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let buffer = gix::refs::packed::Buffer::from_bytes(&bytes, hash)?;
    let mut refs = Vec::new();
    for reference in buffer.iter()? {
        let reference = reference?;
        refs.push((reference.name.as_bstr().to_vec(), reference.target()));
    }
    Ok(PackedRefs(refs))
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

/// Create the missing parent directories of `rel` below `anchor`, making
/// each new entry durable. A non-directory or symlink in the way is a D/F
/// conflict; nothing is created through a symlinked component.
pub(super) fn create_ref_dirs(anchor: &Anchor, rel: &[u8]) -> Result<(), Error> {
    super::failpoint("refs.before-dir")?;
    let Some(slash) = rel.iter().rposition(|byte| *byte == b'/') else {
        return Ok(());
    };
    let parent = &rel[..slash];
    for (offset, byte) in parent.iter().enumerate().chain([(parent.len(), &b'/')]) {
        if *byte != b'/' {
            continue;
        }
        let directory = &parent[..offset];
        match anchor.mkdir(directory) {
            Ok(true) => sync_parent_dir(anchor, directory)?,
            Ok(false) => {}
            Err(Error::AlreadyExists(_)) => {
                return Err(Error::AlreadyExists(shown(anchor, directory)));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Sync the directory containing `rel` (below `anchor`).
fn sync_parent_dir(anchor: &Anchor, rel: &[u8]) -> Result<(), Error> {
    match rel.iter().rposition(|byte| *byte == b'/') {
        Some(slash) => anchor.sub(&rel[..slash])?.sync(),
        None => anchor.sync(),
    }
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

/// Append `line` to the reflog `rel` below the Git directory `anchor`,
/// creating its directories and the file as needed (never through a
/// symlink, never blocking on a FIFO).
fn append_reflog(anchor: &Anchor, rel: &[u8], line: &[u8]) -> std::io::Result<()> {
    let io = |error: Error| match error {
        Error::Io(error) => error,
        other => std::io::Error::other(other),
    };
    create_ref_dirs(anchor, rel).map_err(io)?;
    let (mut file, created) = anchor.open_append(rel, 0o666).map_err(io)?;
    // One write call so concurrent appenders do not interleave within a line.
    file.write_all(line)?;
    file.sync_all()?;
    if created {
        sync_parent_dir(anchor, rel).map_err(io)?;
    }
    Ok(())
}

/// The expected identity of a held lock, from its held descriptor
/// (`LockFile::held_id`). It is compared with the anchored, no-follow entry
/// to refuse a lock that was removed or replaced.
struct LockIdentity {
    identity: FileId,
}

impl LockIdentity {
    fn lock_rel(target: &[u8]) -> Vec<u8> {
        [target, b".lock"].concat()
    }

    fn lock_path(anchor: &Anchor, target: &[u8]) -> std::path::PathBuf {
        shown(anchor, &Self::lock_rel(target))
    }

    /// The lock path, looked up through the anchor (no-follow), still names
    /// the lock we created: a regular file with the expected identity. A
    /// missing entry, a symlink or any other file is a mismatch.
    fn still_held(&self, anchor: &Anchor, target: &[u8]) -> bool {
        self.names_ours(anchor, &Self::lock_rel(target))
    }

    /// The target is now our lock file (used for the non-Unix path form,
    /// whose post-rename errors are untyped).
    fn renamed(&self, anchor: &Anchor, target: &[u8]) -> bool {
        self.names_ours(anchor, target)
    }

    fn names_ours(&self, anchor: &Anchor, rel: &[u8]) -> bool {
        matches!(
            anchor.lstat(rel),
            Ok(Some(meta)) if matches!(meta.kind, Kind::File { .. }) && meta.id == self.identity
        )
    }
}

/// Map a post-publication error from `LockFile::commit` (see
/// `lock::after_publication`) explicitly by type:
/// `LockPublishedUnsyncedError` becomes [`RefPublishedUnsyncedError`] and
/// `LockPublishedUnreachableError` becomes [`RefPublishedUnreachableError`].
/// Any other post-publication error is returned unchanged: relabelling it
/// could turn an uncertain outcome into "was updated".
fn published_after_error(reference: Vec<u8>, logged: bool, source: Error) -> Error {
    let Error::Other(error) = &source else {
        return source;
    };
    if error.is::<super::LockPublishedUnsyncedError>() {
        Error::Other(Box::new(RefPublishedUnsyncedError { reference, source }))
    } else if error.is::<super::LockPublishedUnreachableError>() {
        Error::Other(Box::new(RefPublishedUnreachableError {
            reference,
            logged,
            source,
        }))
    } else {
        source
    }
}

/// The reference WAS updated (renamed into place), but a step after the
/// rename failed (syncing its directory), so its durability across a crash is
/// unknown. Its reflogs had already been appended. Returned inside
/// [`Error::Other`].
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
            "reference {:?} was updated, but a step after publishing it failed ({}); its \
             durability is unknown",
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

/// The reference's lock WAS renamed over it inside the directory held since
/// the lock was taken, but afterwards the reference's name no longer reaches
/// the published file (the directory was moved or replaced, or another writer
/// replaced the reference). The state is uncertain, never "not updated", and
/// nothing is rolled back. If `logged()`, its reflogs record the update.
/// Returned inside [`Error::Other`].
#[derive(Debug)]
pub struct RefPublishedUnreachableError {
    reference: Vec<u8>,
    logged: bool,
    source: Error,
}

impl RefPublishedUnreachableError {
    /// The reference whose new value was published.
    pub fn reference(&self) -> &[u8] {
        &self.reference
    }

    /// Whether reflog lines recording the update were appended.
    pub fn logged(&self) -> bool {
        self.logged
    }
}

impl std::fmt::Display for RefPublishedUnreachableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the update of reference {:?} was published{}, but it may not be visible at \
             that name: the name no longer reaches the published file ({}); the state is \
             uncertain",
            BStr::new(&self.reference),
            if self.logged {
                " and its reflog records the update"
            } else {
                ""
            },
            self.source
        )
    }
}

impl std::error::Error for RefPublishedUnreachableError {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A post-publication error of a kind this mapping does not know (for
    /// example one added to `after_publication` later) passes through
    /// unchanged; it is never relabelled as "was updated".
    #[test]
    fn unknown_post_publication_errors_pass_through_unchanged() {
        #[derive(Debug)]
        struct Uncertain;
        impl std::fmt::Display for Uncertain {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("rename outcome uncertain")
            }
        }
        impl std::error::Error for Uncertain {}

        let mapped = published_after_error(
            b"refs/heads/main".to_vec(),
            true,
            Error::Other(Box::new(Uncertain)),
        );
        assert!(
            matches!(&mapped, Error::Other(error) if error.is::<Uncertain>()),
            "{mapped:?}"
        );
        assert_eq!(mapped.to_string(), "rename outcome uncertain");
        let injected = published_after_error(
            b"refs/heads/main".to_vec(),
            true,
            Error::Injected("lock.after-rename"),
        );
        assert!(
            matches!(injected, Error::Injected("lock.after-rename")),
            "{injected:?}"
        );
    }

    #[test]
    fn unreachable_publication_is_never_reported_as_not_updated() {
        for logged in [true, false] {
            let text = RefPublishedUnreachableError {
                reference: b"refs/heads/main".to_vec(),
                logged,
                source: Error::Injected("lock.after-rename"),
            }
            .to_string();
            assert!(text.contains("was published"), "{text}");
            assert!(text.contains("may not be visible at that name"), "{text}");
            assert_eq!(text.contains("reflog records the update"), logged, "{text}");
            assert!(!text.contains("was not updated"), "{text}");
        }
    }
}
