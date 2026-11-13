//! OWNER: WP1. Durable loose object storage.

use crate::{Error, ObjectId, Repository};

impl Repository {
    /// Store `data` as a blob, returning its ID in the repository's hash.
    /// The object file and its directory are synced before returning.
    pub fn write_blob(&self, data: &[u8]) -> Result<ObjectId, Error> {
        let _ = data;
        Err(Error::UnsupportedCapability(
            "write_blob is not implemented yet",
        ))
    }
}
