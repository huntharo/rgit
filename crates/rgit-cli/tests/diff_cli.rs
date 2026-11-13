//! CLI formatting and exit semantics, independently compared with C Git.
use std::fs;

mod support;
use support::*;

#[test]
fn diff_names_nul_and_exit_codes_match_c_git() -> Result {
    for hash in ["sha1", "sha256"] {
        let root = fixture(hash)?;
        for args in [
            vec!["diff", "--quiet"],
            vec!["diff", "--exit-code"],
            vec!["diff", "--cached", "--quiet"],
        ] {
            same(root.path(), &args)?;
        }
        fs::write(root.path().join("file.txt"), b"changed\n")?;
        let quoted = if cfg!(windows) {
            "plain name"
        } else {
            "quote\"name"
        };
        for name in ["space name", quoted, "new.txt"] {
            fs::write(root.path().join(name), b"staged\n")?;
            git(root.path(), &["add", name])?;
            fs::write(root.path().join(name), b"unstaged\n")?;
        }
        for cached in [false, true] {
            for mode in ["--name-only", "--name-status", "--quiet"] {
                for nul in [false, true] {
                    let mut args = vec![
                        "diff",
                        "--no-renames",
                        "--no-ext-diff",
                        "--no-textconv",
                        mode,
                    ];
                    if cached {
                        args.push("--cached");
                    }
                    if nul {
                        args.push("-z");
                    }
                    args.push("--exit-code");
                    same(root.path(), &args)?;
                }
            }
            let mut args = vec!["diff", "--quiet"];
            if cached {
                args.push("--staged");
            }
            same(root.path(), &args)?;
        }
    }
    Ok(())
}

#[test]
fn diff_patch_and_errors_never_report_partial_success() -> Result {
    let root = fixture("sha1")?;
    fs::write(root.path().join("file.txt"), b"changed\n")?;
    let patch = rgit(root.path(), &["diff", "--exit-code"])?;
    assert_eq!(patch.status.code(), Some(1));
    assert!(patch.stdout.starts_with(b"diff --git "));
    assert!(patch.stderr.is_empty());
    let normal = rgit(root.path(), &["diff"])?;
    assert!(normal.status.success());
    assert_eq!(normal.stdout, patch.stdout);
    git(
        root.path(),
        &["config", "filter.unsafe.clean", "never-execute"],
    )?;
    for args in [
        vec!["diff"],
        vec!["diff", "--quiet"],
        vec!["diff", "--name-status"],
    ] {
        let output = rgit(root.path(), &args)?;
        assert_eq!(output.status.code(), Some(128), "{args:?}");
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    for args in [
        vec!["diff", "HEAD"],
        vec!["diff", "--", "file.txt"],
        vec!["diff", "--name-only", "--name-status"],
    ] {
        let output = rgit(root.path(), &args)?;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
    Ok(())
}
