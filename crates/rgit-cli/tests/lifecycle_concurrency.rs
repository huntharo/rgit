//! Competing writers (ADR 0003 invariants 2-4): held locks refuse without
//! being unlinked, compare-and-swap losers leave refs byte-identical, and a
//! writer whose lock was replaced never publishes or unlinks it.

#[allow(dead_code)]
mod lifecycle_support;

use std::fs;
use std::path::{Path, PathBuf};

use lifecycle_support::Scenario;
use lifecycle_support::crash::{Paused, exists, run_expect_no_pause};
use lifecycle_support::driver::{self, Driver, ok, refused};
use lifecycle_support::model::Hash;
use lifecycle_support::observe;
use lifecycle_support::proc::{Preserve, Result, Snap, assert_preserved, describe, git, snapshot};

/// Committed `a`, `b` on `main`.
fn based(driver: Driver, hash: Hash) -> Result<Scenario> {
    let mut s = Scenario::init(driver, hash, "main")?;
    s.model.write(0, b"a", b"a1\n")?;
    s.model.write(0, b"b", b"b1\n")?;
    s.add(0, &[b"a", b"b"])?;
    s.commit(0, &["base"])?;
    Ok(s)
}

fn relative(s: &Scenario, path: &Path) -> Result<PathBuf> {
    Ok(path.strip_prefix(s.dir())?.to_path_buf())
}

fn mentions(output: &std::process::Output, needle: &str) -> Result {
    if String::from_utf8_lossy(&output.stderr).contains(needle) {
        Ok(())
    } else {
        Err(format!("diagnostic should name {needle}: {}", describe(output)).into())
    }
}

#[cfg(unix)]
fn inode(path: &Path) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(fs::symlink_metadata(path)?.ino())
}

#[cfg(not(unix))]
fn inode(_path: &Path) -> Result<u64> {
    Ok(0)
}

// ---------------------------------------------------------------------------
// Pre-existing locks (both drivers).

fn preexisting_index_lock_refuses_add(driver: Driver) -> Result {
    if driver::lacks(driver, &["add"], "preexisting_index_lock_refuses_add") {
        return Ok(());
    }
    for hash in Hash::ALL {
        let mut s = based(driver, hash)?;
        let lock = s.root().join(".git/index.lock");
        fs::write(&lock, b"another writer\n")?;
        let lock_inode = inode(&lock)?;
        s.model.write(0, b"a", b"a2\n")?;
        s.model.write(0, b"new", b"new\n")?;
        let before = snapshot(s.dir())?;
        let output = driver.add(&s.root(), &["a", "new"])?;
        refused(&output, 128, "add with held index.lock")?;
        mentions(&output, "index.lock")?;
        assert_preserved(
            s.dir(),
            &before,
            &snapshot(s.dir())?,
            Preserve::AllowNewObjects,
            "add refusal",
        )?;
        assert_eq!(inode(&lock)?, lock_inode, "lock was replaced");
        // The operator removes the stale lock; the retry succeeds.
        fs::remove_file(&lock)?;
        s.add(0, &[b"a", b"new"])?;
        s.commit(0, &["after lock"])?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum HeadShape {
    Branch,
    Unborn,
    Detached,
}

fn preexisting_ref_lock_refuses_commit(driver: Driver) -> Result {
    if driver::lacks(driver, &["commit"], "preexisting_ref_lock_refuses_commit") {
        return Ok(());
    }
    for hash in Hash::ALL {
        for shape in [HeadShape::Branch, HeadShape::Unborn, HeadShape::Detached] {
            let mut s = match shape {
                HeadShape::Unborn => Scenario::init(driver, hash, "main")?,
                _ => based(driver, hash)?,
            };
            let lock = match shape {
                HeadShape::Detached => {
                    git(&s.root(), &["checkout", "-q", "--detach"])?;
                    s.model.detach(0)?;
                    s.root().join(".git/HEAD.lock")
                }
                _ => s.root().join(".git/refs/heads/main.lock"),
            };
            fs::create_dir_all(lock.parent().ok_or("parent")?)?;
            s.model.write(0, b"a", b"staged\n")?;
            s.add(0, &[b"a"])?;
            fs::write(&lock, b"another writer\n")?;
            let lock_inode = inode(&lock)?;
            let before = snapshot(s.dir())?;
            let when = s.model.tick();
            let output = driver.commit(&s.root(), &["blocked"], when)?;
            let context = format!(
                "{} {hash:?} {shape:?} commit with held ref lock",
                driver.name()
            );
            refused(&output, 128, &context)?;
            mentions(
                &output,
                lock.file_name().ok_or("name")?.to_str().ok_or("utf8")?,
            )?;
            let after = snapshot(s.dir())?;
            if driver.index_bytes_stable_on_refusal() {
                assert_preserved(
                    s.dir(),
                    &before,
                    &after,
                    Preserve::AllowNewObjects,
                    &context,
                )?;
            } else {
                // C Git refreshes the index cache-tree and writes
                // COMMIT_EDITMSG even when the ref update fails; compare
                // everything else byte-for-byte and the index by entries
                // (s.check() below).
                let scratch = [
                    relative(&s, &s.root().join(".git/index"))?,
                    relative(&s, &s.root().join(".git/COMMIT_EDITMSG"))?,
                ];
                let strip = |snap: &std::collections::BTreeMap<PathBuf, Snap>| {
                    let mut snap = snap.clone();
                    for path in &scratch {
                        snap.remove(path);
                    }
                    snap
                };
                assert_preserved(
                    s.dir(),
                    &strip(&before),
                    &strip(&after),
                    Preserve::AllowNewObjects,
                    &context,
                )?;
            }
            assert_eq!(inode(&lock)?, lock_inode, "{context}: lock replaced");
            // Old state is fully intact (apart from the held lock).
            fs::remove_file(&lock)?;
            s.check()?;
            s.commit(0, &["after lock"])?;
        }
    }
    Ok(())
}

both_drivers!(
    preexisting_index_lock_refuses_add,
    preexisting_ref_lock_refuses_commit
);

// ---------------------------------------------------------------------------
// Live competing rgit writers (failpoint pause; rgit only).

/// Two commits race on one branch: the first holds the ref lock (paused at
/// `commit.ref-locked`); the second is refused with exit 128, never reaches
/// the pause, and leaves the ref and the first writer's lock untouched.
fn concurrent_commits_one_winner(driver: Driver) -> Result {
    if driver::lacks(driver, &["commit"], "concurrent_commits_one_winner") {
        return Ok(());
    }
    for hash in Hash::ALL {
        let mut s = based(driver, hash)?;
        s.model.write(0, b"a", b"race\n")?;
        s.add(0, &[b"a"])?;
        let ref_path = s.root().join(".git/refs/heads/main");
        let lock = s.root().join(".git/refs/heads/main.lock");
        let when_a = s.model.tick();
        let when_b = s.model.tick();
        let winner = Paused::spawn(
            &mut driver.commit_cmd(&s.root(), &["writer A"], when_a, &[]),
            "commit.ref-locked",
        )?;
        if !exists(&lock) {
            return Err("commit.ref-locked paused without refs/heads/main.lock".into());
        }
        // Every lock the winner holds (the branch lock, plus HEAD.lock when
        // the implementation also locks HEAD for its reflog, as C Git does).
        let held: Vec<(PathBuf, Vec<u8>, u64)> = observe::locks(&s.root().join(".git"))?
            .into_iter()
            .map(|path| Ok((fs::read(&path)?, inode(&path)?, path)))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .map(|(bytes, ino, path)| (path, bytes, ino))
            .collect();
        let before = snapshot(s.dir())?;
        let loser = run_expect_no_pause(
            &mut driver.commit_cmd(&s.root(), &["writer B"], when_b, &[]),
            "commit.ref-locked",
        )?;
        refused(&loser, 128, "losing concurrent commit")?;
        let stderr = String::from_utf8_lossy(&loser.stderr).into_owned();
        if !held.iter().any(|(path, _, _)| {
            stderr.contains(&*path.to_string_lossy())
                || path
                    .file_name()
                    .is_some_and(|n| stderr.contains(&*n.to_string_lossy()))
        }) {
            return Err(format!(
                "loser's diagnostic names none of the held locks {held:?}: {stderr}"
            )
            .into());
        }
        assert_preserved(
            s.dir(),
            &before,
            &snapshot(s.dir())?,
            Preserve::AllowNewObjects,
            "loser",
        )?;
        for (path, bytes, ino) in &held {
            assert_eq!(&fs::read(path)?, bytes, "loser touched {}", path.display());
            assert_eq!(inode(path)?, *ino, "loser replaced {}", path.display());
        }
        let output = winner.release()?;
        ok(&output, "winning commit")?;
        let winner_oid = s.model.commit(0, &["writer A"], when_a);
        // A fresh process sees the winner; the loser's objects are dangling.
        assert_eq!(fs::read_to_string(&ref_path)?.trim(), winner_oid);
        s.check()?;
    }
    Ok(())
}

/// Compare-and-swap: a writer paused after writing its objects loses to a
/// competitor that moves the branch first, and leaves every published byte
/// exactly as the competitor left it. Covers born and unborn branches and
/// both rgit and C Git competitors.
fn cas_loser_leaves_refs_byte_identical(driver: Driver) -> Result {
    if driver::lacks(driver, &["commit"], "cas_loser_leaves_refs_byte_identical") {
        return Ok(());
    }
    for hash in Hash::ALL {
        for unborn in [false, true] {
            for competitor in [Driver::Rgit, Driver::CGit] {
                let mut s = if unborn {
                    Scenario::init(driver, hash, "main")?
                } else {
                    based(driver, hash)?
                };
                s.model.write(0, b"a", b"contended\n")?;
                s.add(0, &[b"a"])?;
                let when_loser = s.model.tick();
                let when_winner = s.model.tick();
                let loser = Paused::spawn(
                    &mut driver.commit_cmd(&s.root(), &["loser"], when_loser, &[]),
                    "commit.objects-written",
                )?;
                ok(
                    &competitor.commit(&s.root(), &["winner"], when_winner)?,
                    "competing commit",
                )?;
                s.model.commit(0, &["winner"], when_winner);
                let before = snapshot(s.dir())?;
                let output = loser.release()?;
                let context = format!("{hash:?} unborn={unborn} vs {}", competitor.name());
                refused(&output, 128, &format!("CAS loser {context}"))?;
                assert_preserved(
                    s.dir(),
                    &before,
                    &snapshot(s.dir())?,
                    Preserve::AllowNewObjects,
                    &context,
                )?;
                s.check()?;
            }
        }
    }
    Ok(())
}

/// The second `add` is refused while the first holds `index.lock` and never
/// unlinks it; the first then publishes only its own change.
fn second_add_refused_while_index_locked(driver: Driver) -> Result {
    if driver::lacks(driver, &["add"], "second_add_refused_while_index_locked") {
        return Ok(());
    }
    for hash in Hash::ALL {
        let mut s = based(driver, hash)?;
        s.model.write(0, b"a", b"first writer\n")?;
        let lock = s.root().join(".git/index.lock");
        let first = Paused::spawn(&mut driver.add_cmd(&s.root(), &["a"]), "add.index-locked")?;
        if !exists(&lock) {
            return Err("add.index-locked paused without index.lock".into());
        }
        let held = fs::read(&lock)?;
        let held_inode = inode(&lock)?;
        fs::write(s.root().join("b"), b"second writer\n")?;
        let before = snapshot(s.dir())?;
        let second =
            run_expect_no_pause(&mut driver.add_cmd(&s.root(), &["b"]), "add.index-locked")?;
        refused(&second, 128, "second add")?;
        mentions(&second, "index.lock")?;
        assert_preserved(
            s.dir(),
            &before,
            &snapshot(s.dir())?,
            Preserve::AllowNewObjects,
            "second add",
        )?;
        assert_eq!(fs::read(&lock)?, held);
        assert_eq!(inode(&lock)?, held_inode);
        ok(&first.release()?, "first add")?;
        s.model.stage(0, &[b"a"])?;
        s.model.write(0, b"b", b"second writer\n")?; // worktree only
        s.check()?;
    }
    Ok(())
}

/// A writer whose lock is removed and replaced by another writer's lock
/// while it is paused must neither publish nor unlink the replacement.
fn replaced_lock_never_published_or_unlinked(driver: Driver) -> Result {
    for hash in Hash::ALL {
        for commit in [false, true] {
            let subject = if commit { "commit" } else { "add" };
            if driver::lacks(driver, &[subject], &format!("replaced_lock ({subject})")) {
                continue;
            }
            let mut s = based(driver, hash)?;
            s.model.write(0, b"a", b"paused writer\n")?;
            let (lock, mut command, point) = if commit {
                s.add(0, &[b"a"])?;
                let when = s.model.tick();
                (
                    s.root().join(".git/refs/heads/main.lock"),
                    driver.commit_cmd(&s.root(), &["paused"], when, &[]),
                    "commit.ref-locked",
                )
            } else {
                (
                    s.root().join(".git/index.lock"),
                    driver.add_cmd(&s.root(), &["a"]),
                    "add.index-locked",
                )
            };
            let before_spawn = snapshot(s.dir())?;
            let paused = Paused::spawn(&mut command, point)?;
            if !exists(&lock) {
                return Err(format!("{point} paused without {}", lock.display()).into());
            }
            if cfg!(windows) {
                // The held lock is opened without FILE_SHARE_DELETE, so no
                // other process can remove or replace it: the scenario this
                // test guards against cannot be set up. Assert that instead.
                let removed = fs::remove_file(&lock);
                match removed {
                    Err(error) if error.raw_os_error() == Some(32) => {}
                    other => {
                        return Err(format!("held lock removable on Windows: {other:?}").into());
                    }
                }
                let output = paused.release()?;
                if !output.status.success() {
                    return Err(format!("{point} after refused removal: {output:?}").into());
                }
                eprintln!("NOTE: {point}: Windows refused deleting the held lock (os error 32)");
                continue;
            }
            fs::remove_file(&lock)?;
            let foreign = b"foreign writer's lock\n";
            fs::write(&lock, foreign)?;
            let foreign_inode = inode(&lock)?;
            let output = paused.release()?;
            let context = format!("{hash:?} {point} with replaced lock");
            refused(&output, 128, &context)?;
            assert_eq!(fs::read(&lock)?, foreign, "{context}: replacement changed");
            assert_eq!(
                inode(&lock)?,
                foreign_inode,
                "{context}: replacement unlinked"
            );
            let mut after = snapshot(s.dir())?;
            let lock_rel = relative(&s, &lock)?;
            after.remove(&lock_rel);
            assert_preserved(
                s.dir(),
                &before_spawn,
                &after,
                Preserve::AllowNewObjects,
                &context,
            )?;
            fs::remove_file(&lock)?;
            s.check()?;
        }
    }
    Ok(())
}

rgit_only!(
    concurrent_commits_one_winner,
    cas_loser_leaves_refs_byte_identical,
    second_add_refused_while_index_locked,
    replaced_lock_never_published_or_unlinked,
);
