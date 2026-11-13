use crate::{BStr, Error, ObjectId, ObjectKind, Repository};

/// A persisted index entry, including conflict stages and intentional opt-outs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub path: Vec<u8>,
    pub mode: u32,
    pub id: ObjectId,
    pub stage: u32,
    pub assume_unchanged: bool,
    pub skip_worktree: bool,
    pub intent_to_add: bool,
}

impl Repository {
    pub fn index_entries(&self) -> Result<Vec<IndexEntry>, Error> {
        let index = self.read_index()?;
        entries(&index)
    }

    /// Flatten a tree, retaining gitlinks and symlinks without visiting worktrees.
    pub fn tree_entries(&self, revision: &BStr) -> Result<Vec<IndexEntry>, Error> {
        let mut id = self.resolve(revision)?;
        for _ in 0..64 {
            let object = self.inner.find_object(id)?;
            match object.kind {
                ObjectKind::Tree => {
                    let tree_index = self.inner.index_from_tree(&id)?;
                    return entries(&tree_index);
                }
                ObjectKind::Commit => id = object.try_to_commit_ref()?.tree(),
                ObjectKind::Tag => id = object.try_to_tag_ref()?.target(),
                actual => {
                    return Err(Error::UnexpectedKind {
                        expected: ObjectKind::Tree,
                        actual,
                    });
                }
            }
        }
        Err(Error::PeelLimit)
    }

    pub(crate) fn read_index(&self) -> Result<gix::index::File, Error> {
        match std::fs::metadata(self.inner.index_path()) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(gix::index::File::from_state(
                    gix::index::State::new(self.inner.object_hash()),
                    self.inner.index_path(),
                ));
            }
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let index = self.inner.open_index()?;
        index.verify_integrity()?;
        index.verify_entries()?;
        if index.link().is_some() {
            return Err(Error::UnsupportedCapability("split index"));
        }
        Ok(index)
    }
}

fn entries(index: &gix::index::State) -> Result<Vec<IndexEntry>, Error> {
    use gix::index::entry::Flags;
    index
        .entries()
        .iter()
        .map(|entry| {
            let path = entry.path(index).to_vec();
            validate_path(&path)?;
            if entry.mode == gix::index::entry::Mode::DIR {
                return Err(Error::UnsupportedCapability(
                    "sparse directory index entries",
                ));
            }
            Ok(IndexEntry {
                path,
                mode: entry.mode.bits(),
                id: entry.id,
                stage: entry.stage_raw(),
                assume_unchanged: entry.flags.contains(Flags::ASSUME_VALID),
                skip_worktree: entry.flags.contains(Flags::SKIP_WORKTREE),
                intent_to_add: entry.flags.contains(Flags::INTENT_TO_ADD),
            })
        })
        .collect()
}

pub(crate) fn validate_path(path: &[u8]) -> Result<(), Error> {
    if path.is_empty()
        || path.contains(&0)
        || path.split(|&byte| byte == b'/').any(|part| {
            part.is_empty() || part == b"." || part == b".." || part.eq_ignore_ascii_case(b".git")
        })
    {
        return Err(Error::InvalidPath(path.to_vec()));
    }
    #[cfg(windows)]
    if path.contains(&b'\\') || path.contains(&b':') {
        return Err(Error::InvalidPath(path.to_vec()));
    }
    Ok(())
}
