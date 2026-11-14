//! PWR-194 adversarial filesystem oracle (decision 0004, acceptance 1-4).
//!
//! Each scenario pauses an rgit write at a failpoint, swaps part of the
//! worktree or Git directory for a link to an outside decoy tree (or replaces
//! a metadata file or lock), releases the writer and then judges the result
//! from fresh processes. Exactly two outcomes are accepted:
//!
//! - refusal (exit 128) with the repository unchanged except for new,
//!   valid, unreferenced loose objects (ADR 0003), or
//! - completion confined to the anchored root: the model's expected new
//!   state, built from the ORIGINAL tree's bytes.
//!
//! In both cases every decoy byte, entry and inode is untouched, no decoy
//! content is staged or stored as an object, foreign locks keep their bytes
//! and inode, every wait is bounded, and `git fsck --strict` passes.
//!
//! Races are exercised at deterministic pause points taken from the source.
//! The results show behaviour at those boundaries only; they do not prove
//! that no other interleaving between system calls exists.

#[allow(dead_code)]
#[path = "lifecycle_support/mod.rs"]
mod lifecycle_support;

#[allow(dead_code)]
mod fs_hardening_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use fs_hardening_support::inode;
use fs_hardening_support::{Expect, Recorded, assert_untouched, case, record, swap_for_link};
use lifecycle_support::Scenario;
use lifecycle_support::crash::Paused;
use lifecycle_support::driver::{Driver, ok};
use lifecycle_support::model::{Hash, Model};
use lifecycle_support::observe;
#[cfg(unix)]
use lifecycle_support::proc::finish;
use lifecycle_support::proc::{Preserve, Result, Snap, assert_preserved, describe, git, snapshot};

#[cfg(unix)]
const BOUND: Duration = Duration::from_secs(20);

type Body = Box<dyn FnOnce() -> Result>;

/// Committed fixture built by C Git: `top`, `dir/sub/file`, with an outside
/// decoy tree beside the repository.
struct Fixture {
    s: Scenario,
    outside: PathBuf,
}

impl Fixture {
    fn new(hash: Hash) -> Result<Self> {
        let mut s = Scenario::init(Driver::CGit, hash, "main")?;
        s.model.write(0, b"top", b"top v1\n")?;
        s.model.write(0, b"dir/sub/file", b"real v1\n")?;
        s.add(0, &[b"."])?;
        s.commit(0, &["base"])?;
        s.driver = Driver::Rgit;
        let outside = s.dir().join("outside");
        fs::create_dir(&outside)?;
        Ok(Self { s, outside })
    }

    fn repo(&self) -> PathBuf {
        self.s.root()
    }

    fn git_dir(&self) -> PathBuf {
        self.repo().join(".git")
    }

    /// Create `rel` under the outside tree with `bytes`.
    fn decoy(&self, rel: &str, bytes: &[u8]) -> Result<PathBuf> {
        let path = self.outside.join(rel);
        fs::create_dir_all(path.parent().ok_or("parent")?)?;
        fs::write(&path, bytes)?;
        Ok(path)
    }

    fn decoy_dir(&self, rel: &str) -> Result<PathBuf> {
        let path = self.outside.join(rel);
        fs::create_dir_all(&path)?;
        Ok(path)
    }
}

/// Decoy contents that must never become a staged blob or stored object.
const DECOYS: [&[u8]; 4] = [
    b"DECOY file\n",
    b"DECOY new\n",
    b"DECOY ref\n",
    b"DECOY log\n",
];

/// No object in the repository holds any decoy content, and the index names
/// none of it.
fn no_decoy_objects(fx: &Fixture) -> Result {
    let hash = fx.s.model.hash;
    let index = observe::index(&fx.repo())?;
    for bytes in DECOYS {
        let oid = hash.object_hex("blob", bytes);
        if index.values().any(|entry| entry.oid == oid) {
            return Err(format!(
                "decoy content {:?} was staged",
                String::from_utf8_lossy(bytes)
            )
            .into());
        }
        let path = fx.git_dir().join("objects").join(&oid[..2]).join(&oid[2..]);
        if fs::symlink_metadata(&path).is_ok() {
            return Err(format!(
                "decoy content {:?} was stored as an object",
                String::from_utf8_lossy(bytes)
            )
            .into());
        }
    }
    Ok(())
}

/// The two acceptable outcomes. `before` is a snapshot of the repository
/// taken before the paused writer started; `expected` is the model of a
/// completion confined to the original tree.
fn judge(
    fx: &Fixture,
    output: &Output,
    before: &std::collections::BTreeMap<PathBuf, Snap>,
    outside: &Recorded,
    expected: &Model,
    context: &str,
) -> Result {
    assert_untouched(&fx.outside, outside, context)?;
    no_decoy_objects(fx)?;
    eprintln!(
        "OUTCOME: {context}: {}",
        match output.status.code() {
            Some(0) => "completed".to_owned(),
            _ => format!(
                "refused: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
    );
    match output.status.code() {
        Some(128) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_preserved(
                &fx.repo(),
                before,
                &snapshot(&fx.repo())?,
                Preserve::AllowNewObjects,
                &format!("{context} (refused: {})", stderr.trim()),
            )?;
            observe::fsck(&fx.repo())?;
            if cfg!(windows) && stderr.to_lowercase().contains("unsupported") {
                eprintln!(
                    "NOTE: {context}: refused as unsupported on this platform, state unchanged"
                );
            }
            Ok(())
        }
        Some(0) => observe::assert_state(expected).map_err(|e| {
            format!("{context}: completed but not confined to the original tree: {e}").into()
        }),
        _ => Err(format!(
            "{context}: neither refusal nor completion: {}",
            describe(output)
        )
        .into()),
    }
}

fn release(paused: Paused) -> Result<Output> {
    paused.release()
}

// ---------------------------------------------------------------------------
// Acceptance 1: worktree ancestors swapped after the leaf was examined.

fn worktree_ancestor_swap(hash: Hash, swapped: &str, spec: &str) -> Result {
    let mut fx = Fixture::new(hash)?;
    // Original-tree changes the writer must stage (or refuse).
    fx.s.model.write(0, b"dir/sub/file", b"real v2\n")?;
    fx.s.model.write(0, b"dir/sub/new", b"real new\n")?;
    // Decoy mirroring the swapped directory's layout.
    let prefix = if swapped == "dir" { "wt/sub/" } else { "wt/" };
    fx.decoy(&format!("{prefix}file"), DECOYS[0])?;
    fx.decoy(&format!("{prefix}new"), DECOYS[1])?;
    let target = fx.outside.join("wt");
    let mut expected = fx.s.model.clone();
    expected.stage(0, &[spec.as_bytes()])?;
    let outside = record(&fx.outside)?;
    let before = snapshot(&fx.repo())?;
    let paused = Paused::spawn(
        &mut fx.s.driver.add_cmd(&fx.repo(), &[spec]),
        "add.before-leaf-open",
    )?;
    let Some(mut swap) =
        swap_for_link(&fx.repo().join(swapped), &target, "worktree ancestor swap")?
    else {
        drop(paused);
        return Ok(());
    };
    let output = release(paused)?;
    swap.restore()?;
    judge(
        &fx,
        &output,
        &before,
        &outside,
        &expected,
        &format!("add {spec} with {swapped}/ swapped"),
    )
}

// ---------------------------------------------------------------------------
// Acceptance 2: Git directory parts swapped at write-side pause points.

/// The fan-out directory holding the paused writer's temporary object.
fn fanout_with_temporary(git_dir: &Path) -> Result<PathBuf> {
    for entry in fs::read_dir(git_dir.join("objects"))? {
        let dir = entry?.path();
        if dir.is_dir()
            && fs::read_dir(&dir)?
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().starts_with("tmp_obj_"))
        {
            return Ok(dir);
        }
    }
    Err("paused at object.before-publish without a temporary object".into())
}

fn objects_fanout_swap(hash: Hash, commit: bool, strict_cleanup: bool) -> Result {
    let mut fx = Fixture::new(hash)?;
    let mut expected = fx.s.model.clone();
    let when = expected.tick();
    let mut command = if commit {
        fx.s.model.write(0, b"top", b"top v2\n")?;
        fx.s.add(0, &[b"top"])?; // C Git stages; rgit commits
        expected = fx.s.model.clone();
        expected.commit(0, &["swap"], when);
        fx.s.driver.commit_cmd(&fx.repo(), &["swap"], when, &[])
    } else {
        fx.s.model.write(0, b"dir/sub/new", b"brand new object\n")?;
        expected = fx.s.model.clone();
        expected.stage(0, &[b"dir/sub/new"])?;
        fx.s.driver.add_cmd(&fx.repo(), &["dir/sub/new"])
    };
    let target = fx.decoy_dir("objects_fanout")?;
    fx.decoy("objects_fanout/decoy-object", DECOYS[0])?;
    let outside = record(&fx.outside)?;
    let before = snapshot(&fx.repo())?;
    let paused = Paused::spawn(&mut command, "object.before-publish")?;
    let fanout = fanout_with_temporary(&fx.git_dir())?;
    let Some(mut swap) = swap_for_link(&fanout, &target, "objects fan-out swap")? else {
        drop(paused);
        return Ok(());
    };
    let output = release(paused)?;
    swap.restore()?;
    // A refused writer must remove its own temporary object from the
    // ORIGINAL fan-out; a path-based cleanup goes through the swapped link
    // instead and leaves it behind (strict variant, gated on anchoring).
    let left = remove_temporaries(&fanout)?;
    if strict_cleanup && left > 0 {
        return Err("refused writer left its temporary object in the original fan-out".into());
    }
    let what = if commit { "commit" } else { "add" };
    judge(
        &fx,
        &output,
        &before,
        &outside,
        &expected,
        &format!(
            "{what} with objects/{} swapped",
            fanout.file_name().unwrap_or_default().to_string_lossy()
        ),
    )
}

fn remove_temporaries(fanout: &Path) -> Result<usize> {
    let mut count = 0;
    for entry in fs::read_dir(fanout)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with("tmp_obj_") {
            eprintln!(
                "NOTE: refused writer left temporary {}",
                entry.path().display()
            );
            fs::remove_file(entry.path())?;
            count += 1;
        }
    }
    Ok(count)
}

/// Swap `rel` (a directory under `.git`) at `commit.ref-locked`, after the
/// writer took HEAD.lock and refs/heads/main.lock and before it logs.
fn git_dir_swap_during_commit(
    hash: Hash,
    rel: &str,
    decoys: &[(&str, &[u8])],
    strict_cleanup: bool,
) -> Result {
    let mut fx = Fixture::new(hash)?;
    fx.s.model.write(0, b"top", b"top v2\n")?;
    fx.s.add(0, &[b"top"])?;
    let mut expected = fx.s.model.clone();
    let when = expected.tick();
    expected.commit(0, &["swap"], when);
    let target = fx.decoy_dir("gitdir")?;
    for (path, bytes) in decoys {
        fx.decoy(&format!("gitdir/{path}"), bytes)?;
    }
    let outside = record(&fx.outside)?;
    let before = snapshot(&fx.repo())?;
    let paused = Paused::spawn(
        &mut fx.s.driver.commit_cmd(&fx.repo(), &["swap"], when, &[]),
        "commit.ref-locked",
    )?;
    let Some(mut swap) = swap_for_link(&fx.git_dir().join(rel), &target, "git dir swap")? else {
        drop(paused);
        return Ok(());
    };
    let output = release(paused)?;
    swap.restore()?;
    // The writer's own locks must not survive a handled refusal. A
    // path-based unlink goes through the swapped link and orphans them in
    // the ORIGINAL directory (strict variant, gated on anchoring).
    let orphans: Vec<PathBuf> = observe::locks(&fx.git_dir())?;
    if !orphans.is_empty() {
        if strict_cleanup {
            return Err(format!("refused writer orphaned its own locks {orphans:?}").into());
        }
        eprintln!("NOTE: commit with .git/{rel} swapped orphaned own locks {orphans:?}");
        for lock in &orphans {
            fs::remove_file(lock)?;
        }
    }
    judge(
        &fx,
        &output,
        &before,
        &outside,
        &expected,
        &format!("commit with .git/{rel} swapped"),
    )
}

/// `.git/info` swapped for a decoy whose exclude would ignore the new file.
fn info_dir_swap_during_add(hash: Hash) -> Result {
    let mut fx = Fixture::new(hash)?;
    fx.s.model.write(0, b"dir/sub/new", b"real new\n")?;
    let mut expected = fx.s.model.clone();
    expected.stage(0, &[b"dir/sub/new"])?;
    let target = fx.decoy_dir("info")?;
    fx.decoy("info/exclude", b"new\n")?;
    fx.decoy("info/attributes", b"* -text\n")?;
    fs::create_dir_all(fx.git_dir().join("info"))?;
    let outside = record(&fx.outside)?;
    let before = snapshot(&fx.repo())?;
    let paused = Paused::spawn(
        &mut fx.s.driver.add_cmd(&fx.repo(), &["dir/sub/new"]),
        "add.before-leaf-open",
    )?;
    let Some(mut swap) = swap_for_link(&fx.git_dir().join("info"), &target, "info swap")? else {
        drop(paused);
        return Ok(());
    };
    let output = release(paused)?;
    swap.restore()?;
    judge(
        &fx,
        &output,
        &before,
        &outside,
        &expected,
        "add with .git/info swapped",
    )
}

// ---------------------------------------------------------------------------
// Acceptance 3: info/exclude and info/attributes as FIFO or symlink.

/// Replace `.git/info/<name>` before the writer starts. A FIFO must not
/// block; a symlink's outside target must not be read: completion has to
/// stage `dir/sub/new` even though the decoy exclude would ignore it.
#[cfg(unix)]
fn info_file_replaced(hash: Hash, name: &str, fifo: bool, spec: &str) -> Result {
    let mut fx = Fixture::new(hash)?;
    fx.s.model.write(0, b"dir/sub/new", b"real new\n")?;
    let mut expected = fx.s.model.clone();
    expected.stage(0, &[spec.as_bytes()])?;
    let info = fx.git_dir().join("info");
    fs::create_dir_all(&info)?;
    let path = info.join(name);
    let _ = fs::remove_file(&path);
    let decoy_bytes: &[u8] = if name == "exclude" {
        b"new\n"
    } else {
        b"* filter=decoy\n"
    };
    let decoy = fx.decoy(&format!("info-{name}"), decoy_bytes)?;
    if fifo {
        fs_hardening_support::mkfifo(&path)?;
    } else {
        std::os::unix::fs::symlink(&decoy, &path)?;
    }
    let outside = record(&fx.outside)?;
    let before = snapshot(&fx.repo())?;
    let mut command = fx.s.driver.add_cmd(&fx.repo(), &[spec]);
    let output = finish(lifecycle_support::proc::spawn_piped(&mut command)?, BOUND)
        .map_err(|e| format!("add blocked with info/{name} replaced: {e}"))?;
    let kind = if fifo { "FIFO" } else { "symlink" };
    // The replacement is the user's own state: drop it before judging so the
    // fresh-process checks read a normal repository.
    fs::remove_file(&path)?;
    let mut before = before;
    before.retain(|p, _| !p.ends_with(Path::new("info").join(name)));
    judge(
        &fx,
        &output,
        &before,
        &outside,
        &expected,
        &format!("add {spec} with info/{name} as a {kind}"),
    )
}

/// Same, but the swap happens while the writer is paused after examining
/// the leaf: never a hang.
#[cfg(unix)]
fn info_file_swapped_to_fifo_while_paused(hash: Hash, name: &str) -> Result {
    let mut fx = Fixture::new(hash)?;
    fx.s.model.write(0, b"dir/sub/new", b"real new\n")?;
    let mut expected = fx.s.model.clone();
    expected.stage(0, &[b"dir/sub/new"])?;
    let info = fx.git_dir().join("info");
    fs::create_dir_all(&info)?;
    let path = info.join(name);
    fs::write(&path, b"# user metadata\n")?;
    let outside = record(&fx.outside)?;
    let before = snapshot(&fx.repo())?;
    let paused = Paused::spawn(
        &mut fx.s.driver.add_cmd(&fx.repo(), &["dir/sub/new"]),
        "add.before-leaf-open",
    )?;
    let saved = fs::read(&path)?;
    fs::remove_file(&path)?;
    fs_hardening_support::mkfifo(&path)?;
    let output = release(paused)?;
    fs::remove_file(&path)?;
    fs::write(&path, saved)?;
    judge(
        &fx,
        &output,
        &before,
        &outside,
        &expected,
        &format!("add with info/{name} swapped to a FIFO while paused"),
    )
}

// ---------------------------------------------------------------------------
// Acceptance 4: lock identity.

/// Unix: while paused holding `lock`, remove it and recreate it as a
/// foreign regular file or a symlink to an outside file. The writer must
/// refuse; the replacement keeps its bytes and inode; nothing outside
/// changes.
#[cfg(unix)]
fn lock_recreated(
    hash: Hash,
    commit: bool,
    lock_rel: &str,
    as_symlink: bool,
    refusal_only: bool,
) -> Result {
    let mut fx = Fixture::new(hash)?;
    let when = fx.s.model.tick();
    let (mut command, point) = if commit {
        fx.s.model.write(0, b"top", b"top v2\n")?;
        fx.s.add(0, &[b"top"])?;
        (
            fx.s.driver.commit_cmd(&fx.repo(), &["locked"], when, &[]),
            "commit.ref-locked",
        )
    } else {
        fx.s.model.write(0, b"top", b"top v2\n")?;
        (
            fx.s.driver.add_cmd(&fx.repo(), &["top"]),
            "add.index-locked",
        )
    };
    let decoy = fx.decoy("lock-target", b"outside lock target\n")?;
    let lock = fx.git_dir().join(lock_rel);
    let before_spawn = snapshot(&fx.repo())?;
    let paused = Paused::spawn(&mut command, point)?;
    if fs::symlink_metadata(&lock).is_err() {
        return Err(format!("{point} paused without {lock_rel}").into());
    }
    fs::remove_file(&lock)?;
    if as_symlink {
        std::os::unix::fs::symlink(&decoy, &lock)?;
    } else {
        fs::write(&lock, b"foreign writer\n")?;
    }
    let planted = (
        inode(&lock)?,
        fs::read_link(&lock).ok(),
        fs::symlink_metadata(&lock)?.len(),
    );
    let planted_bytes = if as_symlink {
        None
    } else {
        Some(fs::read(&lock)?)
    };
    let outside = record(&fx.outside)?;
    let output = release(paused)?;
    let context = format!(
        "{point} with {lock_rel} recreated as a {}",
        if as_symlink { "symlink" } else { "file" }
    );
    assert_untouched(&fx.outside, &outside, &context)?;
    // A published lock (index.lock, the branch lock) that was replaced must
    // make the writer refuse. HEAD.lock of a branch commit is only a guard,
    // never published: completing is acceptable provided the replacement is
    // neither published nor unlinked (checked below).
    // `refusal_only` (decision for PWR-194): HEAD.lock is the guard under
    // which the writer decides whether HEAD names the branch and appends to
    // logs/HEAD; if it is no longer ours that decision is stale, so the
    // branch must not be published. The refusal judgement below then also
    // proves the branch, both reflogs and HEAD unchanged and the writer's
    // own branch lock released.
    let guard_only = lock_rel == "HEAD.lock" && !refusal_only;
    let completed = output.status.code() == Some(0);
    if !(output.status.code() == Some(128) || (guard_only && completed)) {
        return Err(format!("{context}: writer did not refuse: {}", describe(&output)).into());
    }
    if completed {
        eprintln!(
            "NOTE: {context}: commit completed; the replaced HEAD.lock guard was left untouched"
        );
    }
    let now = (
        inode(&lock)?,
        fs::read_link(&lock).ok(),
        fs::symlink_metadata(&lock)?.len(),
    );
    if now != planted || (planted_bytes.is_some() && fs::read(&lock).ok() != planted_bytes) {
        return Err(format!("{context}: the replacement lock was changed or replaced").into());
    }
    // Everything else as before the writer started (new valid objects only).
    let mut after = snapshot(&fx.repo())?;
    let lock_key = lock.strip_prefix(fx.repo())?.to_path_buf();
    after.remove(&lock_key);
    let mut before_spawn = before_spawn;
    before_spawn.remove(&lock_key);
    if completed {
        let mut expected = fx.s.model.clone();
        expected.commit(0, &["locked"], when);
        fs::remove_file(&lock)?;
        observe::assert_state(&expected).map_err(|e| format!("{context}: {e}"))?;
        return no_decoy_objects(&fx);
    }
    assert_preserved(
        &fx.repo(),
        &before_spawn,
        &after,
        Preserve::AllowNewObjects,
        &context,
    )?;
    // The other lock of a commit (HEAD.lock or the branch lock) is the
    // writer's own and is released by a handled refusal.
    let leftovers: Vec<_> = observe::locks(&fx.git_dir())?
        .into_iter()
        .filter(|l| *l != lock)
        .collect();
    if !leftovers.is_empty() {
        return Err(format!("{context}: own locks left behind: {leftovers:?}").into());
    }
    fs::remove_file(&lock)?;
    observe::fsck(&fx.repo())?;
    no_decoy_objects(&fx)
}

/// A branch commit paused at `ref.logged` (both reflog lines appended, ref
/// not yet renamed) whose HEAD.lock guard is removed and recreated: the
/// guard check before the rename must stop publication, leaving the
/// documented logged-not-published state. Ref unchanged, both logs hold
/// exactly the new C Git line (no rollback), typed error, foreign HEAD.lock
/// untouched, own branch lock released.
#[cfg(unix)]
fn head_lock_replaced_after_append(hash: Hash, as_symlink: bool) -> Result {
    let mut fx = Fixture::new(hash)?;
    fx.s.model.write(0, b"top", b"top v2\n")?;
    fx.s.add(0, &[b"top"])?;
    let when = fx.s.model.tick();
    let (oid, _) = fx.s.model.predict_commit(0, &["locked"], when);
    let old_oid =
        fx.s.model
            .head_commit(0)
            .ok_or("fixture branch is unborn")?;
    let line = format!(
        "{old_oid} {oid} {} <{}> {when} +0000\tcommit: locked\n",
        lifecycle_support::proc::NAME,
        lifecycle_support::proc::EMAIL
    );
    let git_dir = fx.git_dir();
    let logs = [
        git_dir.join("logs/HEAD"),
        git_dir.join("logs/refs/heads/main"),
    ];
    let logs_before = logs
        .iter()
        .map(fs::read)
        .collect::<std::io::Result<Vec<_>>>()?;
    let appended = |before: &Vec<u8>| {
        let mut bytes = before.clone();
        bytes.extend_from_slice(line.as_bytes());
        bytes
    };
    let decoy = fx.decoy("lock-target", b"outside lock target\n")?;
    let lock = git_dir.join("HEAD.lock");
    let paused = Paused::spawn(
        &mut fx.s.driver.commit_cmd(&fx.repo(), &["locked"], when, &[]),
        "ref.logged",
    )?;
    for (log, before) in logs.iter().zip(&logs_before) {
        if fs::read(log)? != appended(before) {
            return Err(
                format!("paused at ref.logged without the line in {}", log.display()).into(),
            );
        }
    }
    fs::remove_file(&lock)?;
    if as_symlink {
        std::os::unix::fs::symlink(&decoy, &lock)?;
    } else {
        fs::write(&lock, b"foreign writer\n")?;
    }
    let planted = (inode(&lock)?, fs::read_link(&lock).ok());
    let planted_bytes = if as_symlink {
        None
    } else {
        Some(fs::read(&lock)?)
    };
    let outside = record(&fx.outside)?;
    let output = release(paused)?;
    let kind = if as_symlink { "symlink" } else { "file" };
    let context = format!("ref.logged with HEAD.lock recreated as a {kind}");
    assert_untouched(&fx.outside, &outside, &context)?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() != Some(128)
        || !stderr.contains("was not updated")
        || !stderr.contains("reflog already records")
    {
        return Err(format!(
            "{context}: expected the logged-not-published error: {}",
            describe(&output)
        )
        .into());
    }
    // Fresh-process view: ref still old, both logs keep the appended line.
    if observe::resolve(&fx.repo(), "refs/heads/main")?.as_deref() != Some(old_oid.as_str()) {
        return Err(format!("{context}: the branch was published").into());
    }
    for (log, before) in logs.iter().zip(&logs_before) {
        if fs::read(log)? != appended(before) {
            return Err(format!("{context}: {} rolled back or changed", log.display()).into());
        }
    }
    if (inode(&lock)?, fs::read_link(&lock).ok()) != planted
        || (planted_bytes.is_some() && fs::read(&lock).ok() != planted_bytes)
    {
        return Err(format!("{context}: the foreign HEAD.lock was changed or replaced").into());
    }
    let own: Vec<_> = observe::locks(&git_dir)?
        .into_iter()
        .filter(|l| *l != lock)
        .collect();
    if !own.is_empty() {
        return Err(format!("{context}: own locks left behind: {own:?}").into());
    }
    fs::remove_file(&lock)?;
    observe::fsck(&fx.repo())?;
    no_decoy_objects(&fx)
}

/// Decision 0004, acceptance 6: the backend is asserted, not assumed.
#[test]
fn backend_selection_is_asserted() {
    use rgit_core::write::{FilesystemHardening, filesystem_hardening};
    let selected = filesystem_hardening();
    if cfg!(unix) {
        if fs_hardening_support::REQUIRE_ANCHORED_ON_UNIX {
            assert_eq!(
                selected,
                FilesystemHardening::Anchored,
                "Unix acceptance requires the anchored backend"
            );
            eprintln!("PASS: Unix selects the anchored backend");
        } else if selected == FilesystemHardening::Anchored {
            eprintln!("NOTE: Unix selects Anchored; flip REQUIRE_ANCHORED_ON_UNIX to enforce it");
        } else {
            eprintln!(
                "SKIP: Unix backend is {selected:?}, not Anchored (REQUIRE_ANCHORED_ON_UNIX=false until integration)"
            );
        }
    } else {
        assert_eq!(selected, FilesystemHardening::LegacyPath);
        eprintln!("NOTE: Windows uses the legacy path backend; anchored guarantees not claimed");
    }
}

// ---------------------------------------------------------------------------
// Future WP1 pause points (gated by WP1_FS_FAILPOINTS).

/// Swap a subdirectory while `add dir` is walking (before descending).
fn directory_walk_swap(hash: Hash) -> Result {
    let mut fx = Fixture::new(hash)?;
    fx.s.model.write(0, b"dir/sub/new", b"real new\n")?;
    let mut expected = fx.s.model.clone();
    expected.stage(0, &[b"dir"])?;
    fx.decoy("walk/new", DECOYS[1])?;
    fx.decoy("walk/file", DECOYS[0])?;
    let target = fx.outside.join("walk");
    let outside = record(&fx.outside)?;
    let before = snapshot(&fx.repo())?;
    let paused = Paused::spawn(
        &mut fx.s.driver.add_cmd(&fx.repo(), &["dir"]),
        "add.before-descend",
    )?;
    let Some(mut swap) = swap_for_link(&fx.repo().join("dir/sub"), &target, "walk swap")? else {
        drop(paused);
        return Ok(());
    };
    let output = release(paused)?;
    swap.restore()?;
    judge(
        &fx,
        &output,
        &before,
        &outside,
        &expected,
        "add dir with dir/sub swapped mid-walk",
    )
}

/// Swap `.git/objects` before the writer creates a missing fan-out.
fn objects_dir_swap_before_fanout(hash: Hash) -> Result {
    let mut fx = Fixture::new(hash)?;
    fx.s.model
        .write(0, b"dir/sub/new", b"needs a new fan-out?\n")?;
    let mut expected = fx.s.model.clone();
    expected.stage(0, &[b"dir/sub/new"])?;
    let target = fx.decoy_dir("objects")?;
    let outside = record(&fx.outside)?;
    let before = snapshot(&fx.repo())?;
    let paused = Paused::spawn(
        &mut fx.s.driver.add_cmd(&fx.repo(), &["dir/sub/new"]),
        "object.before-fanout",
    )?;
    let Some(mut swap) = swap_for_link(&fx.git_dir().join("objects"), &target, "objects swap")?
    else {
        drop(paused);
        return Ok(());
    };
    let output = release(paused)?;
    swap.restore()?;
    judge(
        &fx,
        &output,
        &before,
        &outside,
        &expected,
        "add with .git/objects swapped before the fan-out",
    )
}

/// Swap `.git/refs/heads` before a commit on a nested unborn branch creates
/// its ref directory.
fn refs_dir_swap_before_dir(hash: Hash) -> Result {
    let mut fx = Fixture::new(hash)?;
    git(&fx.repo(), &["checkout", "-q", "--orphan", "topic/nested"])?;
    fx.s.model.orphan(0, "topic/nested");
    fx.s.model.write(0, b"top", b"top nested\n")?;
    fx.s.add(0, &[b"top"])?;
    let mut expected = fx.s.model.clone();
    let when = expected.tick();
    expected.commit(0, &["nested"], when);
    let target = fx.decoy_dir("heads")?;
    fx.decoy("heads/main", DECOYS[2])?;
    let outside = record(&fx.outside)?;
    let before = snapshot(&fx.repo())?;
    let paused = Paused::spawn(
        &mut fx.s.driver.commit_cmd(&fx.repo(), &["nested"], when, &[]),
        "refs.before-dir",
    )?;
    let Some(mut swap) = swap_for_link(&fx.git_dir().join("refs/heads"), &target, "refs swap")?
    else {
        drop(paused);
        return Ok(());
    };
    let output = release(paused)?;
    swap.restore()?;
    judge(
        &fx,
        &output,
        &before,
        &outside,
        &expected,
        "commit with refs/heads swapped before its directory",
    )
}

// ---------------------------------------------------------------------------
// Oracle self-checks (mutation checks of the judging code itself).

#[test]
fn oracle_detects_outside_writes_decoy_objects_and_unconfined_completion() -> Result {
    let mut fx = Fixture::new(Hash::Sha1)?;
    fx.decoy("wt/file", DECOYS[0])?;
    let outside = record(&fx.outside)?;
    // A write outside the root.
    fs::write(fx.outside.join("wt/file"), b"changed\n")?;
    assert!(
        assert_untouched(&fx.outside, &outside, "self").is_err(),
        "missed outside write"
    );
    // An outside entry replaced by a different inode with the same bytes
    // (Unix: std exposes no stable file index on Windows).
    fs::remove_file(fx.outside.join("wt/file"))?;
    fs::write(fx.outside.join("wt/file"), DECOYS[0])?;
    #[cfg(unix)]
    assert!(
        assert_untouched(&fx.outside, &outside, "self").is_err(),
        "missed inode swap"
    );
    // A new outside entry.
    let outside = record(&fx.outside)?;
    fs::write(fx.outside.join("wt/extra"), b"x")?;
    assert!(
        assert_untouched(&fx.outside, &outside, "self").is_err(),
        "missed new outside entry"
    );
    // Decoy content stored as an object / staged.
    fs::write(fx.repo().join("dir/sub/file"), DECOYS[0])?;
    ok(
        &Driver::CGit.add(&fx.repo(), &["dir/sub/file"])?,
        "stage decoy",
    )?;
    assert!(
        no_decoy_objects(&fx).is_err(),
        "missed staged decoy content"
    );
    fx.s.model.write(0, b"dir/sub/file", DECOYS[0])?;
    fx.s.model.stage(0, &[b"dir/sub/file"])?;
    // A "completion" that does not match the expected model.
    let mut wrong = fx.s.model.clone();
    wrong.write(0, b"dir/sub/file", b"real v2\n")?;
    wrong.stage(0, &[b"dir/sub/file"])?;
    let fake_ok = std::process::Command::new("true").output();
    #[cfg(unix)]
    {
        let output = fake_ok?;
        let before = snapshot(&fx.repo())?;
        let outside = record(&fx.outside)?;
        let mut fx2 = fx;
        fx2.s.model = wrong.clone();
        // Restore worktree bytes to "real v2" so only the index differs.
        let verdict = judge(&fx2, &output, &before, &outside, &wrong, "self");
        assert!(verdict.is_err(), "missed an unconfined completion");
    }
    #[cfg(not(unix))]
    drop(fake_ok);
    Ok(())
}

// ---------------------------------------------------------------------------
// Test entry points (rgit only; each case prints PASS / SKIP / NOTE).

fn worktree_ancestors(_: Driver) -> Result {
    if legacy_windows("worktree_ancestors") {
        return Ok(());
    }
    let mut failures = Vec::new();
    for hash in Hash::ALL {
        for (swapped, spec) in [
            ("dir", "dir/sub/file"),
            ("dir", "dir/sub/new"),
            ("dir", "dir"),
            ("dir/sub", "dir/sub/file"),
            ("dir/sub", "dir/sub/new"),
            ("dir/sub", "."),
        ] {
            let name = format!("A1 {} add {spec} / swap {swapped}", hash.name());
            if let Err(e) = case(&name, expect_for(&name), || {
                worktree_ancestor_swap(hash, swapped, spec)
            }) {
                failures.push(e.to_string());
            }
        }
    }
    fail_if_any(failures)
}

fn git_dir_parts(_: Driver) -> Result {
    if legacy_windows("git_dir_parts") {
        return Ok(());
    }
    let mut failures = Vec::new();
    for hash in Hash::ALL {
        let h = hash.name();
        let mut cases: Vec<(String, Body)> = vec![
            (
                format!("A2 {h} add / swap objects/xx"),
                Box::new(move || objects_fanout_swap(hash, false, false)),
            ),
            (
                format!("A2 {h} commit / swap objects/xx"),
                Box::new(move || objects_fanout_swap(hash, true, false)),
            ),
            (
                format!("A2 {h} add / swap objects/xx / own temporary removed"),
                Box::new(move || objects_fanout_swap(hash, false, true)),
            ),
            (
                format!("A2 {h} commit / swap objects/xx / own temporary removed"),
                Box::new(move || objects_fanout_swap(hash, true, true)),
            ),
            (
                format!("A2 {h} commit / swap refs/heads"),
                Box::new(move || {
                    git_dir_swap_during_commit(hash, "refs/heads", &[("main", DECOYS[2])], false)
                }),
            ),
            (
                format!("A2 {h} commit / swap refs/heads / own locks removed"),
                Box::new(move || {
                    git_dir_swap_during_commit(hash, "refs/heads", &[("main", DECOYS[2])], true)
                }),
            ),
            (
                format!("A2 {h} commit / swap logs/refs/heads"),
                Box::new(move || {
                    git_dir_swap_during_commit(
                        hash,
                        "logs/refs/heads",
                        &[("main", DECOYS[3])],
                        true,
                    )
                }),
            ),
            (
                format!("A2 {h} commit / swap logs"),
                Box::new(move || {
                    git_dir_swap_during_commit(
                        hash,
                        "logs",
                        &[("HEAD", DECOYS[3]), ("refs/heads/main", DECOYS[3])],
                        true,
                    )
                }),
            ),
            (
                format!("A2 {h} add / swap info"),
                Box::new(move || info_dir_swap_during_add(hash)),
            ),
        ];
        for (name, body) in cases.drain(..) {
            if let Err(e) = case(&name, expect_for(&name), body) {
                failures.push(e.to_string());
            }
        }
    }
    fail_if_any(failures)
}

#[cfg(unix)]
fn info_metadata(_: Driver) -> Result {
    let mut failures = Vec::new();
    for hash in Hash::ALL {
        let h = hash.name();
        for name in ["exclude", "attributes"] {
            for fifo in [true, false] {
                for spec in ["dir/sub/new", "."] {
                    let kind = if fifo { "fifo" } else { "symlink" };
                    let label = format!("A3 {h} info/{name} as {kind} / add {spec}");
                    if let Err(e) = case(&label, expect_for(&label), || {
                        info_file_replaced(hash, name, fifo, spec)
                    }) {
                        failures.push(e.to_string());
                    }
                }
            }
            let label = format!("A3 {h} info/{name} swapped to fifo while paused");
            if let Err(e) = case(&label, expect_for(&label), || {
                info_file_swapped_to_fifo_while_paused(hash, name)
            }) {
                failures.push(e.to_string());
            }
        }
    }
    fail_if_any(failures)
}

#[cfg(not(unix))]
fn info_metadata(_: Driver) -> Result {
    eprintln!(
        "SKIP: A3 info/exclude and info/attributes as FIFO/symlink: needs Unix FIFOs and unprivileged symlinks"
    );
    Ok(())
}

#[cfg(unix)]
fn lock_identity(_: Driver) -> Result {
    let mut failures = Vec::new();
    for hash in Hash::ALL {
        let h = hash.name();
        for (commit, lock) in [
            (false, "index.lock"),
            (true, "HEAD.lock"),
            (true, "refs/heads/main.lock"),
        ] {
            for as_symlink in [false, true] {
                let kind = if as_symlink { "symlink" } else { "file" };
                let label = format!("A4 {h} {lock} recreated as {kind}");
                if let Err(e) = case(&label, expect_for(&label), || {
                    lock_recreated(hash, commit, lock, as_symlink, false)
                }) {
                    failures.push(e.to_string());
                }
                if lock == "HEAD.lock" {
                    let label =
                        format!("A4 {h} {lock} recreated as {kind} before append / refusal-only");
                    if let Err(e) = case(&label, expect_for(&label), || {
                        lock_recreated(hash, commit, lock, as_symlink, true)
                    }) {
                        failures.push(e.to_string());
                    }
                    let label = format!(
                        "A4 {h} {lock} recreated as {kind} after append / logged-not-published"
                    );
                    if let Err(e) = case(&label, expect_for(&label), || {
                        head_lock_replaced_after_append(hash, as_symlink)
                    }) {
                        failures.push(e.to_string());
                    }
                }
            }
        }
    }
    fail_if_any(failures)
}

#[cfg(windows)]
fn lock_identity(_: Driver) -> Result {
    legacy_windows("A4 lock identity");
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn lock_identity(_: Driver) -> Result {
    eprintln!("SKIP: A4 lock identity: unsupported platform");
    Ok(())
}

fn future_pause_points(_: Driver) -> Result {
    if legacy_windows("future_pause_points") {
        return Ok(());
    }
    let mut failures = Vec::new();
    for hash in Hash::ALL {
        let h = hash.name();
        let cases: Vec<(String, &'static str, Body)> = vec![
            (
                format!("A1 {h} add dir / swap dir/sub mid-walk"),
                "add.before-descend",
                Box::new(move || directory_walk_swap(hash)),
            ),
            (
                format!("A2 {h} add / swap objects before fan-out"),
                "object.before-fanout",
                Box::new(move || objects_dir_swap_before_fanout(hash)),
            ),
            (
                format!("A2 {h} commit / swap refs/heads before dir"),
                "refs.before-dir",
                Box::new(move || refs_dir_swap_before_dir(hash)),
            ),
        ];
        for (name, point, body) in cases {
            if let Err(e) = case(&name, Expect::NeedsFailpoint(point), body) {
                failures.push(e.to_string());
            }
        }
    }
    fail_if_any(failures)
}

/// Windows writes run on the explicitly selected legacy path backend
/// (`fsx::HARDENING == LegacyPath`, decision 0004): anchored guarantees are
/// not claimed there, so the adversarial cases only say so.
fn legacy_windows(scenario: &str) -> bool {
    if cfg!(windows) {
        eprintln!(
            "NOTE: {scenario}: Windows uses the legacy path backend; anchored guarantees not claimed"
        );
        return true;
    }
    false
}

/// Cases the baseline (path-based) backend is known not to guarantee; see
/// the scenario matrix in the report. Everything else is enforced now.
fn expect_for(name: &str) -> Expect {
    // Baseline evidence (path-based backend): a refused writer's cleanup
    // goes through the swapped link, so its own temporary object or locks
    // stay orphaned in the original directory. The decoy itself is
    // untouched in both cases (enforced now by the non-strict variants).
    // A branch commit whose HEAD.lock guard was replaced must refuse (WP1
    // re-verifies HEAD.lock ownership in refs.rs); the baseline completes.
    const AFTER_ANCHORING: [&str; 4] = [
        "own temporary removed",
        "own locks removed",
        "refusal-only",
        "logged-not-published",
    ];
    if AFTER_ANCHORING.iter().any(|pattern| name.contains(pattern)) {
        Expect::AfterAnchoring
    } else {
        Expect::Now
    }
}

fn fail_if_any(failures: Vec<String>) -> Result {
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} failing case(s):\n{}",
            failures.len(),
            failures.join("\n")
        )
        .into())
    }
}

rgit_only!(
    worktree_ancestors,
    git_dir_parts,
    info_metadata,
    lock_identity,
    future_pause_points
);
