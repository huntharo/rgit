//! OWNER: WP1. Staging and index publication.

use crate::{BStr, Error, Repository};

/// Paths are worktree-relative native bytes, sorted in index order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddOutcome {
    pub staged: Vec<Vec<u8>>,
    pub removed: Vec<Vec<u8>>,
}

impl Repository {
    /// Stage worktree-relative literal paths (files or directories; `.` is the
    /// whole worktree). Blobs are stored durably first, then the complete new
    /// index is written through `index.lock` and renamed into place.
    pub fn add(&self, paths: &[&BStr]) -> Result<AddOutcome, Error> {
        let _ = paths;
        Err(Error::UnsupportedCapability("add is not implemented yet"))
    }
}
