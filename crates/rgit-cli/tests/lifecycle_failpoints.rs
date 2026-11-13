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
use lifecycle_support::driver::{Driver, ok, ok_quiet, refused};
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

const COMMIT_POINTS: [&str; 5] = [
    "lock.acquired",
    "lock.before-rename",
    "lock.after-rename",
    "commit.objects-written",
    "commit.ref-locked",
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
        .filter_map(|(name, result)| result.err().map(|e| format!("{name}: {e}")))
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
        &before,
        &snapshot(s.dir())?,
        Preserve::AllowNewObjects,
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
        paths.push(git_dir.join("logs/HEAD"));
        paths.push(git_dir.join("logs/refs/heads/main"));
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
    stale_locks_refuse_then_clear(&s, &locks, &mut || {
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
    let before = snapshot(s.dir())?;
    let mut command = Driver::Rgit.commit_cmd(&s.root(), &["interrupted"], when, &[]);
    with_failpoint(&mut command, &spec(point, mode));
    let output = run(&mut command)?;
    match mode {
        Mode::Abort => expect_abort(&output, point)?,
        Mode::Error => expect_injected(&output, point)?,
    }

    if classify_index(&old, &new, 0)? != Published::Old {
        return Err("commit changed index entries".into());
    }
    let head_state = classify_head(&old, &new, 0)?;
    match point {
        // The first lock taken has not been renamed: nothing is published.
        "lock.acquired" | "lock.before-rename" | "commit.objects-written" | "commit.ref-locked"
            if head_state != Published::Old =>
        {
            return Err(format!("HEAD published at {point}").into());
        }
        _ => {}
    }
    if head_state == Published::New {
        eprintln!("NOTE: {hash:?} {shape:?} {point} {mode:?}: commit {oid} already published");
    }
    assert_unpublished_untouched(
        &before,
        &snapshot(s.dir())?,
        &published_paths(&s, head_state == Published::New)?,
        "commit",
    )?;
    if head_state == Published::Old {
        // Old state includes byte-identical HEAD, branch and reflogs.
        let all = published_paths(&s, true)?;
        let after = snapshot(s.dir())?;
        for path in all.iter().filter(|p| !p.ends_with("index")) {
            if before.get(path) != after.get(path) {
                return Err(format!("{} changed although HEAD is old", path.display()).into());
            }
        }
    }
    every_ref_object_exists(&s)?;

    let git_dir = s.root().join(".git");
    let ref_lock = match shape {
        Shape::Detached => git_dir.join("HEAD.lock"),
        _ => git_dir.join("refs/heads/main.lock"),
    };
    let allowed = [
        ref_lock.clone(),
        git_dir.join("HEAD.lock"),
        git_dir.join("index.lock"),
    ];
    let locks = observe::locks(&git_dir)?;
    if let Some(stray) = locks.iter().find(|l| !allowed.contains(l)) {
        return Err(format!("unexpected lock {}", stray.display()).into());
    }
    let must_hold = mode == Mode::Abort
        && matches!(
            point,
            "lock.acquired" | "lock.before-rename" | "commit.ref-locked"
        );
    if must_hold && locks.is_empty() {
        return Err(format!("abort at {point} left no stale lock").into());
    }
    if point == "commit.ref-locked" && mode == Mode::Abort && !locks.contains(&ref_lock) {
        return Err(format!(
            "commit.ref-locked left {locks:?}, not {}",
            ref_lock.display()
        )
        .into());
    }
    if (mode == Mode::Error || point == "commit.objects-written") && !locks.is_empty() {
        return Err(format!("{point} {mode:?} left locks {locks:?}").into());
    }
    let root = s.root();
    stale_locks_refuse_then_clear(&s, &locks, &mut || {
        Driver::Rgit.commit(&root, &["interrupted"], when)
    })?;

    if head_state == Published::Old {
        ok_quiet(
            &Driver::Rgit.commit(&s.root(), &["interrupted"], when)?,
            "retry commit",
        )?;
    }
    if let Head::Detached(_) = &new.checkouts[0].head {
        assert_eq!(observe::symbolic_head(&s.root())?, None);
    }
    s.model = new;
    assert_state_with(&s.model, head_state == Published::New)?;
    Ok(())
}

fn init_case(hash: Hash, mode: Mode) -> Result {
    let point = "init.before-publish";
    let sandbox = sandbox()?;
    let args = ["-q", "-b", "main", "--object-format", hash.name(), "repo"];
    let mut command = Driver::Rgit.init_cmd(sandbox.path(), &args);
    with_failpoint(&mut command, &spec(point, mode));
    let output = run(&mut command)?;
    match mode {
        Mode::Abort => expect_abort(&output, point)?,
        Mode::Error => expect_injected(&output, point)?,
    }
    let repo = sandbox.path().join("repo");
    if fs::symlink_metadata(repo.join(".git")).is_ok() {
        return Err("init published .git before init.before-publish".into());
    }
    let leftovers: Vec<_> = snapshot(sandbox.path())?
        .into_keys()
        .filter(|p| p.as_os_str() != ".lifecycle-home")
        .collect();
    if !leftovers.is_empty() {
        eprintln!("NOTE: init {hash:?} {mode:?} left private state: {leftovers:?}");
    }
    ok_quiet(&Driver::Rgit.init(sandbox.path(), &args)?, "retry init")?;
    let model = Model::new(hash, &repo, "main");
    // A clean, unborn repository: no refs, no locks, empty status (so no
    // leftover private directory is visible inside the worktree).
    assert_state_with(&model, false)?;
    assert_eq!(
        git_text(&repo, &["rev-parse", "--is-bare-repository"])?,
        "false"
    );
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

fn interrupted_add(_: Driver) -> Result {
    run_matrix(add_case, &ADD_POINTS, "add")
}

fn interrupted_commit_on_branch(_: Driver) -> Result {
    run_matrix(
        |h, p, m| commit_case(h, Shape::Branch, p, m),
        &COMMIT_POINTS,
        "commit/branch",
    )
}

fn interrupted_root_commit_on_unborn_branch(_: Driver) -> Result {
    run_matrix(
        |h, p, m| commit_case(h, Shape::Unborn, p, m),
        &COMMIT_POINTS,
        "commit/unborn",
    )
}

fn interrupted_commit_on_detached_head(_: Driver) -> Result {
    run_matrix(
        |h, p, m| commit_case(h, Shape::Detached, p, m),
        &COMMIT_POINTS,
        "commit/detached",
    )
}

fn interrupted_init(_: Driver) -> Result {
    run_matrix(|h, _, m| init_case(h, m), &["init.before-publish"], "init")
}

rgit_only!(
    interrupted_add,
    interrupted_commit_on_branch,
    interrupted_root_commit_on_unborn_branch,
    interrupted_commit_on_detached_head,
    interrupted_init,
);
