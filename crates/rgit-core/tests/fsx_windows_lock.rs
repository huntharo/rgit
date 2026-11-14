//! Windows legacy path lock, end to end: the target is replaced at the
//! `lock.renamed-unverified` pause point (after `MoveFileExW` returned
//! success, before the identity check). The update reports an UNCERTAIN
//! outcome with the observed target state and rolls nothing back. This
//! detects one replacement outcome; it does not close the close-before-rename
//! window (decision 0004, PWR-197). The test binary re-executes itself as the
//! child that runs `update_ref`, with the failpoint set only for the child.
#![cfg(all(windows, feature = "failpoints", debug_assertions))]

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

use common::{Result, git};
use rgit_core::write::{RefExpectation, RefUpdate, RenameOutcomeUncertainError};
use rgit_core::{Error, ObjectId, Repository};

const CHILD: &str = "RGIT_TEST_LOCK_CHILD";

fn text(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Child: update `refs/heads/other` from `<old>` to `<new>` without a reflog
/// and write a classification of the result.
#[test]
fn lock_child() -> Result {
    let Ok(spec) = std::env::var(CHILD) else {
        return Ok(());
    };
    let mut fields = spec.split('|');
    let (repo, old, new, out) = (
        fields.next().ok_or("repo")?,
        fields.next().ok_or("old")?,
        fields.next().ok_or("new")?,
        fields.next().ok_or("out")?,
    );
    let result = Repository::discover(repo)?.update_ref(&RefUpdate {
        name: b"refs/heads/other".to_vec(),
        new: ObjectId::from_hex(new.as_bytes())?,
        expected: RefExpectation::MustEqual(ObjectId::from_hex(old.as_bytes())?),
        log: None,
    });
    let report = match &result {
        Ok(()) => "ok".to_owned(),
        Err(Error::Other(error)) => match error.downcast_ref::<RenameOutcomeUncertainError>() {
            Some(uncertain) => format!(
                "uncertain\nobserved={}\nours={:?}\ncontent={}\nmessage={error}",
                uncertain
                    .observed()
                    .is_some_and(|id| id != uncertain.ours()),
                uncertain.ours(),
                String::from_utf8_lossy(uncertain.observed_content().unwrap_or_default()),
            ),
            None => format!("other: {error}"),
        },
        Err(error) => format!("error: {error:?}"),
    };
    std::fs::write(out, report)?;
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

#[test]
fn replacement_after_rename_is_reported_as_uncertain_and_kept() -> Result {
    let temp = tempfile::tempdir()?;
    let dir = temp.path().join("repo");
    std::fs::create_dir(&dir)?;
    git(&dir, &["init", "-q", "-b", "main"])?;
    git(&dir, &["commit", "-q", "--allow-empty", "-m", "first"])?;
    let first = text(&git(&dir, &["rev-parse", "HEAD"])?);
    git(&dir, &["branch", "other"])?;
    git(&dir, &["commit", "-q", "--allow-empty", "-m", "second"])?;
    let second = text(&git(&dir, &["rev-parse", "HEAD"])?);
    let target = dir.join(".git/refs/heads/other");
    let sync = temp.path().join("sync");
    std::fs::create_dir(&sync)?;
    let out = temp.path().join("result");

    let mut child = Command::new(std::env::current_exe()?)
        .args(["--exact", "lock_child", "--test-threads=1"])
        .env(
            CHILD,
            format!("{}|{first}|{second}|{}", dir.display(), out.display()),
        )
        .env("RGIT_FAILPOINT", "lock.renamed-unverified:pause")
        .env("RGIT_FAILPOINT_READY", sync.join("ready"))
        .env("RGIT_FAILPOINT_RELEASE", sync.join("release"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    wait_for(&sync.join("ready"))?;
    // The rename has happened: our value is visible. Another writer now
    // replaces the target with a different file.
    assert_eq!(std::fs::read_to_string(&target)?.trim(), second);
    std::fs::remove_file(&target)?;
    let replacement = format!("{first}\n");
    std::fs::write(&target, &replacement)?;
    std::fs::write(sync.join("release"), b"")?;
    assert!(child.wait()?.success());

    let report = std::fs::read_to_string(&out)?;
    assert!(report.starts_with("uncertain\n"), "{report}");
    assert!(report.contains("observed=true"), "{report}");
    assert!(
        report.contains(&format!("content={replacement}")),
        "{report}"
    );
    assert!(report.contains("no longer has our identity"), "{report}");
    // Never rolled back or rewritten; no lock left behind.
    assert_eq!(std::fs::read_to_string(&target)?, replacement);
    assert!(!dir.join(".git/refs/heads/other.lock").exists());
    Ok(())
}
