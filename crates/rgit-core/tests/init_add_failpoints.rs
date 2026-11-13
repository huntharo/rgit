//! Injected failures (`RGIT_FAILPOINT=<name>:error`) at each init/add crash
//! boundary. Each action runs in a child process of this test binary so the
//! failpoint variable never leaks into concurrently running tests.
//! Run with `cargo test -p rgit-core --features failpoints`; abort-mode
//! crash recovery is exercised through the CLI by the lifecycle suite.
#![cfg(all(feature = "failpoints", debug_assertions))]

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use common::{Result, fsck, git};
use rgit_core::write::{InitOptions, ObjectFormat, init};
use rgit_core::{BStr, Error, Repository};

fn snapshot(dir: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut result = BTreeMap::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(directory) = pending.pop() {
        for child in fs::read_dir(&directory)? {
            let child = child?;
            if child.file_type()?.is_dir() {
                pending.push(child.path());
            } else {
                result.insert(child.path(), fs::read(child.path()).unwrap_or_default());
            }
        }
    }
    Ok(result)
}

/// Re-run this binary's `child_action` test with the failpoint set in the
/// child's environment (the parent never mutates its own environment).
fn spawn_child(spec: &str, scenario: &str, path: &Path) -> Result<std::process::Output> {
    Ok(std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "child_action", "--nocapture", "--test-threads=1"])
        .env("RGIT_FAILPOINT", spec)
        .env("RGIT_TEST_SCENARIO", scenario)
        .env("RGIT_TEST_PATH", path)
        .output()?)
}

fn run_child(point: &str, scenario: &str, path: &Path) -> Result<String> {
    let output = spawn_child(&format!("{point}:error"), scenario, path)?;
    let stdout = String::from_utf8(output.stdout)?;
    assert!(output.status.success(), "child failed: {stdout}");
    Ok(stdout
        .lines()
        .find_map(|line| line.split_once("RESULT ").map(|(_, result)| result))
        .ok_or("child printed no result")?
        .to_owned())
}

#[test]
fn child_action() -> Result {
    let (Some(scenario), Some(path)) = (
        std::env::var_os("RGIT_TEST_SCENARIO"),
        std::env::var_os("RGIT_TEST_PATH"),
    ) else {
        return Ok(());
    };
    let path = PathBuf::from(path);
    let sha256 = scenario.to_string_lossy().ends_with("-sha256");
    let options = InitOptions {
        object_format: if sha256 {
            ObjectFormat::Sha256
        } else {
            ObjectFormat::Sha1
        },
        bare: scenario.to_string_lossy().starts_with("init-bare"),
        ..InitOptions::default()
    };
    let result = if scenario.to_string_lossy().starts_with("init") {
        init(&path, &options).map(drop)
    } else {
        Repository::discover(&path)?
            .add(&[BStr::new("new.txt")])
            .map(drop)
    };
    match result {
        Err(Error::Injected(point)) => println!("RESULT injected {point}"),
        other => println!("RESULT unexpected {other:?}"),
    }
    Ok(())
}

fn injected(outcome: String, name: &str) {
    assert_eq!(outcome, format!("injected {name}"));
}

/// No Git (C Git or rgit) may treat `path` as a repository.
fn assert_not_a_repository(path: &Path) -> Result {
    assert!(Repository::discover(path).is_err());
    let c_git = common::git_command(path)
        .args(["--git-dir", "."])
        .args(["rev-parse", "--git-dir"])
        .output()?;
    assert!(!c_git.status.success(), "C Git accepted {}", path.display());
    Ok(())
}

/// An existing empty bare target is populated in place with HEAD last: an
/// injected error restores the empty directory, and a crash (abort) before
/// HEAD leaves a directory no Git recognizes as a repository.
#[test]
fn bare_in_place_init_is_not_a_repository_before_head() -> Result {
    for (format, suffix) in [(ObjectFormat::Sha1, ""), (ObjectFormat::Sha256, "-sha256")] {
        let temp = tempfile::tempdir()?;
        let target = temp.path().join("existing.git");
        fs::create_dir(&target)?;
        injected(
            run_child("init.before-head", &format!("init-bare{suffix}"), &target)?,
            "init.before-head",
        );
        assert_eq!(
            fs::read_dir(&target)?.count(),
            0,
            "entries were not removed"
        );

        let output = spawn_child("init.before-head", &format!("init-bare{suffix}"), &target)?;
        assert!(!output.status.success(), "the child should have aborted");
        assert!(target.join("objects").is_dir() && target.join("config").is_file());
        assert!(!target.join("HEAD").exists());
        assert_not_a_repository(&target)?;
        // The leftover is not empty, so a retry refuses rather than merging.
        let retry = init(
            &target,
            &InitOptions {
                object_format: format,
                bare: true,
                ..InitOptions::default()
            },
        );
        assert!(
            matches!(retry, Err(Error::AlreadyExists(_))),
            "{:?}",
            retry.err()
        );
    }
    Ok(())
}

#[test]
fn injected_failures_leave_previous_state_intact() -> Result {
    for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
        let temp = tempfile::tempdir()?;
        let options = InitOptions {
            object_format: format,
            ..InitOptions::default()
        };

        // init: nothing is published and the private directory is removed.
        let root = temp.path().join("repo");
        let suffix = if format == ObjectFormat::Sha256 {
            "-sha256"
        } else {
            ""
        };
        injected(
            run_child("init.before-publish", &format!("init{suffix}"), &root)?,
            "init.before-publish",
        );
        assert!(!root.join(".git").exists());
        assert_eq!(fs::read_dir(&root)?.count(), 0, "no temporary remains");
        let bare = temp.path().join("bare.git");
        injected(
            run_child("init.before-publish", &format!("init-bare{suffix}"), &bare)?,
            "init.before-publish",
        );
        assert!(!bare.exists());
        assert_eq!(
            fs::read_dir(temp.path())?.count(),
            1,
            "only the worktree dir"
        );

        init(&root, &options)?;
        fs::write(root.join("base.txt"), b"base\n")?;
        git(&root, &["add", "base.txt"])?;
        fs::write(root.join("new.txt"), b"new content\n")?;
        let index = root.join(".git/index");
        for point in [
            "add.objects-written",
            "lock.acquired",
            "add.index-locked",
            "lock.before-rename",
        ] {
            let before = fs::read(&index)?;
            injected(run_child(point, "add", &root)?, point);
            assert_eq!(fs::read(&index)?, before, "{point}: index unchanged");
            assert!(
                !root.join(".git/index.lock").exists(),
                "{point}: lock removed"
            );
            // Objects precede the index: the blob is stored and harmless.
            let id = git(&root, &["hash-object", "new.txt"])?.stdout;
            let id = String::from_utf8(id)?.trim().to_owned();
            git(&root, &["cat-file", "-e", &id])?;
            fsck(&root)?;
            assert!(!String::from_utf8(git(&root, &["ls-files"])?.stdout)?.contains("new.txt"));
        }

        // Injected after the rename: the new index is visible; the error
        // only reports unknown durability.
        let objects = snapshot(&root.join(".git/objects"))?;
        injected(
            run_child("lock.after-rename", "add", &root)?,
            "lock.after-rename",
        );
        assert!(String::from_utf8(git(&root, &["ls-files"])?.stdout)?.contains("new.txt"));
        assert_eq!(snapshot(&root.join(".git/objects"))?, objects);
        fsck(&root)?;
    }
    Ok(())
}
