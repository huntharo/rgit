//! Repository operations. This crate never invokes a Git executable, shell,
//! hook, filter or helper. Mutating operations live in [`write`] and refuse
//! unsupported repository capabilities before changing any state.

use std::path::Path;

pub use gix::ObjectId;
pub use gix::bstr::BStr;
pub use gix::object::Kind as ObjectKind;

mod diff;
pub use diff::DiffEntry;

mod graph;
pub use graph::GraphLimits;
mod inspection;
pub use inspection::IndexEntry;
mod status;
pub use status::{StatusEntry, UntrackedMode};
mod fsx;
pub mod write;

/// Operation errors retain the underlying Gitoxide error as their source.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    Git(gix::Error),
    UnexpectedKind {
        expected: ObjectKind,
        actual: ObjectKind,
    },
    PeelLimit,
    UnsupportedFormat(i64),
    UnsupportedExtension(String),
    Io(std::io::Error),
    InvalidShallow,
    GraphLimit {
        resource: &'static str,
        limit: usize,
    },
    GraphCycle,
    UnsupportedHistory(&'static str),
    UnsupportedCapability(&'static str),
    InvalidPath(Vec<u8>),
    /// Another writer holds this lock file; nothing was changed.
    Locked(std::path::PathBuf),
    /// The destination already exists and the operation does not overwrite it.
    AlreadyExists(std::path::PathBuf),
    /// Compare-and-swap reference update found a different current value.
    RefConflict {
        name: Vec<u8>,
        expected: Option<ObjectId>,
        actual: Option<ObjectId>,
    },
    /// A referenced object is absent from the object database.
    MissingObject(ObjectId),
    InvalidRefName(Vec<u8>),
    /// The index contains conflict stages, so no tree can be written.
    UnmergedIndex,
    NothingToCommit,
    /// A failure from a Gitoxide sub-crate or another typed source.
    Other(Box<dyn std::error::Error + Send + Sync + 'static>),
    /// A test-only failpoint injected this error (feature `failpoints`).
    Injected(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Git(error) => write!(f, "{error}"),
            Self::UnexpectedKind { expected, actual } => {
                write!(f, "expected {expected}, found {actual}")
            }
            Self::PeelLimit => write!(f, "object peeling exceeded 64 steps"),
            Self::UnsupportedFormat(version) => {
                write!(f, "unsupported repository format version: {version}")
            }
            Self::UnsupportedExtension(name) => {
                write!(f, "unsupported repository extension: {name}")
            }
            Self::Io(error) => write!(f, "I/O failure: {error}"),
            Self::InvalidShallow => write!(f, "invalid shallow boundary object ID"),
            Self::GraphLimit { resource, limit } => {
                write!(f, "commit graph exceeds {resource} limit ({limit})")
            }
            Self::GraphCycle => write!(f, "commit graph contains a cycle"),
            Self::UnsupportedCapability(feature) => write!(f, "unsupported capability: {feature}"),
            Self::InvalidPath(path) => write!(f, "unsafe repository path: {:?}", BStr::new(path)),
            Self::UnsupportedHistory(feature) => {
                write!(f, "unsupported history feature: {feature}")
            }
            Self::Locked(path) => write!(
                f,
                "unable to create '{}': lock file exists; another rgit or git process may be running",
                path.display()
            ),
            Self::AlreadyExists(path) => write!(f, "already exists: {}", path.display()),
            Self::RefConflict {
                name,
                expected,
                actual,
            } => {
                let show = |id: &Option<ObjectId>| {
                    id.map_or_else(|| "(absent)".to_owned(), |id| id.to_string())
                };
                write!(
                    f,
                    "reference {:?} changed concurrently: expected {}, found {}",
                    BStr::new(name),
                    show(expected),
                    show(actual)
                )
            }
            Self::MissingObject(id) => write!(f, "object {id} is missing"),
            Self::InvalidRefName(name) => {
                write!(f, "invalid reference name: {:?}", BStr::new(name))
            }
            Self::UnmergedIndex => write!(f, "index contains unmerged entries"),
            Self::NothingToCommit => write!(f, "nothing to commit"),
            Self::Other(error) => write!(f, "{error}"),
            Self::Injected(name) => write!(f, "injected failure at {name}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Git(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Other(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<gix::Error> for Error {
    fn from(error: gix::Error) -> Self {
        Self::Git(error)
    }
}

/// A repository opened with only repository-local configuration.
///
/// Global configuration, includes and repository-selection environment variables
/// are deliberately outside the initial compatibility contract.
pub struct Repository {
    inner: gix::Repository,
}

/// An object's uncompressed type and byte length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    pub kind: ObjectKind,
    pub size: u64,
}

/// A direct or resolved symbolic reference, without peeling annotated tags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub name: Vec<u8>,
    pub id: ObjectId,
}

impl Repository {
    /// Discover a repository at `directory` or an ancestor without changing cwd.
    pub fn discover(directory: impl AsRef<Path>) -> Result<Self, Error> {
        let inner = gix::discover_opts(
            directory,
            Default::default(),
            gix::open::Options::isolated()
                .bail_if_untrusted(true)
                .strict_config(true),
        )?;
        validate_format(&inner)?;
        Ok(Self { inner })
    }

    pub fn git_dir(&self) -> &Path {
        self.inner.git_dir()
    }

    pub fn work_dir(&self) -> Option<&Path> {
        self.inner.workdir()
    }

    /// A repository-local configuration value as raw bytes, if set.
    pub fn config_bytes(&self, key: &str) -> Option<Vec<u8>> {
        self.inner
            .config_snapshot()
            .string(key)
            .map(|value| value.to_vec())
    }

    pub fn is_bare(&self) -> bool {
        self.inner.is_bare()
    }

    pub fn resolve(&self, revision: &BStr) -> Result<ObjectId, Error> {
        Ok(self.inner.rev_parse_single(revision)?.detach())
    }

    pub fn object_info(&self, revision: &BStr) -> Result<ObjectInfo, Error> {
        let id = self.resolve(revision)?;
        let header = self.inner.find_header(id)?;
        Ok(ObjectInfo {
            kind: header.kind(),
            size: header.size(),
        })
    }

    /// Read raw object data, peeling only when needed to obtain `kind`.
    pub fn object_data(&self, revision: &BStr, kind: ObjectKind) -> Result<Vec<u8>, Error> {
        let mut id = self.resolve(revision)?;
        // Decode fallibly instead of using helpers that assume valid commits/tags.
        // Limit traversal even for corrupt or adversarial object relationships.
        for _ in 0..64 {
            let mut object = self.inner.find_object(id)?;
            if object.kind == kind {
                return Ok(std::mem::take(&mut object.data));
            }
            id = match object.kind {
                ObjectKind::Commit => object.try_to_commit_ref()?.tree(),
                ObjectKind::Tag => object.try_to_tag_ref()?.target(),
                actual => {
                    return Err(Error::UnexpectedKind {
                        expected: kind,
                        actual,
                    });
                }
            };
        }
        Err(Error::PeelLimit)
    }

    /// Collect before returning so an iterator error never becomes partial success.
    pub fn references(&self) -> Result<Vec<Reference>, Error> {
        let platform = self.inner.references()?;
        let mut result = Vec::new();
        for reference in platform.all()? {
            let mut reference = reference?;
            let name = reference.name().as_bstr().to_vec();
            let id = reference.follow_to_object()?.detach();
            // Like `git show-ref`, reject references to absent objects.
            self.inner.find_header(id)?;
            result.push(Reference { name, id });
        }
        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(result)
    }
}

fn validate_format(repo: &gix::Repository) -> Result<(), Error> {
    let snapshot = repo.config_snapshot();
    let config = snapshot.plumbing();
    let version = config.integer("core.repositoryformatversion")?.unwrap_or(0);
    if ![0, 1].contains(&version) {
        return Err(Error::UnsupportedFormat(version));
    }
    for section in config.sections_by_name("extensions").into_iter().flatten() {
        for name in section.value_names() {
            let name = name.to_string().to_ascii_lowercase();
            let historical = ["noop", "preciousobjects", "partialclone", "worktreeconfig"]
                .contains(&name.as_str());
            if version == 0 {
                // Git ignores unknown v0 extensions but rejects known v1-only ones.
                if [
                    "objectformat",
                    "compatobjectformat",
                    "refstorage",
                    "relativeworktrees",
                ]
                .contains(&name.as_str())
                {
                    return Err(Error::UnsupportedExtension(name));
                }
                continue;
            }
            if name == "refstorage"
                && section.header().subsection_name().is_none()
                && config
                    .string("extensions.refstorage")
                    .is_some_and(|value| *value == *BStr::new(b"files"))
            {
                continue;
            }
            if section.header().subsection_name().is_some()
                || !(historical || ["objectformat", "relativeworktrees"].contains(&name.as_str()))
            {
                // No reftable, compatibility object map, or private backend until
                // it has an explicit implementation and conformance contract.
                return Err(Error::UnsupportedExtension(name));
            }
        }
    }
    Ok(())
}
