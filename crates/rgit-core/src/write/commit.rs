//! OWNER: WP2 (commit/ref publication). Frozen public signatures.

use crate::{Error, ObjectId, Repository};

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

impl Repository {
    /// Write the current index as tree objects; refuses unmerged entries.
    pub fn write_tree_from_index(&self) -> Result<ObjectId, Error> {
        Err(Error::UnsupportedCapability(
            "write_tree_from_index is not implemented yet",
        ))
    }

    /// Commit the index on top of HEAD (or as a root commit on an unborn
    /// branch), publishing via compare-and-swap against the HEAD target read
    /// before writing objects.
    pub fn commit(&self, request: &CommitRequest) -> Result<CommitOutcome, Error> {
        let _ = request;
        Err(Error::UnsupportedCapability(
            "commit is not implemented yet",
        ))
    }
}
