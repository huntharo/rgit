//! Tree/commit creation and compare-and-swap reference publication, compared
//! with C Git as an independent implementation. C Git builds every fixture
//! (init, add, update-index) so these tests do not depend on rgit's own
//! init/add. Every scenario ends with `git fsck --strict`.
// Several helpers exercise Unix-only behaviour (symlinks, FIFOs, modes).
#![cfg_attr(not(unix), allow(unused_imports, unused_variables, dead_code))]

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Output, Stdio};

use common::{Result, fsck, git, git_command};
use rgit_core::write::{CommitRequest, RefExpectation, RefLog, RefUpdate, Signature};
use rgit_core::{Error, ObjectId, Repository};

const FORMATS: [&str; 2] = ["sha1", "sha256"];
/// 2026-01-01T00:00:00Z, the date `common::git_command` gives C Git.
const FIXTURE_SECONDS: i64 = 1_767_225_600;

fn init(dir: &Path, format: &str) -> Result {
    git(
        dir,
        &[
            "init",
            "-q",
            "-b",
            "main",
            &format!("--object-format={format}"),
        ],
    )?;
    Ok(())
}

fn text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn rev(dir: &Path, revision: &str) -> Result<String> {
    Ok(text(&git(dir, &["rev-parse", "--verify", "-q", revision])?))
}

fn oid(hex: &str) -> Result<ObjectId> {
    Ok(ObjectId::from_hex(hex.as_bytes())?)
}

fn git_stdin(dir: &Path, args: &[&str], input: &[u8]) -> Result<Output> {
    let mut child = git_command(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().ok_or("stdin")?.write_all(input)?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

fn blob(dir: &Path, content: &[u8]) -> Result<String> {
    Ok(text(&git_stdin(
        dir,
        &["hash-object", "-w", "--stdin"],
        content,
    )?))
}

fn signature() -> Signature {
    Signature {
        name: b"Test".to_vec(),
        email: b"test@example.invalid".to_vec(),
        seconds: FIXTURE_SECONDS,
        offset_minutes: 0,
    }
}

fn request(message: &[u8]) -> CommitRequest {
    CommitRequest {
        message: message.to_vec(),
        author: signature(),
        committer: signature(),
        allow_empty: false,
    }
}

fn write_file(dir: &Path, name: &str, content: &str) -> Result {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}

/// Repository with one C Git commit on `main` (file `a`).
fn repo_with_commit(dir: &Path, format: &str) -> Result<String> {
    init(dir, format)?;
    write_file(dir, "a", "one\n")?;
    git(dir, &["add", "a"])?;
    git(dir, &["commit", "-q", "-m", "first"])?;
    rev(dir, "HEAD")
}

fn read(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    Ok(std::fs::read(path)?)
}

fn reflog_subjects(dir: &Path, name: &str) -> Result<Vec<String>> {
    let output = git(dir, &["reflog", "show", "--format=%H %gs", name, "--"])?;
    Ok(text(&output).lines().map(str::to_owned).collect())
}

#[test]
fn write_tree_matches_c_git_for_nested_modes_and_native_paths() -> Result {
    for format in FORMATS {
        let temp = tempfile::tempdir()?;
        let dir = temp.path();
        init(dir, format)?;
        let file = blob(dir, b"file\n")?;
        let exec = blob(dir, b"#!/bin/sh\n")?;
        let link = blob(dir, b"target/path")?;
        let paths: [(&str, &str, &[u8]); 11] = [
            ("100644", &file, b"a-b"),
            ("100644", &file, b"a.b"),
            ("100644", &file, b"a/b.txt"),
            ("100644", &file, b"a/c/d"),
            ("100644", &file, b"a0"),
            ("100644", &file, b"a=b"),
            ("100644", &file, b"b\xff\xfe-native"),
            ("100644", &file, b"deep/1/2/3/4/5/6/7/file"),
            ("100755", &exec, b"exec.sh"),
            ("120000", &link, b"link"),
            ("100644", &file, b"z/\xe9t\xe9"),
        ];
        let mut info = Vec::new();
        for (mode, id, path) in paths {
            info.extend_from_slice(format!("{mode} {id}\t").as_bytes());
            info.extend_from_slice(path);
            info.push(b'\n');
        }
        git_stdin(dir, &["update-index", "--index-info"], &info)?;

        let repo = Repository::discover(dir)?;
        let ours = repo.write_tree_from_index()?;
        let theirs = text(&git(dir, &["write-tree"])?);
        assert_eq!(ours.to_string(), theirs, "{format}");
        fsck(dir)?;
    }
    Ok(())
}

#[test]
fn empty_index_writes_the_empty_tree_object() -> Result {
    for format in FORMATS {
        let temp = tempfile::tempdir()?;
        let dir = temp.path();
        init(dir, format)?;
        let repo = Repository::discover(dir)?;
        let ours = repo.write_tree_from_index()?.to_string();
        assert_eq!(ours, text(&git(dir, &["write-tree"])?));
        // C Git treats the empty tree as virtual; check the stored file.
        let stored = dir.join(".git/objects").join(&ours[..2]).join(&ours[2..]);
        assert!(stored.is_file(), "{format}: empty tree not stored");
        fsck(dir)?;
    }
    Ok(())
}

#[test]
fn commit_bytes_match_c_git_commit_tree() -> Result {
    let message: &[u8] = b"subject line\n\n  body  with\tspacing\n\n\ntrailing\n\n";
    for format in FORMATS {
        let temp = tempfile::tempdir()?;
        let dir = temp.path();
        init(dir, format)?;
        write_file(dir, "x/y", "content\n")?;
        git(dir, &["add", "."])?;
        let author = Signature {
            name: "Ä Author".as_bytes().to_vec(),
            email: b"author@example.invalid".to_vec(),
            seconds: 1_700_000_000,
            offset_minutes: -330,
        };
        let committer = Signature {
            name: b"Committer".to_vec(),
            email: b"committer@example.invalid".to_vec(),
            seconds: 1_700_000_100,
            offset_minutes: 60,
        };
        let repo = Repository::discover(dir)?;
        let request = CommitRequest {
            message: message.to_vec(),
            author: author.clone(),
            committer: committer.clone(),
            allow_empty: false,
        };
        let root = repo.commit(&request)?;
        let message_file = dir.join(".git/test-message");
        std::fs::write(&message_file, message)?;
        let commit_tree = |parent: Option<&str>| -> Result<String> {
            let tree = rev(dir, "HEAD^{tree}")?;
            let mut command = git_command(dir);
            command
                .args(["commit-tree", &tree, "-F"])
                .arg(&message_file)
                .env("GIT_AUTHOR_NAME", "Ä Author")
                .env("GIT_AUTHOR_EMAIL", "author@example.invalid")
                .env("GIT_AUTHOR_DATE", "1700000000 -0530")
                .env("GIT_COMMITTER_NAME", "Committer")
                .env("GIT_COMMITTER_EMAIL", "committer@example.invalid")
                .env("GIT_COMMITTER_DATE", "1700000100 +0100");
            if let Some(parent) = parent {
                command.args(["-p", parent]);
            }
            let output = command.output()?;
            assert!(output.status.success());
            Ok(text(&output))
        };
        assert_eq!(root.id.to_string(), commit_tree(None)?, "{format} root");
        assert_eq!(root.parent, None);
        assert_eq!(root.reference, b"refs/heads/main");

        write_file(dir, "x/z", "more\n")?;
        git(dir, &["add", "."])?;
        let child = repo.commit(&request)?;
        assert_eq!(child.parent, Some(root.id));
        assert_eq!(
            child.id.to_string(),
            commit_tree(Some(&root.id.to_string()))?,
            "{format} child"
        );
        assert_eq!(rev(dir, "refs/heads/main")?, child.id.to_string());
        fsck(dir)?;
    }
    Ok(())
}

/// The same commits made by C Git and by rgit in twin repositories produce
/// identical object IDs and byte-identical branch and HEAD reflogs.
#[test]
fn commits_and_reflogs_match_c_git_twin() -> Result {
    for format in FORMATS {
        let theirs = tempfile::tempdir()?;
        let ours = tempfile::tempdir()?;
        for dir in [theirs.path(), ours.path()] {
            init(dir, format)?;
            write_file(dir, "a", "one\n")?;
            git(dir, &["add", "a"])?;
        }
        git(theirs.path(), &["commit", "-q", "-m", "initial subject"])?;
        let repo = Repository::discover(ours.path())?;
        let first = repo.commit(&request(b"initial subject\n"))?;
        assert_eq!(first.id.to_string(), rev(theirs.path(), "HEAD")?);

        for dir in [theirs.path(), ours.path()] {
            write_file(dir, "b/c", "two\n")?;
            git(dir, &["add", "b"])?;
        }
        git(
            theirs.path(),
            &["commit", "-q", "-m", "second\tcommit  here"],
        )?;
        let second = repo.commit(&request(b"second\tcommit  here\n\nbody\n"))?;
        // Different bodies, so only the reflogs (subject-only) can match.
        assert_ne!(second.id.to_string(), rev(theirs.path(), "HEAD")?);
        assert_eq!(
            reflog_subjects(ours.path(), "HEAD")?
                .iter()
                .map(|line| line.split_once(' ').map(|x| x.1.to_owned()))
                .collect::<Vec<_>>(),
            reflog_subjects(theirs.path(), "HEAD")?
                .iter()
                .map(|line| line.split_once(' ').map(|x| x.1.to_owned()))
                .collect::<Vec<_>>()
        );
        // Byte-compare the reflog files with the commit IDs normalized.
        let normalize = |dir: &Path, log: &str, ids: &[String]| -> Result<String> {
            let mut text = String::from_utf8(read(dir.join(".git").join(log))?)?;
            for (index, id) in ids.iter().enumerate() {
                text = text.replace(id, &format!("<{index}>"));
            }
            Ok(text)
        };
        let their_ids = [rev(theirs.path(), "HEAD~1")?, rev(theirs.path(), "HEAD")?];
        let our_ids = [first.id.to_string(), second.id.to_string()];
        for log in ["logs/HEAD", "logs/refs/heads/main"] {
            assert_eq!(
                normalize(ours.path(), log, &our_ids)?,
                normalize(theirs.path(), log, &their_ids)?,
                "{format} {log}"
            );
        }
        assert_eq!(
            reflog_subjects(ours.path(), "refs/heads/main")?,
            vec![
                format!("{} commit: second commit here", second.id),
                format!("{} commit (initial): initial subject", first.id),
            ]
        );
        fsck(ours.path())?;
    }
    Ok(())
}

#[test]
fn nothing_to_commit_is_refused_unless_allowed() -> Result {
    for format in FORMATS {
        let temp = tempfile::tempdir()?;
        let dir = temp.path();
        init(dir, format)?;
        let repo = Repository::discover(dir)?;
        // Unborn branch with an empty index.
        assert!(matches!(
            repo.commit(&request(b"empty\n")),
            Err(Error::NothingToCommit)
        ));
        let unborn = git_command(dir)
            .args(["rev-parse", "-q", "--verify", "HEAD"])
            .output()?;
        assert!(!unborn.status.success());

        let temp = tempfile::tempdir()?;
        let dir = temp.path();
        let head = repo_with_commit(dir, format)?;
        let repo = Repository::discover(dir)?;
        assert!(matches!(
            repo.commit(&request(b"same\n")),
            Err(Error::NothingToCommit)
        ));
        assert_eq!(rev(dir, "HEAD")?, head);
        let mut allowed = request(b"allowed\n");
        allowed.allow_empty = true;
        let outcome = repo.commit(&allowed)?;
        assert_eq!(outcome.parent.map(|id| id.to_string()), Some(head));
        assert_eq!(rev(dir, "HEAD^{tree}")?, rev(dir, "HEAD~1^{tree}")?);
        fsck(dir)?;
    }
    Ok(())
}

#[test]
fn detached_head_commit_moves_only_head() -> Result {
    for format in FORMATS {
        let temp = tempfile::tempdir()?;
        let dir = temp.path();
        let base = repo_with_commit(dir, format)?;
        git(dir, &["checkout", "-q", "--detach"])?;
        write_file(dir, "a", "two\n")?;
        git(dir, &["add", "a"])?;
        let branch_log = read(dir.join(".git/logs/refs/heads/main"))?;
        let outcome = Repository::discover(dir)?.commit(&request(b"detached\n"))?;
        assert_eq!(outcome.reference, b"HEAD");
        assert_eq!(rev(dir, "refs/heads/main")?, base);
        assert_eq!(
            read(dir.join(".git/HEAD"))?,
            format!("{}\n", outcome.id).into_bytes()
        );
        assert_eq!(read(dir.join(".git/logs/refs/heads/main"))?, branch_log);
        assert_eq!(
            reflog_subjects(dir, "HEAD")?.first(),
            Some(&format!("{} commit: detached", outcome.id))
        );
        fsck(dir)?;
    }
    Ok(())
}

#[test]
fn linked_worktree_commit_touches_only_its_head_and_branch() -> Result {
    for format in FORMATS {
        let temp = tempfile::tempdir()?;
        let main = temp.path().join("main");
        std::fs::create_dir(&main)?;
        let base = repo_with_commit(&main, format)?;
        let linked = temp.path().join("linked");
        git(
            &main,
            &["worktree", "add", "-q", "-b", "feature", "../linked"],
        )?;
        write_file(&linked, "b", "linked\n")?;
        git(&linked, &["add", "b"])?;
        let git_dir = main.join(".git");
        let snapshot = |paths: &[&str]| -> Result<Vec<Vec<u8>>> {
            paths.iter().map(|path| read(git_dir.join(path))).collect()
        };
        let main_state = ["HEAD", "index", "logs/HEAD", "logs/refs/heads/main"];
        let before = snapshot(&main_state)?;

        let outcome = Repository::discover(&linked)?.commit(&request(b"in linked\n"))?;
        assert_eq!(outcome.reference, b"refs/heads/feature");
        assert_eq!(snapshot(&main_state)?, before);
        assert_eq!(rev(&main, "refs/heads/main")?, base);
        assert_eq!(rev(&main, "refs/heads/feature")?, outcome.id.to_string());
        assert_eq!(rev(&linked, "HEAD")?, outcome.id.to_string());
        assert!(git_dir.join("refs/heads/feature").is_file());
        assert!(!git_dir.join("worktrees/linked/refs/heads").exists());
        assert_eq!(
            reflog_subjects(&linked, "HEAD")?.first(),
            Some(&format!("{} commit: in linked", outcome.id))
        );
        assert_eq!(
            reflog_subjects(&main, "refs/heads/feature")?.first(),
            Some(&format!("{} commit: in linked", outcome.id))
        );
        assert!(text(&git(&linked, &["status", "--porcelain"])?).is_empty());

        // Detached in the linked worktree: only its own HEAD file changes.
        git(&linked, &["checkout", "-q", "--detach"])?;
        write_file(&linked, "b", "again\n")?;
        git(&linked, &["add", "b"])?;
        let feature = rev(&main, "refs/heads/feature")?;
        let detached = Repository::discover(&linked)?.commit(&request(b"detached\n"))?;
        assert_eq!(detached.reference, b"HEAD");
        assert_eq!(
            read(git_dir.join("worktrees/linked/HEAD"))?,
            format!("{}\n", detached.id).into_bytes()
        );
        assert_eq!(rev(&main, "refs/heads/feature")?, feature);
        assert_eq!(snapshot(&main_state)?, before);
        fsck(&main)?;
    }
    Ok(())
}

#[test]
fn stale_expectation_is_a_ref_conflict_and_changes_nothing() -> Result {
    for format in FORMATS {
        let temp = tempfile::tempdir()?;
        let dir = temp.path();
        let first = repo_with_commit(dir, format)?;
        git(dir, &["commit", "-q", "--allow-empty", "-m", "second"])?;
        let second = rev(dir, "HEAD")?;
        let repo = Repository::discover(dir)?;
        let log_before = read(dir.join(".git/logs/refs/heads/main"))?;
        for expected in [
            RefExpectation::MustEqual(oid(&first)?),
            RefExpectation::MustNotExist,
        ] {
            let result = repo.update_ref(&RefUpdate {
                name: b"refs/heads/main".to_vec(),
                new: oid(&first)?,
                expected,
                log: Some(RefLog {
                    committer: signature(),
                    message: b"rewind".to_vec(),
                }),
            });
            match result {
                Err(Error::RefConflict { name, actual, .. }) => {
                    assert_eq!(name, b"refs/heads/main");
                    assert_eq!(actual, Some(oid(&second)?));
                }
                other => panic!("expected conflict, got {other:?}"),
            }
            assert_eq!(rev(dir, "refs/heads/main")?, second);
            assert!(!dir.join(".git/refs/heads/main.lock").exists());
            assert_eq!(read(dir.join(".git/logs/refs/heads/main"))?, log_before);
        }
        // Creating with MustEqual on an absent ref is also a conflict.
        assert!(matches!(
            repo.update_ref(&RefUpdate {
                name: b"refs/heads/new".to_vec(),
                new: oid(&first)?,
                expected: RefExpectation::MustEqual(oid(&first)?),
                log: None,
            }),
            Err(Error::RefConflict { actual: None, .. })
        ));
        assert!(!dir.join(".git/refs/heads/new").exists());

        // The correct expectation succeeds; HEAD's reflog follows its branch
        // exactly as `git update-ref -m` would record it.
        repo.update_ref(&RefUpdate {
            name: b"refs/heads/main".to_vec(),
            new: oid(&first)?,
            expected: RefExpectation::MustEqual(oid(&second)?),
            log: Some(RefLog {
                committer: signature(),
                message: b"  rewind   to\nfirst  ".to_vec(),
            }),
        })?;
        assert_eq!(rev(dir, "refs/heads/main")?, first);
        let entry = format!("{first} rewind to first");
        assert_eq!(
            reflog_subjects(dir, "refs/heads/main")?.first(),
            Some(&entry)
        );
        assert_eq!(reflog_subjects(dir, "HEAD")?.first(), Some(&entry));
        let twin = tempfile::tempdir()?;
        repo_with_commit(twin.path(), format)?;
        git(
            twin.path(),
            &["commit", "-q", "--allow-empty", "-m", "second"],
        )?;
        git(
            twin.path(),
            &[
                "update-ref",
                "-m",
                "  rewind   to\nfirst  ",
                "refs/heads/main",
                &first,
                &second,
            ],
        )?;
        for log in ["logs/HEAD", "logs/refs/heads/main"] {
            assert_eq!(
                read(dir.join(".git").join(log))?,
                read(twin.path().join(".git").join(log))?,
                "{format} {log}"
            );
        }
        fsck(dir)?;
    }
    Ok(())
}

#[test]
fn held_locks_refuse_and_are_left_alone() -> Result {
    for format in FORMATS {
        let temp = tempfile::tempdir()?;
        let dir = temp.path();
        let head = repo_with_commit(dir, format)?;
        write_file(dir, "a", "changed\n")?;
        git(dir, &["add", "a"])?;
        let repo = Repository::discover(dir)?;
        for lock in ["refs/heads/main.lock", "HEAD.lock"] {
            let path = dir.join(".git").join(lock);
            std::fs::write(&path, b"someone else\n")?;
            match repo.commit(&request(b"blocked\n")) {
                Err(Error::Locked(locked)) => assert_eq!(locked, path),
                other => panic!("expected Locked for {lock}, got {other:?}"),
            }
            assert_eq!(read(&path)?, b"someone else\n");
            assert_eq!(rev(dir, "HEAD")?, head);
            assert_eq!(
                text(&git(dir, &["symbolic-ref", "HEAD"])?),
                "refs/heads/main"
            );
            std::fs::remove_file(&path)?;
        }
        assert_eq!(reflog_subjects(dir, "HEAD")?.len(), 1);
        fsck(dir)?;
        repo.commit(&request(b"unblocked\n"))?;
        fsck(dir)?;
    }
    Ok(())
}

#[test]
fn missing_or_mistyped_objects_are_never_published() -> Result {
    for format in FORMATS {
        let temp = tempfile::tempdir()?;
        let dir = temp.path();
        let head = repo_with_commit(dir, format)?;
        let repo = Repository::discover(dir)?;
        let absent = if format == "sha1" {
            "1111111111111111111111111111111111111111".to_owned()
        } else {
            "1".repeat(64)
        };
        assert!(matches!(
            repo.update_ref(&RefUpdate {
                name: b"refs/heads/main".to_vec(),
                new: oid(&absent)?,
                expected: RefExpectation::Any,
                log: None,
            }),
            Err(Error::MissingObject(id)) if id == oid(&absent)?
        ));
        let tree = rev(dir, "HEAD^{tree}")?;
        assert!(matches!(
            repo.update_ref(&RefUpdate {
                name: b"refs/heads/tree".to_vec(),
                new: oid(&tree)?,
                expected: RefExpectation::Any,
                log: None,
            }),
            Err(Error::UnexpectedKind { .. })
        ));
        // A blob may be named outside refs/heads (as `git update-ref` allows).
        let blob_id = blob(dir, b"tagged blob\n")?;
        repo.update_ref(&RefUpdate {
            name: b"refs/tags/blob".to_vec(),
            new: oid(&blob_id)?,
            expected: RefExpectation::MustNotExist,
            log: None,
        })?;
        assert_eq!(rev(dir, "refs/tags/blob")?, blob_id);
        assert!(!dir.join(".git/logs/refs/tags/blob").exists());

        // An index entry naming an absent blob refuses the whole commit.
        git_stdin(
            dir,
            &["update-index", "--index-info"],
            format!("100644 {absent}\tghost\n").as_bytes(),
        )?;
        assert!(matches!(
            repo.commit(&request(b"ghost\n")),
            Err(Error::MissingObject(id)) if id == oid(&absent)?
        ));
        assert_eq!(rev(dir, "HEAD")?, head);
        assert!(!dir.join(".git/refs/heads/tree").exists());
        git(dir, &["update-index", "--force-remove", "ghost"])?;
        fsck(dir)?;
    }
    Ok(())
}

#[test]
fn packed_only_branch_is_shadowed_by_a_new_loose_ref() -> Result {
    for format in FORMATS {
        let temp = tempfile::tempdir()?;
        let dir = temp.path();
        let first = repo_with_commit(dir, format)?;
        git(dir, &["tag", "-a", "-m", "annotated", "v1"])?;
        git(dir, &["pack-refs", "--all"])?;
        assert!(!dir.join(".git/refs/heads/main").exists());
        let packed = read(dir.join(".git/packed-refs"))?;
        assert!(packed.windows(2).any(|w| w == b"\n^"), "peeled line");

        let repo = Repository::discover(dir)?;
        // Stale expectation against the packed value is still detected.
        assert!(matches!(
            repo.update_ref(&RefUpdate {
                name: b"refs/heads/main".to_vec(),
                new: oid(&first)?,
                expected: RefExpectation::MustNotExist,
                log: None,
            }),
            Err(Error::RefConflict {
                actual: Some(_),
                ..
            })
        ));
        write_file(dir, "a", "two\n")?;
        git(dir, &["add", "a"])?;
        let outcome = repo.commit(&request(b"after pack\n"))?;
        assert_eq!(outcome.parent.map(|id| id.to_string()), Some(first));
        assert_eq!(read(dir.join(".git/packed-refs"))?, packed);
        assert_eq!(rev(dir, "refs/heads/main")?, outcome.id.to_string());
        assert_eq!(
            rev(dir, "v1^{commit}")?,
            outcome.parent.ok_or("parent")?.to_string()
        );
        git(dir, &["show-ref"])?;
        fsck(dir)?;
    }
    Ok(())
}

#[test]
fn directory_file_ref_conflicts_are_refused_without_changes() -> Result {
    for format in FORMATS {
        let temp = tempfile::tempdir()?;
        let dir = temp.path();
        let head = oid(&repo_with_commit(dir, format)?)?;
        let repo = Repository::discover(dir)?;
        let update = |name: &[u8]| RefUpdate {
            name: name.to_vec(),
            new: head,
            expected: RefExpectation::Any,
            log: None,
        };
        // Loose file vs. new child.
        let result = repo.update_ref(&update(b"refs/heads/main/child"));
        assert!(matches!(result, Err(Error::AlreadyExists(_))), "{result:?}");
        // Loose child vs. new parent.
        repo.update_ref(&update(b"refs/heads/topic/one"))?;
        assert!(matches!(
            repo.update_ref(&update(b"refs/heads/topic")),
            Err(Error::AlreadyExists(_))
        ));
        // Packed parent/child, with no loose files left.
        git(dir, &["pack-refs", "--all"])?;
        assert!(!dir.join(".git/refs/heads/topic").exists());
        assert!(matches!(
            repo.update_ref(&update(b"refs/heads/topic")),
            Err(Error::AlreadyExists(_))
        ));
        assert!(matches!(
            repo.update_ref(&update(b"refs/heads/main/child")),
            Err(Error::AlreadyExists(_))
        ));
        assert!(!dir.join(".git/refs/heads/main").exists());
        assert!(!dir.join(".git/refs/heads/topic").exists());
        let refs = text(&git(dir, &["for-each-ref", "--format=%(refname)"])?);
        assert_eq!(refs, "refs/heads/main\nrefs/heads/topic/one");
        fsck(dir)?;
    }
    Ok(())
}

#[test]
fn reference_names_follow_check_ref_format() -> Result {
    let temp = tempfile::tempdir()?;
    let dir = temp.path();
    let head = oid(&repo_with_commit(dir, "sha1")?)?;
    let repo = Repository::discover(dir)?;
    let names: &[&[u8]] = &[
        b"refs/heads/ok",
        b"refs/heads/nested/ok-1_2",
        b"refs/heads/@",
        b"refs/heads/caf\xc3\xa9",
        b"refs/heads/a..b",
        b"refs/heads/a.lock",
        b"refs/heads/x/y.lock/z",
        b"refs/heads/.hidden",
        b"refs/heads/x/.y",
        b"refs/heads/a b",
        b"refs/heads/a~1",
        b"refs/heads/a^",
        b"refs/heads/a:b",
        b"refs/heads/a?",
        b"refs/heads/a*",
        b"refs/heads/a[",
        b"refs/heads/a\\b",
        b"refs/heads/a@{1}",
        b"refs/heads/",
        b"refs/heads//x",
        b"refs/heads/x.",
        b"refs/heads/\x7f",
        b"refs/heads/tab\there",
    ];
    for name in names {
        let utf8 = std::str::from_utf8(name)?;
        let valid = git_command(dir)
            .args(["check-ref-format", utf8])
            .output()?
            .status
            .success();
        let result = repo.update_ref(&RefUpdate {
            name: name.to_vec(),
            new: head,
            expected: RefExpectation::MustNotExist,
            log: None,
        });
        if valid {
            result?;
            assert_eq!(rev(dir, utf8)?, head.to_string(), "{utf8:?}");
        } else {
            assert!(
                matches!(result, Err(Error::InvalidRefName(ref bad)) if bad == name),
                "{utf8:?}: {result:?}"
            );
        }
    }
    // Only HEAD and refs/... are accepted, even when Git would allow more.
    for name in [&b"main"[..], b"FETCH_HEAD", b"ORIG_HEAD", b"head"] {
        assert!(matches!(
            repo.update_ref(&RefUpdate {
                name: name.to_vec(),
                new: head,
                expected: RefExpectation::Any,
                log: None,
            }),
            Err(Error::InvalidRefName(_))
        ));
    }
    // A symbolic HEAD is never overwritten with an object ID.
    assert!(matches!(
        repo.update_ref(&RefUpdate {
            name: b"HEAD".to_vec(),
            new: head,
            expected: RefExpectation::Any,
            log: None,
        }),
        Err(Error::UnsupportedCapability(_))
    ));
    assert_eq!(
        text(&git(dir, &["symbolic-ref", "HEAD"])?),
        "refs/heads/main"
    );
    fsck(dir)?;
    Ok(())
}

#[test]
fn in_progress_merge_and_unmerged_index_are_refused() -> Result {
    for format in FORMATS {
        let temp = tempfile::tempdir()?;
        let dir = temp.path();
        repo_with_commit(dir, format)?;
        git(dir, &["checkout", "-q", "-b", "other"])?;
        write_file(dir, "a", "other\n")?;
        git(dir, &["commit", "-q", "-am", "other"])?;
        git(dir, &["checkout", "-q", "main"])?;
        write_file(dir, "a", "main\n")?;
        git(dir, &["commit", "-q", "-am", "main"])?;
        let head = rev(dir, "HEAD")?;
        let merge = git_command(dir).args(["merge", "-q", "other"]).output()?;
        assert!(!merge.status.success());
        assert!(dir.join(".git/MERGE_HEAD").exists());
        let repo = Repository::discover(dir)?;
        assert!(matches!(
            repo.commit(&request(b"merge\n")),
            Err(Error::UnsupportedCapability(_))
        ));
        assert!(matches!(
            repo.write_tree_from_index(),
            Err(Error::UnmergedIndex)
        ));
        git(dir, &["merge", "--abort"])?;
        // Conflict stages without MERGE_HEAD: the index check itself refuses.
        let ours = rev(dir, "HEAD:a")?;
        git(dir, &["update-index", "--force-remove", "a"])?;
        git_stdin(
            dir,
            &["update-index", "--index-info"],
            format!("100644 {ours} 2\ta\n100644 {ours} 3\ta\n").as_bytes(),
        )?;
        assert!(matches!(
            repo.commit(&request(b"unmerged\n")),
            Err(Error::UnmergedIndex)
        ));
        assert_eq!(rev(dir, "HEAD")?, head);
        git(dir, &["reset", "-q", "--hard"])?;
        // A rebase in progress (state directory) is refused too.
        std::fs::create_dir(dir.join(".git/rebase-merge"))?;
        write_file(dir, "a", "rebasing\n")?;
        git(dir, &["add", "a"])?;
        assert!(matches!(
            repo.commit(&request(b"rebase\n")),
            Err(Error::UnsupportedCapability(_))
        ));
        std::fs::remove_dir(dir.join(".git/rebase-merge"))?;
        assert_eq!(rev(dir, "HEAD")?, head);
        fsck(dir)?;
    }
    Ok(())
}

#[test]
fn intent_to_add_gitlinks_and_bad_identities_are_refused_before_writing() -> Result {
    for format in FORMATS {
        let temp = tempfile::tempdir()?;
        let dir = temp.path();
        let head = repo_with_commit(dir, format)?;
        let repo = Repository::discover(dir)?;
        let objects = || -> Result<usize> {
            Ok(text(&git(dir, &["count-objects"])?)
                .split(' ')
                .next()
                .ok_or("count")?
                .parse()?)
        };

        write_file(dir, "new", "later\n")?;
        git(dir, &["add", "-N", "new"])?;
        let before = objects()?;
        assert!(matches!(
            repo.commit(&request(b"ita\n")),
            Err(Error::UnsupportedCapability(_))
        ));
        assert_eq!(objects()?, before);
        git(dir, &["rm", "-q", "--cached", "new"])?;

        let commit = rev(dir, "HEAD")?;
        git(
            dir,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{commit},sub"),
            ],
        )?;
        assert!(matches!(
            repo.write_tree_from_index(),
            Err(Error::UnsupportedCapability(_))
        ));
        git(dir, &["update-index", "--force-remove", "sub"])?;

        write_file(dir, "a", "changed\n")?;
        git(dir, &["add", "a"])?;
        let before = objects()?;
        for (name, email) in [
            (&b"A <x>"[..], &b"a@b"[..]),
            (b"A", b"a>b"),
            (b"A\nB", b"a@b"),
            (b"", b"a@b"),
        ] {
            let mut bad = request(b"identity\n");
            bad.author.name = name.to_vec();
            bad.author.email = email.to_vec();
            assert!(matches!(repo.commit(&bad), Err(Error::Other(_))));
        }
        assert!(matches!(
            repo.commit(&request(b"nul\0byte\n")),
            Err(Error::Other(_))
        ));
        assert_eq!(objects()?, before);
        assert_eq!(rev(dir, "HEAD")?, head);
        fsck(dir)?;
    }
    Ok(())
}

#[test]
fn reflogs_follow_core_log_all_ref_updates() -> Result {
    let temp = tempfile::tempdir()?;
    let dir = temp.path();
    let head = oid(&repo_with_commit(dir, "sha1")?)?;
    let repo = Repository::discover(dir)?;
    let log = || {
        Some(RefLog {
            committer: signature(),
            message: b"note".to_vec(),
        })
    };
    // Default (non-bare): tags are not logged, branches are.
    for name in ["refs/tags/t1", "refs/heads/b1"] {
        repo.update_ref(&RefUpdate {
            name: name.as_bytes().to_vec(),
            new: head,
            expected: RefExpectation::MustNotExist,
            log: log(),
        })?;
    }
    assert!(!dir.join(".git/logs/refs/tags/t1").exists());
    assert!(dir.join(".git/logs/refs/heads/b1").exists());
    // always: every ref; false: only refs whose log already exists.
    git(dir, &["config", "core.logAllRefUpdates", "always"])?;
    let repo = Repository::discover(dir)?;
    repo.update_ref(&RefUpdate {
        name: b"refs/tags/t2".to_vec(),
        new: head,
        expected: RefExpectation::MustNotExist,
        log: log(),
    })?;
    assert!(dir.join(".git/logs/refs/tags/t2").exists());
    // Updating to the current value writes nothing, as in C Git.
    repo.update_ref(&RefUpdate {
        name: b"refs/heads/b1".to_vec(),
        new: head,
        expected: RefExpectation::MustEqual(head),
        log: log(),
    })?;
    assert_eq!(reflog_subjects(dir, "refs/heads/b1")?.len(), 1);
    git(dir, &["config", "core.logAllRefUpdates", "false"])?;
    git(dir, &["commit", "-q", "--allow-empty", "-m", "next"])?;
    let next = oid(&rev(dir, "HEAD")?)?;
    let repo = Repository::discover(dir)?;
    for name in ["refs/heads/b2", "refs/heads/b1"] {
        repo.update_ref(&RefUpdate {
            name: name.as_bytes().to_vec(),
            new: next,
            expected: RefExpectation::Any,
            log: log(),
        })?;
    }
    assert!(!dir.join(".git/logs/refs/heads/b2").exists());
    assert_eq!(reflog_subjects(dir, "refs/heads/b1")?.len(), 2);
    fsck(dir)?;
    Ok(())
}

/// Reflog lines are appended before the rename, so a failed append leaves
/// the reference unchanged. If the branch log was appended but HEAD's failed,
/// the error says the update was partially logged.
#[cfg(unix)]
#[test]
fn reflog_failure_leaves_the_ref_unchanged() -> Result {
    use rgit_core::write::ReflogAppendError;
    use std::os::unix::fs::PermissionsExt;
    for (readonly, partial) in [("logs/refs/heads/main", false), ("logs/HEAD", true)] {
        let temp = tempfile::tempdir()?;
        let dir = temp.path();
        let first = oid(&repo_with_commit(dir, "sha1")?)?;
        git(dir, &["commit", "-q", "--allow-empty", "-m", "second"])?;
        let second = oid(&rev(dir, "HEAD")?)?;
        let git_dir = dir.join(".git");
        let branch_log = read(git_dir.join("logs/refs/heads/main"))?;
        let head_log = read(git_dir.join("logs/HEAD"))?;
        let locked = git_dir.join(readonly);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o444))?;
        let result = Repository::discover(dir)?.update_ref(&RefUpdate {
            name: b"refs/heads/main".to_vec(),
            new: first,
            expected: RefExpectation::MustEqual(second),
            log: Some(RefLog {
                committer: signature(),
                message: b"rewind".to_vec(),
            }),
        });
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644))?;
        match result {
            Err(Error::Other(error)) => {
                assert!(error.to_string().contains("was not updated"), "{error}");
                let typed = error
                    .downcast_ref::<ReflogAppendError>()
                    .ok_or("ReflogAppendError")?;
                assert_eq!(typed.reference(), b"refs/heads/main");
                assert_eq!(typed.partially_logged(), partial, "{readonly}");
            }
            other => panic!("expected a reflog error, got {other:?}"),
        }
        assert_eq!(
            rev(dir, "refs/heads/main")?,
            second.to_string(),
            "{readonly}"
        );
        assert_eq!(read(git_dir.join("logs/HEAD"))?, head_log);
        if partial {
            // The branch log gained the line before HEAD's append failed.
            let now = read(git_dir.join("logs/refs/heads/main"))?;
            assert!(now.starts_with(&branch_log) && now.len() > branch_log.len());
        } else {
            assert_eq!(read(git_dir.join("logs/refs/heads/main"))?, branch_log);
        }
        assert!(!git_dir.join("refs/heads/main.lock").exists());
        assert!(!git_dir.join("HEAD.lock").exists());
        fsck(dir)?;
    }
    Ok(())
}

/// HEAD's reflog follows the HEAD target observed under HEAD.lock: after
/// HEAD is retargeted away, an update of the old branch logs nothing to
/// HEAD; once it names the branch again, HEAD's log gets the line.
#[test]
fn head_reflog_follows_the_current_head_target() -> Result {
    for format in FORMATS {
        let temp = tempfile::tempdir()?;
        let dir = temp.path();
        let first = oid(&repo_with_commit(dir, format)?)?;
        git(dir, &["commit", "-q", "--allow-empty", "-m", "second"])?;
        let second = oid(&rev(dir, "HEAD")?)?;
        git(dir, &["branch", "other"])?;
        git(dir, &["symbolic-ref", "HEAD", "refs/heads/other"])?;
        let head_log = read(dir.join(".git/logs/HEAD"))?;
        let repo = Repository::discover(dir)?;
        let update = |new, old, message: &[u8]| RefUpdate {
            name: b"refs/heads/main".to_vec(),
            new,
            expected: RefExpectation::MustEqual(old),
            log: Some(RefLog {
                committer: signature(),
                message: message.to_vec(),
            }),
        };
        repo.update_ref(&update(first, second, b"while elsewhere"))?;
        assert_eq!(read(dir.join(".git/logs/HEAD"))?, head_log, "{format}");
        assert_eq!(
            reflog_subjects(dir, "refs/heads/main")?.first(),
            Some(&format!("{first} while elsewhere"))
        );
        git(dir, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
        repo.update_ref(&update(second, first, b"while current"))?;
        assert_eq!(
            reflog_subjects(dir, "HEAD")?.first(),
            Some(&format!("{second} while current"))
        );
        fsck(dir)?;
    }
    Ok(())
}

/// Each reflog line's old value equals the previous line's new value.
#[cfg_attr(not(all(feature = "failpoints", debug_assertions)), allow(dead_code))]
fn assert_reflog_chain(path: &Path) -> Result {
    let text = String::from_utf8(read(path)?)?;
    let mut previous: Option<String> = None;
    for line in text.lines() {
        let mut fields = line.split(' ');
        let old = fields.next().ok_or("old")?.to_owned();
        let new = fields.next().ok_or("new")?.to_owned();
        if let Some(previous) = &previous {
            assert_eq!(
                &old,
                previous,
                "{}: chain broken at {line:?}",
                path.display()
            );
        }
        previous = Some(new);
    }
    Ok(())
}

/// Loose object files in `.git/objects/xx/` (excluding info/ and pack/).
fn loose_objects(dir: &Path) -> Result<Vec<String>> {
    let mut found = Vec::new();
    for fanout in std::fs::read_dir(dir.join(".git/objects"))? {
        let fanout = fanout?;
        let name = fanout.file_name().to_string_lossy().into_owned();
        if name.len() != 2 {
            continue;
        }
        for object in std::fs::read_dir(fanout.path())? {
            found.push(format!("{name}{}", object?.file_name().to_string_lossy()));
        }
    }
    found.sort();
    Ok(found)
}

/// Every commit refusal is decided before the first object write: trees are
/// computed in memory and the target reference is preflighted.
#[cfg(unix)]
#[test]
fn commit_refusals_write_no_objects() -> Result {
    for format in FORMATS {
        // Unborn branch, empty index: the empty tree is not even stored.
        let temp = tempfile::tempdir()?;
        let dir = temp.path();
        init(dir, format)?;
        assert!(matches!(
            Repository::discover(dir)?.commit(&request(b"empty\n")),
            Err(Error::NothingToCommit)
        ));
        assert_eq!(loose_objects(dir)?, Vec::<String>::new());

        type Setup = fn(&Path) -> Result;
        let cases: [(&str, Setup); 4] = [
            ("D/F conflict on the unborn target branch", |dir| {
                git(dir, &["symbolic-ref", "HEAD", "refs/heads/main/topic"])?;
                Ok(())
            }),
            ("symlinked branch ref", |dir| {
                let refs = dir.join(".git/refs/heads");
                std::fs::rename(refs.join("main"), dir.join(".git/main-moved"))?;
                std::os::unix::fs::symlink("../../main-moved", refs.join("main"))?;
                Ok(())
            }),
            ("symbolic branch ref", |dir| {
                git(dir, &["branch", "other"])?;
                std::fs::write(dir.join(".git/refs/heads/main"), b"ref: refs/heads/other\n")?;
                Ok(())
            }),
            ("reflog path is a directory", |dir| {
                let log = dir.join(".git/logs/refs/heads/main");
                std::fs::remove_file(&log)?;
                std::fs::create_dir_all(log.join("x"))?;
                Ok(())
            }),
        ];
        for (what, setup) in cases {
            let temp = tempfile::tempdir()?;
            let dir = temp.path();
            repo_with_commit(dir, format)?;
            write_file(dir, "nested/new", "never stored\n")?;
            git(dir, &["add", "nested/new"])?;
            setup(dir)?;
            let before = loose_objects(dir)?;
            let result = Repository::discover(dir)?.commit(&request(b"refused\n"));
            assert!(
                matches!(
                    result,
                    Err(Error::AlreadyExists(_) | Error::UnsupportedCapability(_))
                ),
                "{format} {what}: {result:?}"
            );
            assert_eq!(loose_objects(dir)?, before, "{format} {what}");
        }
    }
    Ok(())
}

/// A symlink at a loose object path is never trusted as the stored object,
/// whether rgit would write it (a tree) or only name it (update_ref).
#[cfg(unix)]
#[test]
fn symlinked_loose_objects_are_refused() -> Result {
    for format in FORMATS {
        let temp = tempfile::tempdir()?;
        let dir = temp.path();
        let head = repo_with_commit(dir, format)?;
        write_file(dir, "a", "two\n")?;
        git(dir, &["add", "a"])?;
        // C Git stores the tree rgit will compute; replace it with a symlink
        // to a valid copy, which the object store would happily follow.
        let tree = text(&git(dir, &["write-tree"])?);
        let path = dir.join(".git/objects").join(&tree[..2]).join(&tree[2..]);
        let copy = dir.join(".git/tree-copy");
        std::fs::rename(&path, &copy)?;
        std::os::unix::fs::symlink(&copy, &path)?;
        let repo = Repository::discover(dir)?;
        assert!(matches!(
            repo.commit(&request(b"through symlink\n")),
            Err(Error::UnsupportedCapability(_))
        ));
        assert_eq!(rev(dir, "HEAD")?, head);
        std::fs::remove_file(&path)?;
        std::fs::rename(&copy, &path)?;

        let commit = text(&git(dir, &["commit-tree", &tree, "-p", &head, "-m", "x"])?);
        let path = dir
            .join(".git/objects")
            .join(&commit[..2])
            .join(&commit[2..]);
        std::fs::rename(&path, &copy)?;
        std::os::unix::fs::symlink(&copy, &path)?;
        assert!(matches!(
            repo.update_ref(&RefUpdate {
                name: b"refs/heads/linked".to_vec(),
                new: oid(&commit)?,
                expected: RefExpectation::MustNotExist,
                log: None,
            }),
            Err(Error::UnsupportedCapability(_))
        ));
        assert!(!dir.join(".git/refs/heads/linked").exists());
        std::fs::remove_file(&path)?;
        std::fs::rename(&copy, &path)?;
        fsck(dir)?;
    }
    Ok(())
}

/// Crash and race boundaries. The test binary re-executes itself as a child
/// running `failpoint_child`, with `RGIT_FAILPOINT` set only for the child.
#[cfg(all(feature = "failpoints", debug_assertions))]
mod failpoints {
    use super::*;
    use std::process::Command;

    const CHILD_REPO: &str = "RGIT_TEST_CHILD_REPO";
    const CHILD_RESULT: &str = "RGIT_TEST_CHILD_RESULT";

    /// Runs only as a child; a no-op in the normal test run.
    #[test]
    fn failpoint_child() -> Result {
        let (Some(repo), Some(result)) =
            (std::env::var_os(CHILD_REPO), std::env::var_os(CHILD_RESULT))
        else {
            return Ok(());
        };
        let repo = Repository::discover(&repo)?;
        let outcome = match std::env::var("RGIT_TEST_CHILD_UPDATE") {
            // "<old> <new>": update_ref of main with a reflog message.
            Ok(spec) => {
                let (old, new) = spec.split_once(' ').ok_or("update spec")?;
                repo.update_ref(&RefUpdate {
                    name: b"refs/heads/main".to_vec(),
                    new: oid(new)?,
                    expected: RefExpectation::MustEqual(oid(old)?),
                    log: Some(RefLog {
                        committer: signature(),
                        message: b"child update".to_vec(),
                    }),
                })
                .map(|()| new.to_owned())
            }
            Err(_) => repo
                .commit(&request(b"child commit\n"))
                .map(|outcome| outcome.id.to_string()),
        };
        let line = match outcome {
            Ok(id) => format!("ok {id}"),
            Err(error) => format!("err {error:?} | {error}"),
        };
        std::fs::write(result, line)?;
        Ok(())
    }

    fn child(dir: &Path, failpoint: &str, sync: Option<&Path>) -> Result<Command> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(["--exact", "failpoints::failpoint_child", "--test-threads=1"])
            .env(CHILD_REPO, dir)
            .env(CHILD_RESULT, dir.join("../child-result"))
            .env("RGIT_FAILPOINT", failpoint)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(sync) = sync {
            command
                .env("RGIT_FAILPOINT_READY", sync.join("ready"))
                .env("RGIT_FAILPOINT_RELEASE", sync.join("release"));
        }
        Ok(command)
    }

    fn child_result(dir: &Path) -> Result<String> {
        Ok(std::fs::read_to_string(dir.join("../child-result")).unwrap_or_default())
    }

    fn staged_repo(root: &Path, format: &str) -> Result<(std::path::PathBuf, String)> {
        let dir = root.join("repo");
        std::fs::create_dir(&dir)?;
        let head = repo_with_commit(&dir, format)?;
        write_file(&dir, "a", "crash\n")?;
        git(&dir, &["add", "a"])?;
        Ok((dir, head))
    }

    /// Simulated crash (abort) at each boundary: a fresh process sees either
    /// the old or the new branch value, never a missing object.
    #[test]
    fn aborts_leave_old_or_new_state() -> Result {
        for format in FORMATS {
            // (point, ref published, reflog line appended, stale ref lock)
            for (point, published, logged, stale_lock) in [
                ("commit.objects-written", false, false, None),
                // The first lock a branch commit takes is HEAD.lock.
                ("lock.acquired", false, false, None),
                (
                    "commit.ref-locked",
                    false,
                    false,
                    Some("refs/heads/main.lock"),
                ),
                ("ref.logged", false, true, Some("refs/heads/main.lock")),
                (
                    "lock.before-rename",
                    false,
                    true,
                    Some("refs/heads/main.lock"),
                ),
                ("lock.after-rename", true, true, None),
            ] {
                let temp = tempfile::tempdir()?;
                let (dir, head) = staged_repo(temp.path(), format)?;
                let status = child(&dir, point, None)?.status()?;
                // Abort, not an ordinary failure that would pass vacuously.
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    assert_eq!(status.signal(), Some(6), "{point}: {status:?}");
                }
                assert!(!status.success(), "{point}: child should abort");
                assert_eq!(child_result(&dir)?, "", "{point}");
                let now = rev(&dir, "refs/heads/main")?;
                if published {
                    assert_ne!(now, head, "{point}");
                    assert_eq!(rev(&dir, &format!("{now}^"))?, head);
                } else {
                    assert_eq!(now, head, "{point}");
                }
                // Reflogs are appended under the locks, before the rename: a
                // crash after the append leaves a line even if the ref kept
                // its old value (as in C Git); its object exists.
                let expected_lines = if logged { 2 } else { 1 };
                for log in ["refs/heads/main", "HEAD"] {
                    let lines = reflog_subjects(&dir, log)?;
                    assert_eq!(lines.len(), expected_lines, "{point} {log}");
                    if logged {
                        let id = lines[0].split(' ').next().ok_or("id")?;
                        git(&dir, &["cat-file", "-e", id])?;
                        assert!(lines[0].ends_with("commit: child commit"), "{point}");
                    }
                }
                let git_dir = dir.join(".git");
                // A crash while holding HEAD.lock leaves it, as in C Git.
                let head_lock = git_dir.join("HEAD.lock");
                assert_eq!(head_lock.exists(), point != "commit.objects-written");
                std::fs::remove_file(&head_lock).ok();
                if let Some(lock) = stale_lock {
                    let lock = git_dir.join(lock);
                    assert!(lock.exists(), "{point}: stale lock kept");
                    let repo = Repository::discover(&dir)?;
                    assert!(matches!(
                        repo.commit(&request(b"retry\n")),
                        Err(Error::Locked(_))
                    ));
                    std::fs::remove_file(lock)?;
                }
                fsck(&dir)?;
                if !published {
                    Repository::discover(&dir)?.commit(&request(b"retry\n"))?;
                    fsck(&dir)?;
                }
            }
        }
        Ok(())
    }

    /// A failure after the rename (directory sync) reports that the branch
    /// WAS updated, still appends both reflogs, and leaves no lock behind.
    #[test]
    fn failure_after_rename_reports_the_update() -> Result {
        for format in FORMATS {
            let temp = tempfile::tempdir()?;
            let (dir, head) = staged_repo(temp.path(), format)?;
            let status = child(&dir, "lock.after-rename:error", None)?.status()?;
            assert!(status.success(), "{status:?}");
            let result = child_result(&dir)?;
            assert!(
                result.starts_with("err Other(RefPublishedUnsyncedError"),
                "{result}"
            );
            assert!(
                result.contains("Injected(\"lock.after-rename\")"),
                "{result}"
            );
            assert!(result.contains("was updated"), "{result}");
            let now = rev(&dir, "refs/heads/main")?;
            assert_ne!(now, head);
            assert_eq!(rev(&dir, &format!("{now}^"))?, head);
            let entry = format!("{now} commit: child commit");
            assert_eq!(
                reflog_subjects(&dir, "refs/heads/main")?.first(),
                Some(&entry)
            );
            assert_eq!(reflog_subjects(&dir, "HEAD")?.first(), Some(&entry));
            assert!(!dir.join(".git/refs/heads/main.lock").exists());
            assert!(!dir.join(".git/HEAD.lock").exists());
            fsck(&dir)?;
        }
        Ok(())
    }

    /// A commit object that is not on any branch, for update_ref tests.
    fn detached_commit(dir: &Path, parent: &str, message: &str) -> Result<String> {
        Ok(text(&git(
            dir,
            &[
                "commit-tree",
                &format!("{parent}^{{tree}}"),
                "-p",
                parent,
                "-m",
                message,
            ],
        )?))
    }

    fn update_child(
        dir: &Path,
        failpoint: &str,
        sync: Option<&Path>,
        old: &str,
        new: &str,
    ) -> Result<Command> {
        let mut command = child(dir, failpoint, sync)?;
        command.env("RGIT_TEST_CHILD_UPDATE", format!("{old} {new}"));
        Ok(command)
    }

    /// (A) While update_ref of the branch HEAD names holds its locks, HEAD
    /// cannot be retargeted (HEAD.lock is held), so the HEAD log line it
    /// appends matches HEAD's target at publication.
    #[test]
    fn head_cannot_be_retargeted_during_update_ref() -> Result {
        for format in FORMATS {
            let temp = tempfile::tempdir()?;
            let dir = temp.path().join("repo");
            std::fs::create_dir(&dir)?;
            let head = repo_with_commit(&dir, format)?;
            git(&dir, &["branch", "other"])?;
            let next = detached_commit(&dir, &head, "next")?;
            let sync = temp.path().join("sync");
            std::fs::create_dir(&sync)?;
            let mut running =
                update_child(&dir, "ref.locked:pause", Some(&sync), &head, &next)?.spawn()?;
            wait_for(&sync.join("ready"))?;
            let retarget = git_command(&dir)
                .args(["symbolic-ref", "HEAD", "refs/heads/other"])
                .output()?;
            assert!(!retarget.status.success(), "HEAD retargeted mid-update");
            std::fs::write(sync.join("release"), b"")?;
            assert!(running.wait()?.success());
            assert_eq!(child_result(&dir)?, format!("ok {next}"));
            assert_eq!(
                text(&git(&dir, &["symbolic-ref", "HEAD"])?),
                "refs/heads/main"
            );
            let entry = format!("{next} child update");
            assert_eq!(reflog_subjects(&dir, "HEAD")?.first(), Some(&entry));
            assert_eq!(
                reflog_subjects(&dir, "refs/heads/main")?.first(),
                Some(&entry)
            );
            assert_reflog_chain(&dir.join(".git/logs/HEAD"))?;
            fsck(&dir)?;
        }
        Ok(())
    }

    /// (B) A second writer (rgit or C Git) cannot update the ref while the
    /// first holds its locks after appending its reflog line, so the final
    /// reflog chains stay strictly ordered.
    #[test]
    fn concurrent_updates_keep_the_reflog_chain_ordered() -> Result {
        for format in FORMATS {
            let temp = tempfile::tempdir()?;
            let dir = temp.path().join("repo");
            std::fs::create_dir(&dir)?;
            let head = repo_with_commit(&dir, format)?;
            let x = detached_commit(&dir, &head, "x")?;
            let y = detached_commit(&dir, &x, "y")?;
            let z = detached_commit(&dir, &y, "z")?;
            let sync = temp.path().join("sync");
            std::fs::create_dir(&sync)?;
            let mut running =
                update_child(&dir, "ref.logged:pause", Some(&sync), &head, &x)?.spawn()?;
            wait_for(&sync.join("ready"))?;
            let repo = Repository::discover(&dir)?;
            let rival = repo.update_ref(&RefUpdate {
                name: b"refs/heads/main".to_vec(),
                new: oid(&y)?,
                expected: RefExpectation::Any,
                log: Some(RefLog {
                    committer: signature(),
                    message: b"rival".to_vec(),
                }),
            });
            assert!(matches!(rival, Err(Error::Locked(_))), "{rival:?}");
            let c_git = git_command(&dir)
                .args(["update-ref", "-m", "c git rival", "refs/heads/main", &y])
                .output()?;
            assert!(!c_git.status.success(), "C Git updated a locked ref");
            std::fs::write(sync.join("release"), b"")?;
            assert!(running.wait()?.success());
            assert_eq!(child_result(&dir)?, format!("ok {x}"));
            // Sequential follow-ups from both implementations.
            repo.update_ref(&RefUpdate {
                name: b"refs/heads/main".to_vec(),
                new: oid(&y)?,
                expected: RefExpectation::MustEqual(oid(&x)?),
                log: Some(RefLog {
                    committer: signature(),
                    message: b"after".to_vec(),
                }),
            })?;
            git(
                &dir,
                &["update-ref", "-m", "c git after", "refs/heads/main", &z, &y],
            )?;
            assert_eq!(rev(&dir, "refs/heads/main")?, z);
            for log in ["logs/refs/heads/main", "logs/HEAD"] {
                let path = dir.join(".git").join(log);
                assert_reflog_chain(&path)?;
                assert_eq!(String::from_utf8(read(&path)?)?.lines().count(), 4, "{log}");
            }
            fsck(&dir)?;
        }
        Ok(())
    }

    /// If the held ref lock is removed and replaced by another writer while
    /// paused, the update is refused before any reflog line is appended and
    /// the foreign lock is neither published nor unlinked.
    #[test]
    fn replaced_lock_refuses_before_logging() -> Result {
        for format in FORMATS {
            let temp = tempfile::tempdir()?;
            let (dir, head) = staged_repo(temp.path(), format)?;
            let git_dir = dir.join(".git");
            let logs = ["logs/HEAD", "logs/refs/heads/main"]
                .map(|log| read(git_dir.join(log)))
                .into_iter()
                .collect::<Result<Vec<_>>>()?;
            let sync = temp.path().join("sync");
            std::fs::create_dir(&sync)?;
            let mut running = child(&dir, "commit.ref-locked:pause", Some(&sync))?.spawn()?;
            wait_for(&sync.join("ready"))?;
            let lock = git_dir.join("refs/heads/main.lock");
            if cfg!(windows) {
                // A held lock cannot be deleted on Windows (no FILE_SHARE_DELETE),
                // so the replacement cannot happen; the paused commit publishes.
                match std::fs::remove_file(&lock) {
                    Err(error) if error.raw_os_error() == Some(32) => {}
                    other => return Err(format!("held lock removable: {other:?}").into()),
                }
                std::fs::write(sync.join("release"), b"")?;
                assert!(running.wait()?.success());
                let result = child_result(&dir)?;
                assert!(result.starts_with("ok "), "{result}");
                assert!(!lock.exists() && !git_dir.join("HEAD.lock").exists());
                fsck(&dir)?;
                continue;
            }
            std::fs::remove_file(&lock)?;
            std::fs::write(&lock, b"foreign\n")?;
            std::fs::write(sync.join("release"), b"")?;
            assert!(running.wait()?.success());
            let result = child_result(&dir)?;
            assert!(result.starts_with("err Locked"), "{result}");
            assert_eq!(read(&lock)?, b"foreign\n");
            assert_eq!(rev(&dir, "refs/heads/main")?, head);
            for (log, before) in ["logs/HEAD", "logs/refs/heads/main"].iter().zip(&logs) {
                assert_eq!(&read(git_dir.join(log))?, before, "{log}");
            }
            assert!(!git_dir.join("HEAD.lock").exists());
            std::fs::remove_file(&lock)?;
            fsck(&dir)?;
        }
        Ok(())
    }

    /// An error after the reflog append but before the rename is reported as
    /// "not updated, but logged"; no locks are left.
    #[test]
    fn error_after_reflog_append_reports_logged_not_published() -> Result {
        for format in FORMATS {
            let temp = tempfile::tempdir()?;
            let dir = temp.path().join("repo");
            std::fs::create_dir(&dir)?;
            let head = repo_with_commit(&dir, format)?;
            let next = detached_commit(&dir, &head, "next")?;
            for point in ["ref.logged:error", "lock.before-rename:error"] {
                let status = update_child(&dir, point, None, &head, &next)?.status()?;
                assert!(status.success(), "{point}");
                let result = child_result(&dir)?;
                assert!(
                    result.starts_with("err Other(RefLoggedNotPublishedError"),
                    "{point}: {result}"
                );
                assert!(result.contains("was not updated"), "{result}");
                assert_eq!(rev(&dir, "refs/heads/main")?, head);
                assert!(!dir.join(".git/refs/heads/main.lock").exists());
                assert!(!dir.join(".git/HEAD.lock").exists());
            }
            // Two logged attempts, neither published.
            assert_eq!(reflog_subjects(&dir, "refs/heads/main")?.len(), 3);
            fsck(&dir)?;
        }
        Ok(())
    }

    /// A crash after the reflog append but before the rename: the ref keeps
    /// its old value, both logs have the extra line, fsck accepts it, and a
    /// retry succeeds once the stale locks are removed.
    #[test]
    fn crash_after_reflog_append_keeps_the_old_ref() -> Result {
        for format in FORMATS {
            let temp = tempfile::tempdir()?;
            let dir = temp.path().join("repo");
            std::fs::create_dir(&dir)?;
            let head = repo_with_commit(&dir, format)?;
            let next = detached_commit(&dir, &head, "next")?;
            let status = update_child(&dir, "ref.logged", None, &head, &next)?.status()?;
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(status.signal(), Some(6), "{status:?}");
            }
            assert_eq!(rev(&dir, "refs/heads/main")?, head);
            let entry = format!("{next} child update");
            for log in ["refs/heads/main", "HEAD"] {
                assert_eq!(reflog_subjects(&dir, log)?.first(), Some(&entry), "{log}");
            }
            fsck(&dir)?;
            let git_dir = dir.join(".git");
            for lock in ["refs/heads/main.lock", "HEAD.lock"] {
                assert!(git_dir.join(lock).exists(), "{lock} left by the crash");
            }
            assert!(matches!(
                Repository::discover(&dir)?.update_ref(&RefUpdate {
                    name: b"refs/heads/main".to_vec(),
                    new: oid(&next)?,
                    expected: RefExpectation::MustEqual(oid(&head)?),
                    log: None,
                }),
                Err(Error::Locked(_))
            ));
            for lock in ["refs/heads/main.lock", "HEAD.lock"] {
                std::fs::remove_file(git_dir.join(lock))?;
            }
            let status = update_child(&dir, "none", None, &head, &next)?.status()?;
            assert!(status.success());
            assert_eq!(child_result(&dir)?, format!("ok {next}"));
            assert_eq!(rev(&dir, "refs/heads/main")?, next);
            assert_eq!(reflog_subjects(&dir, "refs/heads/main")?.len(), 3);
            fsck(&dir)?;
        }
        Ok(())
    }

    fn wait_for(path: &Path) -> Result {
        for _ in 0..3000 {
            if path.exists() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Err("failpoint never reached".into())
    }

    /// Another writer moves the branch after the child read HEAD and wrote
    /// its objects: publication must fail with a conflict and keep the
    /// other writer's value.
    #[test]
    fn concurrent_move_before_publication_is_a_conflict() -> Result {
        for format in FORMATS {
            let temp = tempfile::tempdir()?;
            let (dir, head) = staged_repo(temp.path(), format)?;
            let sync = temp.path().join("sync");
            std::fs::create_dir(&sync)?;
            let mut running = child(&dir, "commit.objects-written:pause", Some(&sync))?.spawn()?;
            wait_for(&sync.join("ready"))?;
            git(&dir, &["commit", "-q", "--allow-empty", "-m", "racer"])?;
            let racer = rev(&dir, "HEAD")?;
            std::fs::write(sync.join("release"), b"")?;
            assert!(running.wait()?.success());
            let result = child_result(&dir)?;
            assert!(result.starts_with("err RefConflict"), "{result}");
            assert!(result.contains(&head), "{result}");
            assert_eq!(rev(&dir, "refs/heads/main")?, racer);
            assert!(!dir.join(".git/refs/heads/main.lock").exists());
            assert!(!dir.join(".git/HEAD.lock").exists());
            fsck(&dir)?;
        }
        Ok(())
    }

    /// While the child holds the ref lock (expected value verified), C Git
    /// cannot move the branch; the child then publishes successfully.
    #[test]
    fn held_ref_lock_excludes_c_git_writers() -> Result {
        for format in FORMATS {
            let temp = tempfile::tempdir()?;
            let (dir, head) = staged_repo(temp.path(), format)?;
            let sync = temp.path().join("sync");
            std::fs::create_dir(&sync)?;
            let mut running = child(&dir, "commit.ref-locked:pause", Some(&sync))?.spawn()?;
            wait_for(&sync.join("ready"))?;
            let blocked = git_command(&dir)
                .args(["update-ref", "refs/heads/main", &head])
                .output()?;
            assert!(!blocked.status.success());
            let switch = git_command(&dir)
                .args(["symbolic-ref", "HEAD", "refs/heads/other"])
                .output()?;
            assert!(!switch.status.success(), "HEAD.lock must be held");
            std::fs::write(sync.join("release"), b"")?;
            assert!(running.wait()?.success());
            let result = child_result(&dir)?;
            assert!(result.starts_with("ok "), "{result}");
            assert_eq!(rev(&dir, "refs/heads/main")?, result[3..]);
            assert_eq!(rev(&dir, "HEAD~1")?, head);
            fsck(&dir)?;
        }
        Ok(())
    }
}
