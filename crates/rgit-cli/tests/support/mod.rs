use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::{TempDir, tempdir};

pub type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;

pub fn git(root: &Path, args: &[&str]) -> Result<Output> {
    let output = git_command(root).args(args).output()?;
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

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

pub fn rgit(root: &Path, args: &[&str]) -> Result<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_rgit"))
        .arg("-C")
        .arg(root)
        .args(args)
        .output()?)
}

pub fn fixture(hash: &str) -> Result<TempDir> {
    let root = tempdir()?;
    git(
        root.path(),
        &["init", "-b", "main", "--object-format", hash],
    )?;
    fs::write(root.path().join("file.txt"), b"first\n\0binary\xff\n")?;
    git(root.path(), &["add", "file.txt"])?;
    git(root.path(), &["commit", "-m", "first"])?;
    git(root.path(), &["branch", "other"])?;
    git(root.path(), &["tag", "-a", "v1", "-m", "annotated"])?;
    Ok(root)
}

pub fn same(root: &Path, args: &[&str]) -> Result {
    let expected = git_command(root).args(args).output()?;
    let actual = rgit(root, args)?;
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "{args:?}: {}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(actual.stdout, expected.stdout, "{args:?}");
    if expected.status.success() {
        assert!(
            actual.stderr.is_empty(),
            "{args:?}: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
    }
    Ok(())
}
