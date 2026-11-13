//! C Git creates all repository transitions; a separate membership model owns
//! the expected index and committed tree. No Rust repository writes are claimed.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use rgit_core::{BStr, ObjectId, Repository};
use tempfile::tempdir;

mod support;
use support::*;

fn git_input(root: &Path, args: &[&str], data: &[u8]) -> Result<Output> {
    let mut child = git_command(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().ok_or("missing stdin")?.write_all(data)?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

fn text(output: Output) -> Result<String> {
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn inspect(root: &Path) -> Result {
    for args in [
        vec!["ls-files"],
        vec!["ls-files", "--stage"],
        vec!["ls-files", "--stage", "-z"],
        vec!["ls-files", "--unmerged"],
        vec!["ls-files", "--unmerged", "-z"],
    ] {
        same(root, &args)?;
    }
    Ok(())
}

fn inspect_tree(root: &Path, revision: &str) -> Result {
    for args in [
        vec!["ls-tree", "-r", revision],
        vec!["ls-tree", "-r", "-z", revision],
        vec!["ls-tree", "-r", "--name-only", revision],
        vec!["ls-tree", "-r", "--name-only", "-z", revision],
    ] {
        same(root, &args)?;
    }
    Ok(())
}

#[test]
fn index_versions_flags_and_conflict_stages_match_c_git() -> Result {
    for hash in ["sha1", "sha256"] {
        for version in [2_u32, 3, 4] {
            let root = fixture(hash)?;
            for name in [
                "dir/a",
                "dir/ab",
                "dir/ac",
                "dir/b",
                "space name",
                "tab\tname",
            ] {
                let path = root.path().join(name);
                fs::create_dir_all(path.parent().ok_or("missing parent")?)?;
                fs::write(path, name)?;
            }
            git(root.path(), &["add", "."])?;
            if version != 2 {
                git(
                    root.path(),
                    &["update-index", "--assume-unchanged", "dir/a"],
                )?;
                git(root.path(), &["update-index", "--skip-worktree", "dir/ab"])?;
                fs::write(root.path().join("intent"), b"not staged content")?;
                git(root.path(), &["add", "--intent-to-add", "intent"])?;
            }
            let id = text(git(root.path(), &["rev-parse", "HEAD:file.txt"])?)?;
            // No merge machinery or Rust parser supplies the expected stages.
            let records = format!(
                "100644 {id} 1\tconflict\n100755 {id} 2\tconflict\n120000 {id} 3\tconflict\n100644 {id} 2\tonly-ours\n"
            );
            git_input(
                root.path(),
                &["update-index", "--index-info"],
                records.as_bytes(),
            )?;
            git(
                root.path(),
                &["update-index", &format!("--index-version={version}")],
            )?;
            let bytes = fs::read(root.path().join(".git/index"))?;
            assert_eq!(&bytes[4..8], &version.to_be_bytes());
            inspect(root.path())?;
            let entries = Repository::discover(root.path())?.index_entries()?;
            let conflict: Vec<_> = entries.iter().filter(|e| e.path == b"conflict").collect();
            assert_eq!(
                conflict.iter().map(|e| e.stage).collect::<Vec<_>>(),
                [1, 2, 3]
            );
            assert_eq!(
                conflict.iter().map(|e| e.mode).collect::<Vec<_>>(),
                [0o100644, 0o100755, 0o120000]
            );
            if version != 2 {
                assert!(
                    entries
                        .iter()
                        .any(|e| e.path == b"dir/a" && e.assume_unchanged)
                );
                assert!(
                    entries
                        .iter()
                        .any(|e| e.path == b"dir/ab" && e.skip_worktree)
                );
                assert!(
                    entries
                        .iter()
                        .any(|e| e.path == b"intent" && e.intent_to_add)
                );
            }
            assert_eq!(fs::read(root.path().join(".git/index"))?, bytes);
        }
    }
    Ok(())
}

type Membership = BTreeMap<Vec<u8>, Vec<u8>>;

fn model_check(root: &Path, staged: &Membership, head: &Membership) -> Result {
    let repo = Repository::discover(root)?;
    for (actual, expected) in [
        (repo.index_entries()?, staged),
        (repo.tree_entries(BStr::new(b"HEAD"))?, head),
    ] {
        assert_eq!(actual.len(), expected.len());
        for entry in actual {
            let content = expected.get(&entry.path).ok_or("unexpected model member")?;
            let expected_id = text(git_input(root, &["hash-object", "--stdin"], content)?)?;
            assert_eq!(entry.id.to_string(), expected_id);
            assert_eq!(entry.mode, 0o100644);
            assert_eq!(entry.stage, 0);
        }
    }
    same(root, &["ls-files", "--stage", "-z"])?;
    same(root, &["ls-tree", "-r", "--name-only", "-z", "HEAD"])?;
    Ok(())
}

#[test]
fn independent_membership_model_tracks_c_git_transitions() -> Result {
    for hash in ["sha1", "sha256"] {
        let root = tempdir()?;
        git(
            root.path(),
            &["init", "-b", "main", "--object-format", hash],
        )?;
        git(root.path(), &["commit", "--allow-empty", "-m", "empty"])?;
        let mut staged = Membership::new();
        let mut head = Membership::new();
        model_check(root.path(), &staged, &head)?;
        fs::create_dir(root.path().join("nested"))?;
        for name in ["anchor", "nested/anchor"] {
            fs::write(root.path().join(name), name)?;
            git(root.path(), &["add", "--", name])?;
            staged.insert(name.as_bytes().to_vec(), name.as_bytes().to_vec());
        }
        git(root.path(), &["commit", "-m", "persistent members"])?;
        head = staged.clone();
        model_check(root.path(), &staged, &head)?;
        for turn in 0..4 {
            let name = format!("file-{turn}");
            let content = format!("turn {turn}\n").into_bytes();
            fs::write(root.path().join(&name), &content)?;
            model_check(root.path(), &staged, &head)?; // write alone cannot stage
            git(root.path(), &["add", "--", &name])?;
            staged.insert(name.as_bytes().to_vec(), content.clone());
            model_check(root.path(), &staged, &head)?;
            git(root.path(), &["restore", "--staged", "--", &name])?;
            staged.remove(name.as_bytes());
            model_check(root.path(), &staged, &head)?; // restore removes a new path
            git(root.path(), &["add", "--", &name])?;
            staged.insert(name.as_bytes().to_vec(), content);
            let previous = text(git(root.path(), &["rev-parse", "HEAD"])?)?;
            let previous_head = head.clone();
            git(root.path(), &["commit", "-m", "addition"])?;
            head = staged.clone();
            model_check(root.path(), &staged, &head)?;
            fs::write(root.path().join(&name), b"replacement\n")?;
            git(root.path(), &["add", "--", &name])?;
            staged.insert(name.as_bytes().to_vec(), b"replacement\n".to_vec());
            model_check(root.path(), &staged, &head)?;
            git(root.path(), &["restore", "--staged", "--", &name])?;
            staged.insert(
                name.as_bytes().to_vec(),
                head[&name.as_bytes().to_vec()].clone(),
            );
            model_check(root.path(), &staged, &head)?; // restores committed content
            git(root.path(), &["rm", "--cached", "--", &name])?;
            staged.remove(name.as_bytes());
            model_check(root.path(), &staged, &head)?; // worktree file still exists
            git(root.path(), &["restore", "--staged", "--", &name])?;
            staged.insert(
                name.as_bytes().to_vec(),
                head[&name.as_bytes().to_vec()].clone(),
            );
            git(root.path(), &["reset", "--soft", &previous])?;
            head = previous_head;
            model_check(root.path(), &staged, &head)?; // only committed membership moves
            git(root.path(), &["commit", "-m", "recommit"])?;
            head = staged.clone();
            fs::remove_file(root.path().join(&name))?;
            model_check(root.path(), &staged, &head)?; // unlink alone cannot unstage
            git(root.path(), &["add", "-u", "--", &name])?;
            staged.remove(name.as_bytes());
            model_check(root.path(), &staged, &head)?;
            git(root.path(), &["commit", "-m", "deletion"])?;
            head = staged.clone();
            model_check(root.path(), &staged, &head)?;
        }
    }
    Ok(())
}

#[test]
fn recursive_trees_modes_tags_and_packing_match_c_git() -> Result {
    for hash in ["sha1", "sha256"] {
        let root = fixture(hash)?;
        let blob = text(git(root.path(), &["rev-parse", "HEAD:file.txt"])?)?;
        let commit = text(git(root.path(), &["rev-parse", "HEAD"])?)?;
        let subtree = text(git_input(root.path(), &["mktree"], format!("100755 blob {blob}\texecutable\n120000 blob {blob}\tsymlink\n160000 commit {commit}\tgitlink\n").as_bytes())?)?;
        let tree = text(git_input(
            root.path(),
            &["mktree"],
            format!("100644 blob {blob}\ta.c\n040000 tree {subtree}\ta\n100644 blob {blob}\ta0\n")
                .as_bytes(),
        )?)?;
        git(root.path(), &["tag", "-a", "tree-tag", &tree, "-m", "tree"])?;
        for revision in [&tree, "tree-tag", "v1", "HEAD"] {
            inspect_tree(root.path(), revision)?;
        }
        let entries =
            Repository::discover(root.path())?.tree_entries(BStr::new(tree.as_bytes()))?;
        assert_eq!(
            entries
                .iter()
                .map(|e| e.path.as_slice())
                .collect::<Vec<_>>(),
            [
                b"a.c".as_slice(),
                b"a/executable",
                b"a/gitlink",
                b"a/symlink",
                b"a0"
            ]
        );
        git(root.path(), &["gc", "--prune=now"])?;
        inspect_tree(root.path(), "tree-tag")?;
        assert_eq!(
            Repository::discover(root.path())?.tree_entries(BStr::new(b"tree-tag"))?,
            entries
        );
    }
    Ok(())
}

#[test]
fn missing_indexes_unborn_bare_and_linked_worktrees() -> Result {
    let root = tempdir()?;
    git(root.path(), &["init", "-b", "main"])?;
    inspect(root.path())?;
    assert!(!root.path().join(".git/index").exists());
    assert!(
        Repository::discover(root.path())?
            .tree_entries(BStr::new(b"HEAD"))
            .is_err()
    );
    fs::write(root.path().join("new"), b"new")?;
    git(root.path(), &["add", "new"])?;
    inspect(root.path())?;
    git(root.path(), &["commit", "-m", "first"])?;
    let other = tempdir()?;
    let linked = other.path().join("linked");
    git(
        root.path(),
        &[
            "worktree",
            "add",
            "-b",
            "linked",
            linked.to_str().ok_or("UTF-8 fixture path")?,
        ],
    )?;
    fs::write(linked.join("linked-only"), b"linked")?;
    git(&linked, &["add", "linked-only"])?;
    inspect(&linked)?;
    inspect_tree(&linked, "HEAD")?;
    assert_eq!(Repository::discover(&linked)?.index_entries()?.len(), 2);
    assert_eq!(Repository::discover(root.path())?.index_entries()?.len(), 1);
    let bare = other.path().join("bare");
    git(
        other.path(),
        &[
            "clone",
            "--bare",
            root.path().to_str().ok_or("UTF-8 fixture path")?,
            bare.to_str().ok_or("UTF-8 fixture path")?,
        ],
    )?;
    inspect(&bare)?;
    inspect_tree(&bare, "HEAD")?;
    git(&bare, &["read-tree", "HEAD"])?; // an explicitly supplied bare index
    inspect(&bare)?;
    fs::remove_file(root.path().join(".git/index"))?;
    inspect(root.path())?; // missing index is empty, never synthesized from HEAD
    Ok(())
}

fn refused(root: &Path, args: &[&str], diagnostic: Option<&str>) -> Result {
    let output = rgit(root, args)?;
    assert_eq!(output.status.code(), Some(128), "{args:?}: {output:?}");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    if let Some(diagnostic) = diagnostic {
        assert!(stderr.contains(diagnostic), "{stderr}");
    }
    Ok(())
}

#[test]
fn checksum_and_truncation_fail_without_partial_output() -> Result {
    for hash in ["sha1", "sha256"] {
        let root = fixture(hash)?;
        let path = root.path().join(".git/index");
        let original = fs::read(&path)?;
        for skip_hash in ["false", "true"] {
            git(root.path(), &["config", "index.skipHash", skip_hash])?;
            for offset in [12, original.len() - 1] {
                let mut damaged = original.clone();
                damaged[offset] ^= 1;
                fs::write(&path, damaged)?;
                refused(root.path(), &["ls-files", "--stage"], None)?;
            }
        }
        for size in [0, 1, 11, 12, 31, original.len() - 1] {
            fs::write(&path, &original[..size])?;
            refused(root.path(), &["ls-files", "--stage"], None)?;
        }
        let mut omitted = original.clone();
        let hash_len = if hash == "sha1" { 20 } else { 32 };
        let end = omitted.len();
        omitted[end - hash_len..].fill(0);
        fs::write(&path, &omitted)?;
        inspect(root.path())?; // Git permits index.skipHash's all-zero trailer
        fs::remove_file(&path)?;
        fs::create_dir(&path)?;
        refused(root.path(), &["ls-files"], None)?;
    }
    Ok(())
}

// Independent, deliberately minimal format-v2 encoder. The zero checksum is
// explicitly allowed by Git, so malformed entry tests reach semantic validation.
fn index_v2(entries: &[(&[u8], u32, u32, ObjectId)]) -> Vec<u8> {
    let mut data = b"DIRC\0\0\0\x02".to_vec();
    data.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (path, mode, stage, id) in entries {
        let start = data.len();
        data.extend_from_slice(&[0; 24]);
        data.extend_from_slice(&mode.to_be_bytes());
        data.extend_from_slice(&[0; 12]);
        data.extend_from_slice(id.as_bytes());
        data.extend_from_slice(&(((*stage as u16) << 12) | path.len() as u16).to_be_bytes());
        data.extend_from_slice(path);
        data.push(0);
        while !(data.len() - start).is_multiple_of(8) {
            data.push(0);
        }
    }
    let hash_len = entries.first().map_or(20, |e| e.3.as_bytes().len());
    data.resize(data.len() + hash_len, 0);
    data
}

#[test]
fn unsafe_index_paths_and_inconsistent_entries_are_refused() -> Result {
    let root = fixture("sha1")?;
    let repo = Repository::discover(root.path())?;
    let id = repo.resolve(BStr::new(b"HEAD:file.txt"))?;
    let index_path = root.path().join(".git/index");
    for path in [
        b"".as_slice(),
        b"/absolute",
        b"../outside",
        b"a/../../outside",
        b"./x",
        b"a//x",
        b"a/",
        b".git/config",
        b"a/.GiT/config",
        b"nul\0path",
    ] {
        fs::write(&index_path, index_v2(&[(path, 0o100644, 0, id)]))?;
        refused(
            root.path(),
            &["ls-files", "--stage"],
            Some("unsafe repository path"),
        )?;
        assert!(repo.index_entries().is_err());
    }
    for entries in [
        vec![
            (b"same".as_slice(), 0o100644, 0, id),
            (b"same", 0o100644, 0, id),
        ],
        vec![
            (b"same".as_slice(), 0o100644, 0, id),
            (b"same", 0o100644, 1, id),
        ],
        vec![(b"z".as_slice(), 0o100644, 0, id), (b"a", 0o100644, 0, id)],
        vec![
            (b"a".as_slice(), 0o100644, 0, id),
            (b"a.", 0o100644, 0, id),
            (b"a/x", 0o100644, 0, id),
        ],
        vec![(b"mode".as_slice(), 0, 0, id)],
    ] {
        fs::write(&index_path, index_v2(&entries))?;
        refused(root.path(), &["ls-files", "--stage"], None)?;
    }
    // A real directory/file merge conflict can put the file and child in
    // different stages. Refusing it would discard a valid Git index state.
    fs::write(
        &index_path,
        index_v2(&[(b"a", 0o100644, 2, id), (b"a/x", 0o100644, 3, id)]),
    )?;
    inspect(root.path())?;
    Ok(())
}

#[test]
fn split_and_sparse_indexes_fail_explicitly() -> Result {
    let root = fixture("sha1")?;
    git(root.path(), &["update-index", "--split-index"])?;
    refused(root.path(), &["ls-files"], Some("split index"))?;
    // Refuse the link marker before attempting to load any shared index.
    for entry in fs::read_dir(root.path().join(".git"))? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("sharedindex.")
        {
            fs::remove_file(entry.path())?;
        }
    }
    refused(root.path(), &["ls-files"], Some("split index"))?;
    let sparse = fixture("sha1")?;
    for name in ["included/file", "excluded/file"] {
        let path = sparse.path().join(name);
        fs::create_dir_all(path.parent().ok_or("missing parent")?)?;
        fs::write(path, name)?;
    }
    git(sparse.path(), &["add", "."])?;
    git(sparse.path(), &["commit", "-m", "directories"])?;
    git(
        sparse.path(),
        &[
            "sparse-checkout",
            "set",
            "--cone",
            "--sparse-index",
            "included",
        ],
    )?;
    refused(sparse.path(), &["ls-files"], Some("sparse index"))?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn raw_index_and_tree_path_bytes_match_c_git() -> Result {
    let root = fixture("sha1")?;
    let repo = Repository::discover(root.path())?;
    let id = repo.resolve(BStr::new(b"HEAD:file.txt"))?;
    let names: &[&[u8]] = &[
        b"back\\slash",
        b"colon:name",
        b"high-\xff",
        b"line\nname",
        b"quote\"name",
        b"tab\tname",
    ];
    let entries: Vec<_> = names.iter().map(|name| (*name, 0o100644, 0, id)).collect();
    fs::write(root.path().join(".git/index"), index_v2(&entries))?;
    inspect(root.path())?;
    let mut records = Vec::new();
    for name in names {
        records.extend_from_slice(format!("100644 blob {id}\t").as_bytes());
        records.extend_from_slice(name);
        records.push(0);
    }
    let tree = text(git_input(root.path(), &["mktree", "-z"], &records)?)?;
    inspect_tree(root.path(), &tree)?;
    assert_eq!(
        repo.tree_entries(BStr::new(tree.as_bytes()))?
            .iter()
            .map(|e| e.path.as_slice())
            .collect::<Vec<_>>(),
        names
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn native_path_bytes_are_used_when_filesystem_accepts_them() -> Result {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let root = fixture("sha1")?;
    let name = OsStr::from_bytes(b"native-\xff");
    match fs::write(root.path().join(name), b"native bytes") {
        Ok(()) => {}
        Err(error)
            if cfg!(target_os = "macos") && matches!(error.raw_os_error(), Some(1) | Some(92)) =>
        {
            eprintln!(
                "SKIP native non-UTF-8 filename: filesystem rejected it ({error}); raw index/tree byte tests still run"
            );
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    }
    let output = git_command(root.path())
        .args(["add", "--"])
        .arg(name)
        .output()?;
    assert!(output.status.success(), "{output:?}");
    inspect(root.path())?;
    git(root.path(), &["commit", "-m", "native bytes"])?;
    inspect_tree(root.path(), "HEAD")?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn index_fifo_and_symlinks_are_refused_before_opening() -> Result {
    use std::os::unix::fs::symlink;
    use std::time::{Duration, Instant};
    let root = fixture("sha1")?;
    let index = root.path().join(".git/index");
    let regular = root.path().join("saved-index");
    fs::rename(&index, &regular)?;
    for target in [None, Some(regular.as_path()), Some(Path::new("/dev/zero"))] {
        if let Some(target) = target {
            symlink(target, &index)?;
        } else {
            assert!(Command::new("mkfifo").arg(&index).status()?.success());
        }
        for args in [
            vec!["ls-files", "--stage"],
            vec!["status", "--porcelain", "-z"],
        ] {
            let mut child = Command::new(env!("CARGO_BIN_EXE_rgit"))
                .arg("-C")
                .arg(root.path())
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let deadline = Instant::now() + Duration::from_secs(2);
            while child.try_wait()?.is_none() {
                if Instant::now() >= deadline {
                    child.kill()?;
                    child.wait()?;
                    return Err(format!("{args:?} blocked on non-regular index {target:?}").into());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output()?;
            assert_eq!(output.status.code(), Some(128), "{output:?}");
            assert!(output.stdout.is_empty());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("symlink or special index file")
            );
        }
        fs::remove_file(&index)?;
    }
    Ok(())
}

#[test]
fn malformed_or_missing_recursive_tree_is_not_partial_success() -> Result {
    let root = fixture("sha1")?;
    let repo = Repository::discover(root.path())?;
    let blob = repo.resolve(BStr::new(b"HEAD:file.txt"))?;
    for name in [b"../outside".as_slice(), b".git", b"a//b", b"a\0b"] {
        let mut data = b"100644 ".to_vec();
        data.extend_from_slice(name);
        data.push(0);
        data.extend_from_slice(blob.as_bytes());
        let tree = text(git_input(
            root.path(),
            &["hash-object", "-w", "--stdin", "--literally", "-t", "tree"],
            &data,
        )?)?;
        refused(root.path(), &["ls-tree", "-r", "--name-only", &tree], None)?;
    }
    let mut data = b"100644 a\0".to_vec();
    data.extend_from_slice(blob.as_bytes());
    data.extend_from_slice(b"40000 missing\0");
    data.extend_from_slice(&[1; 20]);
    let tree = text(git_input(
        root.path(),
        &["hash-object", "-w", "--stdin", "--literally", "-t", "tree"],
        &data,
    )?)?;
    refused(root.path(), &["ls-tree", "-r", "--name-only", &tree], None)?;
    Ok(())
}

#[test]
fn inspection_runs_without_git_and_preserves_repository_bytes() -> Result {
    let root = fixture("sha1")?;
    let index = fs::read(root.path().join(".git/index"))?;
    let config = fs::read(root.path().join(".git/config"))?;
    let refs = git(root.path(), &["show-ref"])?.stdout;
    let empty_path = tempdir()?;
    for args in [
        vec!["ls-files", "--stage", "-z"],
        vec!["ls-tree", "-r", "--name-only", "-z", "HEAD"],
    ] {
        let expected = git(root.path(), &args)?.stdout;
        let output = Command::new(env!("CARGO_BIN_EXE_rgit"))
            .arg("-C")
            .arg(root.path())
            .args(args)
            .env("PATH", empty_path.path())
            .output()?;
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, expected);
    }
    assert_eq!(fs::read(root.path().join(".git/index"))?, index);
    assert_eq!(fs::read(root.path().join(".git/config"))?, config);
    assert_eq!(git(root.path(), &["show-ref"])?.stdout, refs);
    assert!(!root.path().join(".git/index.lock").exists());
    Ok(())
}
