//! Decision 0004 acceptance on Unix (descriptor-root identity): directories
//! and files swapped for symlinks or FIFOs at pause points must not redirect
//! a read or write outside the root that was opened, and must never hang.
//! Each operation runs in a child process of this test binary paused at a
//! failpoint while the test plays the competing process.
#![cfg(all(unix, feature = "failpoints", debug_assertions))]

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{Result, git};
use rgit_core::write::{
    CommitRequest, InitOptions, ObjectFormat, RefExpectation, RefLog, RefUpdate, Signature, init,
};
use rgit_core::{BStr, Error, Repository};

const FORMATS: [ObjectFormat; 2] = [ObjectFormat::Sha1, ObjectFormat::Sha256];
const BLOB: &[u8] = b"object raced by a directory swap\n";

fn options(format: ObjectFormat) -> InitOptions {
    InitOptions {
        object_format: format,
        ..InitOptions::default()
    }
}

/// Every entry below `dir` with its inode, mode and bytes, plus `dir`'s own
/// inode: the outside directory must be byte- and inode-identical afterwards.
/// Path to (inode, mode, bytes) for every entry of a tree.
type Tree = BTreeMap<PathBuf, (u64, u32, Vec<u8>)>;

fn fingerprint(dir: &Path) -> Result<Tree> {
    let mut result = BTreeMap::new();
    let meta = fs::symlink_metadata(dir)?;
    result.insert(dir.to_owned(), (meta.ino(), meta.mode(), Vec::new()));
    let mut pending = vec![dir.to_owned()];
    while let Some(directory) = pending.pop() {
        for child in fs::read_dir(&directory)? {
            let path = child?.path();
            let meta = fs::symlink_metadata(&path)?;
            let bytes = if meta.is_file() {
                fs::read(&path)?
            } else {
                Vec::new()
            };
            if meta.is_dir() {
                pending.push(path.clone());
            }
            result.insert(path, (meta.ino(), meta.mode(), bytes));
        }
    }
    Ok(result)
}

// ------------------------------------------------------------- harness ---

fn signature() -> Signature {
    Signature {
        name: b"Test".to_vec(),
        email: b"test@example.invalid".to_vec(),
        seconds: 1_767_225_600,
        offset_minutes: 0,
    }
}

/// Child side: run the scenario named in the environment, print its result.
#[test]
fn child_action() -> Result {
    let (Some(scenario), Some(path)) = (
        std::env::var_os("RGIT_TEST_SCENARIO"),
        std::env::var_os("RGIT_TEST_PATH"),
    ) else {
        return Ok(());
    };
    let scenario = scenario.to_string_lossy().into_owned();
    let repo = Repository::discover(PathBuf::from(path))?;
    let result = if let Some(spec) = scenario.strip_prefix("add:") {
        repo.add(&[BStr::new(spec)]).map(drop)
    } else if scenario == "write-blob" {
        repo.write_blob(BLOB).map(drop)
    } else if scenario == "update-ref" {
        let head = repo.resolve(BStr::new("HEAD"))?;
        repo.update_ref(&RefUpdate {
            name: b"refs/heads/topic".to_vec(),
            new: head,
            expected: RefExpectation::MustNotExist,
            log: Some(RefLog {
                committer: signature(),
                message: b"branch: Created from HEAD".to_vec(),
            }),
        })
    } else {
        repo.commit(&CommitRequest {
            message: b"raced\n".to_vec(),
            author: signature(),
            committer: signature(),
            allow_empty: true,
        })
        .map(drop)
    };
    match result {
        Ok(()) => println!("RESULT ok"),
        Err(Error::AlreadyExists(_)) => println!("RESULT already-exists"),
        Err(error) => println!("RESULT error {error}"),
    }
    Ok(())
}

struct Child {
    child: std::process::Child,
    release: Option<PathBuf>,
    _control: tempfile::TempDir,
}

/// Start `scenario` in a child; with `pause`, wait until it reaches it.
fn start(pause: Option<&str>, scenario: &str, repo: &Path) -> Result<Child> {
    let control = tempfile::tempdir()?;
    let ready = control.path().join("ready");
    let release = control.path().join("release");
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args(["--exact", "child_action", "--nocapture", "--test-threads=1"])
        .env("RGIT_TEST_SCENARIO", scenario)
        .env("RGIT_TEST_PATH", repo)
        .env_remove("RGIT_FAILPOINT")
        .stdout(std::process::Stdio::piped());
    if let Some(point) = pause {
        command
            .env("RGIT_FAILPOINT", format!("{point}:pause"))
            .env("RGIT_FAILPOINT_READY", &ready)
            .env("RGIT_FAILPOINT_RELEASE", &release);
    }
    let mut child = command.spawn()?;
    if let Some(point) = pause {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ready.exists() {
            if child.try_wait()?.is_some() || Instant::now() > deadline {
                let _ = child.kill();
                return Err(format!("child never reached {point}").into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(Child {
        child,
        release: pause.map(|_| release),
        _control: control,
    })
}

impl Child {
    /// Release the pause (if any) and collect the result within 30 seconds;
    /// a child still running then has hung and fails the test.
    fn finish(mut self) -> Result<String> {
        use std::io::Read;
        if let Some(release) = &self.release {
            fs::write(release, b"go")?;
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.child.try_wait()?.is_none() {
            if Instant::now() > deadline {
                let _ = self.child.kill();
                return Err("child did not finish (blocked)".into());
            }
            std::thread::sleep(Duration::from_millis(10));
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

fn mkfifo(path: &Path) -> Result {
    assert!(
        std::process::Command::new("mkfifo")
            .arg(path)
            .status()?
            .success()
    );
    Ok(())
}

fn hash(repo: &Path, bytes: &[u8], scratch: &Path) -> Result<String> {
    let file = scratch.join(format!("hash-input-{}", bytes.len()));
    fs::write(&file, bytes)?;
    let output = git(
        repo,
        &["hash-object", "--no-filters", &file.to_string_lossy()],
    )?;
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn has_object(repo: &Path, id: &str) -> Result<bool> {
    Ok(common::git_command(repo)
        .args(["cat-file", "-e", id])
        .status()?
        .success())
}

// ---------------------------------------------------------- acceptance ---

/// 1. An ancestor of a staged path is swapped for a symlink to an outside
///    directory holding a same-named file: add refuses (it completes only on
///    the original tree), the outside content is never staged, and the
///    outside directory is byte- and inode-identical.
#[test]
fn add_never_reads_through_a_swapped_ancestor() -> Result {
    for format in FORMATS {
        for point in ["add.before-descend", "add.before-leaf-open"] {
            let temp = tempfile::tempdir()?;
            let repo = temp.path().join("repo");
            init(&repo, &options(format))?;
            fs::create_dir_all(repo.join("a/b"))?;
            fs::write(repo.join("a/b/new.txt"), b"inside\n")?;
            let outside = temp.path().join("outside");
            fs::create_dir_all(outside.join("b"))?;
            fs::write(outside.join("b/new.txt"), b"OUTSIDE\n")?;
            let before = fingerprint(&outside)?;

            let child = start(Some(point), "add:a/b/new.txt", &repo)?;
            fs::rename(repo.join("a"), repo.join("a.moved"))?;
            symlink(&outside, repo.join("a"))?;
            let result = child.finish()?;

            assert!(result.starts_with("error"), "{format:?} {point}: {result}");
            assert_eq!(fingerprint(&outside)?, before, "{point}: outside changed");
            let outside_id = hash(&repo, b"OUTSIDE\n", temp.path())?;
            assert!(
                !has_object(&repo, &outside_id)?,
                "{point}: outside content stored"
            );
            assert!(!repo.join(".git/index").exists(), "{point}: index written");
        }
    }
    Ok(())
}

/// 2a. `objects/xx` appears as a symlink to an outside directory before the
///     fan-out is opened: the write is refused and nothing lands outside.
/// 2b. The fan-out directory is moved away and replaced by such a symlink
///     after it was opened: the object stays inside the Git directory (in
///     the moved directory), the write is refused because the published name
///     is no longer reachable, and nothing lands outside.
#[test]
fn object_writes_are_not_redirected_by_a_swapped_fanout() -> Result {
    for format in FORMATS {
        for point in ["object.before-fanout", "object.before-publish"] {
            let temp = tempfile::tempdir()?;
            let repo = temp.path().join("repo");
            init(&repo, &options(format))?;
            let outside = temp.path().join("outside");
            fs::create_dir(&outside)?;
            let before = fingerprint(&outside)?;
            let id = hash(&repo, BLOB, temp.path())?;
            let fanout = repo.join(".git/objects").join(&id[..2]);

            let child = start(Some(point), "write-blob", &repo)?;
            if point == "object.before-publish" {
                fs::rename(&fanout, repo.join(".git/objects/moved"))?;
            }
            symlink(&outside, &fanout)?;
            let result = child.finish()?;

            assert!(
                result.starts_with("error") || result == "already-exists",
                "{point}: {result}"
            );
            assert_eq!(fingerprint(&outside)?, before, "{point}: outside changed");
        }
    }
    Ok(())
}

/// 3. During a commit, `refs/heads` (after the ref preflight) or the reflog
///    directory is replaced by a symlink to an outside directory: the commit
///    is refused, the branch is not created, nothing lands outside.
#[test]
fn ref_and_reflog_writes_are_not_redirected() -> Result {
    for format in FORMATS {
        for swap in ["refs/heads", "logs"] {
            let temp = tempfile::tempdir()?;
            let repo = temp.path().join("repo");
            init(&repo, &options(format))?;
            fs::write(repo.join("file"), b"content\n")?;
            Repository::discover(&repo)?.add(&[BStr::new("file")])?;
            let outside = temp.path().join("outside");
            fs::create_dir(&outside)?;
            let before = fingerprint(&outside)?;

            let child = start(Some("refs.before-dir"), "commit", &repo)?;
            let target = repo.join(".git").join(swap);
            if target.exists() {
                fs::rename(&target, repo.join(".git").join(format!("{swap}.moved")))?;
            }
            symlink(&outside, &target)?;
            let result = child.finish()?;

            assert!(
                result.starts_with("error") || result == "already-exists",
                "{swap}: {result}"
            );
            assert_eq!(fingerprint(&outside)?, before, "{swap}: outside changed");
            assert!(
                !repo.join(".git/refs/heads.moved/master").exists()
                    && !repo.join(".git/refs/heads/master").exists(),
                "{swap}: branch published"
            );
        }
    }
    Ok(())
}

/// 4a. `info/exclude` or `info/attributes` is a FIFO or a symlink to an
///     outside file when add starts: bounded refusal, outside unchanged.
#[test]
fn special_info_files_are_refused_without_blocking() -> Result {
    for name in ["exclude", "attributes"] {
        for kind in ["fifo", "symlink"] {
            let temp = tempfile::tempdir()?;
            let repo = temp.path().join("repo");
            init(&repo, &InitOptions::default())?;
            fs::write(repo.join("file.txt"), b"content\n")?;
            fs::create_dir_all(repo.join(".git/info"))?;
            let outside = temp.path().join("outside");
            fs::create_dir(&outside)?;
            fs::write(outside.join("patterns"), b"*.txt\n")?;
            let before = fingerprint(&outside)?;
            let info = repo.join(".git/info").join(name);
            if kind == "fifo" {
                mkfifo(&info)?;
            } else {
                symlink(outside.join("patterns"), &info)?;
            }
            let result = start(None, "add:file.txt", &repo)?.finish()?;
            assert!(
                result.contains("attribute/ignore file"),
                "{name} {kind}: {result}"
            );
            assert_eq!(fingerprint(&outside)?, before);
            assert!(!repo.join(".git/index").exists());
        }
    }
    Ok(())
}

/// 4b. `info/exclude` is swapped for a FIFO after add read it: Gitoxide never
///     reopens it, so add completes consistently with the content it read.
#[test]
fn info_files_swapped_after_the_snapshot_do_not_block() -> Result {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    init(&repo, &InitOptions::default())?;
    fs::create_dir_all(repo.join(".git/info"))?;
    fs::write(repo.join(".git/info/exclude"), b"*.log\n")?;
    fs::write(repo.join(".git/info/attributes"), b"*.bin binary\n")?;
    fs::write(repo.join("keep.txt"), b"keep\n")?;
    fs::write(repo.join("drop.log"), b"drop\n")?;
    let child = start(Some("add.before-descend"), "add:.", &repo)?;
    for name in ["exclude", "attributes"] {
        let info = repo.join(".git/info").join(name);
        fs::remove_file(&info)?;
        mkfifo(&info)?;
    }
    assert_eq!(child.finish()?, "ok");
    let staged = String::from_utf8(git(&repo, &["ls-files"])?.stdout)?;
    assert_eq!(staged, "keep.txt\n");
    Ok(())
}

/// 4c. `.git/info` itself is a symlink to an outside directory: its files are
///     never read through it (the walk does not follow the link), so add is
///     refused, even though the outside patterns would not change the result.
#[test]
fn a_symlinked_info_directory_is_not_followed() -> Result {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    init(&repo, &InitOptions::default())?;
    fs::write(repo.join("file.txt"), b"content\n")?;
    let outside = temp.path().join("outside");
    fs::create_dir(&outside)?;
    fs::write(outside.join("exclude"), b"# nothing excluded\n")?;
    let before = fingerprint(&outside)?;
    if repo.join(".git/info").exists() {
        fs::remove_dir_all(repo.join(".git/info"))?;
    }
    symlink(&outside, repo.join(".git/info"))?;
    let result = start(None, "add:file.txt", &repo)?.finish()?;
    assert!(result.starts_with("error"), "{result}");
    assert_eq!(fingerprint(&outside)?, before);
    assert!(!repo.join(".git/index").exists());
    Ok(())
}

/// A4 (decision 0004, item 4): while a branch commit or a logged
/// `update_ref` holds `HEAD.lock` (a guard that is never renamed), another
/// writer removes and recreates it.
///
/// - At the locked point (before any reflog append): `Locked`; no reflog
///   line, ref unchanged, own `<ref>.lock` released.
/// - At `ref.logged` (after the append, before the rename): the reflog
///   records the update but the ref is NOT updated
///   (`RefLoggedNotPublishedError`); nothing is rolled back.
///
/// In both cases the foreign `HEAD.lock` keeps its bytes and inode, and a
/// fresh C Git process sees consistent refs, logs and objects.
#[test]
fn a_replaced_head_lock_stops_publication() -> Result {
    for format in FORMATS {
        for (scenario, locked_point) in [
            ("commit", "commit.ref-locked"),
            ("update-ref", "ref.locked"),
        ] {
            for point in [locked_point, "ref.logged"] {
                let temp = tempfile::tempdir()?;
                let repo = temp.path().join("repo");
                init(&repo, &options(format))?;
                git(&repo, &["config", "maintenance.auto", "false"])?;
                fs::write(repo.join("base"), b"base\n")?;
                git(&repo, &["add", "base"])?;
                git(&repo, &["commit", "-q", "-m", "base"])?;
                fs::write(repo.join("next"), b"next\n")?;
                Repository::discover(&repo)?.add(&[BStr::new("next")])?;
                let git_dir = repo.join(".git");
                let (reference, log) = if scenario == "commit" {
                    ("refs/heads/master", "logs/refs/heads/master")
                } else {
                    ("refs/heads/topic", "logs/refs/heads/topic")
                };
                let master = fs::read(git_dir.join("refs/heads/master"))?;
                let head_log = fs::read(git_dir.join("logs/HEAD"))?;
                let ref_log = fs::read(git_dir.join(log)).unwrap_or_default();

                let child = start(Some(point), scenario, &repo)?;
                let head_lock = git_dir.join("HEAD.lock");
                fs::remove_file(&head_lock)?;
                fs::write(&head_lock, b"foreign writer\n")?;
                let foreign = fs::symlink_metadata(&head_lock)?.ino();
                let result = child.finish()?;
                let case = format!("{format:?} {scenario} at {point}: {result}");

                // The foreign guard is untouched; our own ref lock is gone.
                assert_eq!(fs::read(&head_lock)?, b"foreign writer\n", "{case}");
                assert_eq!(fs::symlink_metadata(&head_lock)?.ino(), foreign, "{case}");
                let ref_lock = git_dir.join(format!("{reference}.lock"));
                assert!(!ref_lock.exists(), "{case}");
                // The reference is not published.
                assert_eq!(
                    fs::read(git_dir.join("refs/heads/master"))?,
                    master,
                    "{case}"
                );
                assert!(!git_dir.join("refs/heads/topic").exists(), "{case}");
                let ref_log_now = fs::read(git_dir.join(log)).unwrap_or_default();
                let head_log_now = fs::read(git_dir.join("logs/HEAD"))?;
                if point == "ref.logged" {
                    assert!(result.contains("its reflog already records"), "{case}");
                    // Exactly one line appended to the ref's log (and to
                    // HEAD's for the branch commit); none removed.
                    assert!(ref_log_now.starts_with(&ref_log), "{case}");
                    assert_eq!(
                        ref_log_now[ref_log.len()..].split(|b| *b == b'\n').count(),
                        2,
                        "{case}"
                    );
                    if scenario == "commit" {
                        assert!(head_log_now.starts_with(&head_log), "{case}");
                        assert!(head_log_now.len() > head_log.len(), "{case}");
                    } else {
                        assert_eq!(head_log_now, head_log, "{case}");
                    }
                } else {
                    assert!(result.contains("lock file exists"), "{case}");
                    assert_eq!(ref_log_now, ref_log, "{case}");
                    assert_eq!(head_log_now, head_log, "{case}");
                }
                // A fresh process: the reflog's new value is a stored object
                // and the repository checks out clean.
                fs::remove_file(&head_lock)?;
                git(&repo, &["fsck", "--no-progress"])?;
                assert_eq!(
                    String::from_utf8(git(&repo, &["rev-parse", "HEAD"])?.stdout)?
                        .trim()
                        .as_bytes(),
                    master.trim_ascii(),
                    "{case}"
                );
            }
        }
    }
    Ok(())
}
