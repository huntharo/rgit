//! OWNER: WP2 (commit/ref publication). Frozen public signatures.
//!
//! Order of a commit: refuse unsupported state, read the `HEAD` target, write
//! and sync tree and commit objects, then publish through a compare-and-swap
//! reference update against the value read first (docs/decisions/0003).

use std::io::Write;
use std::path::Path;

use crate::{Error, ObjectId, ObjectKind, Repository};

use super::refs::{Loose, format_signature, read_loose};
use super::{LockFile, RefExpectation, RefLog, RefUpdate, sync_parent};

/// A Git identity line: `name <email> seconds ±HHMM`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: Vec<u8>,
    pub email: Vec<u8>,
    pub seconds: i64,
    pub offset_minutes: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRequest {
    /// Final message bytes, written verbatim (the CLI owns cleanup/joining).
    pub message: Vec<u8>,
    pub author: Signature,
    pub committer: Signature,
    pub allow_empty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOutcome {
    pub id: ObjectId,
    pub tree: ObjectId,
    /// Updated reference: `refs/heads/<branch>`, or `HEAD` when detached.
    pub reference: Vec<u8>,
    pub parent: Option<ObjectId>,
}

/// Per-worktree state files whose presence means another operation is in
/// progress. Bisection is deliberately absent: C Git commits during bisect.
const IN_PROGRESS: &[(&str, &str)] = &[
    ("MERGE_HEAD", "commit during an in-progress merge"),
    (
        "CHERRY_PICK_HEAD",
        "commit during an in-progress cherry-pick",
    ),
    ("REVERT_HEAD", "commit during an in-progress revert"),
    ("rebase-merge", "commit during an in-progress rebase"),
    ("rebase-apply", "commit during an in-progress rebase or am"),
    (
        "sequencer",
        "commit during an in-progress sequencer operation",
    ),
];

impl Repository {
    /// Write the current index as tree objects; refuses unmerged entries.
    ///
    /// Every entry is checked before any object is written: conflict stages
    /// ([`Error::UnmergedIndex`]), intent-to-add entries and gitlinks are
    /// refused, and each blob must be present ([`Error::MissingObject`]).
    pub fn write_tree_from_index(&self) -> Result<ObjectId, Error> {
        let index = self.read_index()?;
        let hash = self.inner.object_hash();
        if index.entries().iter().any(|entry| entry.stage_raw() != 0) {
            return Err(Error::UnmergedIndex);
        }
        let mut files = Vec::with_capacity(index.entries().len());
        for entry in index.entries() {
            use gix::index::entry::{Flags, Mode};
            if entry.flags.contains(Flags::INTENT_TO_ADD) {
                return Err(Error::UnsupportedCapability("intent-to-add index entries"));
            }
            match entry.mode {
                Mode::FILE | Mode::FILE_EXECUTABLE | Mode::SYMLINK => {}
                Mode::COMMIT => {
                    return Err(Error::UnsupportedCapability(
                        "gitlink (submodule) index entries",
                    ));
                }
                _ => {
                    return Err(Error::UnsupportedCapability(
                        "sparse directory index entries",
                    ));
                }
            }
            if entry.id.kind() != hash {
                return Err(Error::UnsupportedCapability(
                    "object ID of a different hash algorithm",
                ));
            }
            self.require_kind(entry.id, ObjectKind::Blob)?;
            files.push((entry.path(&index), entry.mode.bits(), entry.id));
        }
        self.write_trees(&files)
    }

    /// Commit the index on top of HEAD (or as a root commit on an unborn
    /// branch), publishing via compare-and-swap against the HEAD target read
    /// before writing objects.
    pub fn commit(&self, request: &CommitRequest) -> Result<CommitOutcome, Error> {
        if self.inner.is_bare() {
            return Err(Error::UnsupportedCapability("commit in a bare repository"));
        }
        validate_signature(&request.author)?;
        validate_signature(&request.committer)?;
        if request.message.contains(&0) {
            return Err(invalid_input("commit message contains a NUL byte"));
        }
        self.refuse_in_progress()?;
        self.refuse_unsupported_commit_config()?;

        let hash = self.inner.object_hash();
        let head_path = self.inner.git_dir().join("HEAD");
        let (reference, parent) = match read_loose(&head_path, hash)? {
            None => return Err(super::refs::corrupt("HEAD is missing")),
            Some(Loose::Direct(id)) => (b"HEAD".to_vec(), Some(id)),
            Some(Loose::Symbolic(branch)) => {
                if !branch.starts_with(b"refs/heads/") {
                    return Err(Error::UnsupportedCapability(
                        "HEAD names a reference outside refs/heads/",
                    ));
                }
                match self.read_ref(&branch)? {
                    None => (branch, None),
                    Some(Loose::Direct(id)) => (branch, Some(id)),
                    Some(Loose::Symbolic(_)) => {
                        return Err(Error::UnsupportedCapability(
                            "HEAD names a symbolic reference",
                        ));
                    }
                }
            }
        };
        let parent_tree = match parent {
            Some(id) => {
                self.require_kind(id, ObjectKind::Commit)?;
                Some(self.inner.find_object(id)?.try_to_commit_ref()?.tree())
            }
            None => None,
        };

        let tree = self.write_tree_from_index()?;
        if !request.allow_empty && parent_tree.unwrap_or_else(|| ObjectId::empty_tree(hash)) == tree
        {
            return Err(Error::NothingToCommit);
        }
        let mut data = format!("tree {}\n", tree.to_hex()).into_bytes();
        if let Some(parent) = parent {
            data.extend_from_slice(format!("parent {}\n", parent.to_hex()).as_bytes());
        }
        data.extend_from_slice(b"author ");
        data.extend_from_slice(&format_signature(&request.author));
        data.extend_from_slice(b"\ncommitter ");
        data.extend_from_slice(&format_signature(&request.committer));
        data.extend_from_slice(b"\n\n");
        data.extend_from_slice(&request.message);
        let id = self.write_object(ObjectKind::Commit, &data)?;
        super::failpoint("commit.objects-written")?;

        let subject = request
            .message
            .split(|&byte| byte == b'\n')
            .next()
            .unwrap_or_default();
        let mut message = if parent.is_some() {
            b"commit: ".to_vec()
        } else {
            b"commit (initial): ".to_vec()
        };
        message.extend_from_slice(subject);
        let update = RefUpdate {
            name: reference.clone(),
            new: id,
            expected: parent.map_or(RefExpectation::MustNotExist, RefExpectation::MustEqual),
            log: Some(RefLog {
                committer: request.committer.clone(),
                message,
            }),
        };
        if reference == b"HEAD" {
            self.publish_ref(&update, Some("commit.ref-locked"))?;
        } else {
            // Hold HEAD.lock so HEAD cannot be re-pointed (for example by a
            // branch switch) between this check and the branch publication.
            // It is released without touching HEAD.
            let head_lock = LockFile::acquire(&head_path)?;
            match read_loose(&head_path, hash)? {
                Some(Loose::Symbolic(target)) if target == reference => {}
                other => {
                    return Err(Error::RefConflict {
                        name: b"HEAD".to_vec(),
                        expected: parent,
                        actual: match other {
                            Some(Loose::Direct(id)) => Some(id),
                            _ => None,
                        },
                    });
                }
            }
            self.publish_ref(&update, Some("commit.ref-locked"))?;
            drop(head_lock);
        }
        Ok(CommitOutcome {
            id,
            tree,
            reference,
            parent,
        })
    }

    fn refuse_in_progress(&self) -> Result<(), Error> {
        let git_dir = self.inner.git_dir();
        for (name, what) in IN_PROGRESS {
            match std::fs::symlink_metadata(git_dir.join(name)) {
                Ok(_) => return Err(Error::UnsupportedCapability(what)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    /// Settings that would make C Git produce a different commit object.
    fn refuse_unsupported_commit_config(&self) -> Result<(), Error> {
        let snapshot = self.inner.config_snapshot();
        let config = snapshot.plumbing();
        if config.boolean("commit.gpgSign")?.unwrap_or(false) {
            return Err(Error::UnsupportedCapability(
                "commit signing (commit.gpgSign)",
            ));
        }
        if let Some(encoding) = config.string("i18n.commitEncoding")
            && !(encoding.eq_ignore_ascii_case(b"utf-8") || encoding.eq_ignore_ascii_case(b"utf8"))
        {
            return Err(Error::UnsupportedCapability(
                "non-UTF-8 i18n.commitEncoding",
            ));
        }
        Ok(())
    }

    /// Build nested trees from index-ordered `(path, mode, id)` entries and
    /// write them bottom-up, without recursion (paths may be arbitrarily deep).
    fn write_trees(&self, files: &[(&gix::bstr::BStr, u32, ObjectId)]) -> Result<ObjectId, Error> {
        // Each open level: (directory path without trailing '/', entries).
        let mut stack: Vec<(Vec<u8>, Vec<TreeItem>)> = vec![(Vec::new(), Vec::new())];
        for (path, mode, id) in files {
            let path: &[u8] = path;
            let (directory, name) = match path.iter().rposition(|&byte| byte == b'/') {
                Some(slash) => (&path[..slash], &path[slash + 1..]),
                None => (&path[..0], path),
            };
            // Close levels that are not ancestors of (or equal to) `directory`.
            while stack.len() > 1 {
                let open = &stack[stack.len() - 1].0;
                let inside = directory == open.as_slice()
                    || (directory.starts_with(open) && directory.get(open.len()) == Some(&b'/'));
                if inside {
                    break;
                }
                self.close_level(&mut stack)?;
            }
            // Open the remaining directory components.
            let open_len = stack[stack.len() - 1].0.len();
            if directory.len() > open_len {
                let start = if open_len == 0 { 0 } else { open_len + 1 };
                let mut offset = start;
                for component in directory[start..].split(|&byte| byte == b'/') {
                    offset += component.len();
                    stack.push((directory[..offset].to_vec(), Vec::new()));
                    offset += 1;
                }
            }
            let level = stack.len() - 1;
            stack[level].1.push(TreeItem {
                name: name.to_vec(),
                mode: *mode,
                id: *id,
            });
        }
        while stack.len() > 1 {
            self.close_level(&mut stack)?;
        }
        let (_, items) = stack.pop().ok_or(Error::Injected("tree stack underflow"))?;
        self.write_tree_object(items)
    }

    fn close_level(&self, stack: &mut Vec<(Vec<u8>, Vec<TreeItem>)>) -> Result<(), Error> {
        let (path, items) = stack.pop().ok_or(Error::Injected("tree stack underflow"))?;
        let id = self.write_tree_object(items)?;
        let name = match path.iter().rposition(|&byte| byte == b'/') {
            Some(slash) => path[slash + 1..].to_vec(),
            None => path,
        };
        let parent = stack
            .last_mut()
            .ok_or(Error::Injected("tree stack underflow"))?;
        parent.1.push(TreeItem {
            name,
            mode: 0o040000,
            id,
        });
        Ok(())
    }

    fn write_tree_object(&self, mut items: Vec<TreeItem>) -> Result<ObjectId, Error> {
        // Git orders tree entries as if directory names ended with '/'.
        items.sort_by(|a, b| a.sort_key().cmp(b.sort_key()));
        let mut data = Vec::new();
        for item in &items {
            data.extend_from_slice(format!("{:o} ", item.mode).as_bytes());
            data.extend_from_slice(&item.name);
            data.push(0);
            data.extend_from_slice(item.id.as_slice());
        }
        self.write_object(ObjectKind::Tree, &data)
    }

    /// Store a loose object durably: compressed into a private temporary file
    /// in the fan-out directory, synced and made read-only, then linked into
    /// place without overwriting; the directory is synced afterwards. An
    /// object already present (loose or packed) is not rewritten.
    pub(super) fn write_object(&self, kind: ObjectKind, data: &[u8]) -> Result<ObjectId, Error> {
        let hash = self.inner.object_hash();
        let header = format!("{kind} {}\0", data.len());
        let mut hasher = gix::hash::hasher(hash);
        hasher.update(header.as_bytes());
        hasher.update(data);
        let id = hasher.try_finalize().map_err(gix::Error::from_error)?;
        if self.require_present(id).is_ok() {
            return Ok(id);
        }

        let mut compressed = gix::zlib::stream::deflate::Write::new(
            Vec::with_capacity(data.len() / 2 + 64),
            gix::zlib::Compression::BEST_SPEED,
        );
        compressed.write_all(header.as_bytes())?;
        compressed.write_all(data)?;
        compressed.flush()?;
        let compressed = compressed.into_inner();

        let objects = self.inner.common_dir().join("objects");
        if !std::fs::symlink_metadata(&objects)?.is_dir() {
            return Err(Error::UnsupportedCapability(
                "object directory is not a directory",
            ));
        }
        let hex = id.to_hex().to_string();
        let fanout = objects.join(&hex[..2]);
        match std::fs::create_dir(&fanout) {
            Ok(()) => sync_parent(&fanout)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if !std::fs::symlink_metadata(&fanout)?.is_dir() {
                    return Err(Error::UnsupportedCapability(
                        "object fan-out entry is not a directory",
                    ));
                }
            }
            Err(error) => return Err(error.into()),
        }
        let destination = fanout.join(&hex[2..]);
        let mut temp = TempObject::create(&fanout)?;
        let file = temp.file.as_mut().ok_or(Error::Injected("temp reused"))?;
        file.write_all(&compressed)?;
        file.sync_all()?;
        make_read_only(file)?;
        drop(temp.file.take());
        match std::fs::hard_link(&temp.path, &destination) {
            Ok(()) => {}
            // Another writer stored the same content first; never overwrite.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            // Filesystems without hard links: rename, unless already present.
            Err(_) => match std::fs::symlink_metadata(&destination) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::rename(&temp.path, &destination)?;
                    temp.renamed = true;
                }
                Err(error) => return Err(error.into()),
            },
        }
        drop(temp);
        sync_parent(&destination)?;
        Ok(id)
    }
}

struct TreeItem {
    name: Vec<u8>,
    mode: u32,
    id: ObjectId,
}

impl TreeItem {
    fn sort_key(&self) -> impl Iterator<Item = u8> + '_ {
        let suffix = (self.mode == 0o040000).then_some(b'/');
        self.name.iter().copied().chain(suffix)
    }
}

/// A uniquely named temporary object file, removed on drop unless renamed.
struct TempObject {
    path: std::path::PathBuf,
    file: Option<std::fs::File>,
    renamed: bool,
}

impl TempObject {
    fn create(directory: &Path) -> Result<Self, Error> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.subsec_nanos())
            .unwrap_or_default();
        for _ in 0..100 {
            let path = directory.join(format!(
                "tmp_obj_rgit_{}_{}_{nanos}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        renamed: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(Error::AlreadyExists(directory.to_owned()))
    }
}

impl Drop for TempObject {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.renamed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn make_read_only(file: &std::fs::File) -> Result<(), Error> {
    let mut permissions = file.metadata()?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o444);
    }
    #[cfg(not(unix))]
    permissions.set_readonly(true);
    file.set_permissions(permissions)?;
    Ok(())
}

/// Refuse identities C Git could not parse back: `<`, `>`, newline or NUL in
/// the name or email, an empty name, a negative timestamp or an offset that
/// does not fit `±HHMM`. Values are written verbatim, without C Git's
/// trimming of surrounding punctuation and whitespace.
pub(super) fn validate_signature(signature: &Signature) -> Result<(), Error> {
    let bad = |bytes: &[u8]| bytes.iter().any(|b| matches!(b, b'<' | b'>' | b'\n' | 0));
    if bad(&signature.name) || bad(&signature.email) {
        return Err(invalid_input(
            "identity name or email contains '<', '>', newline or NUL",
        ));
    }
    if signature.name.is_empty() {
        return Err(invalid_input("empty identity name"));
    }
    if signature.seconds < 0 {
        return Err(invalid_input("negative identity timestamp"));
    }
    if signature.offset_minutes.unsigned_abs() > 99 * 60 + 59 {
        return Err(invalid_input("identity timezone offset out of range"));
    }
    Ok(())
}

fn invalid_input(message: &'static str) -> Error {
    Error::Other(Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        message,
    )))
}
