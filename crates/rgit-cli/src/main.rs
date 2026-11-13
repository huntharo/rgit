use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{ArgGroup, Args, Parser, Subcommand};
use rgit_core::{BStr, ObjectKind, Repository};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(version, about = "An experimental rewrite of Git using Rust.")]
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
