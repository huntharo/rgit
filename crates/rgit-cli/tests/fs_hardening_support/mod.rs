//! Adversarial filesystem oracle for PWR-194 (ADR 0004). Tests only.
//!
//! Builds on the lifecycle oracle (`lifecycle_support`, shared by `#[path]`)
//! and adds: an outside-the-policy-root decoy tree recorded with inodes, the
//! directory swap used by every scenario (move the original aside, put a
//! symlink or junction to the decoy in its place, restore afterwards), and
//! the two switches that gate cases on work not yet integrated.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::lifecycle_support::proc::{Result, Snap, snapshot};

/// Whether this build's write operations use the anchored backend, as the
/// library reports it (decision 0004, acceptance 6): selection is asserted,
/// never assumed. Cases whose guarantee needs anchoring are enforced exactly
/// when this is true; otherwise they still run and a failure prints an
/// explicit `SKIP: ... requires the anchored backend` line carrying the
/// baseline failure as evidence.
pub fn anchored() -> bool {
    use rgit_core::write::{FilesystemHardening, filesystem_hardening};
    filesystem_hardening() == FilesystemHardening::Anchored
}

/// THE integration switch for acceptance 6 on Unix. While false, the
/// selection test prints an explicit SKIP if Unix is not yet Anchored; flip
/// to true at integration and the test FAILS unless Unix selects Anchored.
pub const REQUIRE_ANCHORED_ON_UNIX: bool = true;

/// THE failpoint switch for pause points WP1 may add (`add.before-descend`,
/// `object.before-fanout`, `refs.before-dir`). While false, those cases print
/// an explicit SKIP; flip to true once the names exist in the binary.
pub const WP1_FS_FAILPOINTS: bool = true;

/// What a case may currently be held to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    /// Must pass on every backend, including the baseline.
    Now,
    /// Guaranteed only by the anchored backend (enforced when `anchored()`).
    AfterAnchoring,
    /// Needs a WP1 pause point that does not exist yet (WP1_FS_FAILPOINTS).
    NeedsFailpoint(&'static str),
}

/// Run one case under its expectation, printing exactly one result line:
/// `PASS`, `SKIP` (gated, with the reason or the baseline failure) or
/// `NOTE` (gated case that already passes). Returns the error only when the
/// case is enforced.
pub fn case(name: &str, expect: Expect, body: impl FnOnce() -> Result) -> Result {
    if let Expect::NeedsFailpoint(point) = expect
        && !WP1_FS_FAILPOINTS
    {
        eprintln!("SKIP: {name}: needs WP1 failpoint `{point}` (WP1_FS_FAILPOINTS=false)");
        return Ok(());
    }
    let result = body();
    let gated = expect == Expect::AfterAnchoring && !anchored();
    match (result, gated) {
        (Ok(()), false) => {
            eprintln!("PASS: {name}");
            Ok(())
        }
        (Ok(()), true) => {
            eprintln!(
                "NOTE: {name}: already holds on the baseline backend (backend is not Anchored)"
            );
            Ok(())
        }
        (Err(error), true) => {
            eprintln!(
                "SKIP: {name}: requires the anchored backend (backend is not Anchored); baseline failure: {}",
                first_line(&error.to_string())
            );
            Ok(())
        }
        (Err(error), false) => Err(format!("{name}: {error}").into()),
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(400)
        .collect()
}

/// Inode (Unix) of an entry, not following a final symlink; 0 elsewhere
/// (std exposes no stable file index on Windows).
pub fn inode(path: &Path) -> Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(fs::symlink_metadata(path)?.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = fs::symlink_metadata(path)?;
        Ok(0)
    }
}

/// Bytes, kind and inode of every entry beneath a root (symlinks not
/// followed; FIFOs never opened).
pub type Recorded = BTreeMap<PathBuf, (Snap, u64)>;

pub fn record(root: &Path) -> Result<Recorded> {
    snapshot(root)?
        .into_iter()
        .map(|(rel, snap)| {
            let id = inode(&root.join(&rel))?;
            Ok((rel, (snap, id)))
        })
        .collect()
}

/// Every byte, entry and inode beneath `root` must equal `before`.
pub fn assert_untouched(root: &Path, before: &Recorded, context: &str) -> Result {
    let after = record(root)?;
    for (path, old) in before {
        match after.get(path) {
            Some(new) if new == old => {}
            Some(new) => {
                return Err(format!(
                    "{context}: outside entry {} changed ({:?} -> {:?})",
                    path.display(),
                    old.0,
                    new.0
                )
                .into());
            }
            None => {
                return Err(
                    format!("{context}: outside entry {} was removed", path.display()).into(),
                );
            }
        }
    }
    if let Some(path) = after.keys().find(|p| !before.contains_key(*p)) {
        return Err(format!("{context}: new outside entry {}", path.display()).into());
    }
    Ok(())
}

/// A directory (or file) moved aside and replaced by a link to `target`.
pub struct Swapped {
    pub path: PathBuf,
    pub original: PathBuf,
    restored: bool,
}

impl Swapped {
    /// Put the original back (removing the link). Idempotent.
    pub fn restore(&mut self) -> Result {
        if self.restored {
            return Ok(());
        }
        remove_link(&self.path)?;
        fs::rename(&self.original, &self.path)?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for Swapped {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Move `path` aside and replace it with a directory link to `target`.
/// `Ok(None)` (after an explicit SKIP line) where links cannot be created
/// without privileges.
pub fn swap_for_link(path: &Path, target: &Path, context: &str) -> Result<Option<Swapped>> {
    let mut name = path.file_name().ok_or("no file name")?.to_os_string();
    name.push(".fsx-original");
    let original = path.with_file_name(name);
    fs::rename(path, &original)?;
    match link_dir(target, path) {
        Ok(()) => Ok(Some(Swapped {
            path: path.to_path_buf(),
            original,
            restored: false,
        })),
        Err(error) => {
            fs::rename(&original, path)?;
            eprintln!("SKIP: {context}: cannot create a directory link here: {error}");
            Ok(None)
        }
    }
}

#[cfg(unix)]
fn link_dir(target: &Path, link: &Path) -> Result {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

/// Windows: a directory junction needs no privilege; created with the
/// built-in `mklink /J` (test-only use of the system shell).
#[cfg(windows)]
fn link_dir(target: &Path, link: &Path) -> Result {
    let output = std::process::Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("mklink /J: {}", String::from_utf8_lossy(&output.stderr)).into())
    }
}

#[cfg(not(any(unix, windows)))]
fn link_dir(_target: &Path, _link: &Path) -> Result {
    Err("directory links unsupported on this platform".into())
}

fn remove_link(path: &Path) -> Result {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        // A Windows junction reports as a directory; removing it removes the
        // link, not the target's contents.
        fs::remove_dir(path)?;
    } else {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(_) => fs::remove_dir(path)?,
        }
    }
    Ok(())
}

/// Create a FIFO (Unix) with the system `mkfifo`.
#[cfg(unix)]
pub fn mkfifo(path: &Path) -> Result {
    let status = std::process::Command::new("mkfifo").arg(path).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("mkfifo {} failed", path.display()).into())
    }
}
