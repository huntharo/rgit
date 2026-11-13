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
    let scenario = scenario.to_string_lossy();
    let result = if scenario.starts_with("init") {
        init(&path, &options).map(drop)
    } else if scenario == "write-blob" {
        Repository::discover(&path)?.write_blob(RACE_BLOB).map(drop)
    } else {
        Repository::discover(&path)?
            .add(&[BStr::new("new.txt")])
            .map(drop)
    };
    match result {
        Ok(()) => println!("RESULT ok"),
        Err(Error::Injected(point)) => println!("RESULT injected {point}"),
        Err(Error::AlreadyExists(_)) => println!("RESULT already-exists"),
        Err(error) => println!("RESULT error {error}"),
    }
    Ok(())
}

const RACE_BLOB: &[u8] = b"content raced by a competing writer\n";

/// A child paused at `point`: the competitor acts while it waits, then
/// `finish` releases it and collects its result with a bounded wait.
struct Paused {
    child: std::process::Child,
    release: PathBuf,
    _control: tempfile::TempDir,
}

fn pause_child(point: &str, scenario: &str, path: &Path) -> Result<Paused> {
    let control = tempfile::tempdir()?;
    let ready = control.path().join("ready");
    let release = control.path().join("release");
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "child_action", "--nocapture", "--test-threads=1"])
        .env("RGIT_FAILPOINT", format!("{point}:pause"))
        .env("RGIT_FAILPOINT_READY", &ready)
        .env("RGIT_FAILPOINT_RELEASE", &release)
        .env("RGIT_TEST_SCENARIO", scenario)
        .env("RGIT_TEST_PATH", path)
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !ready.exists() {
        if child.try_wait()?.is_some() || std::time::Instant::now() > deadline {
            let _ = child.kill();
            return Err(format!("child never reached {point}").into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(Paused {
        child,
        release,
        _control: control,
    })
}

impl Paused {
    fn finish(mut self) -> Result<String> {
        use std::io::Read;
        fs::write(&self.release, b"go")?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while self.child.try_wait()?.is_none() {
            if std::time::Instant::now() > deadline {
                let _ = self.child.kill();
                return Err("child blocked after release".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let mut stdout = String::new();
        if let Some(mut pipe) = self.child.stdout.take() {
            pipe.read_to_string(&mut stdout)?;
        }
        Ok(stdout
            .lines()
            .find_map(|line| line.split_once("RESULT ").map(|(_, result)| result))
            .ok_or_else(|| format!("child printed no result: {stdout}"))?
            .to_owned())
    }
}

#[cfg(unix)]
fn identity(path: &Path) -> Result<(u64, u64, u32, Vec<u8>)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path)?;
    let bytes = if metadata.is_file() {
        fs::read(path)?
    } else {
        Vec::new()
    };
    Ok((metadata.dev(), metadata.ino(), metadata.mode(), bytes))
}

fn names(dir: &Path) -> Result<Vec<String>> {
    let mut names: Vec<String> = fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<_>>()?;
    names.sort();
    Ok(names)
}

/// A competitor creates the destination after rgit's last absence check;
/// rgit must fail with AlreadyExists, leave the competitor's entry exactly as
/// it was, and remove only what it created.
#[cfg(unix)]
#[test]
fn init_never_replaces_a_destination_created_after_its_last_check() -> Result {
    use std::os::unix::fs::PermissionsExt;
    // Non-bare: an empty .git directory appears before the publication.
    let temp = tempfile::tempdir()?;
    let worktree = temp.path().join("work");
    fs::create_dir(&worktree)?;
    let paused = pause_child("init.before-publish", "init", &worktree)?;
    fs::create_dir(worktree.join(".git"))?;
    let competitor = identity(&worktree.join(".git"))?;
    assert_eq!(paused.finish()?, "already-exists");
    assert_eq!(identity(&worktree.join(".git"))?, competitor);
    assert!(names(&worktree.join(".git"))?.is_empty());
    assert_eq!(
        names(&worktree)?,
        [".git"],
        "our private directory is removed"
    );

    // Bare, absent target: an empty directory appears at the target.
    let bare = temp.path().join("bare.git");
    let paused = pause_child("init.before-publish", "init-bare", &bare)?;
    fs::create_dir(&bare)?;
    let competitor = identity(&bare)?;
    assert_eq!(paused.finish()?, "already-exists");
    assert_eq!(identity(&bare)?, competitor);
    assert!(names(&bare)?.is_empty());
    assert_eq!(names(temp.path())?, ["bare.git", "work"]);

    // Bare, existing empty target: a HEAD appears before ours is linked.
    let target = temp.path().join("inplace.git");
    fs::create_dir(&target)?;
    let paused = pause_child("init.before-head", "init-bare", &target)?;
    fs::write(target.join("HEAD"), b"competitor\n")?;
    fs::set_permissions(target.join("HEAD"), fs::Permissions::from_mode(0o600))?;
    let competitor = identity(&target.join("HEAD"))?;
    assert_eq!(paused.finish()?, "already-exists");
    assert_eq!(identity(&target.join("HEAD"))?, competitor);
    assert_eq!(
        names(&target)?,
        ["HEAD"],
        "only our own entries are removed"
    );
    Ok(())
}

/// A competitor publishes the object file between rgit's absence check and
/// its link: a valid copy of the same object is accepted untouched; any other
/// content is refused and also left untouched. Our temporary file is removed.
#[cfg(unix)]
#[test]
fn object_publication_never_replaces_a_competing_object_file() -> Result {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir()?;
    // The same object as C Git writes it, from an independent repository.
    let oracle = temp.path().join("oracle");
    git(temp.path(), &["init", "-q", "oracle"])?;
    let source = oracle.join("source");
    fs::write(&source, RACE_BLOB)?;
    let hex = String::from_utf8(git(&oracle, &["hash-object", "-w", "source"])?.stdout)?
        .trim()
        .to_owned();
    let valid = fs::read(oracle.join(".git/objects").join(&hex[..2]).join(&hex[2..]))?;

    for (content, expected) in [(valid.clone(), "ok"), (b"not zlib".to_vec(), "error")] {
        let root = temp.path().join(format!("repo-{expected}"));
        init(&root, &InitOptions::default())?;
        let object = root.join(".git/objects").join(&hex[..2]).join(&hex[2..]);
        let paused = pause_child("object.before-publish", "write-blob", &root)?;
        fs::write(&object, &content)?;
        fs::set_permissions(&object, fs::Permissions::from_mode(0o600))?;
        let competitor = identity(&object)?;
        let result = paused.finish()?;
        assert!(result.starts_with(expected), "{expected}: {result}");
        assert_eq!(identity(&object)?, competitor);
        assert_eq!(
            names(object.parent().ok_or("fan-out")?)?,
            [hex[2..].to_owned()],
            "our temporary object file is removed"
        );
        if expected == "ok" {
            fsck(&root)?;
        }
    }
    Ok(())
}

/// The tracked leaf is replaced after add's lstat and before its open: a FIFO
/// must not block and a symlink must not be followed; both are refused with
/// nothing staged. A metadata file swapped for a FIFO at the same point is
/// never reopened, because ignore/attribute data was read beforehand.
#[cfg(unix)]
#[test]
fn add_refuses_leaves_swapped_after_lstat_without_blocking_or_following() -> Result {
    for swap in ["fifo", "symlink"] {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("repo");
        init(&root, &InitOptions::default())?;
        fs::write(temp.path().join("secret"), b"outside the worktree\n")?;
        fs::write(root.join("new.txt"), b"staged content\n")?;
        let paused = pause_child("add.before-leaf-open", "add", &root)?;
        fs::remove_file(root.join("new.txt"))?;
        if swap == "fifo" {
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(root.join("new.txt"))
                    .status()?
                    .success()
            );
        } else {
            std::os::unix::fs::symlink(temp.path().join("secret"), root.join("new.txt"))?;
        }
        let result = paused.finish()?;
        assert!(
            result.contains("changed while being added"),
            "{swap}: {result}"
        );
        assert!(!root.join(".git/index").exists(), "{swap}: nothing staged");
        let objects = names(&root.join(".git/objects"))?;
        assert_eq!(objects, ["info", "pack"], "{swap}: no blob written");
    }

    let temp = tempfile::tempdir()?;
    let root = temp.path().join("repo");
    init(&root, &InitOptions::default())?;
    fs::write(root.join(".gitattributes"), b"*.bin binary\n")?;
    fs::write(root.join("new.txt"), b"staged content\n")?;
    let paused = pause_child("add.before-leaf-open", "add", &root)?;
    fs::remove_file(root.join(".gitattributes"))?;
    assert!(
        std::process::Command::new("mkfifo")
            .arg(root.join(".gitattributes"))
            .status()?
            .success()
    );
    assert_eq!(paused.finish()?, "ok");
    let staged = String::from_utf8(git(&root, &["ls-files"])?.stdout)?;
    assert_eq!(staged, "new.txt\n");
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
