//! Local lifecycle scenarios (ADR 0003) checked against an independent model.
//! Every scenario runs with the C Git driver (proving the oracle is not
//! tailored to rgit) and with the rgit driver.

#[allow(dead_code)]
mod lifecycle_support;

use std::fs;

use lifecycle_support::Scenario;
use lifecycle_support::driver::{self, Driver, ok, refused};
use lifecycle_support::model::{Hash, Model, tree_id};
use lifecycle_support::observe::{self, assert_state, c_git_hash_object};
use lifecycle_support::proc::{Preserve, Result, assert_preserved, git, git_text, snapshot};
use lifecycle_support::sha::{hex, sha1, sha256};

// ---------------------------------------------------------------------------
// Oracle self-checks.

#[test]
fn sha_known_answers() {
    let million = vec![b'a'; 1_000_000];
    let two_block = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
    assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    assert_eq!(
        hex(&sha1(b"abc")),
        "a9993e364706816aba3e25717850c26c9cd0d89d"
    );
    assert_eq!(
        hex(&sha1(two_block)),
        "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
    );
    assert_eq!(
        hex(&sha1(&million)),
        "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
    );
    assert_eq!(
        hex(&sha256(b"")),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        hex(&sha256(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        hex(&sha256(two_block)),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    assert_eq!(
        hex(&sha256(&million)),
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
    );
}

/// Padding boundaries and object headers agree with C Git's hash-object.
#[test]
fn object_ids_match_c_git_hash_object() -> Result {
    for hash in Hash::ALL {
        let scenario = Scenario::init(Driver::CGit, hash, "main")?;
        let root = scenario.root();
        for len in [0usize, 1, 46, 47, 55, 56, 63, 64, 65, 119, 120, 128, 1000] {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            for kind in ["blob", "commit", "tag"] {
                let expected = c_git_hash_object(&root, kind, &bytes)?;
                assert_eq!(
                    hash.object_hex(kind, &bytes),
                    expected,
                    "{hash:?} {kind} len {len}"
                );
            }
        }
    }
    Ok(())
}

/// The oracle must reject corrupted or divergent states, not just accept.
#[test]
fn oracle_detects_divergence_and_corruption() -> Result {
    let mut s = Scenario::init(Driver::CGit, Hash::Sha1, "main")?;
    s.model.write(0, b"f", b"one\n")?;
    s.add(0, &[b"f"])?;
    s.commit(0, &["one"])?;
    let root = s.root();
    let git_dir = root.join(".git");

    // Model divergence: staged content the model does not know about.
    fs::write(root.join("f"), b"two\n")?;
    ok(&Driver::CGit.add(&root, &["f"])?, "add")?;
    assert!(assert_state(&s.model).is_err(), "missed index divergence");
    s.model.write(0, b"f", b"two\n")?;
    s.model.stage(0, &[b"f"])?;
    assert_state(&s.model)?;

    // A leftover lock after a successful operation.
    fs::write(git_dir.join("index.lock"), b"")?;
    assert!(assert_state(&s.model).is_err(), "missed leftover lock");
    fs::remove_file(git_dir.join("index.lock"))?;

    // A branch pointing at a missing object.
    let saved = fs::read(git_dir.join("refs/heads/main"))?;
    fs::write(
        git_dir.join("refs/heads/main"),
        b"1111111111111111111111111111111111111111\n",
    )?;
    assert!(assert_state(&s.model).is_err(), "missed dangling ref");
    assert!(observe::fsck(&root).is_err(), "fsck missed dangling ref");
    fs::write(git_dir.join("refs/heads/main"), &saved)?;

    // A torn (truncated) index.
    let index = fs::read(git_dir.join("index"))?;
    fs::write(git_dir.join("index"), &index[..index.len() / 2])?;
    assert!(observe::index(&root).is_err(), "missed torn index");
    fs::write(git_dir.join("index"), &index)?;

    // A "new loose object" that is not a valid object named by its hash.
    let before = snapshot(s.dir())?;
    let fake_dir = git_dir.join("objects/ab");
    fs::create_dir_all(&fake_dir)?;
    let fake = fake_dir.join("cdef0123456789abcdef0123456789abcdef01");
    fs::write(&fake, b"not a zlib stream")?;
    assert!(
        assert_preserved(
            s.dir(),
            &before,
            &snapshot(s.dir())?,
            Preserve::AllowNewObjects,
            "fake"
        )
        .is_err(),
        "missed an invalid new object"
    );
    fs::remove_dir_all(&fake_dir)?;
    // A valid object that is writable (not immutable) is rejected too.
    let before = snapshot(s.dir())?;
    fs::write(root.join("valid-new"), b"valid new object\n")?;
    let oid = git_text(&root, &["hash-object", "-w", "valid-new"])?;
    fs::remove_file(root.join("valid-new"))?;
    let object = git_dir.join("objects").join(&oid[..2]).join(&oid[2..]);
    assert_preserved(
        s.dir(),
        &before,
        &snapshot(s.dir())?,
        Preserve::AllowNewObjects,
        "valid",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&object, fs::Permissions::from_mode(0o644))?;
        assert!(
            assert_preserved(
                s.dir(),
                &before,
                &snapshot(s.dir())?,
                Preserve::AllowNewObjects,
                "writable"
            )
            .is_err(),
            "missed a writable new object"
        );
        fs::set_permissions(&object, fs::Permissions::from_mode(0o444))?;
    }

    // A commit with a different date than the model predicted.
    let mut wrong = s.model.clone();
    let when = wrong.tick();
    wrong.commit(0, &["two"], when);
    ok(&Driver::CGit.commit(&root, &["two"], when + 1)?, "commit")?;
    assert!(
        assert_state(&wrong).is_err(),
        "missed commit identity drift"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenarios: fn(Driver) -> Result, generated for both drivers.

fn init_stage_commit(driver: Driver) -> Result {
    for hash in Hash::ALL {
        let mut s = Scenario::init(driver, hash, "main")?;
        s.check()?;
        assert_eq!(
            observe::symbolic_head(&s.root())?.as_deref(),
            Some("refs/heads/main")
        );
        assert_eq!(observe::resolve(&s.root(), "HEAD")?, None, "unborn HEAD");
        s.model.write(0, b"README", b"hello\n")?;
        s.model.write(0, b"bin.dat", b"\0\x01\xff\xfe binary\r\n")?;
        s.model.write(0, b"empty", b"")?;
        s.check()?; // untracked only
        s.add(0, &[b"README", b"bin.dat", b"empty"])?;
        s.check()?;
        let root_commit = s.commit(0, &["initial", "second paragraph"])?;
        assert_eq!(observe::resolve(&s.root(), "HEAD")?, Some(root_commit));
    }
    Ok(())
}

fn init_reports_and_refuses_existing(driver: Driver) -> Result {
    if driver::lacks(driver, &["init"], "init_reports_and_refuses_existing") {
        return Ok(());
    }
    let s = Scenario::init(driver, Hash::Sha1, "main")?;
    // Non-quiet init prints the absolute Git directory (rgit CLI contract).
    let output = driver.init(s.dir(), &["second"])?;
    ok(&output, "init second")?;
    if driver == Driver::Rgit {
        let git_dir = s.dir().join("second/.git");
        let canonical = git_dir.canonicalize()?;
        let text = String::from_utf8(output.stdout.clone())?;
        let accepted = [git_dir, canonical]
            .map(|p| format!("Initialized empty Git repository in {}/\n", p.display()));
        assert!(accepted.contains(&text), "init output {text:?}");
    }
    // Initializing an existing repository.
    let before = snapshot(s.dir())?;
    let output = driver.init(s.dir(), &["-q", "repo"])?;
    if driver.refuses_reinit() {
        refused(&output, 128, "re-init")?;
        assert_preserved(
            s.dir(),
            &before,
            &snapshot(s.dir())?,
            Preserve::Exact,
            "re-init",
        )?;
    } else {
        ok(&output, "C Git re-init")?;
    }
    s.check()?;
    Ok(())
}

fn bare_init_and_worktree_refusals(driver: Driver) -> Result {
    for hash in Hash::ALL {
        let sandbox = lifecycle_support::proc::sandbox()?;
        let output = driver.init(
            sandbox.path(),
            &[
                "-q",
                "--bare",
                "-b",
                "trunk",
                "--object-format",
                hash.name(),
                "bare.git",
            ],
        )?;
        ok(&output, "init --bare")?;
        let bare = sandbox.path().join("bare.git");
        assert_eq!(
            git_text(&bare, &["rev-parse", "--is-bare-repository"])?,
            "true"
        );
        assert_eq!(
            git_text(&bare, &["rev-parse", "--show-object-format"])?,
            hash.name()
        );
        assert_eq!(
            observe::symbolic_head(&bare)?.as_deref(),
            Some("refs/heads/trunk")
        );
        assert!(observe::branches(&bare)?.is_empty());
        observe::fsck(&bare)?;
        let before = snapshot(sandbox.path())?;
        fs::write(sandbox.path().join("outside"), b"x\n")?;
        let before_with_file = snapshot(sandbox.path())?;
        drop(before);
        refused(&driver.add(&bare, &["HEAD"])?, 128, "add in bare")?;
        refused(
            &driver.commit(&bare, &["bare"], 1_767_225_660)?,
            128,
            "commit in bare",
        )?;
        assert_preserved(
            sandbox.path(),
            &before_with_file,
            &snapshot(sandbox.path())?,
            Preserve::Exact,
            "bare refusals",
        )?;
    }
    Ok(())
}

/// Stage v1, edit to v2, commit: the commit has v1; v2 stays unstaged.
fn partial_staging_distinct_from_later_edits(driver: Driver) -> Result {
    for hash in Hash::ALL {
        let mut s = Scenario::init(driver, hash, "main")?;
        s.model.write(0, b"file", b"v1\n")?;
        s.model.write(0, b"other", b"unstaged\n")?;
        s.add(0, &[b"file"])?;
        s.model.write(0, b"file", b"v2 longer\n")?;
        let commit = s.commit(0, &["v1"])?;
        let tree = observe::tree(&s.root(), &commit)?;
        assert_eq!(tree[&b"file"[..]].oid, hash.object_hex("blob", b"v1\n"));
        assert!(!tree.contains_key(&b"other"[..]));
        assert_eq!(observe::status(&s.root())?, b" M file\0?? other\0");
        // Next add/commit picks up v2.
        s.add(0, &[b"file"])?;
        s.commit(0, &["v2"])?;
        assert_eq!(observe::status(&s.root())?, b"?? other\0");
    }
    Ok(())
}

fn multi_commit_and_deletion_staging(driver: Driver) -> Result {
    for hash in Hash::ALL {
        let mut s = Scenario::init(driver, hash, "main")?;
        for name in [&b"a"[..], b"b", b"c", b"dir/d", b"dir/e"] {
            s.model.write(0, name, name)?;
        }
        s.add(0, &[b"."])?;
        let first = s.commit(0, &["first"])?;
        // Literal path deletion.
        s.model.remove(0, b"a")?;
        s.add(0, &[b"a"])?;
        let second = s.commit(0, &["delete a"])?;
        // Directory pathspec stages a deletion and a modification beneath it.
        s.model.remove(0, b"dir/d")?;
        s.model.write(0, b"dir/e", b"changed\n")?;
        s.add(0, &[b"dir"])?;
        // "." stages a deletion elsewhere plus a new file.
        s.model.remove(0, b"b")?;
        s.model.write(0, b"new", b"new\n")?;
        s.add(0, &[b"."])?;
        let third = s.commit(0, &["third"])?;
        let log = git_text(&s.root(), &["rev-list", "HEAD"])?;
        assert_eq!(log, format!("{third}\n{second}\n{first}"));
        // Deleting everything yields the empty tree.
        for name in [&b"c"[..], b"dir/e", b"new"] {
            s.model.remove(0, name)?;
        }
        s.add(0, &[b"."])?;
        let empty = s.commit(0, &["empty"])?;
        assert_eq!(
            git_text(&s.root(), &["rev-parse", &format!("{empty}^{{tree}}")])?,
            hash.object_hex("tree", b"")
        );
    }
    Ok(())
}

fn nested_dirs_and_tree_ordering(driver: Driver) -> Result {
    for hash in Hash::ALL {
        let mut s = Scenario::init(driver, hash, "main")?;
        // "a-b" < "a/" < "a0" < "a=": directory sorts as "a/".
        for name in [
            &b"a-b"[..],
            b"a/x",
            b"a/y/z/deep",
            b"a0",
            b"a=",
            b"b/c/d/e/f",
            b"sp ace/t\tab",
        ] {
            s.model.write(0, name, &[name, b"\n"].concat())?;
        }
        s.add(0, &[b"."])?;
        let commit = s.commit(0, &["nested"])?;
        assert_eq!(
            git_text(&s.root(), &["rev-parse", &format!("{commit}^{{tree}}")])?,
            tree_id(hash, &s.model.checkouts[0].index)
        );
    }
    Ok(())
}

#[cfg(unix)]
fn exec_bit(driver: Driver) -> Result {
    for hash in Hash::ALL {
        let mut s = Scenario::init(driver, hash, "main")?;
        s.model.write(0, b"tool", b"#!/bin/sh\n")?;
        s.model.chmod(0, b"tool", true)?;
        s.add(0, &[b"tool"])?;
        assert_eq!(s.model.checkouts[0].index[&b"tool"[..]].mode, 0o100755);
        s.commit(0, &["exec"])?;
        s.model.chmod(0, b"tool", false)?;
        s.check()?; // " M tool" by mode only
        s.add(0, &[b"tool"])?;
        s.commit(0, &["no exec"])?;
    }
    Ok(())
}

#[cfg(unix)]
fn symlinks(driver: Driver) -> Result {
    for hash in Hash::ALL {
        let mut s = Scenario::init(driver, hash, "main")?;
        s.model.write(0, b"target", b"data\n")?;
        s.model.symlink(0, b"link", b"target")?;
        s.model.symlink(0, b"dangling", b"no/such/path")?;
        s.model.symlink(0, b"dir/up", b"../target")?;
        s.add(0, &[b"."])?;
        assert_eq!(s.model.checkouts[0].index[&b"link"[..]].mode, 0o120000);
        s.commit(0, &["links"])?;
        // File -> symlink type change and retarget.
        s.model.symlink(0, b"target", b"link")?;
        s.model.symlink(0, b"link", b"dangling")?;
        s.add(0, &[b"target", b"link"])?;
        s.commit(0, &["retarget"])?;
    }
    Ok(())
}

#[cfg(unix)]
fn non_utf8_paths(driver: Driver) -> Result {
    for hash in Hash::ALL {
        let mut s = Scenario::init(driver, hash, "main")?;
        let name = b"latin1-\xe9\xff";
        let probe = s.root().join(lifecycle_support::model::os_path(name));
        if let Err(error) = fs::write(&probe, b"probe") {
            eprintln!(
                "SKIP: non_utf8_paths[{} {}]: filesystem refuses non-UTF-8 names: {error}",
                driver.name(),
                hash.name()
            );
            return Ok(());
        }
        fs::remove_file(&probe)?;
        s.model.write(0, name, b"bytes\n")?;
        s.model.write(0, b"dir-\x80/inner", b"inner\n")?;
        s.add(0, &[name, b"dir-\x80"])?;
        s.commit(0, &["non-utf8"])?;
    }
    Ok(())
}

/// Root commit on a new unborn branch while other branches exist.
fn unborn_branch_root_commits(driver: Driver) -> Result {
    for hash in Hash::ALL {
        let mut s = Scenario::init(driver, hash, "trunk")?;
        s.model.write(0, b"f", b"trunk\n")?;
        s.add(0, &[b"f"])?;
        let trunk = s.commit(0, &["trunk root"])?;
        git(&s.root(), &["checkout", "-q", "--orphan", "side"])?;
        s.model.orphan(0, "side");
        s.check()?;
        s.model.write(0, b"g", b"side\n")?;
        s.add(0, &[b"g"])?;
        let side = s.commit(0, &["side root"])?;
        assert_eq!(git_text(&s.root(), &["rev-list", "--count", &side])?, "1");
        assert_eq!(
            observe::resolve(&s.root(), "refs/heads/trunk")?,
            Some(trunk)
        );
    }
    Ok(())
}

fn detached_head_commits(driver: Driver) -> Result {
    for hash in Hash::ALL {
        let mut s = Scenario::init(driver, hash, "main")?;
        s.model.write(0, b"f", b"one\n")?;
        s.add(0, &[b"f"])?;
        let main = s.commit(0, &["one"])?;
        git(&s.root(), &["checkout", "-q", "--detach"])?;
        s.model.detach(0)?;
        s.model.write(0, b"f", b"two\n")?;
        s.add(0, &[b"f"])?;
        let detached = s.commit(0, &["two"])?;
        assert_ne!(detached, main);
        assert_eq!(observe::resolve(&s.root(), "refs/heads/main")?, Some(main));
        assert_eq!(observe::symbolic_head(&s.root())?, None);
    }
    Ok(())
}

/// PWR-178: C Git creates the repository and a linked worktree; the driver
/// commits independently in both; C Git validates both branches and the
/// per-worktree HEAD/index isolation.
fn linked_worktree_isolation(driver: Driver) -> Result {
    for hash in Hash::ALL {
        let sandbox = lifecycle_support::proc::sandbox()?;
        let root = sandbox.path().join("main");
        ok(
            &Driver::CGit.init(
                sandbox.path(),
                &["-q", "-b", "main", "--object-format", hash.name(), "main"],
            )?,
            "C Git init",
        )?;
        let mut s = Scenario {
            model: Model::new(hash, &root, "main"),
            sandbox,
            driver: Driver::CGit,
        };
        s.model.write(0, b"shared", b"base\n")?;
        s.add(0, &[b"shared"])?;
        let base = s.commit(0, &["base"])?;
        let linked_root = s.dir().join("linked");
        git(
            &root,
            &[
                "worktree".as_ref(),
                "add".as_ref(),
                "-q".as_ref(),
                "-b".as_ref(),
                "feature".as_ref(),
                linked_root.as_os_str(),
            ],
        )?;
        let linked = s.model.add_linked(0, &linked_root, "feature")?;
        s.check()?;
        let (linked_git_dir, common) = observe::git_dirs(&linked_root)?;
        assert_ne!(linked_git_dir, common);

        s.driver = driver;
        // Commit in the linked worktree; the main worktree must not change.
        let main_index = fs::read(root.join(".git/index"))?;
        let main_head = fs::read(root.join(".git/HEAD"))?;
        s.model.write(linked, b"feature-only", b"feature\n")?;
        s.model.write(linked, b"shared", b"feature edit\n")?;
        s.add(linked, &[b"feature-only", b"shared"])?;
        assert_eq!(fs::read(root.join(".git/index"))?, main_index);
        let feature = s.commit(linked, &["feature work"])?;
        assert_eq!(fs::read(root.join(".git/index"))?, main_index);
        assert_eq!(fs::read(root.join(".git/HEAD"))?, main_head);
        assert_eq!(
            observe::resolve(&root, "refs/heads/main")?,
            Some(base.clone())
        );

        // Commit in the main worktree; the linked worktree must not change.
        let linked_index = fs::read(linked_git_dir.join("index"))?;
        let linked_head = fs::read(linked_git_dir.join("HEAD"))?;
        s.model.write(0, b"main-only", b"main\n")?;
        s.add(0, &[b"main-only"])?;
        let main = s.commit(0, &["main work"])?;
        assert_eq!(fs::read(linked_git_dir.join("index"))?, linked_index);
        assert_eq!(fs::read(linked_git_dir.join("HEAD"))?, linked_head);
        assert_eq!(
            observe::resolve(&root, "refs/heads/feature")?,
            Some(feature.clone())
        );

        // Fresh C Git processes in each worktree.
        assert_eq!(
            observe::resolve(&linked_root, "HEAD")?,
            Some(feature.clone())
        );
        assert_eq!(observe::resolve(&root, "HEAD")?, Some(main.clone()));
        assert_eq!(
            git_text(&root, &["merge-base", &main, &feature])?,
            base,
            "both branches descend from the shared base"
        );
        observe::fsck(&linked_root)?;
        let list = git_text(&root, &["worktree", "list", "--porcelain"])?;
        assert!(list.contains("branch refs/heads/feature"), "{list}");
    }
    Ok(())
}

fn nothing_to_commit_and_allow_empty(driver: Driver) -> Result {
    if driver::lacks(driver, &["commit"], "nothing_to_commit_and_allow_empty") {
        return Ok(());
    }
    let mut s = Scenario::init(driver, Hash::Sha1, "main")?;
    s.model.write(0, b"f", b"x\n")?;
    s.add(0, &[b"f"])?;
    s.commit(0, &["one"])?;
    let before = snapshot(s.dir())?;
    let output = driver.commit(&s.root(), &["nothing"], s.model.clock + 60)?;
    refused(
        &output,
        driver.nothing_to_commit_code(),
        "nothing to commit",
    )?;
    if driver.index_bytes_stable_on_refusal() {
        assert_preserved(
            s.dir(),
            &before,
            &snapshot(s.dir())?,
            Preserve::Exact,
            "nothing to commit",
        )?;
    }
    s.check()?;
    let when = s.model.tick();
    let output = lifecycle_support::proc::run(&mut driver.commit_cmd(
        &s.root(),
        &["empty"],
        when,
        &["--allow-empty"],
    ))?;
    ok(&output, "commit --allow-empty")?;
    s.model.commit(0, &["empty"], when);
    s.check()?;
    Ok(())
}

/// Same-size rewrite in the same second with mtime restored right after add
/// (racy-clean): status must report it and the next add must stage it.
fn racy_same_second_edit(driver: Driver) -> Result {
    if driver::lacks(driver, &["add"], "racy_same_second_edit") {
        return Ok(());
    }
    for hash in Hash::ALL {
        let mut s = Scenario::init(driver, hash, "main")?;
        let path = s.root().join("racy");
        s.model.write(0, b"racy", b"version-1\n")?;
        s.add(0, &[b"racy"])?;
        let staged = fs::metadata(&path)?.modified()?;
        s.model.write(0, b"racy", b"version-2\n")?;
        fs::File::options()
            .write(true)
            .open(&path)?
            .set_modified(staged)?;
        assert_eq!(fs::metadata(&path)?.modified()?, staged);
        let status = lifecycle_support::proc::run(driver.command(&s.root()).args([
            "status",
            "--porcelain=v1",
            "-z",
            "-uall",
            "--no-renames",
        ]))?;
        ok(&status, "status")?;
        assert_eq!(
            status.stdout,
            s.model.status(0),
            "{} racy status",
            driver.name()
        );
        s.check()?;
        s.add(0, &[b"racy"])?;
        assert_eq!(
            s.model.checkouts[0].index[&b"racy"[..]].oid,
            hash.object_hex("blob", b"version-2\n")
        );
        s.commit(0, &["racy"])?;
    }
    Ok(())
}

both_drivers!(
    init_stage_commit,
    init_reports_and_refuses_existing,
    bare_init_and_worktree_refusals,
    partial_staging_distinct_from_later_edits,
    multi_commit_and_deletion_staging,
    nested_dirs_and_tree_ordering,
    unborn_branch_root_commits,
    detached_head_commits,
    linked_worktree_isolation,
    nothing_to_commit_and_allow_empty,
    racy_same_second_edit,
);

#[cfg(unix)]
mod unix {
    use super::*;
    both_drivers!(exec_bit, symlinks, non_utf8_paths);
}
