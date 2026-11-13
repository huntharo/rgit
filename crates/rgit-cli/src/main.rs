use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{ArgGroup, Args, Parser, Subcommand};
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
        #[arg(short = 'u', long, default_value = "normal", num_args = 0..=1, default_missing_value = "all", require_equals = true)]
        untracked_files: String,
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
    let repo = Repository::discover(directory)?;
    match cli.command {
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
        byte < 32 || byte >= 127 || byte == b'"' || byte == b'\\' || (quote_space && byte == b' ')
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
