use std::collections::{BTreeMap, BTreeSet};

use crate::{Error, IndexEntry, Repository};

#[derive(Debug, Clone, Copy, Default)]
pub enum UntrackedMode {
    None,
    #[default]
    Normal,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub path: Vec<u8>,
    pub index: u8,
    pub worktree: u8,
}

impl Repository {
    /// Compare content rather than trusting stat or fsmonitor validity. Never
    /// write the index changes offered by Gitoxide's status iterator.
    pub fn status(&self, untracked: UntrackedMode) -> Result<Vec<StatusEntry>, Error> {
        use gix::status::index_worktree::iter::Summary;
        if self.inner.workdir().is_none() {
            return Err(Error::UnsupportedCapability("status in bare repositories"));
        }
        self.inspection_execution_policy()?;
        let entries = self.index_entries()?;
        self.preflight_status_paths(&entries)?;
        if entries.iter().any(|entry| entry.mode == 0o160000) {
            return Err(Error::UnsupportedCapability("submodule worktree status"));
        }
        let head_id = self.inner.head_tree_id_or_empty()?.detach();
        let head = self.tree_entries(crate::BStr::new(head_id.to_string().as_bytes()))?;
        let staged = staged_changes(&head, &entries);
        let mut result: BTreeMap<Vec<u8>, StatusEntry> = staged
            .into_iter()
            .map(|(path, code)| {
                (
                    path.clone(),
                    StatusEntry {
                        path,
                        index: code,
                        worktree: b' ',
                    },
                )
            })
            .collect();
        let mut conflicts: BTreeMap<Vec<u8>, u8> = BTreeMap::new();
        for entry in &entries {
            if entry.stage > 0 {
                *conflicts.entry(entry.path.clone()).or_default() |= 1 << (entry.stage - 1);
            }
        }
        for (path, stages) in conflicts {
            let xy = match stages {
                1 => *b"DD",
                2 => *b"AU",
                3 => *b"UD",
                4 => *b"UA",
                5 => *b"DU",
                6 => *b"AA",
                _ => *b"UU",
            };
            result.insert(
                path.clone(),
                StatusEntry {
                    path,
                    index: xy[0],
                    worktree: xy[1],
                },
            );
        }
        let mut index = self.read_index()?;
        // Every filesystem mtime represented by gix's unsigned seconds is at or
        // after this timestamp. Even a stat match must take the racy-content
        // comparison path. Explicit user opt-outs remain effective.
        index.set_timestamp(std::time::UNIX_EPOCH.into());
        for entry in index.entries_mut() {
            entry.flags.remove(
                gix::index::entry::Flags::UPTODATE | gix::index::entry::Flags::FSMONITOR_VALID,
            );
        }
        let mode = match untracked {
            UntrackedMode::None => gix::status::UntrackedFiles::None,
            UntrackedMode::Normal => gix::status::UntrackedFiles::Collapsed,
            UntrackedMode::All => gix::status::UntrackedFiles::Files,
        };
        let iter = self
            .inner
            .status(gix::progress::Discard)?
            .index(index.into())
            .untracked_files(mode)
            .index_worktree_submodules(None)
            .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled)
            .into_iter(Vec::<gix::bstr::BString>::new())?;
        for item in iter {
            let item = item?;
            let gix::status::Item::IndexWorktree(item) = item else {
                continue;
            };
            let Some(summary) = item.summary() else {
                continue;
            };
            let mut path = item.rela_path().to_vec();
            if matches!(summary, Summary::Added) {
                if let gix::status::index_worktree::Item::DirectoryContents { entry, .. } = &item
                    && entry.disk_kind.is_some_and(|kind| kind.is_dir())
                {
                    path.push(b'/');
                }
                result.entry(path.clone()).or_insert(StatusEntry {
                    path,
                    index: b'?',
                    worktree: b'?',
                });
                continue;
            }
            let code = match summary {
                Summary::Removed => b'D',
                Summary::TypeChange => b'T',
                Summary::Modified => b'M',
                Summary::IntentToAdd => b'A',
                Summary::Conflict => continue,
                _ => return Err(Error::UnsupportedCapability("worktree rewrite status")),
            };
            let entry = result.entry(path.clone()).or_insert(StatusEntry {
                path,
                index: b' ',
                worktree: b' ',
            });
            entry.worktree = code;
        }
        Ok(result.into_values().collect())
    }

    pub(crate) fn inspection_execution_policy(&self) -> Result<(), Error> {
        let snapshot = self.inner.config_snapshot();
        let config = snapshot.plumbing();
        if config.string("core.attributesfile").is_some()
            || config.string("core.excludesfile").is_some()
        {
            return Err(Error::UnsupportedCapability(
                "custom attribute/ignore paths require a file-access policy",
            ));
        }
        if let Some(sections) = config.sections_by_name("filter") {
            for section in sections {
                for name in section.value_names() {
                    if ["clean", "smudge", "process"]
                        .iter()
                        .any(|key| name.eq_ignore_ascii_case(key))
                    {
                        return Err(Error::UnsupportedCapability(
                            "external conversion filters require an execution grant",
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn preflight_status_paths(&self, entries: &[IndexEntry]) -> Result<(), Error> {
        use std::fs;
        let root = self
            .inner
            .workdir()
            .ok_or(Error::UnsupportedCapability("worktree required"))?;
        for entry in entries {
            crate::inspection::validate_path(&entry.path)?;
            let mut path = root.to_owned();
            let parts: Vec<_> = entry.path.split(|&byte| byte == b'/').collect();
            for (index, part) in parts.iter().enumerate() {
                path.push(gix::path::from_bstr(crate::BStr::new(part))?);
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(value) => value,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        ) =>
                    {
                        break;
                    }
                    Err(error) => return Err(error.into()),
                };
                if index + 1 < parts.len() && metadata.is_symlink() {
                    return Err(Error::UnsupportedCapability("symlinked tracked parent"));
                }
                if !(metadata.is_file() || metadata.is_dir() || metadata.is_symlink()) {
                    return Err(Error::UnsupportedCapability(
                        "special tracked worktree file",
                    ));
                }
            }
        }
        let info_dir = self.inner.common_dir().join("info");
        match fs::symlink_metadata(&info_dir) {
            Ok(metadata) if metadata.is_symlink() || !metadata.is_dir() => {
                return Err(Error::UnsupportedCapability(
                    "symlink or special repository info directory",
                ));
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        for metadata in [
            self.inner.common_dir().join("info/attributes"),
            self.inner.common_dir().join("info/exclude"),
        ] {
            regular_metadata_file(&metadata)?;
        }
        // Metadata loaders can follow symlinks or block on special files. Inspect
        // directory entries without following links before allowing the walk.
        // This conservative scan includes ignored directories; it reads no file
        // content and never descends into another repository or a symlink.
        let mut pending = vec![root.to_owned()];
        let mut seen = 0usize;
        while let Some(directory) = pending.pop() {
            for child in fs::read_dir(directory)? {
                let child = child?;
                seen += 1;
                if seen > 1_000_000 {
                    return Err(Error::UnsupportedCapability(
                        "worktree metadata preflight entry limit",
                    ));
                }
                let name = child.file_name();
                if name == ".git" {
                    continue;
                }
                if name == ".gitattributes" || name == ".gitignore" {
                    regular_metadata_file(&child.path())?;
                }
                if child.file_type()?.is_dir() {
                    let nested = child.path().join(".git");
                    match fs::symlink_metadata(nested) {
                        Ok(_) => continue,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            pending.push(child.path())
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }
        Ok(())
    }
}

fn regular_metadata_file(path: &std::path::Path) -> Result<(), Error> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(metadata) if metadata.is_file() && metadata.len() <= 8 * 1024 * 1024 => Ok(()),
        Ok(_) => Err(Error::UnsupportedCapability(
            "symlink, special or oversized attribute/ignore file",
        )),
    }
}

pub(crate) fn staged_changes(head: &[IndexEntry], index: &[IndexEntry]) -> Vec<(Vec<u8>, u8)> {
    let head: BTreeMap<_, _> = head
        .iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    let staged: BTreeMap<_, _> = index
        .iter()
        .filter(|entry| entry.stage == 0 && !entry.intent_to_add)
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    let conflicted: BTreeSet<_> = index
        .iter()
        .filter(|entry| entry.stage != 0)
        .map(|entry| &entry.path)
        .collect();
    head.keys()
        .chain(staged.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|path| {
            if conflicted.contains(path) {
                return None;
            }
            let code = match (head.get(path), staged.get(path)) {
                (None, Some(_)) => b'A',
                (Some(_), None) => b'D',
                (Some(old), Some(new)) if old.mode & 0o170000 != new.mode & 0o170000 => b'T',
                (Some(old), Some(new)) if old.id != new.id || old.mode != new.mode => b'M',
                _ => return None,
            };
            Some((path.clone(), code))
        })
        .collect()
}
