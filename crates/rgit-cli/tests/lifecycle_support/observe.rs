//! Observe actual repository state with C Git plumbing in fresh processes and
//! compare it with the independent model.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::model::{Entry, Hash, Head, Index, Model};
use super::proc::{Result, describe, git, git_cmd, git_text, run};

fn parse_entries(bytes: &[u8], ls_tree: bool) -> Result<Index> {
    let mut index = Index::new();
    for record in bytes.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = record
            .iter()
            .position(|b| *b == b'\t')
            .ok_or("missing tab")?;
        let meta = std::str::from_utf8(&record[..tab])?;
        let fields: Vec<&str> = meta.split(' ').collect();
        let (mode, oid) = if ls_tree {
            // <mode> SP <type> SP <oid>
            (fields[0], fields[2])
        } else {
            // <mode> SP <oid> SP <stage>
            if fields[2] != "0" {
                return Err(format!("unexpected conflict stage in {meta}").into());
            }
            (fields[0], fields[1])
        };
        let previous = index.insert(
            record[tab + 1..].to_vec(),
            Entry {
                mode: u32::from_str_radix(mode, 8)?,
                oid: oid.to_owned(),
            },
        );
        if previous.is_some() {
            return Err("duplicate path".into());
        }
    }
    Ok(index)
}

/// Independently verify an on-disk index: "DIRC" signature, version 2-4 and
/// a trailing checksum over everything before it (Git's all-zero
/// `index.skipHash` trailer is accepted). C Git's `ls-files` does not verify
/// the checksum and happily parses a torn index, so the oracle must.
pub fn verify_index_file(path: &Path, hash: Hash) -> Result {
    let bytes = fs::read(path)?;
    let width = hash.digest(b"").len();
    if bytes.len() < 12 + width || &bytes[..4] != b"DIRC" {
        return Err(format!("{}: not an index file", path.display()).into());
    }
    let version = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if !(2..=4).contains(&version) {
        return Err(format!("{}: index version {version}", path.display()).into());
    }
    let (body, trailer) = bytes.split_at(bytes.len() - width);
    if trailer.iter().any(|b| *b != 0) && hash.digest(body) != trailer {
        return Err(format!("{}: index checksum mismatch (torn write?)", path.display()).into());
    }
    Ok(())
}

pub fn index(dir: &Path) -> Result<Index> {
    let (git_dir, _) = git_dirs(dir)?;
    let file = git_dir.join("index");
    if file.exists() {
        let hash = match git_text(dir, &["rev-parse", "--show-object-format"])?.as_str() {
            "sha256" => Hash::Sha256,
            _ => Hash::Sha1,
        };
        verify_index_file(&file, hash)?;
    }
    parse_entries(&git(dir, &["ls-files", "--stage", "-z"])?, false)
}

pub fn tree(dir: &Path, rev: &str) -> Result<Index> {
    parse_entries(
        &git(dir, &["ls-tree", "-r", "-z", "--full-tree", rev])?,
        true,
    )
}

pub fn resolve(dir: &Path, rev: &str) -> Result<Option<String>> {
    let output = run(git_cmd(dir).args(["rev-parse", "--verify", "--quiet", rev]))?;
    match output.status.code() {
        Some(0) => Ok(Some(String::from_utf8(output.stdout)?.trim().to_owned())),
        Some(1) => Ok(None),
        _ => Err(format!("rev-parse {rev}: {}", describe(&output)).into()),
    }
}

pub fn symbolic_head(dir: &Path) -> Result<Option<String>> {
    let output = run(git_cmd(dir).args(["symbolic-ref", "-q", "HEAD"]))?;
    match output.status.code() {
        Some(0) => Ok(Some(String::from_utf8(output.stdout)?.trim().to_owned())),
        Some(1) => Ok(None),
        _ => Err(format!("symbolic-ref: {}", describe(&output)).into()),
    }
}

pub fn branches(dir: &Path) -> Result<BTreeMap<String, String>> {
    let text = git_text(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
        ],
    )?;
    Ok(text
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, oid)| {
            (
                name.trim_start_matches("refs/heads/").to_owned(),
                oid.to_owned(),
            )
        })
        .collect())
}

pub fn status(dir: &Path) -> Result<Vec<u8>> {
    git(
        dir,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ],
    )
}

/// `git fsck --strict` must succeed and report nothing but dangling objects.
pub fn fsck(dir: &Path) -> Result {
    let output = run(git_cmd(dir).args([
        "fsck",
        "--strict",
        "--full",
        "--no-progress",
        "--no-dangling",
    ]))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The only tolerated diagnostic: an unborn repository has no refs yet.
    let stderr_ok = stderr.trim().is_empty()
        || (stderr == "notice: No default references\n" && branches(dir)?.is_empty());
    if !output.status.success() || !output.stdout.is_empty() || !stderr_ok {
        return Err(format!(
            "fsck --strict failed in {}: {}",
            dir.display(),
            describe(&output)
        )
        .into());
    }
    Ok(())
}

pub fn git_dirs(dir: &Path) -> Result<(PathBuf, PathBuf)> {
    let text = git_text(
        dir,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
        ],
    )?;
    let mut lines = text.lines();
    let git_dir = PathBuf::from(lines.next().ok_or("git dir")?);
    let common = PathBuf::from(lines.next().ok_or("common dir")?);
    Ok((git_dir, common))
}

/// Every `*.lock` beneath a Git directory (not descending into objects).
pub fn locks(git_dir: &Path) -> Result<Vec<PathBuf>> {
    fn visit(dir: &Path, out: &mut Vec<PathBuf>) -> Result {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if entry.file_name() != "objects" && entry.file_name() != "worktrees" {
                    visit(&path, out)?;
                }
            } else if path.extension().is_some_and(|e| e == "lock") {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    visit(git_dir, &mut out)?;
    out.sort();
    Ok(out)
}

/// Every reflog line must chain old->new and name objects that exist; the
/// last entry must equal the current ref. Returns whether a log existed.
pub fn reflog_consistent(dir: &Path, common: &Path, refname: &str, value: &str) -> Result<bool> {
    let path = common.join("logs").join(refname);
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(false);
    };
    let mut previous: Option<String> = None;
    for line in text.lines() {
        let mut fields = line.split(' ');
        let old = fields.next().ok_or("reflog old")?;
        let new = fields.next().ok_or("reflog new")?;
        if let Some(prev) = &previous
            && prev != old
        {
            return Err(format!("{refname} reflog breaks chain at {line:?}").into());
        }
        git(dir, &["cat-file", "-e", new])?;
        previous = Some(new.to_owned());
    }
    if previous.as_deref() != Some(value) {
        return Err(format!("{refname} reflog ends at {previous:?}, ref is {value}").into());
    }
    Ok(true)
}

/// Compare every observable piece of `model` with a fresh C Git view.
pub fn assert_state(model: &Model) -> Result {
    assert_state_with(model, false)
}

/// As [`assert_state`]; `reflog_may_lag` tolerates a missing final reflog
/// entry after a crash between ref rename and reflog append (ADR 0003:
/// reflog appends are not part of the atomic publication).
pub fn assert_state_with(model: &Model, reflog_may_lag: bool) -> Result {
    let main = model.root(0);
    let object_format = git_text(main, &["rev-parse", "--show-object-format"])?;
    if object_format != model.hash.name() {
        return Err(format!("object format {object_format} != {}", model.hash.name()).into());
    }
    let actual_refs = branches(main)?;
    if actual_refs != model.refs {
        return Err(format!("branches {actual_refs:?} != model {:?}", model.refs).into());
    }
    for (oid, info) in &model.commits {
        let body = git(main, &["cat-file", "commit", oid])?;
        if body != info.body {
            return Err(format!(
                "commit {oid} body {:?} != model {:?}",
                String::from_utf8_lossy(&body),
                String::from_utf8_lossy(&info.body)
            )
            .into());
        }
        if tree(main, oid)? != info.tree {
            return Err(format!("commit {oid} tree differs from model").into());
        }
    }
    for (c, checkout) in model.checkouts.iter().enumerate() {
        let root = &checkout.root;
        let actual_index = index(root)?;
        if actual_index != checkout.index {
            return Err(format!(
                "checkout {c} index {actual_index:?} != model {:?}",
                checkout.index
            )
            .into());
        }
        match &checkout.head {
            Head::Branch(name) => {
                let symbolic = symbolic_head(root)?;
                if symbolic.as_deref() != Some(&format!("refs/heads/{name}")) {
                    return Err(format!("checkout {c} HEAD {symbolic:?} != {name}").into());
                }
            }
            Head::Detached(oid) => {
                if symbolic_head(root)?.is_some() {
                    return Err(format!("checkout {c} HEAD should be detached").into());
                }
                if resolve(root, "HEAD")?.as_deref() != Some(oid) {
                    return Err(format!("checkout {c} detached HEAD != {oid}").into());
                }
            }
        }
        let actual_status = status(root)?;
        let expected_status = model.status(c);
        if actual_status != expected_status {
            return Err(format!(
                "checkout {c} status {:?} != model {:?}",
                String::from_utf8_lossy(&actual_status),
                String::from_utf8_lossy(&expected_status)
            )
            .into());
        }
        let (git_dir, common) = git_dirs(root)?;
        for dir in [&git_dir, &common] {
            let held = locks(dir)?;
            if !held.is_empty() {
                return Err(format!("leftover locks after success: {held:?}").into());
            }
        }
        for (name, oid) in &model.refs {
            let refname = format!("refs/heads/{name}");
            if let Err(error) = reflog_consistent(root, &common, &refname, oid) {
                if !reflog_may_lag {
                    return Err(error);
                }
                eprintln!("NOTE: {refname} reflog lags after crash: {error}");
            }
        }
    }
    fsck(main)?;
    Ok(())
}

/// Hash an object with C Git (`hash-object -t <kind>`) inside `repo`, whose
/// object format selects the algorithm. Used only for oracle self-checks.
pub fn c_git_hash_object(repo: &Path, kind: &str, bytes: &[u8]) -> Result<String> {
    use std::ffi::OsStr;
    let scratch = tempfile::tempdir()?;
    let path = scratch.path().join("hash-input");
    fs::write(&path, bytes)?;
    git_text(
        repo,
        &[
            OsStr::new("hash-object"),
            OsStr::new("--literally"),
            OsStr::new("-t"),
            OsStr::new(kind),
            path.as_os_str(),
        ],
    )
}
