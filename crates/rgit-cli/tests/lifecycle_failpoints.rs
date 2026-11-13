//! Interrupted writes (ADR 0003 failpoints): after an abort or injected error
//! at every crash boundary, a fresh C Git process must observe each published
//! file in its complete old or complete new state, no published name may
//! point at a missing object, fsck --strict must pass (dangling objects are
//! allowed), stale locks must refuse the next writer until removed, and the
//! retry must reach exactly the model's new state.

#[allow(dead_code)]
mod lifecycle_support;

use std::fs;
use std::path::PathBuf;

use lifecycle_support::Scenario;
use lifecycle_support::crash::{
    Published, assert_unpublished_untouched, classify_head, classify_index, expect_abort,
    expect_injected, with_failpoint,
};
use lifecycle_support::driver::{self, Driver, ok, ok_quiet, refused};
use lifecycle_support::model::{Hash, Head, Model};
use lifecycle_support::observe::{self, assert_state_with};
use lifecycle_support::proc::{
    Preserve, Result, assert_preserved, describe, git, git_text, run, sandbox, snapshot,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Abort,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    Branch,
    Unborn,
    Detached,
}

const ADD_POINTS: [&str; 5] = [
    "lock.acquired",
    "lock.before-rename",
    "lock.after-rename",
    "add.objects-written",
    "add.index-locked",
];

const COMMIT_POINTS: [&str; 6] = [
    "lock.acquired",
    "lock.before-rename",
    "lock.after-rename",
    "commit.objects-written",
    "commit.ref-locked",
    "ref.logged",
];

fn based(hash: Hash, shape: Shape) -> Result<Scenario> {
    let mut s = Scenario::init(Driver::Rgit, hash, "main")?;
    if shape != Shape::Unborn {
        s.model.write(0, b"a", b"a1\n")?;
        s.model.write(0, b"dir/b", b"b1\n")?;
        s.add(0, &[b"a", b"dir"])?;
        s.commit(0, &["base"])?;
    }
    if shape == Shape::Detached {
        git(&s.root(), &["checkout", "-q", "--detach"])?;
        s.model.detach(0)?;
    }
    Ok(s)
}

/// Collect every failing case so one run reports the whole matrix.
fn matrix(cases: Vec<(String, Result)>) -> Result {
    let failures: Vec<String> = cases
        .into_iter()
        .filter_map(|(name, result)| {
            eprintln!(
                "CASE {}: {name}",
                if result.is_ok() { "PASS" } else { "FAIL" }
            );
            result.err().map(|e| format!("{name}: {e}"))
        })
        .collect();
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

fn spec(point: &str, mode: Mode) -> String {
    match mode {
        Mode::Abort => point.to_owned(),
        Mode::Error => format!("{point}:error"),
    }
}

/// Stale-lock contract: every remaining lock refuses the same operation
/// (exit 128, diagnostic names the lock, nothing changes, lock kept); after
/// the operator removes them the operation can proceed.
fn stale_locks_refuse_then_clear(
    s: &Scenario,
    locks: &[PathBuf],
    preserve: Preserve,
    rerun: &mut dyn FnMut() -> Result<std::process::Output>,
) -> Result {
    if locks.is_empty() {
        return Ok(());
    }
    let before = snapshot(s.dir())?;
    let output = rerun()?;
    refused(&output, 128, "writer behind a stale lock")?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !locks.iter().any(|lock| {
        lock.file_name()
            .is_some_and(|name| stderr.contains(&*name.to_string_lossy()))
    }) {
        return Err(format!("diagnostic names none of {locks:?}: {}", describe(&output)).into());
    }
    assert_preserved(
        s.dir(),
        &before,
        &snapshot(s.dir())?,
        preserve,
        "stale lock refusal",
    )?;
    for lock in locks {
        fs::remove_file(lock)?;
    }
    Ok(())
}

fn published_paths(s: &Scenario, include_logs: bool) -> Result<Vec<PathBuf>> {
    let git_dir = s.root().join(".git");
    let mut paths = vec![
        git_dir.join("index"),
        git_dir.join("HEAD"),
        git_dir.join("refs/heads/main"),
        git_dir.join("packed-refs"),
    ];
    if include_logs {
        // A first append creates the log directories as well.
        for path in [
            "logs",
            "logs/refs",
            "logs/refs/heads",
            "logs/HEAD",
            "logs/refs/heads/main",
        ] {
            paths.push(git_dir.join(path));
        }
    }
    paths
        .into_iter()
        .map(|p| Ok(p.strip_prefix(s.dir())?.to_path_buf()))
        .collect()
}

fn every_ref_object_exists(s: &Scenario) -> Result {
    for (name, oid) in observe::branches(&s.root())? {
        git(&s.root(), &["cat-file", "-e", &oid])
            .map_err(|e| format!("branch {name} names a missing object: {e}"))?;
    }
    observe::fsck(&s.root())
}

fn add_case(hash: Hash, point: &str, mode: Mode) -> Result {
    let mut s = based(hash, Shape::Branch)?;
    s.model.write(0, b"a", b"a2 modified\n")?;
    s.model.write(0, b"dir/new", b"new\n")?;
    let old = s.model.clone();
    let mut new = s.model.clone();
    new.stage(0, &[b"a", b"dir"])?;
    let before = snapshot(s.dir())?;
    let mut command = Driver::Rgit.add_cmd(&s.root(), &["a", "dir"]);
    with_failpoint(&mut command, &spec(point, mode));
    let output = run(&mut command)?;
    match mode {
        Mode::Abort => expect_abort(&output, point)?,
        Mode::Error => expect_injected(&output, point)?,
    }

    // Fresh-process classification of each published file.
    let index_state = classify_index(&old, &new, 0)?;
    let expected = if point == "lock.after-rename" {
        Published::New
    } else {
        Published::Old
    };
    if index_state != expected {
        return Err(format!("index is {index_state:?}, expected {expected:?}").into());
    }
    if classify_head(&old, &new, 0)? != Published::Old {
        return Err("add moved HEAD".into());
    }
    assert_unpublished_untouched(
        s.dir(),
        &before,
        &snapshot(s.dir())?,
        &published_paths(&s, false)?,
        "add",
    )?;
    every_ref_object_exists(&s)?;

    let git_dir = s.root().join(".git");
    let locks = observe::locks(&git_dir)?;
    let expected_locks = match mode {
        Mode::Abort
            if matches!(
                point,
                "lock.acquired" | "lock.before-rename" | "add.index-locked"
            ) =>
        {
            vec![git_dir.join("index.lock")]
        }
        _ => vec![],
    };
    if locks != expected_locks {
        return Err(format!("locks {locks:?}, expected {expected_locks:?}").into());
    }
    stale_locks_refuse_then_clear(&s, &locks, Preserve::AllowNewObjects, &mut || {
        Driver::Rgit.add(&s.root(), &["a", "dir"])
    })?;

    // Retry (idempotent if already published) reaches the new state.
    let output = Driver::Rgit.add(&s.root(), &["a", "dir"])?;
    ok(&output, "retry add")?;
    s.model = new;
    assert_state_with(&s.model, false)?;
    Ok(())
}

fn commit_case(hash: Hash, shape: Shape, point: &str, mode: Mode) -> Result {
    let mut s = based(hash, shape)?;
    s.model.write(0, b"a", b"committed change\n")?;
    s.model.write(0, b"dir/c", b"c\n")?;
    s.add(0, &[b"a", b"dir"])?;
    let old = s.model.clone();
    let mut new = s.model.clone();
    let when = new.tick();
    let oid = new.commit(0, &["interrupted"], when);
    let foreign = Foreign::plant(&s.root().join(".git"))?;
    let before = snapshot(s.dir())?;
    let mut command = Driver::Rgit.commit_cmd(&s.root(), &["interrupted"], when, &[]);
    with_failpoint(&mut command, &spec(point, mode));
    let output = run(&mut command)?;
    match mode {
        Mode::Abort => expect_abort(&output, point)?,
        Mode::Error => expect_injected(&output, point)?,
    }

    // Never infer "unchanged" from the failure: classify every published
    // file from a fresh process, then hold it to ADR 0003's table ("Reference
    // and reflog ordering") for this point.
    if classify_index(&old, &new, 0)? != Published::Old {
        return Err("commit changed index entries".into());
    }
    let head_state = classify_head(&old, &new, 0)?;
    let published = point == "lock.after-rename";
    let logged = matches!(
        point,
        "ref.logged" | "lock.before-rename" | "lock.after-rename"
    );
    let expected_state = if published {
        Published::New
    } else {
        Published::Old
    };
    if head_state != expected_state {
        return Err(format!(
            "ref is {head_state:?} after {point} {mode:?}, expected {expected_state:?} ({oid})"
        )
        .into());
    }
    if mode == Mode::Error {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let wanted: &[&str] = match point {
            "ref.logged" | "lock.before-rename" => &["was not updated", "reflog already records"],
            "lock.after-rename" => &["was updated", "durability is unknown"],
            _ => &[],
        };
        if wanted.iter().any(|w| !stderr.contains(w)) {
            return Err(format!(
                "{point}:error diagnostic must contain {wanted:?}: {}",
                describe(&output)
            )
            .into());
        }
    }
    // Reflogs: exactly one C Git line (old -> new, model committer, C Git
    // message) in the branch log and HEAD's log once the append happened
    // (only HEAD's when detached), otherwise byte-identical.
    let line = reflog_line(&old, hash, shape, &oid, when, "interrupted");
    let appended: Vec<&str> = if logged { vec![line.as_str()] } else { vec![] };
    let observed = snapshot(s.dir())?;
    check_logs(
        &s,
        shape,
        &before,
        &observed,
        &appended,
        &format!("{point} {mode:?}"),
    )?;
    let git_rel = s.root().join(".git").strip_prefix(s.dir())?.to_path_buf();
    if shape != Shape::Detached
        && observed.get(&git_rel.join("HEAD")) != before.get(&git_rel.join("HEAD"))
    {
        return Err("HEAD target changed during a branch commit".into());
    }
    foreign.verify()?;
    assert_unpublished_untouched(
        s.dir(),
        &before,
        &observed,
        &published_paths(&s, logged || published)?,
        "commit",
    )?;
    if !published {
        let after = snapshot(s.dir())?;
        for path in published_paths(&s, false)?
            .iter()
            .filter(|p| !p.ends_with("index"))
        {
            if before.get(path) != after.get(path) {
                return Err(format!("{} changed although the ref is old", path.display()).into());
            }
        }
    }
    every_ref_object_exists(&s)?;

    // Locks: an abort leaves exactly the locks held at that point (HEAD.lock
    // is commit's first lock; a branch's own lock follows); a handled error
    // releases everything.
    let git_dir = s.root().join(".git");
    let head_lock = git_dir.join("HEAD.lock");
    let branch_lock = git_dir.join("refs/heads/main.lock");
    let expected_locks: Vec<PathBuf> = match (mode, point, shape) {
        (Mode::Error, _, _) | (_, "commit.objects-written", _) => vec![],
        (_, "lock.acquired", _) => vec![head_lock.clone()],
        (_, "lock.after-rename", Shape::Detached) => vec![],
        (_, "lock.after-rename", _) => vec![head_lock.clone()],
        (_, _, Shape::Detached) => vec![head_lock.clone()],
        _ => vec![head_lock.clone(), branch_lock.clone()],
    };
    let locks: Vec<PathBuf> = observe::locks(&git_dir)?
        .into_iter()
        .filter(|l| *l != foreign.path)
        .collect();
    if locks != expected_locks {
        return Err(format!(
            "{point} {mode:?} {shape:?}: locks {locks:?}, expected {expected_locks:?}"
        )
        .into());
    }
    if !locks.is_empty() {
        eprintln!(
            "NOTE: {hash:?} {shape:?} {point} {mode:?}: ref {head_state:?}, orphan locks {locks:?}"
        );
    }

    if !published {
        // A HEAD-contending C Git writer is refused while HEAD.lock is held.
        if locks.contains(&head_lock) {
            c_git_commit_refused(&s, when)?;
        }
        let root = s.root();
        stale_locks_refuse_then_clear(&s, &locks, Preserve::AllowNewObjects, &mut || {
            Driver::Rgit.commit(&root, &["interrupted"], when)
        })?;
        ok_quiet(
            &Driver::Rgit.commit(&s.root(), &["interrupted"], when)?,
            "retry commit",
        )?;
        if let Head::Detached(_) = &new.checkouts[0].head {
            assert_eq!(observe::symbolic_head(&s.root())?, None);
        }
        // The retry logs from the ref's real (old) value. After a pre-rename
        // append the log therefore holds the unpublished line and then the
        // retry's line, both starting at the old value (C Git behaviour).
        let lines: Vec<&str> = if logged {
            vec![&line, &line]
        } else {
            vec![&line]
        };
        check_logs(
            &s,
            shape,
            &before,
            &snapshot(s.dir())?,
            &lines,
            &format!("{point} {mode:?} retry"),
        )?;
        s.model = new;
        foreign.verify_and_withdraw()?;
        return assert_state_with(&s.model, logged);
    }

    // Published (lock.after-rename) with its reflog line. A branch commit
    // aborted here leaves only HEAD.lock (held across the branch rename);
    // a later rgit commit and a HEAD-contending C Git commit are refused
    // until the operator removes exactly that orphan.
    s.model = new;
    follow_up_commit_logs_from(&mut s, shape, &oid, &locks, Some(&foreign))
}

/// C Git's reflog line for a commit made by the model's committer.
fn reflog_line(
    old: &Model,
    hash: Hash,
    shape: Shape,
    new_oid: &str,
    when: i64,
    subject: &str,
) -> String {
    let old_oid = old
        .head_commit(0)
        .unwrap_or_else(|| "0".repeat(hash.digest(b"").len() * 2));
    let action = if shape == Shape::Unborn {
        "commit (initial)"
    } else {
        "commit"
    };
    format!(
        "{old_oid} {new_oid} {} <{}> {when} +0000\t{action}: {subject}\n",
        lifecycle_support::proc::NAME,
        lifecycle_support::proc::EMAIL
    )
}

/// The branch log and HEAD's log in `after` must equal their bytes in
/// `before` followed by exactly `lines` (only HEAD's log changes when
/// detached; the other log must be byte-identical).
fn check_logs(
    s: &Scenario,
    shape: Shape,
    before: &std::collections::BTreeMap<PathBuf, lifecycle_support::proc::Snap>,
    after: &std::collections::BTreeMap<PathBuf, lifecycle_support::proc::Snap>,
    lines: &[&str],
    context: &str,
) -> Result {
    check_log_lines(s, before, after, "logs/HEAD", lines, context)?;
    let branch_lines: &[&str] = if shape == Shape::Detached { &[] } else { lines };
    check_log_lines(
        s,
        before,
        after,
        "logs/refs/heads/main",
        branch_lines,
        context,
    )
}

fn check_log_lines(
    s: &Scenario,
    before: &std::collections::BTreeMap<PathBuf, lifecycle_support::proc::Snap>,
    after: &std::collections::BTreeMap<PathBuf, lifecycle_support::proc::Snap>,
    log: &str,
    lines: &[&str],
    context: &str,
) -> Result {
    use lifecycle_support::proc::Snap;
    let rel = s
        .root()
        .join(".git")
        .join(log)
        .strip_prefix(s.dir())?
        .to_path_buf();
    let bytes = |snap: &std::collections::BTreeMap<PathBuf, Snap>| -> Result<Option<Vec<u8>>> {
        match snap.get(&rel) {
            Some(Snap::File { bytes, .. }) => Ok(Some(bytes.clone())),
            None => Ok(None),
            Some(other) => Err(format!("{log} is {other:?}").into()),
        }
    };
    let old = bytes(before)?;
    let expected = if lines.is_empty() {
        old
    } else {
        let mut data = old.unwrap_or_default();
        for line in lines {
            data.extend_from_slice(line.as_bytes());
        }
        Some(data)
    };
    let actual = bytes(after)?;
    if actual != expected {
        return Err(format!(
            "{context}: {log} expected {:?}, found {:?}",
            expected.as_deref().map(String::from_utf8_lossy),
            actual.as_deref().map(String::from_utf8_lossy)
        )
        .into());
    }
    Ok(())
}

/// A lock planted by the fixture on behalf of some other (live) writer. No
/// rgit operation or recovery step may touch it.
struct Foreign {
    path: PathBuf,
    bytes: Vec<u8>,
    inode: u64,
}

#[cfg(unix)]
fn inode(path: &std::path::Path) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(fs::symlink_metadata(path)?.ino())
}

#[cfg(not(unix))]
fn inode(_path: &std::path::Path) -> Result<u64> {
    Ok(0)
}

impl Foreign {
    fn plant(git_dir: &std::path::Path) -> Result<Self> {
        let path = git_dir.join("refs/tags/foreign-writer.lock");
        fs::create_dir_all(path.parent().ok_or("parent")?)?;
        let bytes = b"held by another writer\n".to_vec();
        fs::write(&path, &bytes)?;
        Ok(Self {
            inode: inode(&path)?,
            path,
            bytes,
        })
    }

    fn verify(&self) -> Result {
        if fs::read(&self.path)? != self.bytes || inode(&self.path)? != self.inode {
            return Err(format!(
                "foreign lock {} was modified or replaced",
                self.path.display()
            )
            .into());
        }
        Ok(())
    }

    /// The fixture (not rgit) withdraws its own lock before the final
    /// no-leftover-lock state check.
    fn verify_and_withdraw(&self) -> Result {
        self.verify()?;
        fs::remove_file(&self.path)?;
        Ok(())
    }
}

/// While the orphaned HEAD.lock exists, a C Git operation that must contend
/// for HEAD is refused as well. Exact command (staged change present, so it
/// really tries to move HEAD's branch):
///   git commit -q -m c-git-contender   (GIT_* identity/date from the model)
/// Only this HEAD-contending operation is asserted; unrelated ref writers
/// are not claimed to be blocked. Afterwards every pre-existing byte (refs,
/// HEAD, index, logs, the stale lock, existing objects, worktree) is
/// unchanged; only new, valid, unreferenced loose objects may appear.
fn c_git_commit_refused(s: &Scenario, when: i64) -> Result {
    let before = snapshot(s.dir())?;
    let index_before = observe::index(&s.root())?;
    let mut command = lifecycle_support::proc::git_cmd(&s.root());
    command.args(["commit", "-q", "-m", "c-git-contender"]);
    lifecycle_support::proc::identity(&mut command, when);
    let output = run(&mut command)?;
    if output.status.success() || !String::from_utf8_lossy(&output.stderr).contains("HEAD.lock") {
        return Err(format!(
            "`git commit` not refused by the stale HEAD.lock: {}",
            describe(&output)
        )
        .into());
    }
    // C Git's own refused commit rewrites the index cache-tree and its
    // COMMIT_EDITMSG scratch file (C Git behaviour, also modelled by
    // Driver::index_bytes_stable_on_refusal). The index must still verify
    // and hold exactly the same entries; every other byte is exact.
    let git_rel = s.root().join(".git").strip_prefix(s.dir())?.to_path_buf();
    let scratch = [git_rel.join("index"), git_rel.join("COMMIT_EDITMSG")];
    let strip = |mut snap: std::collections::BTreeMap<PathBuf, lifecycle_support::proc::Snap>| {
        for path in &scratch {
            snap.remove(path);
        }
        snap
    };
    let added = assert_preserved(
        s.dir(),
        &strip(before),
        &strip(snapshot(s.dir())?),
        Preserve::AllowNewObjects,
        "C Git commit behind HEAD.lock",
    )?;
    if observe::index(&s.root())? != index_before {
        return Err("C Git's refused commit changed index entries".into());
    }
    eprintln!(
        "NOTE: `git commit` refused by stale HEAD.lock (exit {:?}); {} new unreferenced object(s)",
        output.status.code(),
        added.iter().filter(|p| p.components().count() > 4).count()
    );
    Ok(())
}

fn reflog_path(s: &Scenario, shape: Shape) -> PathBuf {
    match shape {
        Shape::Detached => s.root().join(".git/logs/HEAD"),
        _ => s.root().join(".git/logs/refs/heads/main"),
    }
}

/// Commit a further change with rgit (after any orphan locks are shown to
/// refuse it and are removed) and require its reflog line to chain from
/// `published`.
fn follow_up_commit_logs_from(
    s: &mut Scenario,
    shape: Shape,
    published: &str,
    stale_locks: &[PathBuf],
    foreign: Option<&Foreign>,
) -> Result {
    s.model
        .write(0, b"follow-up", b"after partial publication\n")?;
    s.add(0, &[b"follow-up"])?;
    let when = s.model.tick();
    let root = s.root();
    if !stale_locks.is_empty() {
        c_git_commit_refused(s, when)?;
    }
    // The refused follow-up leaves every existing byte (refs, HEAD, index,
    // logs, the stale lock, existing objects) identical. It may add only new
    // unreferenced loose objects: ADR 0003 writes objects before taking the
    // ref lock (`commit.objects-written` precedes `commit.ref-locked`).
    stale_locks_refuse_then_clear(s, stale_locks, Preserve::AllowNewObjects, &mut || {
        Driver::Rgit.commit(&root, &["follow-up"], when)
    })?;
    ok_quiet(
        &Driver::Rgit.commit(&s.root(), &["follow-up"], when)?,
        "follow-up commit",
    )?;
    let next = s.model.commit(0, &["follow-up"], when);
    if let Some(foreign) = foreign {
        foreign.verify_and_withdraw()?;
    }
    // The published commit's line is present, so the chain is intact.
    assert_state_with(&s.model, false)?;
    let text = fs::read_to_string(reflog_path(s, shape))?;
    let last = text.lines().last().unwrap_or_default();
    if !last.starts_with(&format!("{published} {next} ")) {
        return Err(
            format!("follow-up reflog line {last:?} should be {published} -> {next}").into(),
        );
    }
    Ok(())
}

/// Restores a file's permissions however the test exits.
#[cfg(unix)]
struct ReadOnly(PathBuf);

#[cfg(unix)]
impl ReadOnly {
    fn new(path: PathBuf) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444))?;
        Ok(Self(path))
    }
}

#[cfg(unix)]
impl Drop for ReadOnly {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o644));
    }
}

/// Reflog append failure (an existing log made read-only): the append comes
/// before the rename, so the reference is NOT updated; exit 128 with "the
/// reference was not updated", no lock left, fsck clean. Fixtures:
/// - branch, read-only branch log: no log gains a line;
/// - branch, read-only HEAD log: the branch log already gained the line
///   (partially logged), HEAD's log did not;
/// - detached, read-only HEAD log: no log gains a line.
///
/// After restoring permissions the retry commit succeeds and logs from the
/// real old value.
#[cfg(unix)]
fn reflog_append_failure_leaves_ref_unchanged(driver: Driver) -> Result {
    if driver::lacks(
        driver,
        &["commit"],
        "reflog_append_failure_leaves_ref_unchanged",
    ) {
        return Ok(());
    }
    for hash in Hash::ALL {
        for (shape, read_only, partial) in [
            (Shape::Branch, "logs/refs/heads/main", false),
            (Shape::Branch, "logs/HEAD", true),
            (Shape::Detached, "logs/HEAD", false),
        ] {
            let mut s = based(hash, shape)?;
            let log = s.root().join(".git").join(read_only);
            if !log.is_file() {
                return Err(format!(
                    "{hash:?} {shape:?}: no reflog at {} after base commit",
                    log.display()
                )
                .into());
            }
            s.model.write(0, b"a", b"not published\n")?;
            s.add(0, &[b"a"])?;
            let old = s.model.clone();
            let mut new = s.model.clone();
            let when = new.tick();
            let oid = new.commit(0, &["partial"], when);
            let line = reflog_line(&old, hash, shape, &oid, when, "partial");
            let before = snapshot(s.dir())?;
            let guard = ReadOnly::new(log.clone())?;
            let output = Driver::Rgit.commit(&s.root(), &["partial"], when)?;
            let context = format!("{hash:?} {shape:?} read-only {read_only}");
            refused(&output, 128, &context)?;
            if !String::from_utf8_lossy(&output.stderr).contains("the reference was not updated") {
                return Err(format!(
                    "{context}: stderr must say the reference was not updated: {}",
                    describe(&output)
                )
                .into());
            }
            // Fresh-process classification, not inference from the exit code.
            if classify_head(&old, &new, 0)? != Published::Old {
                return Err(format!("{context}: ref changed").into());
            }
            if classify_index(&old, &new, 0)? != Published::Old {
                return Err(format!("{context}: index entries changed").into());
            }
            let after = snapshot(s.dir())?;
            if partial {
                check_log_lines(
                    &s,
                    &before,
                    &after,
                    "logs/refs/heads/main",
                    &[&line],
                    &context,
                )?;
                check_log_lines(&s, &before, &after, "logs/HEAD", &[], &context)?;
                eprintln!(
                    "NOTE: {context}: partially logged (branch log has the unpublished line)"
                );
            } else {
                check_logs(&s, shape, &before, &after, &[], &context)?;
            }
            let locks = observe::locks(&s.root().join(".git"))?;
            if !locks.is_empty() {
                return Err(format!("{context}: leftover locks {locks:?}").into());
            }
            every_ref_object_exists(&s)?;
            drop(guard);
            ok_quiet(
                &Driver::Rgit.commit(&s.root(), &["partial"], when)?,
                "retry after restoring permissions",
            )?;
            let retried = snapshot(s.dir())?;
            if partial {
                check_log_lines(
                    &s,
                    &before,
                    &retried,
                    "logs/refs/heads/main",
                    &[&line, &line],
                    &context,
                )?;
                check_log_lines(&s, &before, &retried, "logs/HEAD", &[&line], &context)?;
            } else {
                check_logs(&s, shape, &before, &retried, &[&line], &context)?;
            }
            s.model = new;
            assert_state_with(&s.model, partial)?;
        }
    }
    Ok(())
}

/// Entries of `snap` beneath `dir` (relative to the snapshot root).
fn subtree(
    snap: &std::collections::BTreeMap<PathBuf, lifecycle_support::proc::Snap>,
    dir: &std::path::Path,
) -> std::collections::BTreeMap<PathBuf, lifecycle_support::proc::Snap> {
    snap.iter()
        .filter(|(p, _)| p.starts_with(dir))
        .map(|(p, v)| (p.clone(), v.clone()))
        .collect()
}

fn without(
    snap: &std::collections::BTreeMap<PathBuf, lifecycle_support::proc::Snap>,
    dirs: &[&std::path::Path],
) -> std::collections::BTreeMap<PathBuf, lifecycle_support::proc::Snap> {
    snap.iter()
        .filter(|(p, _)| !dirs.iter().any(|d| p.starts_with(d)))
        .map(|(p, v)| (p.clone(), v.clone()))
        .collect()
}

/// `init.before-publish`: abort leaves exactly one private assembled
/// directory (`.git.rgit-init-*` in the worktree, or `.<name>.rgit-init-*`
/// beside a bare target) and nothing else; pre-existing user files stay
/// byte-identical; a later init neither consumes nor changes the orphan.
/// `:error` (handled) leaves no temporary directory.
fn init_case(hash: Hash, mode: Mode, bare: bool) -> Result {
    let point = "init.before-publish";
    let sandbox = sandbox()?;
    let name = if bare { "repo.git" } else { "repo" };
    let target = sandbox.path().join(name);
    let mut model = Model::new(hash, &target, "main");
    // User files, including names init's own filesystem probes use.
    if bare {
        fs::write(sandbox.path().join("user.txt"), b"user data\n")?;
    } else {
        for (path, bytes) in [
            (&b"config"[..], &b"user config file\n"[..]),
            (b"tXXXXXX", b"user probe-named file\n"),
            (b"\xc3\xa4", b"precomposed name\n"),
            (b"sub/file", b"nested user file\n"),
        ] {
            model.write(0, path, bytes)?;
        }
    }
    let mut args = vec!["-q", "-b", "main", "--object-format", hash.name()];
    if bare {
        args.push("--bare");
    }
    args.push(name);
    let before = snapshot(sandbox.path())?;
    let mut command = Driver::Rgit.init_cmd(sandbox.path(), &args);
    with_failpoint(&mut command, &spec(point, mode));
    let output = run(&mut command)?;
    match mode {
        Mode::Abort => expect_abort(&output, point)?,
        Mode::Error => expect_injected(&output, point)?,
    }
    let published = if bare {
        target.clone()
    } else {
        target.join(".git")
    };
    if bare && before.contains_key(std::path::Path::new(name)) {
        return Err("fixture: bare target pre-existed".into());
    }
    if fs::symlink_metadata(&published).is_ok() {
        return Err(format!("init published {} before {point}", published.display()).into());
    }
    let (orphan_parent, prefix) = if bare {
        (sandbox.path().to_path_buf(), format!(".{name}.rgit-init-"))
    } else {
        (target.clone(), ".git.rgit-init-".to_owned())
    };
    let orphans: Vec<PathBuf> = fs::read_dir(&orphan_parent)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
        })
        .collect();
    let expected_orphans = usize::from(mode == Mode::Abort);
    if orphans.len() != expected_orphans {
        return Err(format!(
            "{mode:?}: expected {expected_orphans} {prefix}* dir(s), found {orphans:?}"
        )
        .into());
    }
    let after_crash = snapshot(sandbox.path())?;
    let orphan_rel: Vec<PathBuf> = orphans
        .iter()
        .map(|o| Ok(o.strip_prefix(sandbox.path())?.to_path_buf()))
        .collect::<Result<_>>()?;
    let orphan_refs: Vec<&std::path::Path> = orphan_rel.iter().map(|p| p.as_path()).collect();
    // No other destination state changed: user files byte-identical, no
    // other new entry.
    assert_preserved(
        sandbox.path(),
        &before,
        &without(&after_crash, &orphan_refs),
        Preserve::Exact,
        "init crash",
    )?;
    let orphan_before = orphan_refs
        .first()
        .map_or_else(Default::default, |d| subtree(&after_crash, d));
    if let Some(orphan) = orphans.first() {
        // Observed assembled layout.
        if fs::read(orphan.join("HEAD"))? != b"ref: refs/heads/main\n" {
            return Err("orphan HEAD is not the assembled symbolic ref".into());
        }
        let config = fs::read_to_string(orphan.join("config"))?;
        let bare_line = format!("bare = {bare}");
        if !config.contains("[core]") || !config.contains(&bare_line) {
            return Err(format!("orphan config not as assembled: {config:?}").into());
        }
        for dir in ["objects", "refs", "refs/heads"] {
            if !orphan.join(dir).is_dir() {
                return Err(format!("orphan lacks {dir}/").into());
            }
        }
        eprintln!(
            "NOTE: init {} {mode:?} bare={bare}: orphan {} layout {:?}",
            hash.name(),
            orphan.display(),
            orphan_before.keys().collect::<Vec<_>>()
        );
    }
    // Nothing discovers a repository from the destination.
    if !bare {
        let rgit = run(Driver::Rgit
            .command(&target)
            .args(["rev-parse", "--absolute-git-dir"]))?;
        let cgit = run(lifecycle_support::proc::git_cmd(&target).args(["rev-parse", "--git-dir"]))?;
        if rgit.status.success() || cgit.status.success() {
            return Err(format!(
                "a repository was discovered after the crash: rgit {} / C Git {}",
                describe(&rgit),
                describe(&cgit)
            )
            .into());
        }
    }

    // A fresh init succeeds without consuming, deleting or changing the
    // orphan or any user file.
    ok_quiet(&Driver::Rgit.init(sandbox.path(), &args)?, "retry init")?;
    let after_retry = snapshot(sandbox.path())?;
    if orphan_refs
        .first()
        .map_or_else(Default::default, |d| subtree(&after_retry, d))
        != orphan_before
    {
        return Err("retry init consumed or changed the orphan".into());
    }
    let new_repo = published.strip_prefix(sandbox.path())?.to_path_buf();
    let mut excluded = orphan_refs.clone();
    excluded.push(&new_repo);
    assert_preserved(
        sandbox.path(),
        &before,
        &without(&after_retry, &excluded),
        Preserve::Exact,
        "retry init",
    )?;
    assert_eq!(
        git_text(&published, &["rev-parse", "--show-object-format"])?,
        hash.name()
    );
    assert_eq!(
        git_text(&published, &["config", "--get", "core.bare"])?,
        bare.to_string()
    );
    assert_eq!(
        observe::symbolic_head(&published)?.as_deref(),
        Some("refs/heads/main")
    );
    if bare {
        assert_eq!(
            git_text(&published, &["rev-parse", "--is-bare-repository"])?,
            "true"
        );
        assert!(observe::branches(&published)?.is_empty());
        assert!(observe::locks(&published)?.is_empty());
        observe::fsck(&published)?;
        return Ok(());
    }
    // Status equals the model plus only untracked entries from the orphan.
    let status = observe::status(&target)?;
    let expected = model.status(0);
    let mut extra = Vec::new();
    let mut matched = Vec::new();
    for record in status.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let text = String::from_utf8_lossy(record).into_owned();
        let under_orphan = orphans.iter().any(|o| {
            o.file_name()
                .is_some_and(|n| text.starts_with(&format!("?? {}/", n.to_string_lossy())))
        });
        if under_orphan {
            extra.push(text);
        } else {
            matched.extend_from_slice(record);
            matched.push(0);
        }
    }
    if matched != expected {
        return Err(format!(
            "status {:?} (orphan entries removed) != model {:?}",
            String::from_utf8_lossy(&matched),
            String::from_utf8_lossy(&expected)
        )
        .into());
    }
    if !extra.is_empty() {
        eprintln!(
            "NOTE: init {} {mode:?}: status adds orphan entries {extra:?}",
            hash.name()
        );
    }
    // Operator recovery: remove only the observed orphan; then the state is
    // exactly the model (HEAD, format, status, no refs, no locks, fsck).
    for orphan in &orphans {
        fs::remove_dir_all(orphan)?;
    }
    assert_state_with(&model, false)?;
    Ok(())
}

fn run_matrix(f: impl Fn(Hash, &str, Mode) -> Result, points: &[&str], label: &str) -> Result {
    let mut cases = Vec::new();
    for hash in Hash::ALL {
        for point in points {
            for mode in [Mode::Abort, Mode::Error] {
                cases.push((
                    format!("{label} {} {point} {mode:?}", hash.name()),
                    f(hash, point, mode),
                ));
            }
        }
    }
    matrix(cases)
}

fn interrupted_add(driver: Driver) -> Result {
    if driver::lacks(driver, &["add"], "interrupted_add") {
        return Ok(());
    }
    run_matrix(add_case, &ADD_POINTS, "add")
}

fn interrupted_commit_on_branch(driver: Driver) -> Result {
    if driver::lacks(driver, &["commit"], "interrupted_commit_on_branch") {
        return Ok(());
    }
    run_matrix(
        |h, p, m| commit_case(h, Shape::Branch, p, m),
        &COMMIT_POINTS,
        "commit/branch",
    )
}

fn interrupted_root_commit_on_unborn_branch(driver: Driver) -> Result {
    if driver::lacks(
        driver,
        &["commit"],
        "interrupted_root_commit_on_unborn_branch",
    ) {
        return Ok(());
    }
    run_matrix(
        |h, p, m| commit_case(h, Shape::Unborn, p, m),
        &COMMIT_POINTS,
        "commit/unborn",
    )
}

fn interrupted_commit_on_detached_head(driver: Driver) -> Result {
    if driver::lacks(driver, &["commit"], "interrupted_commit_on_detached_head") {
        return Ok(());
    }
    run_matrix(
        |h, p, m| commit_case(h, Shape::Detached, p, m),
        &COMMIT_POINTS,
        "commit/detached",
    )
}

fn interrupted_init(driver: Driver) -> Result {
    if driver::lacks(driver, &["init"], "interrupted_init") {
        return Ok(());
    }
    run_matrix(
        |h, _, m| init_case(h, m, false),
        &["init.before-publish"],
        "init",
    )?;
    run_matrix(
        |h, _, m| init_case(h, m, true),
        &["init.before-publish"],
        "init/bare",
    )
}

#[cfg(unix)]
mod partial {
    use super::*;
    rgit_only!(reflog_append_failure_leaves_ref_unchanged);
}

rgit_only!(
    interrupted_add,
    interrupted_commit_on_branch,
    interrupted_root_commit_on_unborn_branch,
    interrupted_commit_on_detached_head,
    interrupted_init,
);
