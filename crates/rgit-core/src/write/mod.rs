//! Experimental local write operations.
//!
//! Ownership (G1 milestone, see docs/decisions/0003-local-write-lifecycle.md):
//! - `lock`, `failpoint`, this module: integration owner (shared primitives).
//! - `init`, `object`, `index`: repository init, object storage and staging.
//! - `commit`, `refs`: tree/commit creation and reference publication.
//!
//! Invariants every operation preserves:
//! - Capability checks run before the first mutation; unsupported repositories
//!   are refused with a typed error rather than partially written.
//! - Objects are durable before any index or reference names them, and no
//!   published reference ever points at an unavailable object.
//! - Shared files are replaced only via `<path>.lock` then rename; a file is
//!   never rewritten in place. A held lock refuses the operation.

mod commit;
mod failpoint;
mod index;
mod init;
mod lock;
mod object;
mod refs;
mod sys;

pub use crate::fsx::Hardening as FilesystemHardening;
pub use commit::{CommitOutcome, CommitRequest, Signature};
pub use index::{AddOutcome, AddRefusal};
pub use init::{InitOptions, InitPublishedError, ObjectFormat, init};
pub use lock::{
    LockPublishedUnreachableError, LockPublishedUnsyncedError, ObservedTarget,
    RenameOutcomeUncertainError,
};
pub use refs::{
    RefExpectation, RefLog, RefLoggedNotPublishedError, RefPublishedUnreachableError,
    RefPublishedUnsyncedError, RefUpdate, ReflogAppendError,
};

pub(crate) use failpoint::hit as failpoint;
pub(crate) use lock::LockFile;
#[cfg_attr(unix, allow(unused_imports))]
pub(crate) use sys::{open_leaf_nofollow, rename_noreplace};

/// The filesystem guarantee this build's write operations use, selected at
/// compile time: [`FilesystemHardening::Anchored`] on Unix (descriptor-root
/// identity), [`FilesystemHardening::LegacyPath`] elsewhere. There is no
/// runtime fallback between them.
pub fn filesystem_hardening() -> FilesystemHardening {
    crate::fsx::HARDENING
}
