//! OWNER: WP2. Compare-and-swap reference publication.

use crate::{Error, ObjectId, Repository};

use super::Signature;

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
    pub fn update_ref(&self, update: &RefUpdate) -> Result<(), Error> {
        let _ = update;
        Err(Error::UnsupportedCapability(
            "update_ref is not implemented yet",
        ))
    }
}
