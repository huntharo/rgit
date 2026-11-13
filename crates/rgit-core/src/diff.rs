//! Read-only index/tree/worktree comparisons and byte-preserving patches.
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::{BStr, Error, IndexEntry, ObjectId, ObjectKind, Repository, UntrackedMode};

/// A changed path and its Git name-status code (A, D, M, T, or U).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
    pub path: Vec<u8>,
    pub code: u8,
}

struct Version {
    mode: u32,
    id: ObjectId,
    data: Vec<u8>,
}

type WorktreeFilters<'repo> = Option<(
    gix::filter::Pipeline<'repo>,
    gix::worktree::IndexPersistedOrInMemory,
)>;

impl Repository {
    /// Compare HEAD to the index (`cached`) or the index to tracked worktree files.
    /// Rename detection is disabled; conflicts are reported once with code U.
    pub fn diff_entries(&self, cached: bool) -> Result<Vec<DiffEntry>, Error> {
        self.inspection_execution_policy()?;
        let index = self.index_entries()?;
        let conflicts: BTreeSet<_> = index
            .iter()
            .filter(|entry| entry.stage != 0)
            .map(|entry| entry.path.clone())
            .collect();
        let mut changes: BTreeMap<Vec<u8>, u8> = if cached {
            let head = self.diff_head_entries()?;
            crate::status::staged_changes(&head, &index)
                .into_iter()
                .collect()
        } else {
            self.diff_worktree_preflight(&index)?;
            let mut changes = BTreeMap::new();
            for candidate in self.status(UntrackedMode::None)? {
                if candidate.worktree == b' ' || conflicts.contains(&candidate.path) {
                    continue;
                }
                changes.insert(candidate.path, candidate.worktree);
            }
            changes
        };
        for path in conflicts {
            changes.insert(path, b'U');
        }
        Ok(changes
            .into_iter()
            .map(|(path, code)| DiffEntry { path, code })
            .collect())
    }

    /// Render unified patches with three context lines. All data stays in memory;
    /// neither converted blobs nor refreshed index entries are persisted.
    pub fn diff_patch(&self, cached: bool) -> Result<Vec<u8>, Error> {
        let changes = self.diff_entries(cached)?;
        if changes.iter().any(|entry| entry.code == b'U') {
            return Err(Error::UnsupportedCapability("combined conflict patches"));
        }
        let index = self.index_entries()?;
        let index_by_path: BTreeMap<_, _> = index
            .iter()
            .filter(|entry| entry.stage == 0)
            .map(|entry| (entry.path.as_slice(), entry))
            .collect();
        let head = if cached {
            self.diff_head_entries()?
        } else {
            Vec::new()
        };
        let head_by_path: BTreeMap<_, _> = head
            .iter()
            .map(|entry| (entry.path.as_slice(), entry))
            .collect();
        // Initialize lazily: staged diffs never access worktree content or filters.
        let mut filters = None;
        let mut output = Vec::new();
        for change in changes {
            let indexed = index_by_path.get(change.path.as_slice()).copied();
            let old = if cached {
                head_by_path.get(change.path.as_slice()).copied()
            } else {
                // Removed ITA files have the stored empty blob as their old
                // version. Otherwise there would be no version on either side
                // and the deletion header would disappear from the patch.
                indexed.filter(|entry| !entry.intent_to_add || change.code == b'D')
            };
            let old = old.map(|entry| self.diff_blob(entry)).transpose()?;
            let new = if cached {
                indexed
                    .filter(|entry| !entry.intent_to_add)
                    .map(|entry| self.diff_blob(entry))
                    .transpose()?
            } else if change.code == b'D' {
                None
            } else {
                let entry =
                    indexed.ok_or(Error::UnsupportedCapability("diff without index entry"))?;
                self.diff_worktree_version(entry, &mut filters)?
            };
            // A type change is two ordinary patches, which Git can apply. Combining
            // file and symlink modes into a single patch is ambiguous to consumers.
            if old
                .as_ref()
                .zip(new.as_ref())
                .is_some_and(|(a, b)| a.mode & 0o170000 != b.mode & 0o170000)
            {
                render_patch(&mut output, &change.path, old.as_ref(), None)?;
                render_patch(&mut output, &change.path, None, new.as_ref())?;
            } else {
                render_patch(&mut output, &change.path, old.as_ref(), new.as_ref())?;
            }
        }
        Ok(output)
    }

    fn diff_head_entries(&self) -> Result<Vec<IndexEntry>, Error> {
        let id = self.inner.head_tree_id_or_empty()?.detach();
        self.tree_entries(BStr::new(id.to_string().as_bytes()))
    }

    fn diff_blob(&self, entry: &IndexEntry) -> Result<Version, Error> {
        if !matches!(entry.mode, 0o100644 | 0o100755 | 0o120000) {
            return Err(Error::UnsupportedCapability(
                "submodule or unknown-mode patches",
            ));
        }
        let mut object = self.inner.find_object(entry.id)?;
        if object.kind != ObjectKind::Blob {
            return Err(Error::UnexpectedKind {
                expected: ObjectKind::Blob,
                actual: object.kind,
            });
        }
        Ok(Version {
            mode: entry.mode,
            id: entry.id,
            data: std::mem::take(&mut object.data),
        })
    }

    /// Resolve conservative stat-size candidates using the same canonical bytes
    /// as patches. In particular, EOL and ident expansion can change disk size.
    pub(crate) fn canonical_worktree_change<'repo>(
        &'repo self,
        entry: &IndexEntry,
        filters: &mut WorktreeFilters<'repo>,
    ) -> Result<Option<u8>, Error> {
        let old = if entry.intent_to_add {
            None
        } else {
            Some(self.diff_blob(entry)?)
        };
        let new = self.diff_worktree_version(entry, filters)?;
        Ok(match (old.as_ref(), new.as_ref()) {
            (None, Some(_)) => Some(b'A'),
            (Some(_), None) => Some(b'D'),
            (Some(a), Some(b)) if a.mode & 0o170000 != b.mode & 0o170000 => Some(b'T'),
            (Some(a), Some(b)) if a.mode != b.mode || a.data != b.data => Some(b'M'),
            _ => None,
        })
    }

    fn diff_worktree_version<'repo>(
        &'repo self,
        entry: &IndexEntry,
        filters: &mut WorktreeFilters<'repo>,
    ) -> Result<Option<Version>, Error> {
        let root = self.work_dir().ok_or(Error::UnsupportedCapability(
            "worktree diff in bare repositories",
        ))?;
        let relative = native_path(&entry.path)?;
        let path = root.join(&relative);
        if !safe_parents(root, &relative)? {
            return Ok(None);
        }
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let (mode, data) = if meta.file_type().is_symlink() {
            (0o120000, path_bytes(&std::fs::read_link(&path)?)?)
        } else if meta.is_dir() {
            return Ok(None);
        } else if meta.is_file() {
            let config = self.inner.config_snapshot();
            let symlinks = config.plumbing().boolean("core.symlinks")?.unwrap_or(true);
            if entry.mode == 0o120000 && !symlinks {
                (0o120000, std::fs::read(&path)?)
            } else {
                let filemode = config.plumbing().boolean("core.filemode")?.unwrap_or(true);
                let mode = if filemode {
                    regular_mode(&meta)
                } else if entry.mode == 0o100755 {
                    0o100755
                } else {
                    0o100644
                };
                if filters.is_none() {
                    *filters = Some(self.inner.filter_pipeline(None)?);
                }
                let (pipeline, index) = filters
                    .as_mut()
                    .ok_or(Error::UnsupportedCapability("missing conversion pipeline"))?;
                let mut data = Vec::new();
                pipeline
                    .convert_to_git(std::fs::File::open(&path)?, &relative, index)?
                    .read_to_end(&mut data)?;
                (mode, data)
            }
        } else {
            return Err(Error::UnsupportedCapability(
                "special worktree files in diff",
            ));
        };
        let id = gix::objs::compute_hash(self.inner.object_hash(), ObjectKind::Blob, &data)?;
        Ok(Some(Version { mode, id, data }))
    }

    fn diff_worktree_preflight(&self, entries: &[IndexEntry]) -> Result<(), Error> {
        let root = self.work_dir().ok_or(Error::UnsupportedCapability(
            "worktree diff in bare repositories",
        ))?;
        let config = self.inner.config_snapshot();
        for key in ["core.attributesfile", "core.excludesfile"] {
            if config.plumbing().string(key).is_some() {
                return Err(Error::UnsupportedCapability(
                    "custom attributes or excludes files in worktree diff",
                ));
            }
        }
        for git_dir in [self.git_dir(), self.inner.common_dir()] {
            check_metadata_file(&git_dir.join("info"), "attributes")?;
            check_metadata_file(&git_dir.join("info"), "exclude")?;
        }
        for name in [".gitattributes", ".gitignore"] {
            check_metadata_file(root, name)?;
        }
        for entry in entries {
            let relative = native_path(&entry.path)?;
            if !safe_parents(root, &relative)? {
                continue;
            }
            let mut directory = root.to_path_buf();
            if let Some(parent) = relative.parent() {
                for component in parent.components() {
                    directory.push(component);
                    if !directory.is_dir() {
                        break;
                    }
                    for name in [".gitattributes", ".gitignore"] {
                        check_metadata_file(&directory, name)?;
                    }
                }
            }
            match std::fs::symlink_metadata(root.join(relative)) {
                Ok(meta) if !(meta.is_file() || meta.is_dir() || meta.file_type().is_symlink()) => {
                    return Err(Error::UnsupportedCapability(
                        "special worktree files in diff",
                    ));
                }
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error.into());
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn safe_parents(root: &Path, relative: &Path) -> Result<bool, Error> {
    let mut path = root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            path.push(component);
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(Error::UnsupportedCapability(
                        "symlink parents in worktree diff",
                    ));
                }
                Ok(meta) if !meta.is_dir() => return Ok(false),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error.into()),
                _ => {}
            }
        }
    }
    Ok(true)
}

fn check_metadata_file(directory: &Path, name: &str) -> Result<(), Error> {
    match std::fs::symlink_metadata(directory) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(Error::UnsupportedCapability(
                "symlink metadata directories in diff",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        _ => {}
    }
    match std::fs::symlink_metadata(directory.join(name)) {
        Ok(meta) if !meta.is_file() => Err(Error::UnsupportedCapability(
            "non-regular metadata files in diff",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        _ => Ok(()),
    }
}

#[cfg(unix)]
fn native_path(bytes: &[u8]) -> Result<PathBuf, Error> {
    use std::os::unix::ffi::OsStrExt;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}
#[cfg(not(unix))]
fn native_path(bytes: &[u8]) -> Result<PathBuf, Error> {
    Ok(PathBuf::from(
        std::str::from_utf8(bytes).map_err(|_| Error::InvalidPath(bytes.to_vec()))?,
    ))
}
#[cfg(unix)]
fn path_bytes(path: &Path) -> Result<Vec<u8>, Error> {
    use std::os::unix::ffi::OsStrExt;
    Ok(path.as_os_str().as_bytes().to_vec())
}
#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Result<Vec<u8>, Error> {
    Ok(path
        .to_str()
        .ok_or(Error::UnsupportedCapability("non-UTF-8 symlink target"))?
        .as_bytes()
        .to_vec())
}
#[cfg(unix)]
fn regular_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o100 != 0 {
        0o100755
    } else {
        0o100644
    }
}
#[cfg(not(unix))]
fn regular_mode(_: &std::fs::Metadata) -> u32 {
    0o100644
}

fn patch_path(prefix: &[u8], path: &[u8]) -> Vec<u8> {
    let bytes: Vec<_> = prefix.iter().chain(path).copied().collect();
    if bytes
        .iter()
        .all(|byte| (33..127).contains(byte) && ![b'"', b'\\'].contains(byte))
    {
        return bytes;
    }
    let mut quoted = vec![b'"'];
    for byte in bytes {
        match byte {
            b'"' | b'\\' => {
                quoted.push(b'\\');
                quoted.push(byte);
            }
            b'\n' => quoted.extend_from_slice(b"\\n"),
            b'\r' => quoted.extend_from_slice(b"\\r"),
            b'\t' => quoted.extend_from_slice(b"\\t"),
            32..=126 => quoted.push(byte),
            _ => quoted.extend_from_slice(format!("\\{byte:03o}").as_bytes()),
        }
    }
    quoted.push(b'"');
    quoted
}

fn render_patch(
    output: &mut Vec<u8>,
    path: &[u8],
    old: Option<&Version>,
    new: Option<&Version>,
) -> Result<(), Error> {
    if old.is_none() && new.is_none() {
        return Ok(());
    }
    if old
        .zip(new)
        .is_some_and(|(a, b)| a.mode == b.mode && a.data == b.data)
    {
        return Ok(());
    }
    let a = patch_path(b"a/", path);
    let b = patch_path(b"b/", path);
    output.extend_from_slice(b"diff --git ");
    output.extend_from_slice(&a);
    output.push(b' ');
    output.extend_from_slice(&b);
    output.push(b'\n');
    match (old, new) {
        (None, Some(new)) => writeln!(output, "new file mode {:06o}", new.mode)?,
        (Some(old), None) => writeln!(output, "deleted file mode {:06o}", old.mode)?,
        (Some(old), Some(new)) if old.mode != new.mode => {
            writeln!(
                output,
                "old mode {:06o}\nnew mode {:06o}",
                old.mode, new.mode
            )?;
        }
        _ => {}
    }
    let hash = old
        .or(new)
        .ok_or(Error::UnsupportedCapability("empty diff version"))?
        .id
        .kind();
    let old_id = old.map_or(ObjectId::null(hash), |version| version.id);
    let new_id = new.map_or(ObjectId::null(hash), |version| version.id);
    write!(output, "index {old_id}..{new_id}")?;
    if let Some((old, _)) = old.zip(new).filter(|(old, new)| old.mode == new.mode) {
        write!(output, " {:06o}", old.mode)?;
    }
    output.push(b'\n');
    let old_data = old.map_or(&[][..], |version| version.data.as_slice());
    let new_data = new.map_or(&[][..], |version| version.data.as_slice());
    if old_data == new_data {
        return Ok(());
    }
    let old_label = if old.is_some() {
        a.as_slice()
    } else {
        b"/dev/null"
    };
    let new_label = if new.is_some() {
        b.as_slice()
    } else {
        b"/dev/null"
    };
    if old_data
        .iter()
        .take(8000)
        .chain(new_data.iter().take(8000))
        .any(|byte| *byte == 0)
    {
        output.extend_from_slice(b"Binary files ");
        output.extend_from_slice(old_label);
        output.extend_from_slice(b" and ");
        output.extend_from_slice(new_label);
        output.extend_from_slice(b" differ\n");
        return Ok(());
    }
    output.extend_from_slice(b"--- ");
    output.extend_from_slice(old_label);
    output.extend_from_slice(b"\n+++ ");
    output.extend_from_slice(new_label);
    output.push(b'\n');
    let diff = similar::TextDiff::from_lines(old_data, new_data);
    // UnifiedDiff::to_writer uses Display for each hunk in similar 2.7, which
    // replaces invalid UTF-8. Hunk::to_writer writes the original byte slices.
    for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        hunk.to_writer(&mut *output)?;
    }
    Ok(())
}
