//! OWNER: WP1 (init/add core). Frozen public signatures; bodies are stubs.

use std::path::Path;

use crate::{Error, Repository};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitOptions {
    pub object_format: ObjectFormat,
    pub bare: bool,
    /// Branch name without `refs/heads/`; validated before any write.
    pub initial_branch: Vec<u8>,
}

impl Default for InitOptions {
    fn default() -> Self {
        Self {
            object_format: ObjectFormat::Sha1,
            bare: false,
            initial_branch: b"master".to_vec(),
        }
    }
}

/// Create a new repository at `directory` (created if absent). Refuses with
/// [`Error::AlreadyExists`] when a repository already exists there; does not
/// reinitialize. The Git directory is assembled privately and published with
/// one rename so an interruption never leaves a partially initialized one.
pub fn init(directory: &Path, options: &InitOptions) -> Result<Repository, Error> {
    let _ = (directory, options);
    Err(Error::UnsupportedCapability("init is not implemented yet"))
}
