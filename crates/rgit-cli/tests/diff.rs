use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Output, Stdio};

use rgit_core::{DiffEntry, Repository};
use tempfile::{TempDir, tempdir};

#[allow(dead_code)]
mod support;
use support::{Result, git, git_command};

fn init(hash: &str) -> Result<TempDir> {
    let root = tempdir()?;
    git(
        root.path(),
        &["init", "-b", "main", "--object-format", hash],
    )?;
    git(root.path(), &["config", "core.filemode", "true"])?;
    git(root.path(), &["config", "core.symlinks", "true"])?;
    git(root.path(), &["config", "diff.renames", "false"])?;
    Ok(root)
}

fn input(root: &Path, args: &[&str], bytes: &[u8]) -> Result<Output> {
    let mut child = git_command(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("missing stdin")?
        .write_all(bytes)?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "git {args:?}: {}\npatch: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(bytes)
    );
    Ok(output)
}

fn names(root: &Path, cached: bool) -> Result<Vec<DiffEntry>> {
    let mut args = vec!["diff", "--no-renames", "--name-status", "-z"];
    if cached {
        args.push("--cached");
    }
    let output = git(root, &args)?;
    let mut fields = output.stdout.split(|byte| *byte == 0);
    let mut entries = Vec::new();
    while let Some(code) = fields.next().filter(|code| !code.is_empty()) {
        entries.push(DiffEntry {
            code: code[0],
            path: fields.next().ok_or("missing path")?.to_vec(),
        });
    }
    Ok(entries)
}

fn snapshot(root: &Path) -> Result<BTreeMap<std::path::PathBuf, Vec<u8>>> {
    let mut files = BTreeMap::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for item in fs::read_dir(dir)? {
            let item = item?;
            if item.file_type()?.is_dir() {
                dirs.push(item.path());
            } else {
                files.insert(
                    item.path().strip_prefix(root)?.to_path_buf(),
                    fs::read(item.path())?,
                );
            }
        }
    }
    Ok(files)
}

fn baseline(root: &Path) -> Result {
    fs::write(
        root.join("text"),
        b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
    )?;
    fs::write(root.join("deleted"), b"removed\nlast line")?;
    fs::write(root.join("mode"), b"same content\n")?;
    fs::write(root.join("becomes-empty"), b"old\n")?;
    git(root, &["add", "-A"])?;
    git(root, &["commit", "-m", "baseline"])?;
    Ok(())
}

fn modifications(root: &Path) -> Result {
    fs::write(
        root.join("text"),
        b"one\nchanged\nthree\nfour\nfive\nsix\nseven\neight\nnine\nlast without newline",
    )?;
    fs::remove_file(root.join("deleted"))?;
    fs::write(
        root.join("added space\t\"\\"),
        b"new\nnon-UTF-8: \xff\xfe\n",
    )?;
    fs::write(root.join("empty-added"), b"")?;
    fs::write(root.join("becomes-empty"), b"")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.join("mode"), fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// An independent consumer applies our patches to the baseline index. Comparing
/// resulting tree IDs checks paths, bytes and modes without matching diff hunks.
#[test]
fn staged_and_worktree_patches_apply_to_the_expected_tree() -> Result {
    for hash in ["sha1", "sha256"] {
        for cached in [false, true] {
            let root = init(hash)?;
            let root = root.path();
            baseline(root)?;
            modifications(root)?;
            // Ordinary unstaged diff excludes untracked files. Intent-to-add
            // makes the additions visible without storing their content.
            if cached {
                git(root, &["add", "-A"])?;
            } else {
                git(root, &["add", "-N", "added space\t\"\\", "empty-added"])?;
            }
            let repo = Repository::discover(root)?;
            let before = snapshot(&root.join(".git"))?;
            assert_eq!(repo.diff_entries(cached)?, names(root, cached)?);
            let patch = repo.diff_patch(cached)?;
            assert!(patch.windows(2).any(|bytes| bytes == b"\xff\xfe"));
            assert_eq!(
                before,
                snapshot(&root.join(".git"))?,
                "diff mutated repository storage"
            );
            git(root, &["add", "-A"])?;
            let target = git(root, &["write-tree"])?.stdout;
            git(root, &["read-tree", "HEAD"])?;
            input(
                root,
                &["apply", "--cached", "--whitespace=nowarn", "-"],
                &patch,
            )?;
            assert_eq!(
                git(root, &["write-tree"])?.stdout,
                target,
                "cached={cached}, patch={}, actual={}, expected={}",
                String::from_utf8_lossy(&patch),
                String::from_utf8_lossy(&git(root, &["ls-files", "--stage"])?.stdout),
                String::from_utf8_lossy(
                    &git(
                        root,
                        &["ls-tree", "-r", std::str::from_utf8(&target)?.trim()]
                    )?
                    .stdout
                )
            );
        }
    }
    Ok(())
}

#[test]
fn cached_diff_ignores_unstaged_content_and_opt_out_flags() -> Result {
    let root = init("sha1")?;
    let root = root.path();
    baseline(root)?;
    fs::write(root.join("text"), b"staged\n")?;
    git(root, &["add", "text"])?;
    let repo = Repository::discover(root)?;
    let patch = repo.diff_patch(true)?;
    fs::write(root.join("text"), b"unstaged\n")?;
    assert_eq!(repo.diff_patch(true)?, patch);
    git(root, &["update-index", "--assume-unchanged", "text"])?;
    assert!(repo.diff_entries(false)?.is_empty());
    git(root, &["update-index", "--no-assume-unchanged", "text"])?;
    git(root, &["update-index", "--skip-worktree", "text"])?;
    assert!(repo.diff_entries(false)?.is_empty());
    assert_eq!(repo.diff_patch(true)?, patch);
    Ok(())
}

#[test]
fn unborn_index_and_intent_to_add() -> Result {
    for hash in ["sha1", "sha256"] {
        let root = init(hash)?;
        let root = root.path();
        fs::write(root.join("file"), b"new file\n")?;
        git(root, &["add", "-N", "file"])?;
        let repo = Repository::discover(root)?;
        assert!(repo.diff_entries(true)?.is_empty());
        assert!(repo.diff_patch(true)?.is_empty());
        assert_eq!(repo.diff_entries(false)?, names(root, false)?);
        let patch = repo.diff_patch(false)?;
        git(root, &["read-tree", "--empty"])?;
        input(root, &["apply", "--cached", "-"], &patch)?;
        let expected = git(root, &["write-tree"])?.stdout;
        git(root, &["add", "file"])?;
        assert_eq!(expected, git(root, &["write-tree"])?.stdout);
        assert_eq!(repo.diff_entries(true)?, names(root, true)?);
        assert!(!repo.diff_patch(true)?.is_empty());
    }
    Ok(())
}

#[test]
fn binary_reports_add_delete_modify_without_lossy_text() -> Result {
    let root = init("sha1")?;
    let root = root.path();
    fs::write(root.join("modify"), b"old\0\xff")?;
    fs::write(root.join("delete"), b"old\0\xfe")?;
    git(root, &["add", "."])?;
    git(root, &["commit", "-m", "binary"])?;
    fs::write(root.join("modify"), b"new\0\xfe")?;
    fs::remove_file(root.join("delete"))?;
    fs::write(root.join("add"), b"new\0\xfd")?;
    git(root, &["add", "-A"])?;
    let repo = Repository::discover(root)?;
    assert_eq!(repo.diff_entries(true)?, names(root, true)?);
    let patch = repo.diff_patch(true)?;
    for report in [
        b"Binary files /dev/null and b/add differ\n".as_slice(),
        b"Binary files a/delete and /dev/null differ\n",
        b"Binary files a/modify and b/modify differ\n",
    ] {
        assert!(patch.windows(report.len()).any(|part| part == report));
    }
    assert!(!patch.contains(&0));
    Ok(())
}

#[test]
fn builtin_eol_and_ident_filters_are_read_only_and_apply() -> Result {
    for cached in [false, true] {
        let root = init("sha1")?;
        let root = root.path();
        fs::write(root.join(".gitattributes"), b"*.txt text eol=crlf ident\n")?;
        fs::write(root.join("file.txt"), b"$Id$\nold\n")?;
        git(root, &["add", "."])?;
        git(root, &["commit", "-m", "filters"])?;
        fs::remove_file(root.join("file.txt"))?;
        git(root, &["checkout-index", "--index", "--force", "file.txt"])?;
        let blob = git(root, &["rev-parse", "HEAD:file.txt"])?.stdout;
        let blob = std::str::from_utf8(&blob)?.trim();
        assert_eq!(
            fs::read(root.join("file.txt"))?,
            format!("$Id: {blob} $\r\nold\r\n").as_bytes()
        );
        let repo = Repository::discover(root)?;
        assert!(
            repo.diff_entries(false)?.is_empty(),
            "real checkout baseline"
        );
        assert!(names(root, false)?.is_empty());
        fs::write(root.join("file.txt"), b"$Id: expanded $\r\nold\r\n")?;
        assert_eq!(
            repo.diff_entries(false)?,
            names(root, false)?,
            "normalized status"
        );
        fs::write(root.join("file.txt"), b"$Id: expanded $\r\nnew\r\n")?;
        if cached {
            git(root, &["add", "."])?;
        }
        let before = snapshot(&root.join(".git"))?;
        let patch = repo.diff_patch(cached)?;
        assert_eq!(before, snapshot(&root.join(".git"))?);
        assert!(!patch.contains(&b'\r'));
        assert!(patch.windows(6).any(|bytes| bytes == b" $Id$\n"));
        git(root, &["add", "."])?;
        let target = git(root, &["write-tree"])?.stdout;
        git(root, &["read-tree", "HEAD"])?;
        input(root, &["apply", "--cached", "-"], &patch)?;
        assert_eq!(target, git(root, &["write-tree"])?.stdout);
    }
    Ok(())
}

#[test]
fn refuses_external_filters_and_combined_conflicts() -> Result {
    let root = init("sha1")?;
    let root = root.path();
    baseline(root)?;
    let repo = Repository::discover(root)?;
    let blob = git(root, &["rev-parse", "HEAD:text"])?.stdout;
    let blob = String::from_utf8(blob)?.trim().to_owned();
    let records = format!(
        "0 {}\ttext\n100644 {blob} 1\ttext\n100644 {blob} 2\ttext\n100644 {blob} 3\ttext\n",
        "0".repeat(40)
    );
    input(root, &["update-index", "--index-info"], records.as_bytes())?;
    for cached in [false, true] {
        assert_eq!(
            repo.diff_entries(cached)?,
            vec![DiffEntry {
                path: b"text".to_vec(),
                code: b'U'
            }]
        );
        assert!(repo.diff_patch(cached).is_err());
    }
    git(root, &["config", "filter.evil.clean", "touch EXECUTED"])?;
    let repo = Repository::discover(root)?;
    assert!(repo.diff_entries(false).is_err());
    assert!(repo.diff_patch(true).is_err());
    assert!(!root.join("EXECUTED").exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn symlinks_type_changes_and_non_utf8_paths_apply() -> Result {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    for cached in [false, true] {
        let root = init("sha256")?;
        let root = root.path();
        let raw = root.join(OsStr::from_bytes(b"raw-\xff\n\t"));
        let raw_supported = match fs::write(&raw, b"old \xff\n") {
            Ok(()) => true,
            Err(error)
                if error.raw_os_error() == Some(1)
                    || error.kind() == std::io::ErrorKind::InvalidInput =>
            {
                eprintln!("SKIP non-UTF-8 worktree path: {error}");
                false
            }
            Err(error) => return Err(error.into()),
        };
        fs::write(root.join("file-to-link"), b"old file\n")?;
        symlink("old-target", root.join("link"))?;
        symlink("old-target", root.join("link-to-file"))?;
        git(root, &["add", "-A"])?;
        git(root, &["commit", "-m", "native"])?;
        if raw_supported {
            fs::write(&raw, b"new \xfe\n")?;
        }
        fs::remove_file(root.join("file-to-link"))?;
        symlink(
            OsStr::from_bytes(b"new-\xff-target"),
            root.join("file-to-link"),
        )?;
        fs::remove_file(root.join("link"))?;
        symlink("new-target", root.join("link"))?;
        fs::remove_file(root.join("link-to-file"))?;
        fs::write(root.join("link-to-file"), b"new file\n")?;
        if cached {
            git(root, &["add", "-A"])?;
        }
        let repo = Repository::discover(root)?;
        assert_eq!(repo.diff_entries(cached)?, names(root, cached)?);
        let patch = repo.diff_patch(cached)?;
        if raw_supported {
            assert!(patch.contains(&0xfe));
        }
        git(root, &["add", "-A"])?;
        let target = git(root, &["write-tree"])?.stdout;
        git(root, &["read-tree", "HEAD"])?;
        input(root, &["apply", "--cached", "-"], &patch)?;
        assert_eq!(target, git(root, &["write-tree"])?.stdout);
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn rejects_symlink_parents_and_special_files_before_reading() -> Result {
    use std::os::unix::fs::symlink;
    let root = init("sha1")?;
    let root = root.path();
    fs::create_dir(root.join("dir"))?;
    fs::write(root.join("dir/file"), b"old\n")?;
    git(root, &["add", "."])?;
    git(root, &["commit", "-m", "safe"])?;
    let repo = Repository::discover(root)?;
    fs::rename(root.join("dir"), root.join("elsewhere"))?;
    symlink("elsewhere", root.join("dir"))?;
    assert!(repo.diff_entries(false).is_err());
    fs::remove_file(root.join("dir"))?;
    fs::rename(root.join("elsewhere"), root.join("dir"))?;
    for name in [
        "dir/file",
        ".gitattributes",
        ".gitignore",
        ".git/info/attributes",
        ".git/info/exclude",
    ] {
        let path = root.join(name);
        if path.exists() {
            fs::remove_file(&path)?;
        }
        let status = std::process::Command::new("mkfifo").arg(&path).status()?;
        assert!(status.success());
        assert!(repo.diff_entries(false).is_err(), "FIFO {name}");
        fs::remove_file(&path)?;
        if name == "dir/file" {
            fs::write(&path, b"old\n")?;
        }
    }
    symlink("dir/file", root.join(".gitattributes"))?;
    assert!(repo.diff_entries(false).is_err());
    // Cached inspection does not need any of these worktree reads.
    assert!(repo.diff_entries(true)?.is_empty());
    Ok(())
}

#[cfg(unix)]
#[test]
fn index_only_non_utf8_paths_and_symlink_targets_apply_on_every_filesystem() -> Result {
    for hash in ["sha1", "sha256"] {
        let root = init(hash)?;
        let root = root.path();
        baseline(root)?;
        let mut records = Vec::new();
        for (mode, path, contents) in [
            (
                "100644",
                b"raw-\xff\n\t".as_slice(),
                b"raw text \xfe\n".as_slice(),
            ),
            ("120000", b"link-\xfe".as_slice(), b"target-\xff".as_slice()),
        ] {
            let id = input(root, &["hash-object", "-w", "--stdin"], contents)?.stdout;
            let id = std::str::from_utf8(&id)?.trim();
            records.extend_from_slice(format!("{mode} {id}\t").as_bytes());
            records.extend_from_slice(path);
            records.push(0);
        }
        input(root, &["update-index", "-z", "--index-info"], &records)?;
        let repo = Repository::discover(root)?;
        assert_eq!(repo.diff_entries(true)?, names(root, true)?);
        let patch = repo.diff_patch(true)?;
        assert!(patch.contains(&0xfe));
        assert!(patch.contains(&0xff));
        let target = git(root, &["write-tree"])?.stdout;
        git(root, &["read-tree", "HEAD"])?;
        input(root, &["apply", "--cached", "-"], &patch)?;
        assert_eq!(target, git(root, &["write-tree"])?.stdout);
    }
    Ok(())
}

#[test]
fn cached_submodule_metadata_is_available_but_patches_are_refused() -> Result {
    let root = init("sha1")?;
    let root = root.path();
    baseline(root)?;
    let id = git(root, &["rev-parse", "HEAD"])?.stdout;
    let id = std::str::from_utf8(&id)?.trim();
    git(
        root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            id,
            "submodule",
        ],
    )?;
    let repo = Repository::discover(root)?;
    assert_eq!(repo.diff_entries(true)?, names(root, true)?);
    assert!(repo.diff_patch(true).is_err());
    assert!(repo.diff_entries(false).is_err());
    Ok(())
}
