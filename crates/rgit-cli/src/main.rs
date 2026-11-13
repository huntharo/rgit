use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{ArgGroup, Args, Parser, Subcommand};
use rgit_core::write::{CommitRequest, InitOptions, ObjectFormat, Signature};
use rgit_core::{BStr, GraphLimits, ObjectKind, Repository, UntrackedMode};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(
    name = "rgit",
    version,
    about = "An experimental rewrite of Git using Rust."
)]
struct Cli {
    /// Run as if started in this directory (one -C is supported).
    #[arg(short = 'C', default_value = ".", global = true)]
    directory: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create an empty repository (experimental; never reinitializes).
    Init {
        #[arg(short = 'q', long)]
        quiet: bool,
        #[arg(long)]
        bare: bool,
        #[arg(short = 'b', long, value_name = "NAME")]
        initial_branch: Option<OsString>,
        #[arg(long, value_name = "FORMAT", default_value = "sha1")]
        object_format: String,
        /// Distinct id: `directory` is the global `-C` argument.
        #[arg(value_name = "DIRECTORY")]
        target: Option<PathBuf>,
    },
    /// Stage literal paths or directories (experimental; no pathspec magic).
    Add {
        #[arg(required = true, allow_hyphen_values = false)]
        paths: Vec<OsString>,
    },
    /// Record the index as a new commit on HEAD (experimental; no hooks).
    Commit {
        #[arg(short = 'm', long = "message", required = true, value_name = "MSG")]
        messages: Vec<OsString>,
        #[arg(long)]
        allow_empty: bool,
        #[arg(short = 'q', long)]
        quiet: bool,
    },
    /// Resolve one revision or inspect repository paths.
    RevParse(RevParse),
    /// Read object metadata or raw bytes: -t, -s, blob, tree, commit, tag.
    CatFile {
        #[arg(allow_hyphen_values = true)]
        mode: String,
        object: OsString,
    },
    /// List references and their object IDs, without peeling tags.
    ShowRef,
    /// Inspect staged and unstaged changes without refreshing the index.
    Status {
        #[arg(long, num_args = 0..=1, default_missing_value = "v1", require_equals = true)]
        porcelain: Option<String>,
        #[arg(short = 's', long)]
        short: bool,
        #[arg(short = 'z')]
        nul: bool,
        #[arg(long)]
        no_renames: bool,
        #[arg(short = 'u', long, default_value = "normal", num_args = 0..=1, default_missing_value = "all")]
        untracked_files: String,
    },
    /// Show staged or unstaged changes, without invoking external diff drivers.
    Diff {
        #[arg(long, alias = "staged")]
        cached: bool,
        #[arg(long, conflicts_with = "name_status")]
        name_only: bool,
        #[arg(long)]
        name_status: bool,
        #[arg(long)]
        quiet: bool,
        #[arg(long)]
        exit_code: bool,
        #[arg(short = 'z')]
        nul: bool,
        #[arg(long)]
        no_renames: bool,
        #[arg(long)]
        no_ext_diff: bool,
        #[arg(long)]
        no_textconv: bool,
    },
    /// List index paths and optionally their object IDs, modes and stages.
    LsFiles {
        #[arg(short = 's', long)]
        stage: bool,
        #[arg(short = 'u', long)]
        unmerged: bool,
        #[arg(short = 'z')]
        nul: bool,
    },
    /// Recursively list a tree, without accessing working files.
    LsTree {
        #[arg(short = 'r', required = true)]
        recursive: bool,
        #[arg(long)]
        name_only: bool,
        #[arg(short = 'z')]
        nul: bool,
        revision: OsString,
    },
    /// Find best common ancestors, or test whether one commit is an ancestor.
    MergeBase {
        #[arg(short = 'a', long, conflicts_with = "is_ancestor")]
        all: bool,
        #[arg(long)]
        is_ancestor: bool,
        left: OsString,
        right: OsString,
    },
    /// Count the unique commits reachable from one revision.
    RevList {
        #[arg(long, required = true)]
        count: bool,
        revision: OsString,
    },
}

#[derive(Args)]
#[command(group(ArgGroup::new("query").required(true).multiple(false)
    .args(["show_toplevel", "absolute_git_dir", "is_bare_repository", "verify"])))]
struct RevParse {
    #[arg(long)]
    show_toplevel: bool,
    #[arg(long)]
    absolute_git_dir: bool,
    #[arg(long)]
    is_bare_repository: bool,
    #[arg(long, value_name = "REVISION")]
    verify: Option<OsString>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli, &mut io::stdout().lock()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            // Match Unix pipelines without turning an ordinary early reader exit
            // into a Rust panic. Other write failures remain visible.
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
            {
                return ExitCode::SUCCESS;
            }
            eprintln!("fatal: {error}");
            ExitCode::from(128)
        }
    }
}

fn run(cli: Cli, out: &mut impl Write) -> Result<u8> {
    // Canonicalize the explicit start directory, never process-global cwd.
    let directory = dunce::canonicalize(&cli.directory)?;
    if let Command::Init {
        quiet,
        bare,
        initial_branch,
        object_format,
        target,
    } = &cli.command
    {
        let mut options = InitOptions {
            bare: *bare,
            object_format: match object_format.as_str() {
                "sha1" => ObjectFormat::Sha1,
                "sha256" => ObjectFormat::Sha256,
                other => return Err(format!("unknown hash algorithm '{other}'").into()),
            },
            ..InitOptions::default()
        };
        if let Some(branch) = initial_branch {
            options.initial_branch = os_bytes(branch)?.to_vec();
        }
        let target = target
            .as_ref()
            .map_or_else(|| directory.clone(), |path| directory.join(path));
        let repo = rgit_core::write::init(&target, &options)?;
        if !quiet {
            out.write_all(b"Initialized empty Git repository in ")?;
            // Same absolute form as `rev-parse --absolute-git-dir` (forward
            // slashes on Windows, as C Git prints), followed by `/`.
            let mut git_dir = Vec::new();
            write_path(&mut git_dir, repo.git_dir())?;
            git_dir.pop();
            out.write_all(&git_dir)?;
            out.write_all(b"/\n")?;
        }
        out.flush()?;
        return Ok(0);
    }
    let repo = Repository::discover(&directory)?;
    match cli.command {
        Command::Init { .. } => unreachable!("handled before discovery"),
        Command::Add { paths } => {
            let prefix = worktree_prefix(&repo, &directory)?;
            let paths = paths
                .iter()
                .map(|path| repo_relative(&prefix, os_bytes(path)?))
                .collect::<Result<Vec<_>>>()?;
            let root = repo
                .work_dir()
                .ok_or("this operation must be run in a work tree")?;
            let mut literal = Vec::with_capacity(paths.len());
            for path in &paths {
                match path.strip_suffix(b"/") {
                    Some(directory) => {
                        // `path/` must be a directory, as in C Git.
                        let on_disk = root.join(
                            std::str::from_utf8(directory)
                                .ok()
                                .map(PathBuf::from)
                                .map_or_else(|| native_path(directory), Ok)?,
                        );
                        if !std::fs::symlink_metadata(&on_disk).is_ok_and(|m| m.is_dir()) {
                            return Err(format!(
                                "pathspec '{}' did not match any files",
                                BStr::new(path)
                            )
                            .into());
                        }
                        literal.push(BStr::new(directory));
                    }
                    None => literal.push(BStr::new(path)),
                }
            }
            let paths = literal;
            repo.add(&paths)?;
        }
        Command::Commit {
            messages,
            allow_empty,
            quiet,
        } => {
            // Resolve every input before the library writes anything.
            // One clock reading so a defaulted author and committer agree.
            let now: i64 = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs()
                .try_into()?;
            let author = identity(&repo, "AUTHOR", now)?;
            let committer = identity(&repo, "COMMITTER", now)?;
            // Like C Git's `-m`: paragraphs joined by a blank line, then the
            // default `whitespace` cleanup (stripspace without comments).
            let mut raw = Vec::new();
            for part in &messages {
                if !raw.is_empty() {
                    raw.push(b'\n');
                }
                raw.extend_from_slice(os_bytes(part)?);
                if raw.last() != Some(&b'\n') {
                    raw.push(b'\n');
                }
            }
            let message = stripspace(&raw);
            if message.is_empty() {
                return Err("aborting commit due to empty commit message".into());
            }
            let outcome = repo.commit(&CommitRequest {
                message: message.clone(),
                author,
                committer,
                allow_empty,
            })?;
            if !quiet {
                let reference = outcome.reference.as_slice();
                let branch = reference
                    .strip_prefix(b"refs/heads/")
                    .unwrap_or(b"detached HEAD");
                let id = outcome.id.to_string();
                out.write_all(b"[")?;
                out.write_all(branch)?;
                if outcome.parent.is_none() {
                    out.write_all(b" (root-commit)")?;
                }
                write!(out, " {}] ", &id[..7])?;
                let subject = message.split(|&byte| byte == b'\n').next().unwrap_or(b"");
                out.write_all(subject)?;
                out.write_all(b"\n")?;
            }
        }
        Command::RevParse(query) => {
            if query.show_toplevel {
                let path = repo
                    .work_dir()
                    .ok_or("this operation must be run in a work tree")?;
                write_path(out, path)?;
            } else if query.absolute_git_dir {
                write_path(out, repo.git_dir())?;
            } else if query.is_bare_repository {
                writeln!(out, "{}", repo.is_bare())?;
            } else if let Some(revision) = query.verify {
                writeln!(out, "{}", repo.resolve(os_bytes(&revision)?)?)?;
            }
        }
        Command::CatFile { mode, object } => {
            let revision = os_bytes(&object)?;
            match mode.as_str() {
                "-t" => writeln!(out, "{}", repo.object_info(revision)?.kind)?,
                "-s" => writeln!(out, "{}", repo.object_info(revision)?.size)?,
                "blob" | "tree" | "commit" | "tag" => {
                    let kind = match mode.as_str() {
                        "blob" => ObjectKind::Blob,
                        "tree" => ObjectKind::Tree,
                        "commit" => ObjectKind::Commit,
                        _ => ObjectKind::Tag,
                    };
                    out.write_all(&repo.object_data(revision, kind)?)?;
                }
                _ => return Err(format!("unsupported cat-file mode: {mode}").into()),
            }
        }
        Command::ShowRef => {
            let references = repo.references()?;
            if references.is_empty() {
                return Ok(1);
            }
            for reference in references {
                write!(out, "{} ", reference.id)?;
                out.write_all(&reference.name)?;
                writeln!(out)?;
            }
        }
        Command::Status {
            porcelain,
            short: _,
            nul,
            no_renames: _,
            untracked_files,
        } => {
            if porcelain
                .as_deref()
                .is_some_and(|version| !["v1", "1"].contains(&version))
            {
                return Err("only porcelain v1 status is implemented".into());
            }
            let untracked = match untracked_files.as_str() {
                "no" => UntrackedMode::None,
                "normal" => UntrackedMode::Normal,
                "all" => UntrackedMode::All,
                _ => return Err("untracked-files must be no, normal or all".into()),
            };
            for entry in repo.status(untracked)? {
                out.write_all(&[entry.index, entry.worktree, b' '])?;
                write_entry_path(out, &entry.path, nul, true)?;
            }
        }
        Command::Diff {
            cached,
            name_only,
            name_status,
            quiet,
            exit_code,
            nul,
            ..
        } => {
            let entries = repo.diff_entries(cached)?;
            let different = !entries.is_empty();
            if !quiet {
                // Finish decoding and formatting before emitting any bytes.
                let mut output = Vec::new();
                if name_only || name_status {
                    for entry in entries {
                        if name_status {
                            output.extend_from_slice(&[entry.code, if nul { 0 } else { b'\t' }]);
                        }
                        write_entry_path(&mut output, &entry.path, nul, false)?;
                    }
                } else {
                    output = repo.diff_patch(cached)?;
                }
                out.write_all(&output)?;
            }
            out.flush()?;
            return Ok(u8::from((quiet || exit_code) && different));
        }
        Command::LsFiles {
            stage,
            unmerged,
            nul,
        } => {
            for entry in repo.index_entries()? {
                if unmerged && entry.stage == 0 {
                    continue;
                }
                if stage || unmerged {
                    write!(out, "{:06o} {} {}\t", entry.mode, entry.id, entry.stage)?;
                }
                write_entry_path(out, &entry.path, nul, false)?;
            }
        }
        Command::LsTree {
            revision,
            name_only,
            nul,
            ..
        } => {
            for entry in repo.tree_entries(os_bytes(&revision)?)? {
                if !name_only {
                    let kind = if entry.mode == 0o160000 {
                        "commit"
                    } else {
                        "blob"
                    };
                    write!(out, "{:06o} {} {}\t", entry.mode, kind, entry.id)?;
                }
                write_entry_path(out, &entry.path, nul, false)?;
            }
        }
        Command::MergeBase {
            all,
            is_ancestor,
            left,
            right,
        } => {
            let (left, right) = (os_bytes(&left)?, os_bytes(&right)?);
            if is_ancestor {
                return Ok(u8::from(!repo.is_ancestor(
                    left,
                    right,
                    GraphLimits::default(),
                )?));
            }
            let bases = repo.merge_bases(left, right, GraphLimits::default())?;
            if bases.is_empty() {
                return Ok(1);
            }
            for id in bases.into_iter().take(if all { usize::MAX } else { 1 }) {
                writeln!(out, "{id}")?;
            }
        }
        Command::RevList { revision, .. } => {
            writeln!(
                out,
                "{}",
                repo.ancestor_count(os_bytes(&revision)?, GraphLimits::default())?
            )?;
        }
    }
    out.flush()?;
    Ok(0)
}

/// The `-C` directory relative to the worktree root, as `/`-separated bytes.
fn worktree_prefix(repo: &Repository, directory: &Path) -> Result<Vec<u8>> {
    let root = dunce::canonicalize(
        repo.work_dir()
            .ok_or("this operation must be run in a work tree")?,
    )?;
    let relative = directory
        .strip_prefix(&root)
        .map_err(|_| "the -C directory is outside the work tree")?;
    let mut prefix = Vec::new();
    for component in relative.components() {
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(os_bytes(component.as_os_str())?);
    }
    Ok(prefix)
}

/// Lexically join a command-line path onto the prefix without touching the
/// filesystem; `..` may not leave the worktree. `.` for the root is kept.
fn repo_relative(prefix: &[u8], path: &BStr) -> Result<Vec<u8>> {
    if path.is_empty() {
        return Err("empty path is not a valid pathspec".into());
    }
    if path.starts_with(b"/") || path.starts_with(b":") {
        return Err(format!("unsupported pathspec: {path:?}").into());
    }
    if path.ends_with(b"/") && path.iter().any(|&byte| byte != b'/' && byte != b'.') {
        // A trailing slash names a directory; the caller verifies it is one.
        let end = path
            .iter()
            .rposition(|&byte| byte != b'/')
            .map_or(0, |end| end + 1);
        let mut joined = repo_relative(prefix, BStr::new(&path[..end]))?;
        joined.push(b'/');
        return Ok(joined);
    }
    let mut parts: Vec<&[u8]> = prefix
        .split(|&byte| byte == b'/')
        .filter(|part| !part.is_empty())
        .collect();
    let separators: &[u8] = if cfg!(windows) { b"/\\" } else { b"/" };
    for part in path.split(|byte| separators.contains(byte)) {
        match part {
            b"" | b"." => {}
            b".." => {
                parts
                    .pop()
                    .ok_or_else(|| format!("{path:?} is outside the repository"))?;
            }
            part => parts.push(part),
        }
    }
    Ok(if parts.is_empty() {
        b".".to_vec()
    } else {
        parts.join(&b'/')
    })
}

/// Identity from `GIT_<ROLE>_{NAME,EMAIL,DATE}`, else repository-local
/// `user.name`/`user.email`; the timestamp defaults to now in UTC.
fn identity(repo: &Repository, role: &str, now: i64) -> Result<Signature> {
    let lookup = |field: &str, key: &str| -> Result<Option<Vec<u8>>> {
        match std::env::var_os(format!("GIT_{role}_{field}")) {
            Some(value) => Ok(Some(os_bytes(&value)?.to_vec())),
            None => Ok(repo.config_bytes(key)),
        }
    };
    let name = lookup("NAME", "user.name")?;
    let email = lookup("EMAIL", "user.email")?;
    let (Some(name), Some(email)) = (
        name.map(|v| without_crud(&v)),
        email.map(|v| without_crud(&v)),
    ) else {
        return Err(format!(
            "{} identity unknown: set user.name and user.email in the repository config",
            role.to_ascii_lowercase()
        )
        .into());
    };
    if name.is_empty() {
        return Err("empty ident name not allowed".into());
    }
    let (seconds, offset_minutes) = match std::env::var(format!("GIT_{role}_DATE")) {
        Ok(date) => parse_date(&date).ok_or_else(|| format!("invalid date format: {date}"))?,
        Err(std::env::VarError::NotPresent) => (now, 0),
        Err(error) => return Err(error.into()),
    };
    Ok(Signature {
        name,
        email,
        seconds,
        offset_minutes,
    })
}

/// C Git's `strbuf_addstr_without_crud`: drop leading and trailing bytes
/// that are control/space or `,:;<>"'\\` (current C Git keeps `.`), and
/// remove `<`, `>` and newlines
/// inside, so an identity cannot break the header line.
fn without_crud(value: &[u8]) -> Vec<u8> {
    let crud = |byte: &u8| *byte <= 32 || b",:;<>\"\\'".contains(byte);
    let start = value.iter().position(|b| !crud(b)).unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|b| !crud(b))
        .map_or(start, |end| end + 1);
    value[start..end]
        .iter()
        .copied()
        .filter(|byte| !matches!(byte, b'<' | b'>' | b'\n'))
        .collect()
}

/// C Git's `strbuf_stripspace` without comment removal: strip trailing
/// whitespace from each line, drop leading and trailing blank lines, collapse
/// runs of blank lines, and end every line with a newline.
fn stripspace(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pending_blank = false;
    for line in text.split(|&byte| byte == b'\n') {
        let end = line
            .iter()
            .rposition(|byte| !byte.is_ascii_whitespace())
            .map_or(0, |end| end + 1);
        if end == 0 {
            pending_blank = !out.is_empty();
            continue;
        }
        if pending_blank {
            out.push(b'\n');
            pending_blank = false;
        }
        out.extend_from_slice(&line[..end]);
        out.push(b'\n');
    }
    out
}

/// Accept Git's internal `[@]<seconds> <±hhmm>` and ISO 8601
/// `YYYY-MM-DD[T ]HH:MM:SS` with `Z`, `±HH:MM` or `±HHMM`.
fn parse_date(date: &str) -> Option<(i64, i32)> {
    let date = date.trim();
    let offset = |zone: &str| -> Option<i32> {
        if zone == "Z" {
            return Some(0);
        }
        let sign = match zone.as_bytes().first()? {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let digits: String = zone[1..].chars().filter(|c| *c != ':').collect();
        if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let (hours, minutes) = (
            digits[..2].parse::<i32>().ok()?,
            digits[2..].parse::<i32>().ok()?,
        );
        (minutes < 60).then_some(sign * (hours * 60 + minutes))
    };
    let internal = date.strip_prefix('@').unwrap_or(date);
    if let Some((seconds, zone)) = internal.split_once(' ')
        && !seconds.is_empty()
        && seconds.bytes().all(|byte| byte.is_ascii_digit())
        && let Ok(seconds) = seconds.parse::<i64>()
    {
        return Some((seconds, offset(zone)?));
    }
    let bytes = date.as_bytes();
    if bytes.len() < 20 || !matches!(bytes[10], b'T' | b' ') {
        return None;
    }
    let number = |range: std::ops::Range<usize>| {
        let field = date.get(range)?;
        if !field.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        field.parse::<i64>().ok()
    };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let separators = [bytes[4], bytes[7], bytes[13], bytes[16]];
    if separators != [b'-', b'-', b':', b':']
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let zone = offset(&date[19..])?;
    // Days since the Unix epoch for a proleptic Gregorian date.
    let (y, m) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let day_of_year = (153 * m + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let local = days * 86_400 + hour * 3600 + minute * 60 + second;
    Some((local - i64::from(zone) * 60, zone))
}

fn write_path(out: &mut impl Write, path: &Path) -> Result<()> {
    let path = dunce::canonicalize(path)?;
    #[cfg(windows)]
    let path = PathBuf::from(
        path.to_str()
            .ok_or("path is not valid UTF-8")?
            .replace('\\', "/"),
    );
    out.write_all(os_bytes(path.as_os_str())?)?;
    writeln!(out)?;
    Ok(())
}

fn write_entry_path(out: &mut impl Write, path: &[u8], nul: bool, quote_space: bool) -> Result<()> {
    if nul {
        out.write_all(path)?;
        out.write_all(&[0])?;
        return Ok(());
    }
    let quote = path.iter().any(|&byte| {
        !(32..127).contains(&byte) || byte == b'"' || byte == b'\\' || (quote_space && byte == b' ')
    });
    if quote {
        out.write_all(b"\"")?;
    }
    for &byte in path {
        match byte {
            b'\n' => out.write_all(b"\\n")?,
            b'\r' => out.write_all(b"\\r")?,
            b'\t' => out.write_all(b"\\t")?,
            7 => out.write_all(b"\\a")?,
            8 => out.write_all(b"\\b")?,
            11 => out.write_all(b"\\v")?,
            12 => out.write_all(b"\\f")?,
            b'"' | b'\\' => {
                out.write_all(b"\\")?;
                out.write_all(&[byte])?;
            }
            0..=31 | 127..=255 => write!(out, "\\{byte:03o}")?,
            _ => out.write_all(&[byte])?,
        }
    }
    if quote {
        out.write_all(b"\"")?;
    }
    out.write_all(b"\n")?;
    Ok(())
}

#[cfg(unix)]
fn native_path(bytes: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

#[cfg(not(unix))]
fn native_path(bytes: &[u8]) -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::str::from_utf8(bytes).map_err(|_| "path is not valid UTF-8")?,
    ))
}

#[cfg(unix)]
fn os_bytes(value: &std::ffi::OsStr) -> Result<&BStr> {
    use std::os::unix::ffi::OsStrExt;
    Ok(BStr::new(value.as_bytes()))
}

#[cfg(not(unix))]
fn os_bytes(value: &std::ffi::OsStr) -> Result<&BStr> {
    // Refuse unrepresentable paths rather than silently replacing code units.
    Ok(BStr::new(
        value
            .to_str()
            .ok_or("path or revision is not valid UTF-8")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_match_git_internal_and_iso_forms() {
        // `date -u -d 2026-01-01T00:00:00Z +%s` = 1767225600.
        assert_eq!(parse_date("2026-01-01T00:00:00Z"), Some((1_767_225_600, 0)));
        assert_eq!(
            parse_date("2026-01-01 02:30:00+02:30"),
            Some((1_767_225_600, 150))
        );
        assert_eq!(
            parse_date("2025-12-31T19:00:00-0500"),
            Some((1_767_225_600, -300))
        );
        assert_eq!(parse_date("@1767225600 +0000"), Some((1_767_225_600, 0)));
        assert_eq!(parse_date("1767225600 -0130"), Some((1_767_225_600, -90)));
        assert_eq!(parse_date("2000-02-29T00:00:00Z"), Some((951_782_400, 0)));
        assert_eq!(parse_date("1969-12-31T23:59:59Z"), Some((-1, 0)));
        for invalid in [
            "",
            "yesterday",
            "2026-13-01T00:00:00Z",
            "2026-01-01T24:00:00Z",
            "2026-01-01T00:00:00",
            "2026-01-01T00:00:00+0060",
            "@1767225600 0000",
            "2026/01/01T00:00:00Z",
            "2026-01-01T-1:00:00Z",
            "2026-01-01T00:-5:00Z",
            "@@1767225600 +0000",
            "-5 +0000",
        ] {
            assert_eq!(parse_date(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn messages_follow_c_git_whitespace_cleanup() {
        assert_eq!(stripspace(b"a  \nb"), b"a\nb\n");
        assert_eq!(stripspace(b"\n\nsubject\n"), b"subject\n");
        assert_eq!(stripspace(b"a\n\n\n\nb\n"), b"a\n\nb\n");
        assert_eq!(stripspace(b"\nx\n"), b"x\n");
        assert_eq!(stripspace(b"  \n\t\n"), b"");
        assert_eq!(
            stripspace(b"keep  inner\tspace  \n\n"),
            b"keep  inner\tspace\n"
        );
    }

    #[test]
    fn identities_drop_crud_like_c_git() {
        assert_eq!(without_crud(b"Test "), b"Test");
        assert_eq!(without_crud(b"  ,Jane Doe,; "), b"Jane Doe");
        assert_eq!(without_crud(b" .C,;"), b".C");
        assert_eq!(without_crud(b"<user@example.com>"), b"user@example.com");
        assert_eq!(without_crud(b"a<b>c\nd"), b"abcd");
        assert_eq!(without_crud(b" ;,: "), b"");
    }

    #[test]
    fn pathspecs_join_lexically_and_stay_inside_the_worktree() -> Result<()> {
        let join = |prefix: &[u8], path: &[u8]| repo_relative(prefix, BStr::new(path));
        assert_eq!(join(b"", b".")?, b".");
        assert_eq!(join(b"", b"./a//b/")?, b"a/b/");
        assert_eq!(join(b"", b"./")?, b".");
        assert_eq!(join(b"sub/dir", b"../x")?, b"sub/x");
        assert_eq!(join(b"sub", b"..")?, b".");
        assert_eq!(join(b"", b"caf\xe9")?, b"caf\xe9");
        assert!(join(b"sub", b"../..").is_err());
        assert!(join(b"", b"/abs").is_err());
        assert!(join(b"", b":(glob)*").is_err());
        assert!(join(b"", b"").is_err());
        assert_eq!(join(b"sub", b"dir/")?, b"sub/dir/");
        Ok(())
    }
}
