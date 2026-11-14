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
use std::process::{Child, Command, Stdio};

use common::{Result, git};
use rgit_core::write::{
    ObservedTarget, RefExpectation, RefLog, RefUpdate, RenameOutcomeUncertainError, Signature,
};
use rgit_core::{Error, ObjectId, Repository};

const CHILD: &str = "RGIT_TEST_LOCK_CHILD";

fn text(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Child: update `refs/heads/other` from `<old>` to `<new>`, with a reflog
/// when asked, and write a classification of the result.
#[test]
fn lock_child() -> Result {
    let Ok(spec) = std::env::var(CHILD) else {
        return Ok(());
    };
    let mut fields = spec.split('|');
    let (repo, old, new, out, with_log) = (
        fields.next().ok_or("repo")?,
        fields.next().ok_or("old")?,
        fields.next().ok_or("new")?,
        fields.next().ok_or("out")?,
        fields.next().ok_or("log")? == "log",
    );
    let log = with_log.then(|| RefLog {
        committer: Signature {
            name: b"Test".to_vec(),
            email: b"test@example.invalid".to_vec(),
            seconds: 1_767_225_600,
            offset_minutes: 0,
        },
        message: b"child update".to_vec(),
    });
    let result = Repository::discover(repo)?.update_ref(&RefUpdate {
        name: b"refs/heads/other".to_vec(),
        new: ObjectId::from_hex(new.as_bytes())?,
        expected: RefExpectation::MustEqual(ObjectId::from_hex(old.as_bytes())?),
        log,
    });
    let report = match &result {
        Ok(()) => "ok".to_owned(),
        Err(Error::Other(error)) => match error.downcast_ref::<RenameOutcomeUncertainError>() {
            Some(uncertain) => format!(
                "uncertain\nobserved={}\nfile={}\nours={:?}\ncontent={}\nmessage={error}",
                uncertain
                    .observed()
                    .id()
                    .is_some_and(|id| id != uncertain.ours()),
                matches!(uncertain.observed(), ObservedTarget::File { .. }),
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

/// Wait for the child to reach the failpoint; a child that exits first
/// (it failed before the rename) is reported with its result at once.
fn wait_for(path: &Path, child: &mut Child, out: &Path) -> Result {
    for _ in 0..3000 {
        if path.exists() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            let report = std::fs::read_to_string(out).unwrap_or_default();
            return Err(format!("child exited before the failpoint ({status}): {report}").into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let _ = child.kill();
    Err("failpoint never reached".into())
}

/// With and without a reflog: the uncertain outcome passes through
/// unchanged (never "not updated"), and with a reflog the line our update
/// appended before the rename stays.
#[test]
fn replacement_after_rename_is_reported_as_uncertain_and_kept() -> Result {
    for with_log in [false, true] {
        replacement_case(with_log)?;
    }
    Ok(())
}

fn replacement_case(with_log: bool) -> Result {
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
            format!(
                "{}|{first}|{second}|{}|{}",
                dir.display(),
                out.display(),
                if with_log { "log" } else { "nolog" }
            ),
        )
        .env("RGIT_FAILPOINT", "lock.renamed-unverified:pause")
        .env("RGIT_FAILPOINT_READY", sync.join("ready"))
        .env("RGIT_FAILPOINT_RELEASE", sync.join("release"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    wait_for(&sync.join("ready"), &mut child, &out)?;
    // The rename has happened: our value is visible. Another writer now
    // replaces the target with a different file, written first and renamed
    // over it (never delete-then-create, which could reuse the freed file
    // index or hit a delete-pending target).
    assert_eq!(std::fs::read_to_string(&target)?.trim(), second);
    let replacement = format!("{first}\n");
    let staged = dir.join(".git/refs/heads/replacement.tmp");
    std::fs::write(&staged, &replacement)?;
    std::fs::rename(&staged, &target)?;
    std::fs::write(sync.join("release"), b"")?;
    assert!(child.wait()?.success());

    let report = std::fs::read_to_string(&out)?;
    assert!(report.starts_with("uncertain\n"), "{report}");
    assert!(report.contains("observed=true"), "{report}");
    assert!(report.contains("file=true"), "{report}");
    assert!(
        report.contains(&format!("content={replacement}")),
        "{report}"
    );
    assert!(report.contains("no longer has our identity"), "{report}");
    assert!(!report.contains("was not updated"), "{report}");
    // Never rolled back or rewritten; no lock left behind.
    assert_eq!(std::fs::read_to_string(&target)?, replacement);
    assert!(!dir.join(".git/refs/heads/other.lock").exists());
    assert!(!dir.join(".git/HEAD.lock").exists());
    let reflog = std::fs::read_to_string(dir.join(".git/logs/refs/heads/other"))?;
    assert_eq!(reflog.contains("child update"), with_log, "{reflog}");
    Ok(())
}
