//! CLI argument handling for the experimental write commands, compared with
//! C Git on twin repositories (message cleanup, identity trimming, dates,
//! pathspec forms).

#[allow(dead_code)]
mod support;

use std::fs;
use std::path::Path;
use std::process::Command;

use support::{Result, git, git_command};
use tempfile::tempdir;

const IDENTITY: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "  Test Author, "),
    ("GIT_AUTHOR_EMAIL", " <author@example.invalid>"),
    ("GIT_AUTHOR_DATE", "2026-01-01T00:00:00+02:30"),
    ("GIT_COMMITTER_NAME", " .Committer;"),
    ("GIT_COMMITTER_EMAIL", "committer@example.invalid"),
    ("GIT_COMMITTER_DATE", "@1767225600 -0130"),
];

fn twin(root: &Path, name: &str) -> Result<std::path::PathBuf> {
    let repo = root.join(name);
    git(root, &["init", "-q", "-b", "main", name])?;
    git(&repo, &["config", "maintenance.auto", "false"])?;
    fs::write(repo.join("file.txt"), b"content\n")?;
    git(&repo, &["add", "file.txt"])?;
    Ok(repo)
}

fn rgit_command(repo: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rgit"));
    command.arg("-C").arg(repo);
    for (key, value) in IDENTITY {
        command.env(key, value);
    }
    command
}

#[test]
fn commit_ids_match_c_git_after_message_and_identity_cleanup() -> Result {
    let messages: [&[&str]; 5] = [
        &["a  \nb"],
        &["\n\nsubject\n\nbody  \n\n\n"],
        &["a\n\n\n\nb"],
        &["", "x"],
        &["first", "second\n", "  third  "],
    ];
    for parts in messages {
        let root = tempdir()?;
        let c = twin(root.path(), "c")?;
        let r = twin(root.path(), "r")?;
        let mut args = vec!["commit", "-q"];
        for part in parts {
            args.extend(["-m", part]);
        }
        let mut c_git = git_command(&c);
        for (key, value) in IDENTITY {
            c_git.env(key, value);
        }
        let output = c_git.args(&args).output()?;
        assert!(output.status.success(), "{parts:?}: {output:?}");
        let output = rgit_command(&r).args(&args).output()?;
        assert!(output.status.success(), "{parts:?}: {output:?}");
        let id = |repo: &Path| -> Result<Vec<u8>> {
            Ok(git(repo, &["cat-file", "commit", "HEAD"])?.stdout)
        };
        assert_eq!(
            String::from_utf8_lossy(&id(&c)?),
            String::from_utf8_lossy(&id(&r)?),
            "{parts:?}"
        );
    }
    Ok(())
}

#[test]
fn whitespace_only_message_and_bad_dates_fail_before_any_write() -> Result {
    let root = tempdir()?;
    let repo = twin(root.path(), "r")?;
    let objects = || -> Result<Vec<u8>> { Ok(git(&repo, &["count-objects", "-v"])?.stdout) };
    let before = objects()?;
    let output = rgit_command(&repo)
        .args(["commit", "-m", " \n\t", "-m", ""])
        .output()?;
    assert_eq!(output.status.code(), Some(128), "{output:?}");
    for date in ["2026-01-01T-1:00:00Z", "@@1767225600 +0000", "tomorrow"] {
        let output = rgit_command(&repo)
            .env("GIT_AUTHOR_DATE", date)
            .args(["commit", "-m", "x"])
            .output()?;
        assert_eq!(output.status.code(), Some(128), "{date}: {output:?}");
    }
    assert_eq!(before, objects()?);
    assert!(
        git_command(&repo)
            .args(["rev-parse", "--verify", "HEAD"])
            .output()?
            .status
            .code()
            != Some(0)
    );
    Ok(())
}

#[test]
fn trailing_slash_requires_a_directory() -> Result {
    let root = tempdir()?;
    let repo = twin(root.path(), "r")?;
    fs::create_dir(repo.join("dir"))?;
    fs::write(repo.join("dir/inner.txt"), b"inner\n")?;
    let index = fs::read(repo.join(".git/index"))?;
    let output = rgit_command(&repo).args(["add", "file.txt/"]).output()?;
    assert_eq!(output.status.code(), Some(128), "{output:?}");
    assert_eq!(index, fs::read(repo.join(".git/index"))?);
    let output = rgit_command(&repo).args(["add", "dir/"]).output()?;
    assert!(output.status.success(), "{output:?}");
    let listed = git(&repo, &["ls-files"])?.stdout;
    assert_eq!(listed, b"dir/inner.txt\nfile.txt\n");
    Ok(())
}
