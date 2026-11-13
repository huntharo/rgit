use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
#[cfg(unix)]
use std::time::{Duration, Instant};

use rgit_core::{Repository, UntrackedMode};
use tempfile::{TempDir, tempdir};

#[allow(dead_code)] // Other integration targets use the shared fixture builder.
mod support;
use support::*;

fn init(hash: &str) -> Result<TempDir> {
    let root = tempdir()?;
    git(
        root.path(),
        &["init", "-b", "main", "--object-format", hash],
    )?;
    git(root.path(), &["config", "core.autocrlf", "false"])?;
    git(root.path(), &["config", "maintenance.auto", "false"])?;
    git(root.path(), &["config", "gc.auto", "0"])?;
    Ok(root)
}

fn fixture(hash: &str) -> Result<TempDir> {
    let root = init(hash)?;
    write(root.path(), "file.txt", b"first\n\0binary\xff\n")?;
    commit(root.path())?;
    git(root.path(), &["branch", "other"])?;
    git(root.path(), &["tag", "-a", "v1", "-m", "annotated"])?;
    Ok(root)
}

fn write(root: &Path, path: &str, contents: &[u8]) -> Result {
    let path = root.join(path);
    fs::create_dir_all(path.parent().ok_or("missing parent")?)?;
    fs::write(path, contents)?;
    Ok(())
}

fn commit(root: &Path) -> Result {
    git(root, &["add", "."])?;
    git(root, &["commit", "-m", "fixture"])?;
    Ok(())
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
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

// Snapshot every repository file, including index extensions, refs and reflogs.
// C Git refreshes a disposable copy of the index, never the fixture's index.
fn snapshot(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    fn visit(base: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) -> Result {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                visit(base, &entry.path(), files)?;
            } else {
                let bytes = if kind.is_file() {
                    fs::read(entry.path())
                        .map_err(|error| format!("snapshot {:?}: {error}", entry.path()))?
                } else if kind.is_symlink() {
                    fs::read_link(entry.path())?
                        .as_os_str()
                        .as_encoded_bytes()
                        .to_vec()
                } else {
                    b"<special file>".to_vec()
                };
                files.insert(entry.path().strip_prefix(base)?.to_owned(), bytes);
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    visit(&root.join(".git"), &root.join(".git"), &mut files)?;
    Ok(files)
}

fn unchanged(root: &Path, before: &BTreeMap<PathBuf, Vec<u8>>) -> Result {
    let after = snapshot(root)?;
    let changed: std::collections::BTreeSet<_> = before
        .keys()
        .chain(after.keys())
        .filter(|path| before.get(*path) != after.get(*path))
        .collect();
    assert!(changed.is_empty(), "repository files changed: {changed:?}");
    Ok(())
}

fn compare(root: &Path, extra: &[&str]) -> Result<Vec<u8>> {
    let mut args = vec!["status", "--porcelain=v1", "--no-renames"];
    args.extend_from_slice(extra);
    let before = snapshot(root)?;
    let oracle = tempdir()?;
    let oracle_index = oracle.path().join("index");
    if root.join(".git/index").exists() {
        fs::copy(root.join(".git/index"), &oracle_index)?;
    }
    let expected = git_command(root)
        .env("GIT_INDEX_FILE", &oracle_index)
        .env("GIT_OPTIONAL_LOCKS", "1")
        .args(&args)
        .output()?;
    assert!(
        expected.status.success(),
        "{}",
        String::from_utf8_lossy(&expected.stderr)
    );
    let actual = rgit(root, &args)?;
    unchanged(root, &before)?;
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "{args:?}: {}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert!(
        actual.stderr.is_empty(),
        "{args:?}: {}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        actual.stdout,
        expected.stdout,
        "{args:?}: expected {:?}, got {:?}",
        String::from_utf8_lossy(&expected.stdout),
        String::from_utf8_lossy(&actual.stdout)
    );
    Ok(expected.stdout)
}

fn matrix(root: &Path) -> Result {
    for options in [
        vec![],
        vec!["-z"],
        vec!["-uall"],
        vec!["-z", "-uall"],
        vec!["-uno"],
        vec!["-z", "-uno"],
    ] {
        compare(root, &options).map_err(|error| format!("status {options:?}: {error}"))?;
    }
    Ok(())
}

#[test]
fn staged_unstaged_add_delete_and_untracked_match_both_hashes() -> Result {
    for hash in ["sha1", "sha256"] {
        let root = init(hash)?;
        for name in [
            "staged",
            "unstaged",
            "both",
            "deleted",
            "removed",
            "recreated",
            "renamed",
        ] {
            write(root.path(), name, b"original\n")?;
        }
        write(root.path(), ".gitignore", b"ignored\nignored-dir/\n")?;
        commit(root.path())?;
        assert!(compare(root.path(), &[])?.is_empty());
        for name in ["staged", "both", "added"] {
            write(root.path(), name, b"staged\n")?;
            git(root.path(), &["add", name])?;
        }
        for name in ["unstaged", "both", "added"] {
            write(root.path(), name, b"worktree\n")?;
        }
        fs::remove_file(root.path().join("deleted"))?;
        git(root.path(), &["rm", "removed", "recreated"])?;
        write(root.path(), "recreated", b"new untracked\n")?;
        git(root.path(), &["mv", "renamed", "moved"])?;
        write(
            root.path(),
            "a-untracked",
            b"sort after all tracked entries\n",
        )?;
        write(root.path(), "ignored", b"invisible\n")?;
        write(root.path(), "ignored-dir/file", b"invisible\n")?;
        matrix(root.path())?;
    }
    Ok(())
}

#[test]
fn unborn_head_with_and_without_an_index() -> Result {
    for hash in ["sha1", "sha256"] {
        let root = init(hash)?;
        assert!(!root.path().join(".git/index").exists());
        matrix(root.path())?;
        write(root.path(), "untracked", b"untracked\n")?;
        matrix(root.path())?;
        write(root.path(), "added", b"added\n")?;
        git(root.path(), &["add", "added"])?;
        write(root.path(), "added", b"edited after add\n")?;
        matrix(root.path())?;
    }
    Ok(())
}

#[test]
fn ignored_files_and_collapsed_or_expanded_directories() -> Result {
    let root = init("sha1")?;
    write(root.path(), ".gitignore", b"*.ignored\nignored/\n")?;
    write(root.path(), "tracked/file", b"base\n")?;
    commit(root.path())?;
    for name in [
        "new/deep/a",
        "new/deep/b",
        "tracked/untracked/a",
        "tracked/visible",
        "ignored/file",
        "new/deep/a.ignored",
    ] {
        write(root.path(), name, b"new\n")?;
    }
    fs::create_dir(root.path().join("empty"))?;
    matrix(root.path())?;
    assert_eq!(
        compare(root.path(), &["-z"])?,
        b"?? new/\0?? tracked/untracked/\0?? tracked/visible\0"
    );
    Ok(())
}

#[test]
fn every_conflict_stage_combination_matches_porcelain() -> Result {
    for hash in ["sha1", "sha256"] {
        let root = init(hash)?;
        write(root.path(), "base", b"base\n")?;
        commit(root.path())?;
        let id = String::from_utf8(git(root.path(), &["rev-parse", "HEAD:base"])?.stdout)?;
        let mut records = String::new();
        for mask in 1..=7 {
            let path = format!("conflict-{mask}");
            write(root.path(), &path, b"conflicted worktree\n")?;
            for stage in 1..=3 {
                if mask & (1 << (stage - 1)) != 0 {
                    records.push_str(&format!("100644 {} {stage}\t{path}\n", id.trim()));
                }
            }
        }
        input(
            root.path(),
            &["update-index", "--index-info"],
            records.as_bytes(),
        )?;
        matrix(root.path())?;
        assert_eq!(compare(root.path(), &["-z"])?, b"DD conflict-1\0AU conflict-2\0UD conflict-3\0UA conflict-4\0DU conflict-5\0AA conflict-6\0UU conflict-7\0");
    }
    Ok(())
}

#[test]
fn eol_and_ident_conversion_compare_canonical_content() -> Result {
    for hash in ["sha1", "sha256"] {
        let root = init(hash)?;
        write(
            root.path(),
            ".gitattributes",
            b"crlf text eol=crlf\nlf text eol=lf\nident ident\n",
        )?;
        write(root.path(), "crlf", b"one\ntwo\n")?;
        write(root.path(), "lf", b"one\ntwo\n")?;
        write(root.path(), "ident", b"$Id$\n")?;
        commit(root.path())?;
        let id = String::from_utf8(git(root.path(), &["rev-parse", "HEAD:ident"])?.stdout)?;
        // Let the independent implementation generate real checkout forms and
        // their stat cache, rather than leaving the index's pre-conversion size.
        for name in ["crlf", "lf", "ident"] {
            fs::remove_file(root.path().join(name))?;
        }
        git(
            root.path(),
            &["checkout-index", "--force", "--index", "--all"],
        )?;
        assert_eq!(fs::read(root.path().join("crlf"))?, b"one\r\ntwo\r\n");
        assert_eq!(fs::read(root.path().join("lf"))?, b"one\ntwo\n");
        assert_eq!(
            fs::read(root.path().join("ident"))?,
            format!("$Id: {} $\n", id.trim()).as_bytes()
        );
        let clean = compare(root.path(), &["-z"])?;
        assert!(
            clean.is_empty(),
            "{hash}: {:?}",
            String::from_utf8_lossy(&clean)
        );
        for (name, bytes) in [
            ("crlf", b"one\r\nchanged\r\n".as_slice()),
            ("lf", b"one\r\nchanged\r\n"),
            ("ident", b"$Id: arbitrary expansion $\nchanged\n"),
        ] {
            write(root.path(), name, bytes)?;
        }
        matrix(root.path())?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn executable_modes_and_symlink_type_and_target_changes() -> Result {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = init("sha1")?;
    git(root.path(), &["config", "core.filemode", "true"])?;
    git(root.path(), &["config", "core.symlinks", "true"])?;
    write(root.path(), "mode", b"executable\n")?;
    write(root.path(), "type", b"regular\n")?;
    symlink("old-target", root.path().join("link"))?;
    commit(root.path())?;
    fs::set_permissions(root.path().join("mode"), fs::Permissions::from_mode(0o755))?;
    fs::remove_file(root.path().join("type"))?;
    symlink("target", root.path().join("type"))?;
    fs::remove_file(root.path().join("link"))?;
    symlink("new-target", root.path().join("link"))?;
    matrix(root.path())?;
    git(root.path(), &["add", "."])?;
    matrix(root.path())?;
    fs::remove_file(root.path().join("link"))?;
    write(root.path(), "link", b"now regular\n")?;
    matrix(root.path())?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn native_path_bytes_quotes_and_nul_output() -> Result {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let root = init("sha1")?;
    let mut names: Vec<&[u8]> = vec![
        b"space name",
        b"tab\tname",
        b"line\nname",
        b"quote\"name",
        b"back\\slash",
    ];
    match fs::write(root.path().join(OsStr::from_bytes(b"raw-\xff")), b"base\n") {
        Ok(()) => names.push(b"raw-\xff"),
        Err(error)
            if cfg!(target_os = "macos") && [Some(1), Some(92)].contains(&error.raw_os_error()) =>
        {
            eprintln!("SKIP: this macOS environment refuses non-UTF-8 filenames: {error}");
        }
        Err(error) => return Err(error.into()),
    }
    for name in &names {
        fs::write(root.path().join(OsStr::from_bytes(name)), b"base\n")?;
    }
    commit(root.path())?;
    for name in &names {
        fs::write(root.path().join(OsStr::from_bytes(name)), b"changed\n")?;
    }
    matrix(root.path())?;
    Ok(())
}

#[test]
fn intent_to_add_and_explicit_index_opt_outs() -> Result {
    let root = init("sha1")?;
    for name in ["assumed", "skipped", "ordinary"] {
        write(root.path(), name, b"base\n")?;
    }
    commit(root.path())?;
    git(
        root.path(),
        &["update-index", "--assume-unchanged", "assumed"],
    )?;
    git(root.path(), &["update-index", "--skip-worktree", "skipped"])?;
    for name in ["assumed", "skipped", "ordinary", "intent"] {
        write(root.path(), name, b"changed\n")?;
    }
    git(root.path(), &["add", "-N", "intent"])?;
    matrix(root.path())?;
    Ok(())
}

#[test]
fn api_and_fresh_cli_preserve_index_refs_and_need_no_executables() -> Result {
    let root = fixture("sha1")?;
    write(root.path(), "file.txt", b"changed\n")?;
    write(root.path(), "new/nested", b"untracked\n")?;
    let before = snapshot(root.path())?;
    let repo = Repository::discover(root.path())?;
    for (mode, option) in [
        (UntrackedMode::None, "-uno"),
        (UntrackedMode::Normal, "-unormal"),
        (UntrackedMode::All, "-uall"),
    ] {
        let mut encoded = Vec::new();
        for entry in repo.status(mode)? {
            encoded.extend_from_slice(&[entry.index, entry.worktree, b' ']);
            encoded.extend_from_slice(&entry.path);
            encoded.push(0);
        }
        assert_eq!(encoded, compare(root.path(), &["-z", option])?);
    }
    let empty = tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_rgit"))
        .arg("-C")
        .arg(root.path())
        .args(["status", "--porcelain=v1", "--no-renames", "-z", "-uall"])
        .env("PATH", empty.path())
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b" M file.txt\0?? new/nested\0");
    unchanged(root.path(), &before)?;
    same(root.path(), &["show-ref"])?;
    Ok(())
}

fn refused(root: &Path, diagnostic: &str) -> Result {
    let before = snapshot(root)?;
    let output = rgit(root, &["status", "--porcelain=v1", "--no-renames", "-z"])?;
    unchanged(root, &before)?;
    assert_eq!(
        output.status.code(),
        Some(128),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "refusal must not emit partial status"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(diagnostic),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

// A FIFO regression must fail with a deadline rather than wedge cargo test.
// These commands emit only tiny diagnostics, so their pipes cannot fill.
#[cfg(unix)]
fn bounded_output(command: &mut Command, deadline: Duration) -> Result<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let start = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        if start.elapsed() >= deadline {
            child.kill()?;
            let output = child.wait_with_output()?;
            return Err(format!(
                "child exceeded {deadline:?} and was killed/reaped: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn bounded_refusal(root: &Path) -> Result {
    let before = snapshot(root)?;
    let output = bounded_output(
        Command::new(env!("CARGO_BIN_EXE_rgit"))
            .arg("-C")
            .arg(root)
            .args(["status", "--porcelain=v1", "--no-renames", "-z"]),
        Duration::from_secs(3),
    )?;
    unchanged(root, &before)?;
    assert_eq!(
        output.status.code(),
        Some(128),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unsupported"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn symlinked_attribute_and_ignore_metadata_are_refused() -> Result {
    use std::os::unix::fs::symlink;
    for name in [
        ".gitattributes",
        ".gitignore",
        "nested/.gitattributes",
        "nested/.gitignore",
        ".git/info/attributes",
        ".git/info/exclude",
    ] {
        let root = fixture("sha1")?;
        let outside = tempdir()?;
        fs::write(outside.path().join("metadata"), b"* text\n")?;
        write(root.path(), "nested/tracked", b"base\n")?;
        git(root.path(), &["add", "nested/tracked"])?;
        if root.path().join(name).exists() {
            fs::remove_file(root.path().join(name))?;
        }
        symlink(outside.path().join("metadata"), root.path().join(name))?;
        bounded_refusal(root.path())?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn fifo_attribute_and_ignore_metadata_are_refused_without_blocking() -> Result {
    for name in [
        ".gitattributes",
        ".gitignore",
        "nested/.gitattributes",
        "nested/.gitignore",
        ".git/info/attributes",
        ".git/info/exclude",
    ] {
        let root = fixture("sha1")?;
        write(root.path(), "nested/tracked", b"base\n")?;
        git(root.path(), &["add", "nested/tracked"])?;
        if root.path().join(name).exists() {
            fs::remove_file(root.path().join(name))?;
        }
        let output = Command::new("mkfifo")
            .arg(root.path().join(name))
            .output()?;
        assert!(output.status.success());
        bounded_refusal(root.path())?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn tracked_fifo_and_socket_leaves_are_refused_without_blocking() -> Result {
    use std::os::unix::net::UnixListener;
    for socket in [false, true] {
        let root = fixture("sha1")?;
        let path = root.path().join("file.txt");
        fs::remove_file(&path)?;
        let _listener = if socket {
            Some(UnixListener::bind(&path)?)
        } else {
            assert!(Command::new("mkfifo").arg(&path).status()?.success());
            None
        };
        bounded_refusal(root.path())?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn tracked_paths_with_symlinked_parents_are_refused() -> Result {
    use std::os::unix::fs::symlink;
    let root = init("sha1")?;
    write(root.path(), "parent/file", b"base\n")?;
    commit(root.path())?;
    fs::remove_dir_all(root.path().join("parent"))?;
    let outside = tempdir()?;
    fs::write(outside.path().join("file"), b"outside\n")?;
    symlink(outside.path(), root.path().join("parent"))?;
    bounded_refusal(root.path())?;
    Ok(())
}

#[test]
fn custom_attribute_and_exclude_files_are_explicitly_refused() -> Result {
    for key in ["core.attributesFile", "core.excludesFile"] {
        let root = fixture("sha1")?;
        let outside = tempdir()?;
        let path = outside.path().join("metadata");
        fs::write(&path, b"*\n")?;
        let output = git_command(root.path())
            .args(["config", key])
            .arg(&path)
            .output()?;
        assert!(output.status.success());
        refused(root.path(), "unsupported")?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn native_mmap(phase: &str) -> Result {
    let root = init("sha1")?;
    write(root.path(), "mapped", &vec![b'A'; 8192])?;
    commit(root.path())?;
    let before = snapshot(root.path())?;
    let output = bounded_output(
        Command::new("python3")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/native_mmap.py"))
            .arg(env!("CARGO_BIN_EXE_rgit"))
            .arg(root.path())
            .arg(phase),
        Duration::from_secs(30),
    )?;
    assert_eq!(
        snapshot(root.path())?,
        before,
        "mmap inspection changed repository state"
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let evidence = String::from_utf8(output.stdout)?;
    eprintln!("{evidence}");
    assert!(evidence.starts_with("PASS: APFS") || evidence.starts_with("SKIP:"));
    if evidence.starts_with("PASS:") {
        assert_eq!(fs::read(root.path().join("mapped"))?, vec![b'B'; 8192]);
        // The mapper process has now exited as well as dropping its last map.
        assert_eq!(
            rgit(
                root.path(),
                &["status", "--porcelain=v1", "--no-renames", "-z"]
            )?
            .stdout,
            b" M mapped\0"
        );
    }
    Ok(())
}

#[test]
fn native_shared_mmap_live_with_closed_mapper_fd() -> Result {
    #[cfg(target_os = "macos")]
    native_mmap("live")?;
    #[cfg(not(target_os = "macos"))]
    eprintln!("SKIP: native APFS live mmap gate requires macOS");
    Ok(())
}

#[test]
fn native_shared_mmap_final_unmap_with_closed_mapper_fd() -> Result {
    #[cfg(target_os = "macos")]
    native_mmap("final-unmap")?;
    #[cfg(not(target_os = "macos"))]
    eprintln!("SKIP: native APFS final-unmap gate requires macOS");
    Ok(())
}

#[test]
fn external_clean_smudge_and_process_filters_are_refused_without_execution() -> Result {
    for kind in ["clean", "smudge", "process"] {
        let root = fixture("sha1")?;
        write(root.path(), ".gitattributes", b"* filter=blocked\n")?;
        write(root.path(), "file.txt", b"changed\n")?;
        // The shell command is an execution tripwire, never a fixture command.
        // Use an absolute marker so an unexpected helper cwd cannot hide it.
        let marker = root.path().join("FILTER_EXECUTED");
        let quoted_marker = marker
            .to_str()
            .ok_or("non-UTF-8 fixture directory")?
            .replace('\'', "'\\''");
        let tripwire = format!("touch '{quoted_marker}'; cat");
        git(
            root.path(),
            &["config", &format!("filter.blocked.{kind}"), &tripwire],
        )?;
        refused(root.path(), "filter")?;
        assert!(!marker.exists());
    }
    Ok(())
}

#[test]
fn submodule_worktree_status_is_explicitly_refused() -> Result {
    let root = fixture("sha1")?;
    let id = String::from_utf8(git(root.path(), &["rev-parse", "HEAD"])?.stdout)?;
    git(
        root.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},submodule", id.trim()),
        ],
    )?;
    refused(root.path(), "submodule")?;
    Ok(())
}

#[test]
fn split_index_status_is_explicitly_refused() -> Result {
    let root = fixture("sha1")?;
    git(root.path(), &["update-index", "--split-index"])?;
    assert!(
        fs::read_dir(root.path().join(".git"))?.any(|entry| entry.is_ok_and(|entry| entry
            .file_name()
            .to_string_lossy()
            .starts_with("sharedindex.")))
    );
    refused(root.path(), "split")?;
    Ok(())
}

#[test]
fn sparse_index_status_is_explicitly_refused() -> Result {
    let root = init("sha1")?;
    write(root.path(), "included/file", b"inside\n")?;
    write(root.path(), "excluded/file", b"outside\n")?;
    commit(root.path())?;
    git(
        root.path(),
        &["sparse-checkout", "init", "--cone", "--sparse-index"],
    )?;
    git(root.path(), &["sparse-checkout", "set", "included"])?;
    let entries = git(root.path(), &["ls-files", "--sparse", "--stage"])?;
    assert!(String::from_utf8_lossy(&entries.stdout).contains("040000 "));
    refused(root.path(), "sparse")?;
    Ok(())
}
