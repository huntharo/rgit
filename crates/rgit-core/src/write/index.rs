//! OWNER: WP1. Staging and index publication.
//!
//! `add` runs in four phases, and every refusal happens in the first:
//!
//! 1. Preflight: capability/config checks, read the index (twice-checked for
//!    a stable snapshot), expand literal pathspecs, read and hash every file
//!    to stage, apply ignore/attribute policy and build the complete new
//!    index in memory. Nothing is written.
//! 2. Store every new blob durably (failpoint `add.objects-written`).
//! 3. Take `index.lock`, verify the index bytes still equal the snapshot
//!    (otherwise release and retry the whole operation once).
//! 4. Serialize the prepared index into the lock (failpoint
//!    `add.index-locked`), sync, rename and sync the directory.
//!
//! Racy-git protection: stat data is taken from the descriptor the content
//! was read from and verified unchanged across the read. Entries this call
//! does not restage whose mtime is not older than the previous index file
//! are "smudged" (size 0), forcing C Git to compare content, which is a
//! conservative form of Git's `ce_smudge_racily_clean_entry`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use gix::index::entry::{Flags, Mode, Stat};

use crate::fsx::{Anchor, Kind, Meta};
use crate::{BStr, Error, ObjectId, ObjectKind, Repository};

/// Paths are worktree-relative native bytes, sorted in index order.
///
/// `staged` lists paths whose staged content, mode or presence changed
/// (stat-only refreshes are not listed); `removed` lists tracked paths that
/// were deleted from the worktree, or replaced by a directory/file conflict.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddOutcome {
    pub staged: Vec<Vec<u8>>,
    pub removed: Vec<Vec<u8>>,
}

/// Why `add` refused; carried as the source of [`Error::Other`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AddRefusal {
    PathspecNoMatch(Vec<u8>),
    /// Glob characters or `:` magic; only literal paths are supported.
    PathspecMagic(Vec<u8>),
    /// An untracked path that ignore rules exclude was named explicitly.
    Ignored(Vec<u8>),
    BeyondSymlink(Vec<u8>),
    NestedRepository(Vec<u8>),
    Submodule(Vec<u8>),
    SpecialFile(Vec<u8>),
    SkipWorktree(Vec<u8>),
    AssumeUnchanged(Vec<u8>),
    /// Attributes or config require a conversion rgit does not perform.
    Conversion {
        path: Vec<u8>,
        reason: &'static str,
    },
    /// The path differs only in case/normalization from the on-disk or
    /// existing index spelling.
    Spelling(Vec<u8>),
    /// A path changed while it was being read; nothing was staged.
    ChangedWhileReading(Vec<u8>),
    /// The index was replaced concurrently twice in a row.
    IndexChanged,
}

impl std::fmt::Display for AddRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let show = |path: &Vec<u8>| BStr::new(path).to_string();
        match self {
            Self::PathspecNoMatch(path) => {
                write!(f, "pathspec '{}' did not match any files", show(path))
            }
            Self::PathspecMagic(path) => write!(
                f,
                "pathspec '{}' uses glob or magic syntax; only literal paths are supported",
                show(path)
            ),
            Self::Ignored(path) => write!(
                f,
                "path '{}' is ignored by one of your .gitignore files",
                show(path)
            ),
            Self::BeyondSymlink(path) => {
                write!(f, "pathspec '{}' is beyond a symbolic link", show(path))
            }
            Self::NestedRepository(path) => write!(
                f,
                "'{}' is inside or is a nested repository; staging gitlinks is unsupported",
                show(path)
            ),
            Self::Submodule(path) => write!(
                f,
                "'{}' is a submodule (gitlink); staging submodules is unsupported",
                show(path)
            ),
            Self::SpecialFile(path) => write!(
                f,
                "'{}': can only add regular files and symbolic links",
                show(path)
            ),
            Self::SkipWorktree(path) => write!(
                f,
                "'{}' has the skip-worktree bit; sparse staging is unsupported",
                show(path)
            ),
            Self::AssumeUnchanged(path) => write!(
                f,
                "'{}' is marked assume-unchanged; refusing to stage it",
                show(path)
            ),
            Self::Conversion { path, reason } => write!(
                f,
                "'{}' requires {reason}, which rgit does not perform",
                show(path)
            ),
            Self::Spelling(path) => write!(
                f,
                "'{}' differs only in case or Unicode normalization from an existing path",
                show(path)
            ),
            Self::ChangedWhileReading(path) => {
                write!(f, "'{}' changed while being added", show(path))
            }
            Self::IndexChanged => write!(f, "the index was changed concurrently"),
        }
    }
}

impl std::error::Error for AddRefusal {}

fn refuse(refusal: AddRefusal) -> Error {
    Error::Other(Box::new(refusal))
}

#[derive(Clone, Copy)]
struct AddConfig {
    /// `core.symlinks=false` (the C Git default on Windows) only matters for
    /// paths that are, or are tracked as, symbolic links; those are refused.
    symlinks: bool,
    filemode: bool,
    ignorecase: bool,
    precompose: bool,
    autocrlf: bool,
    safecrlf_die: bool,
}

#[derive(Clone, Copy, Default)]
struct Tracked {
    stage0: Option<usize>,
    conflicted: bool,
}

enum Change {
    Stage {
        mode: Mode,
        id: ObjectId,
        stat: Stat,
        symlink: bool,
    },
    Remove,
}

/// Everything decided in preflight, ready to publish.
struct Prepared {
    snapshot: Option<Vec<u8>>,
    state: gix::index::State,
    /// Blobs to make durable before the index names them.
    blobs: Vec<(Vec<u8>, ObjectId, bool)>,
    outcome: AddOutcome,
    changed: bool,
}

impl Repository {
    /// Stage worktree-relative literal paths (files or directories; `.` is the
    /// whole worktree). Blobs are stored durably first, then the complete new
    /// index is written through `index.lock` and renamed into place.
    ///
    /// Untracked ignored files inside a named directory are skipped; naming
    /// an untracked ignored path is refused. Tracked paths missing from the
    /// worktree are removed from the index. Unsupported capabilities are
    /// refused before any object or index write.
    ///
    /// An error after `index.lock` was renamed into place is returned as
    /// [`LockPublishedUnsyncedError`](super::LockPublishedUnsyncedError) (the
    /// new index was written; its durability is unknown) or
    /// [`LockPublishedUnreachableError`](super::LockPublishedUnreachableError)
    /// (the new index was written inside the original `.git` directory, which
    /// may no longer be reachable at that name). Neither means "unchanged",
    /// and neither is retried.
    pub fn add(&self, paths: &[&BStr]) -> Result<AddOutcome, Error> {
        if self.inner.workdir().is_none() {
            return Err(Error::UnsupportedCapability("add in a bare repository"));
        }
        let config = self.add_preflight_config()?;
        let mut pathspecs = Vec::with_capacity(paths.len());
        for path in paths {
            pathspecs.push(normalize_pathspec(path)?);
        }
        if pathspecs.is_empty() {
            return Ok(AddOutcome::default());
        }
        // Every root is opened before the first mutation; all worktree, Git
        // directory and object access below is relative to these handles.
        let anchors = AddAnchors {
            worktree: self.worktree_anchor()?,
            git: self.git_dir_anchor()?,
            common: self.common_anchor()?,
        };
        super::object::check_object_store(&anchors.common)?;
        // The in-progress markers Gitoxide's `Repository::state` looks for,
        // checked through the Git directory anchor (no path lookups).
        for marker in [
            &b"rebase-apply"[..],
            b"rebase-merge",
            b"CHERRY_PICK_HEAD",
            b"MERGE_HEAD",
            b"BISECT_LOG",
            b"REVERT_HEAD",
        ] {
            if anchors.git.lstat(marker)?.is_some() {
                return Err(Error::UnsupportedCapability(
                    "add during an in-progress merge, rebase, cherry-pick or similar operation",
                ));
            }
        }
        for _ in 0..2 {
            if let Some(outcome) = self.add_attempt(&anchors, &pathspecs, config)? {
                return Ok(outcome);
            }
        }
        Err(refuse(AddRefusal::IndexChanged))
    }

    fn add_preflight_config(&self) -> Result<AddConfig, Error> {
        self.inspection_execution_policy()?;
        let snapshot = self.inner.config_snapshot();
        let config = snapshot.plumbing();
        let flag = |key: &str, default: bool| -> Result<bool, Error> {
            Ok(config.boolean(key)?.unwrap_or(default))
        };
        if flag("core.sparseCheckout", false)? || flag("index.sparse", false)? {
            return Err(Error::UnsupportedCapability("sparse checkout"));
        }
        if flag("feature.manyFiles", false)? {
            return Err(Error::UnsupportedCapability(
                "feature.manyFiles (index version 4)",
            ));
        }
        if let Some(version) = config.integer("index.version")?
            && !(2..=3).contains(&version)
        {
            return Err(Error::UnsupportedCapability(
                "index.version other than 2 or 3",
            ));
        }
        let is_input = |key: &str| {
            config
                .string(key)
                .is_some_and(|value| value.eq_ignore_ascii_case(b"input"))
        };
        let is_warn = |key: &str| {
            config
                .string(key)
                .is_some_and(|value| value.eq_ignore_ascii_case(b"warn"))
        };
        let autocrlf = is_input("core.autocrlf") || flag("core.autocrlf", false)?;
        let safecrlf_die = !is_warn("core.safecrlf") && flag("core.safecrlf", false)?;
        Ok(AddConfig {
            symlinks: flag("core.symlinks", true)?,
            filemode: flag("core.fileMode", true)?,
            ignorecase: flag("core.ignoreCase", false)?,
            precompose: flag("core.precomposeUnicode", false)?,
            autocrlf,
            safecrlf_die,
        })
    }

    /// One complete attempt; `Ok(None)` means the index changed between the
    /// preflight snapshot and taking the lock.
    fn add_attempt(
        &self,
        anchors: &AddAnchors,
        pathspecs: &[Vec<u8>],
        config: AddConfig,
    ) -> Result<Option<AddOutcome>, Error> {
        let index_path = self.inner.index_path();
        let Some(prepared) = self.prepare_add(anchors, pathspecs, config)? else {
            return Ok(None);
        };
        for (path, id, symlink) in &prepared.blobs {
            if self.object_is_durable(&anchors.common, id)? {
                continue;
            }
            let (data, _, _) = read_worktree(&anchors.worktree, path, *symlink)?;
            let written = self.write_loose_object_at(&anchors.common, ObjectKind::Blob, &data)?;
            if written != *id {
                return Err(refuse(AddRefusal::ChangedWhileReading(path.clone())));
            }
        }
        super::failpoint("add.objects-written")?;
        let mut lock = super::LockFile::acquire_at(&anchors.git, b"index")?;
        if read_index_bytes(&anchors.git)?.map(|(bytes, _)| bytes) != prepared.snapshot {
            return Ok(None);
        }
        if !prepared.changed {
            return Ok(Some(prepared.outcome));
        }
        let file = gix::index::File::from_state(prepared.state, index_path.clone());
        let mut bytes = Vec::new();
        file.write_to(
            &mut bytes,
            gix::index::write::Options {
                extensions: gix::index::write::Extensions::Given {
                    tree_cache: true,
                    end_of_index_entry: false,
                },
                skip_hash: false,
            },
        )?;
        lock.write_all(&bytes)?;
        super::failpoint("add.index-locked")?;
        // Post-publication errors (`lock::after_publication`) pass through
        // unchanged: their text says the index was written, and `?` ends the
        // operation instead of retrying. Any other error is pre-rename.
        lock.commit()?;
        Ok(Some(prepared.outcome))
    }

    fn prepare_add(
        &self,
        anchors: &AddAnchors,
        pathspecs: &[Vec<u8>],
        config: AddConfig,
    ) -> Result<Option<Prepared>, Error> {
        let wt = &anchors.worktree;
        // Decode exactly the bytes read through the Git directory anchor;
        // the index is never opened by path (a FIFO swapped in cannot block).
        let read = read_index_bytes(&anchors.git)?;
        let file = self.decode_read_index(read.as_ref())?;
        let snapshot = read.map(|(bytes, _)| bytes);
        if file.version() == gix::index::Version::V4 {
            return Err(Error::UnsupportedCapability(
                "writing a version 4 (path-compressed) index",
            ));
        }
        let mut state: gix::index::State = file.into();
        // Like C Git, info/ is read from the common directory only. A
        // symlinked or special info/ is refused rather than read through
        // (or silently treated as absent).
        let (info_exclude, info_attributes) = match anchors.common.lstat(b"info")? {
            None => (None, None),
            Some(meta) if meta.kind == Kind::Dir => (
                read_metadata_leaf(&anchors.common, b"info/exclude")?,
                read_metadata_leaf(&anchors.common, b"info/attributes")?,
            ),
            Some(_) => {
                return Err(Error::UnsupportedCapability(
                    "symlink or special .git/info directory",
                ));
            }
        };

        let mut tracked: BTreeMap<Vec<u8>, Tracked> = BTreeMap::new();
        for (position, entry) in state.entries().iter().enumerate() {
            let slot = tracked.entry(entry.path(&state).to_vec()).or_default();
            if entry.stage_raw() == 0 {
                slot.stage0 = Some(position);
            } else {
                slot.conflicted = true;
            }
        }

        // Every .gitignore/.gitattributes a lookup can consult is read now,
        // without following symlinks or blocking on FIFOs, and served to
        // Gitoxide from memory: it never opens worktree metadata files itself.
        let mut metadata = MetadataSnapshot::new(state.object_hash());
        metadata.prescan(wt, pathspecs, &tracked)?;
        if let Some(bytes) = info_attributes {
            metadata.add_info_attributes(&bytes)?;
        }
        let precious = self
            .inner
            .config_snapshot()
            .plumbing()
            .boolean("gitoxide.parsePrecious")?
            .unwrap_or(false);
        let mut stack = metadata.stack(
            self.work_dir()
                .ok_or(Error::UnsupportedCapability("worktree required"))?,
            config.ignorecase,
            info_exclude.as_deref(),
            precious,
        )?;
        let mut attributes = stack.selected_attribute_matches([
            "text",
            "eol",
            "crlf",
            "ident",
            "filter",
            "working-tree-encoding",
        ]);

        // Expand pathspecs into tracked paths to refresh and new paths.
        let mut selected: BTreeSet<Vec<u8>> = BTreeSet::new();
        let mut fresh: BTreeSet<Vec<u8>> = BTreeSet::new();
        let mut expander = Expander {
            snapshot: &metadata,
            wt,
            state: &state,
            tracked: &tracked,
            stack: &mut stack,
            config,
            selected: &mut selected,
            fresh: &mut fresh,
        };
        for spec in pathspecs {
            expander.expand(spec)?;
        }

        // Read, check and hash everything that will be staged.
        let mut changes: BTreeMap<Vec<u8>, Change> = BTreeMap::new();
        for path in selected.iter().chain(fresh.iter()) {
            let is_tracked = selected.contains(path);
            let probe = probe_path(wt, path, is_tracked)?;
            let Some(kind) = probe else {
                changes.insert(path.clone(), Change::Remove);
                continue;
            };
            let symlink = kind == DiskKind::Symlink;
            let existing = tracked
                .get(path)
                .and_then(|slot| slot.stage0)
                .map(|position| state.entries()[position].mode);
            if !config.symlinks && (symlink || existing == Some(Mode::SYMLINK)) {
                // C Git would stage the link-target text of a plain file as a
                // symlink here; that conversion is not implemented.
                return Err(Error::UnsupportedCapability(
                    "core.symlinks=false for a path that is or was a symbolic link",
                ));
            }
            let (data, stat, executable) = read_worktree(wt, path, symlink)?;
            let mode = if symlink {
                Mode::SYMLINK
            } else {
                file_mode(config, existing, executable)
            };
            if !symlink {
                metadata.check(path, false)?;
                attributes.reset();
                stack
                    .at_entry(BStr::new(path), None, &metadata)?
                    .matching_attributes(&mut attributes);
                if let Some(reason) = conversion_refusal(&attributes, &data, config) {
                    return Err(refuse(AddRefusal::Conversion {
                        path: path.clone(),
                        reason,
                    }));
                }
            }
            let id = gix::objs::compute_hash(state.object_hash(), ObjectKind::Blob, &data)?;
            changes.insert(
                path.clone(),
                Change::Stage {
                    mode,
                    id,
                    stat,
                    symlink,
                },
            );
        }

        // Directory/file replacement, as `git add` does with OK_TO_REPLACE.
        let staged_paths: Vec<Vec<u8>> = changes
            .iter()
            .filter(|(_, change)| matches!(change, Change::Stage { .. }))
            .map(|(path, _)| path.clone())
            .collect();
        for path in &staged_paths {
            let mut displaced = Vec::new();
            for (offset, byte) in path.iter().enumerate() {
                if *byte == b'/' {
                    displaced.push(path[..offset].to_vec());
                }
            }
            let mut prefix = path.clone();
            prefix.push(b'/');
            displaced.extend(
                tracked
                    .range(prefix.clone()..)
                    .take_while(|(candidate, _)| candidate.starts_with(&prefix))
                    .map(|(candidate, _)| candidate.clone()),
            );
            for victim in displaced {
                let Some(slot) = tracked.get(&victim) else {
                    continue;
                };
                check_replaceable(&state, &victim, *slot)?;
                changes.entry(victim).or_insert(Change::Remove);
            }
        }

        // Classify and drop no-op restages.
        let mut outcome = AddOutcome::default();
        let mut invalidate = Vec::new();
        changes.retain(|path, change| {
            let existing = tracked
                .get(path)
                .and_then(|slot| slot.stage0)
                .map(|position| &state.entries()[position]);
            match (change, existing) {
                (Change::Remove, None) => false,
                (Change::Remove, Some(_)) => {
                    outcome.removed.push(path.clone());
                    invalidate.push(path.clone());
                    true
                }
                (Change::Stage { mode, id, stat, .. }, Some(entry))
                    if entry.id == *id
                        && entry.mode == *mode
                        && !entry.flags.contains(Flags::INTENT_TO_ADD) =>
                {
                    entry.stat != *stat
                }
                (Change::Stage { .. }, _) => {
                    outcome.staged.push(path.clone());
                    invalidate.push(path.clone());
                    true
                }
            }
        });

        if config.ignorecase || config.precompose {
            check_spelling(&state, &tracked, &changes, config)?;
        }

        let blobs = changes
            .iter()
            .filter_map(|(path, change)| match change {
                Change::Stage { id, symlink, .. } => Some((path.clone(), *id, *symlink)),
                Change::Remove => None,
            })
            .collect();
        let changed = !changes.is_empty();
        if changed {
            apply_changes(&mut state, &changes, &invalidate, snapshot.is_some());
            state.verify_entries()?;
        }
        Ok(Some(Prepared {
            snapshot,
            state,
            blobs,
            outcome,
            changed,
        }))
    }

    /// True if `id` is already stored durably: an existing loose object is
    /// verified (inflated and hashed) and synced first; packed/alternate
    /// objects are assumed durable.
    fn object_is_durable(&self, common: &Anchor, id: &ObjectId) -> Result<bool, Error> {
        if super::object::existing_loose(&common.sub(b"objects")?, id)? {
            return Ok(true);
        }
        Ok(!id.is_empty_tree() && self.inner.has_object(id))
    }
}

/// The policy roots of one `add`, opened before its first mutation.
struct AddAnchors {
    worktree: Anchor,
    git: Anchor,
    common: Anchor,
}

fn check_replaceable(state: &gix::index::State, path: &[u8], slot: Tracked) -> Result<(), Error> {
    if slot.conflicted {
        return Err(Error::UnmergedIndex);
    }
    if let Some(position) = slot.stage0 {
        let entry = &state.entries()[position];
        if entry.mode == Mode::COMMIT {
            return Err(refuse(AddRefusal::Submodule(path.to_vec())));
        }
        if entry.flags.contains(Flags::SKIP_WORKTREE) {
            return Err(refuse(AddRefusal::SkipWorktree(path.to_vec())));
        }
        if entry.flags.contains(Flags::ASSUME_VALID) {
            return Err(refuse(AddRefusal::AssumeUnchanged(path.to_vec())));
        }
    }
    Ok(())
}

fn apply_changes(
    state: &mut gix::index::State,
    changes: &BTreeMap<Vec<u8>, Change>,
    invalidate: &[Vec<u8>],
    had_index: bool,
) {
    // Racy-git: entries we keep but did not restage, modified in the same
    // second as (or after) the old index was written, must not look clean
    // against the newer index file. Size 0 forces a content comparison.
    let racy_second = had_index.then(|| state.timestamp().unix_seconds());
    for entry in state.entries_mut() {
        if let Some(second) = racy_second
            && entry.stage_raw() == 0
            && entry.mode != Mode::COMMIT
            && i64::from(entry.stat.mtime.secs) >= second
        {
            entry.stat.size = 0;
        }
    }
    state.remove_entries(|_, path, entry| {
        entry.stage_raw() == 0 && changes.contains_key(path.as_ref() as &[u8])
    });
    for (path, change) in changes {
        if let Change::Stage { mode, id, stat, .. } = change {
            state.dangerously_push_entry(*stat, *id, Flags::empty(), *mode, BStr::new(path));
        }
    }
    state.sort_entries();
    // gix-index cannot write resolve-undo data; it is dropped like other
    // caches (untracked cache, fsmonitor) that the writer does not emit.
    state.remove_resolve_undo();
    if let Some(tree) = state.tree_mut() {
        for path in invalidate {
            invalidate_tree(tree, path);
        }
    }
}

/// Git's `cache_tree_invalidate_path`: mark every tree on the way invalid
/// and drop a subtree that a changed leaf now replaces.
fn invalidate_tree(tree: &mut gix::index::extension::Tree, path: &[u8]) {
    tree.num_entries = None;
    match path.iter().position(|byte| *byte == b'/') {
        None => tree.children.retain(|child| child.name.as_slice() != path),
        Some(slash) => {
            if let Some(child) = tree
                .children
                .iter_mut()
                .find(|child| child.name.as_slice() == &path[..slash])
            {
                invalidate_tree(child, &path[slash + 1..]);
            }
        }
    }
}

/// With `core.ignorecase`/`core.precomposeUnicode`, refuse a new path whose
/// case-folded spelling (or that of a leading directory) matches a different
/// existing spelling, and new names that are not precomposed (NFC).
fn check_spelling(
    state: &gix::index::State,
    tracked: &BTreeMap<Vec<u8>, Tracked>,
    changes: &BTreeMap<Vec<u8>, Change>,
    config: AddConfig,
) -> Result<(), Error> {
    let new: Vec<&Vec<u8>> = changes
        .iter()
        .filter(|(path, change)| {
            matches!(change, Change::Stage { .. }) && !tracked.contains_key(path.as_slice())
        })
        .map(|(path, _)| path)
        .collect();
    if config.precompose {
        for path in &new {
            let original = BStr::new(path.as_slice());
            if matches!(
                gix::utils::str::precompose_bstr(original.into()),
                std::borrow::Cow::Owned(_)
            ) {
                return Err(refuse(AddRefusal::Spelling((*path).clone())));
            }
        }
    }
    if !config.ignorecase {
        return Ok(());
    }
    let mut spellings: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut record = |path: &[u8]| -> Result<(), Vec<u8>> {
        let mut prefixes: Vec<&[u8]> = path
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte == b'/')
            .map(|(offset, _)| &path[..offset])
            .collect();
        prefixes.push(path);
        for prefix in prefixes {
            let folded = prefix.to_ascii_lowercase();
            match spellings.get(&folded) {
                Some(existing) if existing.as_slice() != prefix => return Err(prefix.to_vec()),
                Some(_) => {}
                None => {
                    spellings.insert(folded, prefix.to_vec());
                }
            }
        }
        Ok(())
    };
    for entry in state.entries() {
        let path = entry.path(state);
        if changes
            .get(path.as_ref() as &[u8])
            .is_some_and(|change| matches!(change, Change::Remove))
        {
            continue;
        }
        let _ = record(path);
    }
    for path in new {
        if record(path).is_err() {
            return Err(refuse(AddRefusal::Spelling(path.clone())));
        }
    }
    Ok(())
}

/// Builtin conversions that `git add` would apply. Content for which the
/// conversion is the identity is staged verbatim; anything else is refused.
fn conversion_refusal(
    attributes: &gix::attrs::search::Outcome,
    data: &[u8],
    config: AddConfig,
) -> Option<&'static str> {
    use gix::attrs::StateRef;
    let mut text = StateRef::Unspecified;
    let mut crlf = StateRef::Unspecified;
    let mut eol = StateRef::Unspecified;
    let mut ident = StateRef::Unspecified;
    for item in attributes.iter_selected() {
        let state = item.assignment.state;
        match item.assignment.name.as_str() {
            "filter" if !matches!(state, StateRef::Unspecified | StateRef::Unset) => {
                return Some("a filter driver");
            }
            "working-tree-encoding"
                if !matches!(state, StateRef::Unspecified | StateRef::Unset) =>
            {
                return Some("working-tree-encoding conversion");
            }
            "text" => text = state,
            "crlf" => crlf = state,
            "eol" => eol = state,
            "ident" => ident = state,
            _ => {}
        }
    }
    if matches!(ident, StateRef::Set | StateRef::Value(_)) && contains(data, b"$Id") {
        return Some("ident keyword collapsing");
    }
    let text_like = if matches!(text, StateRef::Unspecified) {
        crlf
    } else {
        text
    };
    let converts = match text_like {
        StateRef::Unset => false,
        StateRef::Set | StateRef::Value(_) => true,
        StateRef::Unspecified => {
            matches!(eol, StateRef::Set | StateRef::Value(_)) || config.autocrlf
        }
    };
    if converts {
        if contains(data, b"\r\n") {
            return Some("end-of-line conversion");
        }
        if config.safecrlf_die && data.contains(&b'\n') {
            return Some("a core.safecrlf round-trip check");
        }
    }
    None
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

struct Expander<'a> {
    snapshot: &'a MetadataSnapshot,
    wt: &'a Anchor,
    state: &'a gix::index::State,
    tracked: &'a BTreeMap<Vec<u8>, Tracked>,
    stack: &'a mut gix::worktree::Stack,
    config: AddConfig,
    selected: &'a mut BTreeSet<Vec<u8>>,
    fresh: &'a mut BTreeSet<Vec<u8>>,
}

impl Expander<'_> {
    fn expand(&mut self, spec: &[u8]) -> Result<(), Error> {
        let mut any_tracked = false;
        let prefix = {
            let mut prefix = spec.to_vec();
            if !prefix.is_empty() {
                prefix.push(b'/');
            }
            prefix
        };
        let exact = self.tracked.get(spec).map(|slot| (spec.to_vec(), *slot));
        let under = self
            .tracked
            .range(prefix.clone()..)
            .take_while(|(path, _)| path.starts_with(&prefix))
            .map(|(path, slot)| (path.clone(), *slot));
        for (path, slot) in exact.into_iter().chain(under) {
            any_tracked = true;
            check_replaceable(self.state, &path, slot)?;
            self.selected.insert(path);
        }

        let located = self.locate(spec)?;
        // C Git stages tracked content at or below an ignored directory but
        // then fails ("paths are ignored"); rgit refuses the whole call.
        if self.below_ignored_directory(spec)?
            || (located == Located::Entry(DiskKind::Directory)
                && !spec.is_empty()
                && self.excluded(spec, true)?)
        {
            return Err(refuse(AddRefusal::Ignored(spec.to_vec())));
        }
        match located {
            Located::Missing => {
                if !any_tracked {
                    return Err(refuse(AddRefusal::PathspecNoMatch(spec.to_vec())));
                }
            }
            Located::Special => return Err(refuse(AddRefusal::SpecialFile(spec.to_vec()))),
            Located::Entry(DiskKind::File | DiskKind::Symlink) => {
                // A tracked file matching an ignore pattern is still staged.
                if !self.tracked.contains_key(spec) {
                    if self.excluded(spec, false)? {
                        return Err(refuse(AddRefusal::Ignored(spec.to_vec())));
                    }
                    self.fresh.insert(spec.to_vec());
                }
            }
            Located::Entry(DiskKind::Directory) => {
                if !spec.is_empty() && has_git_marker(self.wt, spec)? {
                    return Err(refuse(AddRefusal::NestedRepository(spec.to_vec())));
                }
                self.walk(spec)?;
            }
        }
        Ok(())
    }

    /// Walk leading components through the worktree anchor, never following
    /// a symlink.
    fn locate(&mut self, spec: &[u8]) -> Result<Located, Error> {
        if spec.is_empty() {
            return Ok(Located::Entry(DiskKind::Directory));
        }
        let parts: Vec<&[u8]> = spec.split(|byte| *byte == b'/').collect();
        let check_spelling = self.config.ignorecase || self.config.precompose;
        for (position, part) in parts.iter().enumerate() {
            let prefix = parts[..=position].join(&b'/');
            let Some(meta) = lstat_absent(self.wt, &prefix)? else {
                return Ok(Located::Missing);
            };
            if check_spelling {
                let parent = if position == 0 {
                    Vec::new()
                } else {
                    parts[..position].join(&b'/')
                };
                if !directory_has_exact_name(self.wt, &parent, part)? {
                    return Err(refuse(AddRefusal::Spelling(spec.to_vec())));
                }
            }
            if position + 1 == parts.len() {
                return Ok(match disk_kind(meta.kind) {
                    Some(kind) => Located::Entry(kind),
                    None => Located::Special,
                });
            }
            match meta.kind {
                Kind::Symlink => return Err(refuse(AddRefusal::BeyondSymlink(spec.to_vec()))),
                Kind::Dir => {}
                _ => return Ok(Located::Missing),
            }
            if let Some(slot) = self.tracked.get(&prefix)
                && let Some(position) = slot.stage0
                && self.state.entries()[position].mode == Mode::COMMIT
            {
                return Err(refuse(AddRefusal::Submodule(prefix)));
            }
            if has_git_marker(self.wt, &prefix)? {
                return Err(refuse(AddRefusal::NestedRepository(prefix)));
            }
        }
        Ok(Located::Missing)
    }

    /// Git semantics: everything below an excluded directory is excluded.
    fn below_ignored_directory(&mut self, path: &[u8]) -> Result<bool, Error> {
        for (offset, byte) in path.iter().enumerate() {
            if *byte == b'/' && self.excluded(&path[..offset], true)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn excluded(&mut self, path: &[u8], is_dir: bool) -> Result<bool, Error> {
        self.snapshot.check(path, is_dir)?;
        let mode = is_dir.then_some(Mode::DIR);
        Ok(self
            .stack
            .at_entry(BStr::new(path), mode, self.snapshot)?
            .is_excluded())
    }

    /// Collect untracked, unignored files and symlinks below `spec`. Tracked
    /// paths are handled separately, so ignored directories still have their
    /// tracked contents refreshed. Untracked special files are skipped, as Git
    /// does during a directory walk.
    fn walk(&mut self, spec: &[u8]) -> Result<(), Error> {
        let mut pending = vec![spec.to_vec()];
        while let Some(directory) = pending.pop() {
            self.snapshot.check(&directory, true)?;
            for (name, kind) in self.wt.read_dir(&directory)? {
                if directory.is_empty() && name == b".git" {
                    continue;
                }
                let mut path = directory.clone();
                if !path.is_empty() {
                    path.push(b'/');
                }
                path.extend_from_slice(&name);
                match kind {
                    Kind::Dir => {
                        if let Some(slot) = self.tracked.get(&path)
                            && let Some(position) = slot.stage0
                            && self.state.entries()[position].mode == Mode::COMMIT
                        {
                            // Gitlinks below a pathspec were refused already.
                            continue;
                        }
                        // A tracked file replaced by a directory is removed by
                        // the tracked pass; its new contents are candidates.
                        if self.excluded(&path, true)? {
                            continue;
                        }
                        crate::inspection::validate_path(&path)?;
                        if has_git_marker(self.wt, &path)? {
                            return Err(refuse(AddRefusal::NestedRepository(path)));
                        }
                        pending.push(path);
                    }
                    Kind::File { .. } | Kind::Symlink => {
                        if self.tracked.contains_key(&path) || self.excluded(&path, false)? {
                            continue;
                        }
                        crate::inspection::validate_path(&path)?;
                        self.fresh.insert(path);
                    }
                    Kind::Other => {}
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Located {
    Missing,
    Special,
    Entry(DiskKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiskKind {
    File,
    Symlink,
    Directory,
}

fn disk_kind(kind: Kind) -> Option<DiskKind> {
    match kind {
        Kind::File { .. } => Some(DiskKind::File),
        Kind::Symlink => Some(DiskKind::Symlink),
        Kind::Dir => Some(DiskKind::Directory),
        Kind::Other => None,
    }
}

/// `lstat` through the anchor, where a missing entry or one below a
/// non-directory is absent. A symlinked ancestor is an error (refusal).
fn lstat_absent(wt: &Anchor, rel: &[u8]) -> Result<Option<Meta>, Error> {
    match wt.lstat(rel) {
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotADirectory => Ok(None),
        other => other,
    }
}

/// Inspect a path to stage. `None` means it no longer exists as a file or
/// symlink (missing, a directory now, or below a non-directory), which for a
/// tracked path stages its removal.
fn probe_path(wt: &Anchor, path: &[u8], tracked: bool) -> Result<Option<DiskKind>, Error> {
    super::failpoint("add.before-descend")?;
    let parts: Vec<&[u8]> = path.split(|byte| *byte == b'/').collect();
    let vanished = || -> Result<Option<DiskKind>, Error> {
        if tracked {
            Ok(None)
        } else {
            Err(refuse(AddRefusal::ChangedWhileReading(path.to_vec())))
        }
    };
    for position in 0..parts.len() {
        let prefix = parts[..=position].join(&b'/');
        let Some(meta) = lstat_absent(wt, &prefix)? else {
            return vanished();
        };
        if position + 1 < parts.len() {
            match meta.kind {
                Kind::Symlink => return Err(refuse(AddRefusal::BeyondSymlink(path.to_vec()))),
                Kind::Dir => continue,
                _ => return vanished(),
            }
        }
        return match disk_kind(meta.kind) {
            Some(DiskKind::Directory) => vanished(),
            Some(kind) => Ok(Some(kind)),
            None => Err(refuse(AddRefusal::SpecialFile(path.to_vec()))),
        };
    }
    vanished()
}

/// Read a regular file's bytes or a symlink's target through the worktree
/// anchor, returning stat data that describes exactly the bytes read and,
/// for a file, its executable bit as seen on the descriptor that was read.
/// The identity and timestamps must be unchanged across the read; otherwise
/// the path is refused.
fn read_worktree(wt: &Anchor, path: &[u8], symlink: bool) -> Result<(Vec<u8>, Stat, bool), Error> {
    let changed = || refuse(AddRefusal::ChangedWhileReading(path.to_vec()));
    let before = lstat_absent(wt, path)?.ok_or_else(changed)?;
    if symlink {
        if before.kind != Kind::Symlink {
            return Err(changed());
        }
        let target = wt.read_link(path)?;
        if lstat_absent(wt, path)? != Some(before) {
            return Err(changed());
        }
        return Ok((target, stat_of_link(&before), false));
    }
    if !matches!(before.kind, Kind::File { .. }) {
        return Err(changed());
    }
    super::failpoint("add.before-leaf-open")?;
    // Never follow a symlink or wait on a FIFO swapped in after the lstat:
    // the open fails (symlink) or returns at once and is refused, and the
    // descriptor must then be the same regular file the lstat saw.
    let (mut file, opened) = match wt.open_read(path) {
        Ok(opened) => opened,
        Err(error) => {
            return Err(match lstat_absent(wt, path) {
                Ok(Some(meta)) if !matches!(meta.kind, Kind::File { .. }) => changed(),
                Ok(None) => changed(),
                _ => error,
            });
        }
    };
    if opened != before {
        return Err(changed());
    }
    let mut data = Vec::with_capacity(usize::try_from(opened.len).unwrap_or(0));
    file.read_to_end(&mut data)?;
    let after = file.metadata()?;
    if !unchanged_since(&opened, &after) || data.len() as u64 != after.len() {
        return Err(changed());
    }
    Ok((data, stat_of(&after)?, executable(&after)))
}

/// Like Git without a trusted executable bit, keep a regular entry's mode
/// (otherwise 100644); with one, use the bit seen on the read descriptor.
fn file_mode(config: AddConfig, existing: Option<Mode>, executable: bool) -> Mode {
    if !config.filemode {
        return match existing {
            Some(mode) if mode == Mode::FILE_EXECUTABLE => Mode::FILE_EXECUTABLE,
            _ => Mode::FILE,
        };
    }
    if executable {
        Mode::FILE_EXECUTABLE
    } else {
        Mode::FILE
    }
}

/// Git's executable test (`S_IXUSR`) on the descriptor's own metadata.
#[cfg(unix)]
fn executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o100 != 0
}

#[cfg(not(unix))]
fn executable(_metadata: &fs::Metadata) -> bool {
    false
}

/// The descriptor's metadata after the read still matches the [`Meta`] of
/// the open (identity, size and both timestamps).
#[cfg(unix)]
fn unchanged_since(opened: &Meta, after: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    opened.id
        == (crate::fsx::FileId {
            volume: after.dev(),
            index: u128::from(after.ino()),
        })
        && opened.len == after.len()
        && opened.mtime
            == (
                after.mtime(),
                after.mtime_nsec().clamp(0, 999_999_999) as u32,
            )
        && opened.ctime
            == (
                after.ctime(),
                after.ctime_nsec().clamp(0, 999_999_999) as u32,
            )
}

/// Legacy backend (no inode identity): size and modification time must
/// match; an unreadable timestamp counts as changed, never a size-only check.
#[cfg(not(unix))]
fn unchanged_since(opened: &Meta, after: &fs::Metadata) -> bool {
    since_epoch(after.modified()).is_ok_and(|modified| {
        opened.len == after.len()
            && opened.mtime == (modified.as_secs() as i64, modified.subsec_nanos())
    })
}

#[cfg(not(unix))]
fn since_epoch(time: std::io::Result<std::time::SystemTime>) -> Result<std::time::Duration, Error> {
    time?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| Error::Other(Box::new(error)))
}

/// Index stat data as C Git records it (`st_ctime`, not birth time).
#[cfg(unix)]
fn stat_of(metadata: &fs::Metadata) -> Result<Stat, Error> {
    use gix::index::entry::stat::Time;
    use std::os::unix::fs::MetadataExt;
    // Truncation to 32 bits matches Git's on-disk index fields.
    Ok(Stat {
        mtime: Time {
            secs: metadata.mtime() as u32,
            nsecs: metadata.mtime_nsec() as u32,
        },
        ctime: Time {
            secs: metadata.ctime() as u32,
            nsecs: metadata.ctime_nsec() as u32,
        },
        dev: metadata.dev() as u32,
        ino: metadata.ino() as u32,
        uid: metadata.uid(),
        gid: metadata.gid(),
        size: metadata.len() as u32,
    })
}

#[cfg(not(unix))]
fn stat_of(metadata: &fs::Metadata) -> Result<Stat, Error> {
    use gix::index::entry::stat::Time;
    let time = |time: std::io::Result<std::time::SystemTime>| -> Result<Time, Error> {
        let duration = since_epoch(time)?;
        Ok(Time {
            secs: duration.as_secs() as u32,
            nsecs: duration.subsec_nanos(),
        })
    };
    Ok(Stat {
        mtime: time(metadata.modified())?,
        ctime: time(metadata.created())?,
        size: metadata.len() as u32,
        ..Stat::default()
    })
}

/// Stat data for a symlink entry from its no-follow `lstat` through the
/// anchor. Ownership is not part of [`Meta`]; it is recorded as 0, which only
/// makes C Git re-check the link's content.
fn stat_of_link(meta: &Meta) -> Stat {
    use gix::index::entry::stat::Time;
    // Truncation to 32 bits matches Git's on-disk index fields.
    Stat {
        mtime: Time {
            secs: meta.mtime.0 as u32,
            nsecs: meta.mtime.1,
        },
        ctime: Time {
            secs: meta.ctime.0 as u32,
            nsecs: meta.ctime.1,
        },
        dev: meta.id.volume as u32,
        ino: meta.id.index as u32,
        uid: 0,
        gid: 0,
        size: meta.len as u32,
    }
}

/// Literal worktree-relative pathspec to a normalized index path; `.` (the
/// whole worktree) becomes the empty path.
fn normalize_pathspec(raw: &BStr) -> Result<Vec<u8>, Error> {
    let raw: &[u8] = raw.as_ref();
    let invalid = || Error::InvalidPath(raw.to_vec());
    if raw.is_empty() || raw.starts_with(b"/") {
        return Err(invalid());
    }
    if raw.starts_with(b":") || raw.iter().any(|byte| b"*?[\\".contains(byte)) {
        return Err(refuse(AddRefusal::PathspecMagic(raw.to_vec())));
    }
    let mut parts: Vec<&[u8]> = raw.split(|byte| *byte == b'/').collect();
    while parts.len() > 1 && parts.last().is_some_and(|part| part.is_empty()) {
        parts.pop();
    }
    while parts.len() > 1 && parts.first() == Some(&&b"."[..]) {
        parts.remove(0);
    }
    if parts == [&b"."[..]] {
        return Ok(Vec::new());
    }
    if parts
        .iter()
        .any(|part| part.is_empty() || *part == b"." || *part == b"..")
    {
        return Err(invalid());
    }
    let path = parts.join(&b'/');
    crate::inspection::validate_path(&path)?;
    Ok(path)
}

fn join(directory: &[u8], name: &[u8]) -> Vec<u8> {
    if directory.is_empty() {
        name.to_vec()
    } else {
        [directory, b"/", name].concat()
    }
}

fn directory_has_exact_name(wt: &Anchor, directory: &[u8], name: &[u8]) -> Result<bool, Error> {
    Ok(wt
        .read_dir(directory)?
        .iter()
        .any(|(entry, _)| entry.as_slice() == name))
}

/// A `.git` entry of any kind inside `directory` marks a nested repository.
fn has_git_marker(wt: &Anchor, directory: &[u8]) -> Result<bool, Error> {
    Ok(lstat_absent(wt, &join(directory, b".git"))?.is_some())
}

/// Largest `.gitignore`/`.gitattributes` file accepted (as for status).
const METADATA_LIMIT: u64 = 8 * 1024 * 1024;

/// The `.gitignore`/`.gitattributes` content of every directory an ignore or
/// attribute lookup of this `add` can consult, read before any lookup with
/// `Anchor::open_read` through the worktree anchor (no-follow, non-blocking)
/// and validated on the opened descriptor:
/// a symlink, FIFO, special or oversized file is refused, never followed or
/// waited on. Gitoxide receives the bytes through id mappings and this
/// in-memory object source, so a later swap of a metadata file cannot affect
/// or block the lookup. A lookup in a directory that was not pre-read is an
/// error rather than a silent "no patterns".
struct MetadataSnapshot {
    hash: gix::hash::Kind,
    scanned: HashSet<Vec<u8>>,
    files: BTreeMap<Vec<u8>, ObjectId>,
    blobs: HashMap<ObjectId, Vec<u8>>,
}

impl MetadataSnapshot {
    fn new(hash: gix::hash::Kind) -> Self {
        Self {
            hash,
            scanned: HashSet::new(),
            files: BTreeMap::new(),
            blobs: HashMap::new(),
        }
    }

    /// Pre-read the root, every ancestor of each pathspec and of each tracked
    /// path below it, and every directory in a pathspec's subtree (each
    /// nested repository's own files, but not below it).
    fn prescan(
        &mut self,
        wt: &Anchor,
        pathspecs: &[Vec<u8>],
        tracked: &BTreeMap<Vec<u8>, Tracked>,
    ) -> Result<(), Error> {
        self.read_directory(wt, b"")?;
        let mut seen = 0usize;
        for spec in pathspecs {
            if !self.read_ancestors(wt, spec, true)? {
                continue;
            }
            let mut prefix = spec.clone();
            if !prefix.is_empty() {
                prefix.push(b'/');
            }
            let under: Vec<Vec<u8>> = tracked
                .range(prefix.clone()..)
                .take_while(|(path, _)| path.starts_with(&prefix))
                .map(|(path, _)| path.clone())
                .collect();
            for path in under {
                self.read_ancestors(wt, &path, false)?;
            }
            if !spec.is_empty()
                && !matches!(lstat_absent(wt, spec)?, Some(meta) if meta.kind == Kind::Dir)
            {
                continue;
            }
            let mut pending = vec![spec.clone()];
            while let Some(directory) = pending.pop() {
                for (name, kind) in wt.read_dir(&directory)? {
                    seen += 1;
                    if seen > 1_000_000 {
                        return Err(Error::UnsupportedCapability(
                            "worktree metadata preflight entry limit",
                        ));
                    }
                    if (directory.is_empty() && name == b".git") || kind != Kind::Dir {
                        continue;
                    }
                    let path = join(&directory, &name);
                    self.read_directory(wt, &path)?;
                    if !has_git_marker(wt, &path)? {
                        pending.push(path);
                    }
                }
            }
        }
        Ok(())
    }

    /// Read the leading directories of `path` (and `path` itself when
    /// `include_self`). A component that is missing, not a directory or a
    /// symlink is recorded as having no metadata, and nothing below it is
    /// read; returns whether every component was a real directory.
    fn read_ancestors(
        &mut self,
        wt: &Anchor,
        path: &[u8],
        include_self: bool,
    ) -> Result<bool, Error> {
        if path.is_empty() {
            return Ok(true);
        }
        let parts: Vec<&[u8]> = path.split(|byte| *byte == b'/').collect();
        let count = if include_self {
            parts.len()
        } else {
            parts.len() - 1
        };
        for position in 0..count {
            let directory = parts[..=position].join(&b'/');
            let real =
                matches!(lstat_absent(wt, &directory)?, Some(meta) if meta.kind == Kind::Dir);
            if real {
                self.read_directory(wt, &directory)?;
            } else {
                // Lookups may still name it; it holds no metadata files.
                for rest in position..parts.len() {
                    self.scanned.insert(parts[..=rest].join(&b'/'));
                }
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn read_directory(&mut self, wt: &Anchor, directory: &[u8]) -> Result<(), Error> {
        if !self.scanned.insert(directory.to_vec()) {
            return Ok(());
        }
        for name in [&b".gitignore"[..], b".gitattributes"] {
            let relative = join(directory, name);
            let Some(bytes) = read_metadata_leaf(wt, &relative)? else {
                continue;
            };
            self.insert(relative, bytes)?;
        }
        Ok(())
    }

    fn insert(&mut self, relative: Vec<u8>, bytes: Vec<u8>) -> Result<(), Error> {
        let id = gix::objs::compute_hash(self.hash, ObjectKind::Blob, &bytes)?;
        self.files.insert(relative, id);
        self.blobs.insert(id, bytes);
        Ok(())
    }

    /// Gitoxide loads `info/attributes` right after the root `.gitattributes`
    /// as a never-popped list. Appending it to the root list reproduces that
    /// precedence exactly while serving it from memory.
    fn add_info_attributes(&mut self, info: &[u8]) -> Result<(), Error> {
        let mut root = self
            .files
            .get(&b".gitattributes"[..])
            .and_then(|id| self.blobs.get(id))
            .cloned()
            .unwrap_or_default();
        if !root.is_empty() && !root.ends_with(b"\n") {
            root.push(b'\n');
        }
        root.extend_from_slice(info);
        self.insert(b".gitattributes".to_vec(), root)
    }

    /// The attribute and ignore stack over this snapshot: per-directory files
    /// through id mappings, `info/exclude` as an in-memory global list. No
    /// file is opened by Gitoxide; `root` only labels the virtual stack.
    fn stack(
        &self,
        root: &Path,
        ignorecase: bool,
        info_exclude: Option<&[u8]>,
        precious: bool,
    ) -> Result<gix::worktree::Stack, Error> {
        use gix::worktree::stack::state;
        let case = if ignorecase {
            gix::glob::pattern::Case::Fold
        } else {
            gix::glob::pattern::Case::Sensitive
        };
        let parse = gix::ignore::search::Ignore {
            support_precious: precious,
        };
        let mut buf = Vec::new();
        let mut collection = gix::attrs::search::MetadataCollection::default();
        let globals = gix::attrs::Search::new_globals(
            std::iter::empty::<PathBuf>(),
            &mut buf,
            &mut collection,
        )?;
        let attributes = state::Attributes::new(
            globals,
            None,
            state::attributes::Source::IdMapping,
            collection,
        );
        let mut excludes = gix::ignore::Search::default();
        if let Some(bytes) = info_exclude {
            excludes.add_patterns_buffer(bytes, "info/exclude", None, parse)?;
        }
        let ignore = state::Ignore::new(
            gix::ignore::Search::default(),
            excludes,
            None,
            state::ignore::Source::IdMapping,
            parse,
        );
        let state = gix::worktree::stack::State::AttributesAndIgnoreStack { attributes, ignore };
        let index = self.index();
        let mappings = state.id_mappings_from_index(&index, index.path_backing(), case);
        Ok(gix::worktree::Stack::new(root, state, case, buf, mappings))
    }

    /// Every directory a lookup of `path` consults must have been pre-read.
    fn check(&self, path: &[u8], is_dir: bool) -> Result<(), Error> {
        let mut directories = vec![&path[..0]];
        for (offset, byte) in path.iter().enumerate() {
            if *byte == b'/' {
                directories.push(&path[..offset]);
            }
        }
        if is_dir && !path.is_empty() {
            directories.push(path);
        }
        if directories
            .iter()
            .all(|directory| self.scanned.contains(*directory))
        {
            Ok(())
        } else {
            Err(Error::UnsupportedCapability(
                "ignore/attribute lookup outside the pre-read directories",
            ))
        }
    }

    /// A synthetic index naming each pre-read file, from which Gitoxide
    /// derives its path-to-blob mappings.
    fn index(&self) -> gix::index::State {
        let mut index = gix::index::State::new(self.hash);
        for (path, id) in &self.files {
            index.dangerously_push_entry(
                Stat::default(),
                *id,
                Flags::empty(),
                Mode::FILE,
                BStr::new(path),
            );
        }
        index.sort_entries();
        index
    }
}

impl gix::objs::Find for MetadataSnapshot {
    fn try_find<'a>(
        &self,
        id: &gix::hash::oid,
        buffer: &'a mut Vec<u8>,
    ) -> gix::Result<Option<gix::objs::Data<'a>>> {
        let Some(bytes) = self.blobs.get(id) else {
            return Ok(None);
        };
        buffer.clear();
        buffer.extend_from_slice(bytes);
        Ok(Some(gix::objs::Data {
            kind: ObjectKind::Blob,
            object_hash: self.hash,
            data: buffer.as_slice(),
        }))
    }
}

/// Read one metadata file through `anchor` without following a symlink
/// (final or ancestor) or blocking on a FIFO. Absent is `None`; anything but
/// a regular file within the size limit, as seen on the opened handle, is
/// refused.
fn read_metadata_leaf(anchor: &Anchor, rel: &[u8]) -> Result<Option<Vec<u8>>, Error> {
    let refused =
        || Error::UnsupportedCapability("symlink, special or oversized attribute/ignore file");
    let (file, meta) = match anchor.open_read(rel) {
        Ok(opened) => opened,
        Err(error) => {
            return match lstat_absent(anchor, rel)? {
                None => Ok(None),
                Some(meta) if !matches!(meta.kind, Kind::File { .. }) => Err(refused()),
                Some(_) => Err(error),
            };
        }
    };
    if meta.len > METADATA_LIMIT {
        return Err(refused());
    }
    let mut bytes = Vec::new();
    file.take(METADATA_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > METADATA_LIMIT {
        return Err(refused());
    }
    Ok(Some(bytes))
}

/// Raw index bytes and modification time, read through the Git directory
/// anchor (no-follow, non-blocking, regular files only); `None` if there is
/// no index.
fn read_index_bytes(git: &Anchor) -> Result<Option<(Vec<u8>, std::time::SystemTime)>, Error> {
    match git.lstat(b"index")? {
        None => return Ok(None),
        Some(meta) if !matches!(meta.kind, Kind::File { .. }) => {
            return Err(Error::UnsupportedCapability(
                "symlink or special index file",
            ));
        }
        Some(_) => {}
    }
    let (mut file, _) = match git.open_read(b"index") {
        Ok(opened) => opened,
        Err(_) if git.lstat(b"index")?.is_none() => return Ok(None),
        Err(Error::UnsupportedCapability(_)) => {
            return Err(Error::UnsupportedCapability(
                "symlink or special index file",
            ));
        }
        Err(error) => return Err(error),
    };
    let modified = file.metadata()?.modified()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some((bytes, modified)))
}

impl Repository {
    /// The index as read through the Git directory anchor (an absent index
    /// is empty); never opened by path.
    pub(super) fn read_index_anchored(&self, git: &Anchor) -> Result<gix::index::File, Error> {
        let read = read_index_bytes(git)?;
        self.decode_read_index(read.as_ref())
    }

    fn decode_read_index(
        &self,
        read: Option<&(Vec<u8>, std::time::SystemTime)>,
    ) -> Result<gix::index::File, Error> {
        match read {
            Some((bytes, modified)) => self.decode_index(bytes, *modified),
            None => Ok(gix::index::File::from_state(
                gix::index::State::new(self.inner.object_hash()),
                self.inner.index_path(),
            )),
        }
    }
}
