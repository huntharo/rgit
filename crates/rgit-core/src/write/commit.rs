//! OWNER: WP2 (commit/ref publication). Frozen public signatures.
//!
//! Order of a commit: refuse unsupported state, read the `HEAD` target and
//! preflight its reference, compute trees in memory and decide whether there
//! is anything to commit, and only then write and sync tree and commit
//! objects. Publication is a compare-and-swap reference update against the
//! value read first (docs/decisions/0003). Every refusal happens before the
//! first object write; only lock contention or a concurrent reference move
//! discovered at publication can leave new, unreferenced objects behind.

use std::collections::HashSet;

use crate::{Error, ObjectId, ObjectKind, Repository};

use super::refs::{Loose, PublishOptions, RefAnchors, format_signature, read_loose};
use super::{RefExpectation, RefLog, RefUpdate};
use crate::fsx::Anchor;

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
        let common = self.common_anchor()?;
        let plan = self.plan_tree_from_index()?;
        self.write_planned_trees(&common, &plan)?;
        Ok(plan.root)
    }

    /// Validate the index and compute every tree object in memory, without
    /// writing anything.
    fn plan_tree_from_index(&self) -> Result<TreePlan, Error> {
        let index = self.read_index()?;
        let hash = self.inner.object_hash();
        if index.entries().iter().any(|entry| entry.stage_raw() != 0) {
            return Err(Error::UnmergedIndex);
        }
        let mut files = Vec::with_capacity(index.entries().len());
        for entry in index.entries() {
            check_index_entry(entry, hash)?;
            self.require_kind(entry.id, ObjectKind::Blob)?;
            files.push((entry.path(&index), entry.mode.bits(), entry.id));
        }
        self.plan_trees(&files)
    }

    /// Store planned trees bottom-up; each is synced (an existing loose copy
    /// too) by `write_loose_object`.
    fn write_planned_trees(&self, common: &Anchor, plan: &TreePlan) -> Result<(), Error> {
        for (expected, data) in &plan.objects {
            let id = self.write_loose_object_at(common, ObjectKind::Tree, data)?;
            if id != *expected {
                return Err(super::refs::corrupt("tree object hash mismatch"));
            }
        }
        Ok(())
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
        // Both Git directory roots are opened before any read that decides
        // the commit and before the first object write.
        let anchors = RefAnchors::open(self)?;
        self.refuse_in_progress(&anchors.git)?;
        self.refuse_unsupported_commit_config()?;

        let hash = self.inner.object_hash();
        let (reference, parent) = match read_loose(&anchors.git, b"HEAD", hash)? {
            None => return Err(super::refs::corrupt("HEAD is missing")),
            Some(Loose::Direct(id)) => {
                self.preflight_ref(&anchors, b"HEAD", true)?;
                (b"HEAD".to_vec(), Some(id))
            }
            Some(Loose::Symbolic(branch)) => {
                if !branch.starts_with(b"refs/heads/") {
                    return Err(Error::UnsupportedCapability(
                        "HEAD names a reference outside refs/heads/",
                    ));
                }
                // D/F conflicts, symlinked or symbolic refs and unusable
                // reflog paths are refused before any object is written.
                self.preflight_ref(&anchors, &branch, true)?;
                match self.read_ref(&anchors, &branch)? {
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

        // Decide on an in-memory tree so a refusal writes nothing.
        let plan = self.plan_tree_from_index()?;
        let tree = plan.root;
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
        self.write_planned_trees(&anchors.common, &plan)?;
        let id = self.write_loose_object_at(&anchors.common, ObjectKind::Commit, &data)?;
        // The parent was not written by this operation; sync its loose copy
        // (packed objects are durable already) before a ref names the child.
        if let Some(parent) = parent {
            self.sync_loose_object(&anchors, parent)?;
        }
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
        // On a branch, publish_ref holds HEAD.lock across the publication and
        // requires HEAD to still name the branch, so HEAD cannot be re-pointed
        // (for example by a branch switch) in between; HEAD is not touched.
        self.publish_ref(
            &anchors,
            &update,
            PublishOptions {
                locked: "commit.ref-locked",
                head_must_name_ref: reference != b"HEAD",
            },
        )?;
        Ok(CommitOutcome {
            id,
            tree,
            reference,
            parent,
        })
    }

    fn refuse_in_progress(&self, git: &Anchor) -> Result<(), Error> {
        for (name, what) in IN_PROGRESS {
            if git.lstat(name.as_bytes())?.is_some() {
                return Err(Error::UnsupportedCapability(what));
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

    /// Build nested trees from index-ordered `(path, mode, id)` entries in
    /// memory, without recursion (paths may be arbitrarily deep). Trees are
    /// listed bottom-up, each distinct tree once.
    fn plan_trees(&self, files: &[(&gix::bstr::BStr, u32, ObjectId)]) -> Result<TreePlan, Error> {
        let mut plan = TreePlan {
            root: ObjectId::empty_tree(self.inner.object_hash()),
            objects: Vec::new(),
            seen: HashSet::new(),
        };
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
                self.close_level(&mut stack, &mut plan)?;
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
            self.close_level(&mut stack, &mut plan)?;
        }
        let (_, items) = stack.pop().ok_or(Error::Injected("tree stack underflow"))?;
        plan.root = self.plan_tree_object(items, &mut plan)?;
        Ok(plan)
    }

    fn close_level(
        &self,
        stack: &mut Vec<(Vec<u8>, Vec<TreeItem>)>,
        plan: &mut TreePlan,
    ) -> Result<(), Error> {
        let (path, items) = stack.pop().ok_or(Error::Injected("tree stack underflow"))?;
        let id = self.plan_tree_object(items, plan)?;
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

    fn plan_tree_object(
        &self,
        mut items: Vec<TreeItem>,
        plan: &mut TreePlan,
    ) -> Result<ObjectId, Error> {
        // Git orders tree entries as if directory names ended with '/'.
        items.sort_by(|a, b| a.sort_key().cmp(b.sort_key()));
        let mut data = Vec::new();
        for item in &items {
            data.extend_from_slice(format!("{:o} ", item.mode).as_bytes());
            data.extend_from_slice(&item.name);
            data.push(0);
            data.extend_from_slice(item.id.as_slice());
        }
        let id = gix::objs::compute_hash(self.inner.object_hash(), ObjectKind::Tree, &data)?;
        if plan.seen.insert(id) {
            plan.objects.push((id, data));
        }
        Ok(id)
    }
}

/// Tree objects computed from the index, bottom-up, not yet written.
struct TreePlan {
    root: ObjectId,
    objects: Vec<(ObjectId, Vec<u8>)>,
    seen: HashSet<ObjectId>,
}

/// Entries `write_tree_from_index` cannot represent are refused explicitly.
fn check_index_entry(entry: &gix::index::Entry, hash: gix::hash::Kind) -> Result<(), Error> {
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
    Ok(())
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
