//! Shared C Git fixture helpers for rgit-core library tests. Integration-owned:
//! add file-local helpers in your own test file rather than editing this one.
#![allow(dead_code)]

use std::error::Error;
use std::path::Path;
use std::process::{Command, Output};

pub type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;

pub fn git_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("no-global-config"))
        .env("GIT_CONFIG_COUNT", "0")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
    command
}

/// Run C Git in `root`, asserting success.
pub fn git(root: &Path, args: &[&str]) -> Result<Output> {
    let output = git_command(root).args(args).output()?;
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

/// `git fsck --strict --no-dangling` must succeed.
pub fn fsck(root: &Path) -> Result {
    git(
        root,
        &["fsck", "--strict", "--no-dangling", "--no-progress"],
    )?;
    Ok(())
}
