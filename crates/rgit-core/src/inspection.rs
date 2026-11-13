use crate::{BStr, Error, ObjectId, ObjectKind, Repository};
use std::io::Read;

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
                    validate_entries(&tree_index)?;
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
        let path = self.inner.index_path();
        let mut file = match std::fs::File::open(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(gix::index::File::from_state(
                    gix::index::State::new(self.inner.object_hash()),
                    path,
                ));
            }
            Err(error) => return Err(error.into()),
            Ok(file) => file,
        };
        let timestamp = file.metadata()?.modified()?.into();
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        // Decode the bytes we verify, without File::at() transparently loading
        // and dissolving a split index. Never let index.skipHash disable checking
        // a present checksum. An all-zero trailer is Git's documented omission.
        let hash = self.inner.object_hash();
        let (state, checksum) = gix::index::State::from_bytes(
            &data,
            timestamp,
            hash,
            gix::index::decode::Options::default(),
        )?;
        if let Some(checksum) = checksum {
            let mut hasher = gix::hash::hasher(hash);
            hasher.update(&data[..data.len() - hash.len_in_bytes()]);
            hasher
                .try_finalize()
                .map_err(gix::Error::from_error)?
                .verify(&checksum)?;
        }
        if state.link().is_some() {
            return Err(Error::UnsupportedCapability("split index"));
        }
        if state.is_sparse() {
            return Err(Error::UnsupportedCapability("sparse index"));
        }
        validate_entries(&state)?;
        Ok(gix::index::File::from_state(state, path))
    }
}

fn validate_entries(index: &gix::index::State) -> Result<(), Error> {
    index.verify_entries()?;
    let mut paths = std::collections::BTreeMap::<&[u8], u8>::new();
    for entry in index.entries() {
        let path = entry.path(index);
        validate_path(path)?;
        let stage = 1 << entry.stage_raw();
        let prior = paths.entry(path).or_default();
        if *prior != 0 && (*prior | stage) & 1 != 0 {
            return Err(
                gix::error::corruption("Index path mixes stage zero and conflict stages")
                    .corrupted_error()
                    .into(),
            );
        }
        *prior |= stage;
        if !matches!(entry.mode.bits(), 0o100644 | 0o100755 | 0o120000 | 0o160000) {
            return Err(gix::error::corruption("Invalid index entry mode")
                .corrupted_error()
                .into());
        }
    }
    // A file and its child may coexist in different conflict stages, but never
    // within one stage (or with an unconflicted file/directory prefix).
    for (path, stages) in &paths {
        for (offset, byte) in path.iter().enumerate() {
            if *byte == b'/'
                && let Some(parent_stages) = paths.get(&path[..offset])
                && (stages & parent_stages != 0 || (stages | parent_stages) & 1 != 0)
            {
                return Err(gix::error::corruption(
                    "Index contains a file/directory conflict in one stage",
                )
                .corrupted_error()
                .into());
            }
        }
    }
    Ok(())
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
            gix::validate::path::component(
                BStr::new(part),
                None,
                gix::validate::path::component::Options {
                    protect_windows: cfg!(windows),
                    protect_hfs: cfg!(target_os = "macos"),
                    protect_ntfs: cfg!(windows),
                },
            )
            .is_err()
        })
    {
        return Err(Error::InvalidPath(path.to_vec()));
    }
    Ok(())
}
