//! Failpoint plumbing (ADR 0003): abort/error/pause a child and classify the
//! repository a fresh process observes afterwards.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use super::model::Model;
use super::observe;
use super::proc::{
    DEADLINE, Preserve, Result, Snap, assert_preserved, describe, finish, spawn_piped,
};

pub fn with_failpoint(command: &mut Command, spec: &str) {
    command.env("RGIT_FAILPOINT", spec);
}

/// True if the child died by `abort()` (the failpoint's simulated crash).
pub fn died_by_abort(output: &Output) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        output.status.signal() == Some(6)
    }
    #[cfg(not(unix))]
    {
        !output.status.success() && output.status.code() != Some(128)
    }
}

pub fn expect_abort(output: &Output, point: &str) -> Result {
    if output.status.success() {
        return Err(format!("failpoint {point} never fired: {}", describe(output)).into());
    }
    if !died_by_abort(output) {
        return Err(format!("{point}: expected abort, got {}", describe(output)).into());
    }
    Ok(())
}

pub fn expect_injected(output: &Output, point: &str) -> Result {
    if output.status.success() {
        return Err(format!("failpoint {point}:error never fired: {}", describe(output)).into());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() != Some(128) || !stderr.contains(point) {
        return Err(format!(
            "{point}:error: expected exit 128 naming it: {}",
            describe(output)
        )
        .into());
    }
    Ok(())
}

/// A child paused at a failpoint, holding whatever that point holds.
pub struct Paused {
    child: Option<Child>,
    ready: PathBuf,
    release: PathBuf,
    _signals: tempfile::TempDir,
}

impl Paused {
    /// Spawn `command` with `<point>:pause` and wait (bounded) until it
    /// reports ready. Fails if it exits first.
    pub fn spawn(command: &mut Command, point: &str) -> Result<Self> {
        let signals = tempfile::tempdir()?;
        let ready = signals.path().join("ready");
        let release = signals.path().join("release");
        command
            .env("RGIT_FAILPOINT", format!("{point}:pause"))
            .env("RGIT_FAILPOINT_READY", &ready)
            .env("RGIT_FAILPOINT_RELEASE", &release);
        let child = spawn_piped(command)?;
        let mut paused = Self {
            child: Some(child),
            ready,
            release,
            _signals: signals,
        };
        let start = Instant::now();
        while !paused.ready.exists() {
            if let Some(child) = paused.child.as_mut()
                && child.try_wait()?.is_some()
            {
                let output = finish(paused.child.take().ok_or("child")?, DEADLINE)?;
                return Err(
                    format!("exited before pausing at {point}: {}", describe(&output)).into(),
                );
            }
            if start.elapsed() > Duration::from_secs(30) {
                return Err(format!("never reached {point} within 30s").into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(paused)
    }

    pub fn release(mut self) -> Result<Output> {
        std::fs::write(&self.release, b"go")?;
        finish(self.child.take().ok_or("child already finished")?, DEADLINE)
    }
}

impl Drop for Paused {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Run `command` armed with `<point>:pause` and assert it finishes without
/// ever reaching that point (e.g. a writer that must be refused first).
pub fn run_expect_no_pause(command: &mut Command, point: &str) -> Result<Output> {
    let signals = tempfile::tempdir()?;
    let ready = signals.path().join("ready");
    command
        .env("RGIT_FAILPOINT", format!("{point}:pause"))
        .env("RGIT_FAILPOINT_READY", &ready)
        .env("RGIT_FAILPOINT_RELEASE", signals.path().join("release"));
    let output = finish(spawn_piped(command)?, Duration::from_secs(20))?;
    if ready.exists() {
        return Err(format!("second writer reached {point}; it must be refused first").into());
    }
    Ok(output)
}

pub fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Published {
    Old,
    New,
}

/// Classify one checkout's index as exactly the old or new model index.
/// The file itself must verify (no torn write) and every entry's object must
/// exist (objects before names).
pub fn classify_index(model_old: &Model, model_new: &Model, c: usize) -> Result<Published> {
    let root = model_old.root(c);
    let actual = observe::index(root)?;
    for entry in actual.values() {
        super::proc::git(root, &["cat-file", "-e", &entry.oid])
            .map_err(|e| format!("index names a missing object: {e}"))?;
    }
    classify(
        &actual,
        &model_old.checkouts[c].index,
        &model_new.checkouts[c].index,
        "index",
    )
}

fn classify<T: PartialEq + std::fmt::Debug>(
    actual: &T,
    old: &T,
    new: &T,
    what: &str,
) -> Result<Published> {
    if actual == old {
        Ok(Published::Old)
    } else if actual == new {
        Ok(Published::New)
    } else {
        Err(format!("{what} is neither old {old:?} nor new {new:?}: {actual:?}").into())
    }
}

/// Classify HEAD's commit (via the branch or detached HEAD).
pub fn classify_head(model_old: &Model, model_new: &Model, c: usize) -> Result<Published> {
    let root = model_old.root(c);
    let actual_head = observe::symbolic_head(root)?;
    let expected_symbolic = match &model_old.checkouts[c].head {
        super::model::Head::Branch(name) => Some(format!("refs/heads/{name}")),
        super::model::Head::Detached(_) => None,
    };
    if actual_head != expected_symbolic {
        return Err(format!("HEAD symbolic target changed: {actual_head:?}").into());
    }
    let actual = observe::resolve(root, "HEAD")?;
    if let Some(oid) = &actual {
        super::proc::git(root, &["cat-file", "-e", oid])
            .map_err(|e| format!("HEAD names a missing object: {e}"))?;
    }
    let state = classify(
        &actual,
        &model_old.head_commit(c),
        &model_new.head_commit(c),
        "HEAD",
    )?;
    let branches = observe::branches(root)?;
    let expected: &BTreeMap<String, String> = match state {
        Published::Old => &model_old.refs,
        Published::New => &model_new.refs,
    };
    if &branches != expected {
        return Err(format!("branches {branches:?} != {expected:?}").into());
    }
    Ok(state)
}

/// Snapshot comparison that ignores the named published files and lock files
/// (classified separately) and allows new loose objects.
pub fn assert_unpublished_untouched(
    before: &BTreeMap<PathBuf, Snap>,
    after: &BTreeMap<PathBuf, Snap>,
    published: &[PathBuf],
    context: &str,
) -> Result<Vec<PathBuf>> {
    let keep =
        |path: &PathBuf| !published.contains(path) && path.extension().is_none_or(|e| e != "lock");
    let filter = |snap: &BTreeMap<PathBuf, Snap>| -> BTreeMap<PathBuf, Snap> {
        snap.iter()
            .filter(|(p, _)| keep(p))
            .map(|(p, s)| (p.clone(), s.clone()))
            .collect()
    };
    assert_preserved(
        &filter(before),
        &filter(after),
        Preserve::AllowNewObjects,
        context,
    )
}
