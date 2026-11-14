//! Decision 0004 on Unix: with separate `GitDir` and `CommonDir` anchors, add
//! and commit in a linked worktree write its own index, `HEAD` and logs and
//! the shared objects and branch, exactly where C Git expects them.
#![cfg(unix)]

mod common;

use std::fs;

use common::{Result, fsck, git};
use rgit_core::write::{CommitRequest, InitOptions, ObjectFormat, Signature, init};
use rgit_core::{BStr, Repository};

fn text(root: &std::path::Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git(root, args)?.stdout)?
        .trim_end()
        .to_owned())
}

#[test]
fn add_and_commit_in_a_linked_worktree_use_the_right_roots() -> Result {
    for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
        let temp = tempfile::tempdir()?;
        let main = temp.path().join("main");
        init(
            &main,
            &InitOptions {
                object_format: format,
                ..InitOptions::default()
            },
        )?;
        git(&main, &["config", "maintenance.auto", "false"])?;
        fs::write(main.join("base"), b"base\n")?;
        git(&main, &["add", "base"])?;
        git(&main, &["commit", "-q", "-m", "base"])?;
        let main_head = text(&main, &["rev-parse", "HEAD"])?;
        let main_index = fs::read(main.join(".git/index"))?;
        let main_log = fs::read(main.join(".git/logs/HEAD"))?;

        let linked = temp.path().join("linked");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                &linked.to_string_lossy(),
            ],
        )?;
        fs::write(linked.join("feature.txt"), b"feature\n")?;
        let repo = Repository::discover(&linked)?;
        repo.add(&[BStr::new("feature.txt")])?;
        let signature = Signature {
            name: b"Test".to_vec(),
            email: b"test@example.invalid".to_vec(),
            seconds: 1_767_225_600,
            offset_minutes: 0,
        };
        let outcome = repo.commit(&CommitRequest {
            message: b"feature\n".to_vec(),
            author: signature.clone(),
            committer: signature,
            allow_empty: false,
        })?;

        assert_eq!(outcome.reference, b"refs/heads/feature");
        assert_eq!(
            text(&linked, &["rev-parse", "HEAD"])?,
            outcome.id.to_string()
        );
        assert_eq!(
            text(&main, &["rev-parse", "feature"])?,
            outcome.id.to_string()
        );
        assert_eq!(text(&linked, &["status", "--porcelain"])?, "");
        // The main worktree's HEAD, index and HEAD log are untouched.
        assert_eq!(text(&main, &["rev-parse", "HEAD"])?, main_head);
        assert_eq!(fs::read(main.join(".git/index"))?, main_index);
        assert_eq!(fs::read(main.join(".git/logs/HEAD"))?, main_log);
        // The linked worktree's own HEAD log and the shared branch log.
        let worktree_log = text(&linked, &["reflog", "-1", "--format=%gs", "HEAD"])?;
        assert_eq!(worktree_log, "commit: feature");
        let branch_log = text(&main, &["reflog", "-1", "--format=%gs", "feature"])?;
        assert_eq!(branch_log, "commit: feature");
        assert!(
            main.join(".git/objects")
                .join(&outcome.id.to_string()[..2])
                .is_dir()
        );
        fsck(&main)?;
    }
    Ok(())
}
