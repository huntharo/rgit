//! Bounded child processes, isolated C Git commands and byte snapshots.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

pub type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Default bound for any single child process.
pub const DEADLINE: Duration = Duration::from_secs(60);

/// Fixed identity for every commit the model predicts.
pub const NAME: &str = "Lifecycle Oracle";
pub const EMAIL: &str = "oracle@example.invalid";

/// Isolate a child from the developer's Git configuration and repository
/// selection variables. Applied to C Git and rgit alike.
pub fn isolate(command: &mut Command, home: &Path) {
    command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", home.join("no-global-config"))
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", "/dev/null")
        .env("GIT_CONFIG_KEY_1", "gc.auto")
        .env("GIT_CONFIG_VALUE_1", "0")
        .env("GIT_CONFIG_KEY_2", "maintenance.auto")
        .env("GIT_CONFIG_VALUE_2", "false")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CEILING_DIRECTORIES", home)
        .env("LC_ALL", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_NAMESPACE")
        .env_remove("RGIT_FAILPOINT")
        .env_remove("RGIT_FAILPOINT_READY")
        .env_remove("RGIT_FAILPOINT_RELEASE");
    for key in [
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_AUTHOR_DATE",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "GIT_COMMITTER_DATE",
    ] {
        command.env_remove(key);
    }
}

pub fn identity(command: &mut Command, when: i64) {
    let date = format!("{when} +0000");
    command
        .env("GIT_AUTHOR_NAME", NAME)
        .env("GIT_AUTHOR_EMAIL", EMAIL)
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_NAME", NAME)
        .env("GIT_COMMITTER_EMAIL", EMAIL)
        .env("GIT_COMMITTER_DATE", &date);
}

/// The directory used as HOME/ceiling for isolated children: the parent of a
/// scenario's root, so discovery can never escape into a developer checkout.
pub fn home_for(dir: &Path) -> PathBuf {
    let mut home = dir.to_path_buf();
    while !home.join(".lifecycle-home").exists() {
        if !home.pop() {
            return dir.parent().unwrap_or(dir).to_path_buf();
        }
    }
    home
}

/// Fresh C Git observer process in `dir`.
pub fn git_cmd(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(dir);
    isolate(&mut command, &home_for(dir));
    command
}

pub fn spawn_piped(command: &mut Command) -> Result<Child> {
    Ok(command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?)
}

/// Wait for a spawned child, draining both pipes so it can never block on a
/// full pipe, and kill/reap it if it overruns `deadline`.
pub fn finish(mut child: Child, deadline: Duration) -> Result<Output> {
    let mut stdout = child.stdout.take().ok_or("stdout not piped")?;
    let mut stderr = child.stderr.take().ok_or("stderr not piped")?;
    let out_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let err_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let err = err_reader.join().unwrap_or_default();
            return Err(format!(
                "child exceeded {deadline:?} and was killed/reaped: {}",
                String::from_utf8_lossy(&err)
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    Ok(Output {
        status,
        stdout: out_reader.join().map_err(|_| "stdout reader panicked")?,
        stderr: err_reader.join().map_err(|_| "stderr reader panicked")?,
    })
}

pub fn run(command: &mut Command) -> Result<Output> {
    run_within(command, DEADLINE)
}

pub fn run_within(command: &mut Command, deadline: Duration) -> Result<Output> {
    finish(spawn_piped(command)?, deadline)
}

pub fn describe(output: &Output) -> String {
    format!(
        "status {:?}; stdout {:?}; stderr {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Run an observer/fixture C Git command that must succeed.
pub fn git<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<Vec<u8>> {
    let output = run(git_cmd(dir).args(args))?;
    if !output.status.success() {
        let shown: Vec<_> = args.iter().map(|a| a.as_ref().to_owned()).collect();
        return Err(format!("git {shown:?} in {}: {}", dir.display(), describe(&output)).into());
    }
    Ok(output.stdout)
}

pub fn git_text<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<String> {
    Ok(String::from_utf8(git(dir, args)?)?.trim_end().to_owned())
}

/// A scenario sandbox. Children use it as HOME and discovery ceiling.
pub fn sandbox() -> Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    fs::write(dir.path().join(".lifecycle-home"), b"")?;
    Ok(dir)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Snap {
    File { bytes: Vec<u8>, exec: bool },
    Symlink(PathBuf),
    Dir,
    Other(String),
}

/// Every entry beneath `root` (not following symlinks), keyed by relative path.
pub fn snapshot(root: &Path) -> Result<BTreeMap<PathBuf, Snap>> {
    fn visit(base: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Snap>) -> Result {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(base)?.to_path_buf();
            let kind = fs::symlink_metadata(&path)?.file_type();
            if kind.is_symlink() {
                out.insert(relative, Snap::Symlink(fs::read_link(&path)?));
            } else if kind.is_dir() {
                out.insert(relative, Snap::Dir);
                visit(base, &path, out)?;
            } else if kind.is_file() {
                #[cfg(unix)]
                let exec = {
                    use std::os::unix::fs::PermissionsExt;
                    fs::symlink_metadata(&path)?.permissions().mode() & 0o111 != 0
                };
                #[cfg(not(unix))]
                let exec = false;
                out.insert(
                    relative,
                    Snap::File {
                        bytes: fs::read(&path)?,
                        exec,
                    },
                );
            } else {
                out.insert(relative, Snap::Other(format!("{kind:?}")));
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out)?;
    Ok(out)
}

fn is_loose_object_or_dir(path: &Path) -> bool {
    // `<gitdir>/objects/xx` or `<gitdir>/objects/xx/<rest>` anywhere in `path`.
    let parts: Vec<_> = path.components().map(|c| c.as_os_str()).collect();
    parts.iter().enumerate().any(|(i, part)| {
        *part == "objects"
            && parts.get(i + 1).is_some_and(|fan| {
                let fan = fan.to_string_lossy();
                fan.len() == 2 && fan.bytes().all(|b| b.is_ascii_hexdigit())
            })
            && parts.len() <= i + 3
    })
}

/// How strictly a refusal must preserve bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preserve {
    /// Everything byte-identical, with no exceptions.
    Exact,
    /// Byte-identical except that new loose objects (and their fan-out
    /// directories) may appear: ADR 0003 invariant 5 permits unreferenced
    /// objects. Existing objects must not change.
    AllowNewObjects,
}

/// Assert `after` equals `before` under `preserve` (both snapshots of
/// `root`). Every newly added loose object must be a complete, valid,
/// read-only object whose content hashes to its name (checked by the
/// oracle's own SHA). Returns the added paths so callers can report them.
pub fn assert_preserved(
    root: &Path,
    before: &BTreeMap<PathBuf, Snap>,
    after: &BTreeMap<PathBuf, Snap>,
    preserve: Preserve,
    context: &str,
) -> Result<Vec<PathBuf>> {
    let mut added = Vec::new();
    for (path, old) in before {
        match after.get(path) {
            Some(new) if new == old => {}
            Some(_) => return Err(format!("{context}: {} changed", path.display()).into()),
            None => return Err(format!("{context}: {} was removed", path.display()).into()),
        }
    }
    for path in after.keys().filter(|p| !before.contains_key(*p)) {
        if preserve == Preserve::AllowNewObjects && is_loose_object_or_dir(path) {
            added.push(path.clone());
        } else {
            return Err(format!("{context}: unexpected new entry {}", path.display()).into());
        }
    }
    for path in &added {
        if matches!(after.get(path), Some(Snap::File { .. })) {
            verify_loose_object(root, path).map_err(|e| format!("{context}: {e}"))?;
        }
    }
    Ok(added)
}

/// `relative` is `<...>/objects/xx/<rest>` beneath `root`.
fn verify_loose_object(root: &Path, relative: &Path) -> Result {
    let full = root.join(relative);
    let fanout = full.parent().ok_or("fan-out")?;
    let objects = fanout.parent().ok_or("objects dir")?;
    let git_dir = objects.parent().ok_or("git dir")?;
    let oid = format!(
        "{}{}",
        fanout.file_name().ok_or("fan-out name")?.to_string_lossy(),
        full.file_name().ok_or("object name")?.to_string_lossy()
    );
    let git_args = |args: &[&str]| -> Result<Vec<u8>> {
        let mut command = git_cmd(root);
        command.arg("--git-dir").arg(git_dir).args(args);
        let output = run(&mut command)?;
        if !output.status.success() {
            return Err(format!("new object {oid}: git {args:?}: {}", describe(&output)).into());
        }
        Ok(output.stdout)
    };
    let kind = String::from_utf8(git_args(&["cat-file", "-t", &oid])?)?
        .trim()
        .to_owned();
    let size: usize = String::from_utf8(git_args(&["cat-file", "-s", &oid])?)?
        .trim()
        .parse()?;
    let body = git_args(&["cat-file", &kind, &oid])?;
    let hash = match String::from_utf8(git_args(&["rev-parse", "--show-object-format"])?)?.trim() {
        "sha256" => super::model::Hash::Sha256,
        _ => super::model::Hash::Sha1,
    };
    if body.len() != size || hash.object_hex(&kind, &body) != oid {
        return Err(format!(
            "new loose object {oid} does not hash to its name ({kind}, {size} bytes)"
        )
        .into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::symlink_metadata(&full)?.permissions().mode() & 0o222 != 0 {
            return Err(format!("new loose object {oid} is writable").into());
        }
    }
    Ok(())
}
