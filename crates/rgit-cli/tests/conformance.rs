//! C Git supplies independent fixture data and observable behavior.
//! The rgit executable itself never needs Git on PATH.

use std::fs;
use std::process::Command;

use tempfile::tempdir;

mod support;
use support::*;

#[test]
fn help_and_version_work_outside_a_repository() -> Result {
    let root = tempdir()?;
    for option in ["--help", "--version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_rgit"))
            .current_dir(root.path())
            .arg(option)
            .output()?;
        assert!(output.status.success());
        assert!(!output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("rgit-cli"));
    }
    Ok(())
}

#[test]
fn loose_and_packed_objects_match_c_git_for_both_hashes() -> Result {
    for hash in ["sha1", "sha256"] {
        let root = fixture(hash)?;
        for packed in [false, true] {
            if packed {
                git(root.path(), &["gc", "--prune=now"])?;
            }
            for revision in ["HEAD", "HEAD^{tree}", "HEAD:file.txt", "v1", "v1^{}"] {
                same(root.path(), &["rev-parse", "--verify", revision])?;
                same(root.path(), &["cat-file", "-t", revision])?;
                same(root.path(), &["cat-file", "-s", revision])?;
            }
            for (kind, revision) in [
                ("blob", "HEAD:file.txt"),
                ("commit", "HEAD"),
                ("tree", "HEAD"),
                ("tag", "v1"),
                ("commit", "v1"),
            ] {
                same(root.path(), &["cat-file", kind, revision])?;
            }
            same(root.path(), &["show-ref"])?;
        }
    }
    Ok(())
}

#[test]
fn discovers_nested_bare_and_linked_worktrees() -> Result {
    let root = fixture("sha1")?;
    let nested = root.path().join("nested/deeper");
    fs::create_dir_all(&nested)?;
    for query in [
        "--show-toplevel",
        "--absolute-git-dir",
        "--is-bare-repository",
    ] {
        same(&nested, &["rev-parse", query])?;
    }
    let other = tempdir()?;
    let linked = other.path().join("linked");
    let linked_arg = linked.to_str().ok_or("test path must be UTF-8")?;
    git(root.path(), &["worktree", "add", linked_arg, "other"])?;
    for query in [
        "--show-toplevel",
        "--absolute-git-dir",
        "--is-bare-repository",
    ] {
        same(&linked, &["rev-parse", query])?;
    }
    same(&linked, &["rev-parse", "--verify", "HEAD"])?;
    same(&linked, &["show-ref"])?;
    let bare = tempdir()?;
    git(bare.path(), &["init", "--bare"])?;
    same(bare.path(), &["rev-parse", "--is-bare-repository"])?;
    same(bare.path(), &["rev-parse", "--absolute-git-dir"])?;
    assert!(
        !rgit(bare.path(), &["rev-parse", "--show-toplevel"])?
            .status
            .success()
    );
    Ok(())
}

#[test]
fn empty_refs_and_unborn_head_are_not_synthetic_success() -> Result {
    let root = tempdir()?;
    git(root.path(), &["init", "-b", "main"])?;
    same(root.path(), &["show-ref"])?;
    let head = rgit(root.path(), &["rev-parse", "--verify", "HEAD"])?;
    assert_eq!(head.status.code(), Some(128));
    assert!(head.stdout.is_empty());
    Ok(())
}

#[test]
fn broken_ref_never_produces_partial_stdout() -> Result {
    let root = fixture("sha1")?;
    fs::write(
        root.path().join(".git/refs/heads/zz-broken"),
        format!("{}\n", "1".repeat(40)),
    )?;
    let output = rgit(root.path(), &["show-ref"])?;
    assert_eq!(output.status.code(), Some(128));
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
    Ok(())
}

#[test]
fn unsupported_commands_options_and_missing_objects_fail() -> Result {
    let root = fixture("sha1")?;
    for args in [
        vec!["fetch", "ext::anything"],
        vec!["add", "file.txt"],
        vec!["show-ref", "--unknown"],
        vec!["rev-parse", "--verify", "does-not-exist"],
        vec!["cat-file", "-p", "HEAD"],
        vec!["cat-file", "blob", "HEAD"],
    ] {
        let output = rgit(root.path(), &args)?;
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(!output.stderr.is_empty(), "{args:?}");
    }
    Ok(())
}

#[test]
fn unknown_repository_extension_is_rejected() -> Result {
    let root = fixture("sha1")?;
    git(
        root.path(),
        &["config", "core.repositoryformatversion", "1"],
    )?;
    git(root.path(), &["config", "extensions.rgitUnknown", "true"])?;
    let output = rgit(root.path(), &["show-ref"])?;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    Ok(())
}

#[test]
fn runtime_does_not_need_git_or_helpers_on_path() -> Result {
    let root = fixture("sha1")?;
    let empty_path = tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_rgit"))
        .arg("-C")
        .arg(root.path())
        .args(["cat-file", "blob", "HEAD:file.txt"])
        .env("PATH", empty_path.path())
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"first\n\0binary\xff\n");
    Ok(())
}

#[test]
fn inspection_leaves_worktree_index_and_refs_unchanged() -> Result {
    let root = fixture("sha1")?;
    fs::write(root.path().join("file.txt"), b"unstaged\n")?;
    let index = fs::read(root.path().join(".git/index"))?;
    let refs = git(root.path(), &["show-ref"])?.stdout;
    let config = fs::read(root.path().join(".git/config"))?;
    for args in [
        vec!["show-ref"],
        vec!["rev-parse", "--verify", "HEAD"],
        vec!["cat-file", "blob", "HEAD:file.txt"],
    ] {
        assert!(rgit(root.path(), &args)?.status.success());
    }
    assert_eq!(fs::read(root.path().join(".git/index"))?, index);
    assert_eq!(git(root.path(), &["show-ref"])?.stdout, refs);
    assert_eq!(fs::read(root.path().join(".git/config"))?, config);
    assert_eq!(fs::read(root.path().join("file.txt"))?, b"unstaged\n");
    Ok(())
}

#[test]
fn malformed_commit_peeling_returns_error_without_panic() -> Result {
    use std::io::Write;
    use std::process::Stdio;
    let root = fixture("sha1")?;
    let mut child = git_command(root.path())
        .args([
            "hash-object",
            "-w",
            "--stdin",
            "--literally",
            "-t",
            "commit",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("missing stdin")?
        .write_all(b"not a commit\n")?;
    let output = child.wait_with_output()?;
    assert!(output.status.success());
    let id = String::from_utf8(output.stdout)?;
    let output = rgit(root.path(), &["cat-file", "tree", id.trim()])?;
    assert_eq!(output.status.code(), Some(128));
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn non_utf8_tree_path_arguments_remain_lossless() -> Result {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let root = fixture("sha1")?;
    // APFS rejects non-UTF-8 filesystem names, but trees can still contain them.
    // Build the tree through C Git without materializing an invalid filename.
    use std::io::Write;
    use std::process::Stdio;
    let blob = git(root.path(), &["rev-parse", "HEAD:file.txt"])?;
    let mut record = format!(
        "100644 blob {}\tfile-",
        String::from_utf8(blob.stdout)?.trim()
    )
    .into_bytes();
    record.extend_from_slice(b"\xff\0");
    let mut child = git_command(root.path())
        .args(["mktree", "-z"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("missing stdin")?
        .write_all(&record)?;
    let output = child.wait_with_output()?;
    assert!(output.status.success());
    let mut revision = String::from_utf8(output.stdout)?.trim().as_bytes().to_vec();
    revision.extend_from_slice(b":file-\xff");
    let revision = OsStr::from_bytes(&revision);
    let actual = Command::new(env!("CARGO_BIN_EXE_rgit"))
        .arg("-C")
        .arg(root.path())
        .args(["cat-file", "blob"])
        .arg(revision)
        .output()?;
    assert!(actual.status.success());
    assert_eq!(actual.stdout, b"first\n\0binary\xff\n");
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_filesystem_paths_remain_lossless() -> Result {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let root = fixture("sha1")?;
    let nested = root.path().join(OsStr::from_bytes(b"raw-\xff"));
    fs::create_dir(&nested)?;
    same(&nested, &["rev-parse", "--show-toplevel"])?;
    Ok(())
}

#[test]
fn reftable_is_refused_before_empty_success() -> Result {
    // The format marker is sufficient to test our capability boundary even on
    // a C Git release that cannot create real reftables.
    let root = fixture("sha1")?;
    git(
        root.path(),
        &["config", "core.repositoryformatversion", "1"],
    )?;
    git(
        root.path(),
        &["config", "extensions.refStorage", "reftable"],
    )?;
    let output = rgit(root.path(), &["show-ref"])?;
    assert_eq!(output.status.code(), Some(128));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("refstorage"));
    Ok(())
}

#[test]
fn explicit_files_backend_and_v0_unknown_extension_follow_git_rules() -> Result {
    let root = fixture("sha1")?;
    git(root.path(), &["config", "extensions.rgitUnknown", "true"])?;
    same(root.path(), &["show-ref"])?;
    git(
        root.path(),
        &["config", "--unset", "extensions.rgitUnknown"],
    )?;
    git(
        root.path(),
        &["config", "core.repositoryformatversion", "1"],
    )?;
    git(root.path(), &["config", "extensions.refStorage", "files"])?;
    same(root.path(), &["show-ref"])?;
    Ok(())
}

#[test]
fn malformed_ref_is_an_error_not_an_empty_listing() -> Result {
    let root = fixture("sha1")?;
    fs::write(
        root.path().join(".git/refs/heads/zz-broken"),
        b"not-an-object-id\n",
    )?;
    let output = rgit(root.path(), &["show-ref"])?;
    assert_eq!(output.status.code(), Some(128));
    assert!(output.stdout.is_empty());
    Ok(())
}
