//! `core.symlinks=false` (the C Git default on Windows): regular paths stage
//! exactly as C Git stages them, while any path that is, or is tracked as, a
//! symbolic link refuses the whole `add` before an object or the index is
//! written. Fixtures are built with C Git; the comparison is C Git's own index.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use common::{Result, fsck, git, git_command};
use rgit_core::{BStr, Error, Repository};
use tempfile::{TempDir, tempdir};

const HASHES: [&str; 2] = ["sha1", "sha256"];

fn repo(hash: &str) -> Result<TempDir> {
    let root = tempdir()?;
    git(
        root.path(),
        &["init", "-q", "-b", "main", "--object-format", hash],
    )?;
    git(root.path(), &["config", "core.symlinks", "false"])?;
    git(root.path(), &["config", "maintenance.auto", "false"])?;
    Ok(root)
}

fn add(root: &Path, paths: &[&str]) -> std::result::Result<(), Error> {
    let repo = Repository::discover(root)?;
    let paths: Vec<&BStr> = paths
        .iter()
        .map(|path| BStr::new(path.as_bytes()))
        .collect();
    repo.add(&paths).map(|_| ())
}

fn staged(root: &Path) -> Result<Vec<u8>> {
    Ok(git(root, &["ls-files", "-s"])?.stdout)
}

/// Index bytes (absent = `None`) and the set of loose object files.
fn snapshot(root: &Path) -> Result<(Option<Vec<u8>>, BTreeSet<PathBuf>)> {
    let index = match fs::read(root.join(".git/index")) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut objects = BTreeSet::new();
    for fanout in fs::read_dir(root.join(".git/objects"))? {
        let fanout = fanout?.path();
        let name = fanout.file_name().map(|n| n.to_string_lossy().into_owned());
        if matches!(name.as_deref(), Some("info" | "pack")) {
            continue;
        }
        for object in fs::read_dir(&fanout)? {
            objects.insert(object?.path());
        }
    }
    Ok((index, objects))
}

fn refused(result: std::result::Result<(), Error>) -> Result {
    match result {
        Err(Error::UnsupportedCapability(reason)) if reason.contains("core.symlinks=false") => {
            Ok(())
        }
        other => Err(format!("expected a core.symlinks=false refusal, got {other:?}").into()),
    }
}

#[test]
fn regular_paths_stage_like_c_git() -> Result {
    for hash in HASHES {
        let ours = repo(hash)?;
        let theirs = repo(hash)?;
        for root in [ours.path(), theirs.path()] {
            fs::create_dir(root.join("dir"))?;
            fs::write(root.join("regular.txt"), b"regular\n")?;
            fs::write(root.join("dir/nested.txt"), b"nested\n")?;
        }
        add(ours.path(), &["regular.txt", "dir"])?;
        git(theirs.path(), &["add", "regular.txt", "dir"])?;
        assert_eq!(staged(ours.path())?, staged(theirs.path())?, "{hash}");
        assert!(!staged(ours.path())?.is_empty(), "{hash}");
        fsck(ours.path())?;
    }
    Ok(())
}

#[test]
fn a_path_tracked_as_a_symlink_refuses_before_any_write() -> Result {
    for hash in HASHES {
        let root = repo(hash)?;
        let path = root.path();
        // How C Git checks out a symlink with core.symlinks=false: index mode
        // 120000, worktree file holding the target text.
        let mut hash_object = git_command(path);
        let output = hash_object
            .args(["hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child
                    .stdin
                    .take()
                    .ok_or_else(|| std::io::Error::other("no stdin"))?
                    .write_all(b"target.txt")?;
                child.wait_with_output()
            })?;
        assert!(output.status.success(), "{hash}");
        let blob = String::from_utf8(output.stdout)?.trim().to_owned();
        git(
            path,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("120000,{blob},link"),
            ],
        )?;
        // A regular sibling proves the refusal covers the whole operation.
        fs::write(path.join("sibling.txt"), b"sibling\n")?;
        // Edit the link text so a successful add would have to write a blob.
        fs::write(path.join("link"), b"other-target.txt")?;

        let before = snapshot(path)?;
        refused(add(path, &["sibling.txt", "link"]))?;
        assert_eq!(snapshot(path)?, before, "{hash}: refusal wrote something");
        refused(add(path, &["."]))?;
        assert_eq!(snapshot(path)?, before, "{hash}: refusal wrote something");
        assert!(
            String::from_utf8(staged(path)?)?.starts_with("120000 "),
            "{hash}: symlink entry changed"
        );
        fsck(path)?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn an_actual_symlink_refuses_before_any_write() -> Result {
    for hash in HASHES {
        let root = repo(hash)?;
        let path = root.path();
        fs::write(path.join("target.txt"), b"target\n")?;
        fs::write(path.join("sibling.txt"), b"sibling\n")?;
        std::os::unix::fs::symlink("target.txt", path.join("link"))?;

        let before = snapshot(path)?;
        refused(add(path, &["sibling.txt", "link"]))?;
        assert_eq!(snapshot(path)?, before, "{hash}: refusal wrote something");
        refused(add(path, &["."]))?;
        assert_eq!(snapshot(path)?, before, "{hash}: refusal wrote something");
        assert!(before.0.is_none() && before.1.is_empty(), "{hash}");

        // The sibling alone still stages, matching C Git.
        add(path, &["sibling.txt"])?;
        assert_eq!(
            String::from_utf8(staged(path)?)?.lines().count(),
            1,
            "{hash}"
        );
        fsck(path)?;
    }
    Ok(())
}
