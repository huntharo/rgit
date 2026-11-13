//! Bare init into an existing EMPTY directory (ADR 0003, WP1): the directory
//! is populated in place (inode and mode, including setgid, preserved) and
//! HEAD is published last, so an interruption never leaves something Git
//! recognizes as a repository. An absent target still goes through the
//! private-directory publication (`init.before-publish`).

#[allow(dead_code)]
mod lifecycle_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use lifecycle_support::crash::{expect_abort, expect_injected, with_failpoint};
use lifecycle_support::driver::{self, Driver, ok_quiet, refused};
use lifecycle_support::model::Hash;
use lifecycle_support::observe;
use lifecycle_support::proc::{
    Preserve, Result, Snap, assert_preserved, describe, git_cmd, git_text, home_for, isolate, run,
    sandbox, snapshot,
};

#[cfg(unix)]
fn identity_of(path: &Path) -> Result<(u64, u32)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(path)?;
    Ok((metadata.ino(), metadata.mode() & 0o7777))
}

#[cfg(not(unix))]
fn identity_of(_path: &Path) -> Result<(u64, u32)> {
    Ok((0, 0))
}

/// An empty directory with mode 2775 (setgid), as a shared bare repository
/// directory is commonly prepared.
fn empty_target(parent: &Path, name: &str) -> Result<PathBuf> {
    let target = parent.join(name);
    fs::create_dir(&target)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o2775))?;
    }
    Ok(target)
}

fn bare_args(hash: Hash, target: &str) -> Vec<String> {
    [
        "-q",
        "--bare",
        "-b",
        "main",
        "--object-format",
        hash.name(),
        target,
    ]
    .map(String::from)
    .to_vec()
}

/// A valid, empty, bare repository at `git_dir`, checked by C Git.
fn assert_valid_bare(git_dir: &Path, hash: Hash) -> Result {
    assert_eq!(
        git_text(git_dir, &["rev-parse", "--is-bare-repository"])?,
        "true"
    );
    assert_eq!(
        git_text(git_dir, &["rev-parse", "--show-object-format"])?,
        hash.name()
    );
    assert_eq!(
        git_text(git_dir, &["config", "--get", "core.bare"])?,
        "true"
    );
    assert_eq!(
        observe::symbolic_head(git_dir)?.as_deref(),
        Some("refs/heads/main")
    );
    assert!(observe::branches(git_dir)?.is_empty());
    let locks = observe::locks(git_dir)?;
    if !locks.is_empty() {
        return Err(format!("locks after init: {locks:?}").into());
    }
    let stray: Vec<_> = fs::read_dir(git_dir)?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|n| n.contains("rgit-init"))
        .collect();
    if !stray.is_empty() {
        return Err(format!("temporary init entries remain: {stray:?}").into());
    }
    observe::fsck(git_dir)
}

/// Neither rgit nor C Git recognizes a repository at `dir`.
fn not_a_repository(dir: &Path) -> Result {
    let rgit = run(Driver::Rgit
        .command(dir)
        .args(["rev-parse", "--absolute-git-dir"]))?;
    let cgit = run(git_cmd(dir).args(["rev-parse", "--absolute-git-dir"]))?;
    if rgit.status.success() || cgit.status.success() {
        return Err(format!(
            "{} was discovered as a repository: rgit {} / C Git {}",
            dir.display(),
            describe(&rgit),
            describe(&cgit)
        )
        .into());
    }
    Ok(())
}

fn populated_in_place(driver: Driver) -> Result {
    if driver::lacks(driver, &["init"], "populated_in_place") {
        return Ok(());
    }
    for hash in Hash::ALL {
        let sandbox = sandbox()?;
        let target = empty_target(sandbox.path(), "shared.git")?;
        let identity = identity_of(&target)?;
        #[cfg(unix)]
        if identity.1 != 0o2775 {
            eprintln!(
                "SKIP: setgid subcase: filesystem reports mode {:o} after chmod 2775",
                identity.1
            );
        }
        let output = driver.init(sandbox.path(), &bare_args(hash, "shared.git"))?;
        ok_quiet(&output, "bare init into an empty directory")?;
        assert_eq!(identity_of(&target)?, identity, "inode/mode not preserved");
        assert_valid_bare(&target, hash)?;

        // Through a symlink to an empty directory: the link stays a link and
        // the directory it names is populated in place.
        let real = empty_target(sandbox.path(), "real.git")?;
        let real_identity = identity_of(&real)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, sandbox.path().join("link.git"))?;
        #[cfg(unix)]
        {
            let output = driver.init(sandbox.path(), &bare_args(hash, "link.git"))?;
            ok_quiet(&output, "bare init through a symlink")?;
            assert!(fs::symlink_metadata(sandbox.path().join("link.git"))?.is_symlink());
            assert_eq!(identity_of(&real)?, real_identity);
            assert_valid_bare(&real, hash)?;
        }
        let _ = real_identity;
    }
    Ok(())
}

/// `rgit init --bare` with no path, run with the empty target as the
/// process's current directory: succeeds, and that working directory stays
/// valid (same inode, now listing the repository) for the caller.
#[cfg(unix)]
fn init_from_inside_the_target(driver: Driver) -> Result {
    if driver::lacks(driver, &["init"], "init_from_inside_the_target") {
        return Ok(());
    }
    for hash in Hash::ALL {
        let sandbox = sandbox()?;
        let target = empty_target(sandbox.path(), "cwd.git")?;
        let identity = identity_of(&target)?;
        // A long-lived "shell": it chdirs into the target, runs rgit with no
        // -C and no path, then proves its own cwd is still valid.
        let script = r#"
import os, subprocess, sys
before = os.stat(".").st_ino
result = subprocess.run(sys.argv[1:], capture_output=True, timeout=30)
sys.stderr.buffer.write(result.stderr)
assert result.returncode == 0, result.returncode
assert os.stat(".").st_ino == before, "cwd inode changed"
assert os.path.samefile(os.getcwd(), "."), "cwd no longer resolves"
assert "HEAD" in os.listdir("."), os.listdir(".")
print("CWD-OK")
"#;
        let mut command = Command::new("python3");
        command
            .arg("-I")
            .arg("-c")
            .arg(script)
            .arg(env!("CARGO_BIN_EXE_rgit"))
            .args([
                "init",
                "--bare",
                "-q",
                "-b",
                "main",
                "--object-format",
                hash.name(),
            ])
            .current_dir(&target);
        isolate(&mut command, &home_for(&target));
        let output = run(&mut command)?;
        if !output.status.success() || output.stdout != b"CWD-OK\n" {
            return Err(format!("init from inside the target: {}", describe(&output)).into());
        }
        assert_eq!(identity_of(&target)?, identity);
        assert_valid_bare(&target, hash)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn init_from_inside_the_target(_: Driver) -> Result {
    eprintln!("SKIP: init_from_inside_the_target requires Unix (python cwd harness)");
    Ok(())
}

/// Abort at `init.before-head`: config, objects/ and refs/ are populated but
/// there is no HEAD, so neither rgit nor C Git sees a repository; a retry is
/// refused because the directory is no longer empty, changing nothing.
fn before_head_abort_leaves_a_non_repository(driver: Driver) -> Result {
    if driver::lacks(driver, &["init"], "before_head_abort") {
        return Ok(());
    }
    for hash in Hash::ALL {
        let sandbox = sandbox()?;
        fs::write(sandbox.path().join("user.txt"), b"user data\n")?;
        let target = empty_target(sandbox.path(), "shared.git")?;
        let identity = identity_of(&target)?;
        let before = snapshot(sandbox.path())?;
        let mut command = driver.init_cmd(sandbox.path(), &bare_args(hash, "shared.git"));
        with_failpoint(&mut command, "init.before-head");
        expect_abort(&run(&mut command)?, "init.before-head")?;

        assert_eq!(identity_of(&target)?, identity, "inode/mode not preserved");
        let after = snapshot(sandbox.path())?;
        // Outside the target nothing changed.
        let outside = |snap: &std::collections::BTreeMap<PathBuf, Snap>| {
            snap.iter()
                .filter(|(p, _)| !p.starts_with("shared.git") || p.as_os_str() == "shared.git")
                .map(|(p, v)| (p.clone(), v.clone()))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        assert_preserved(
            sandbox.path(),
            &outside(&before),
            &outside(&after),
            Preserve::Exact,
            "outside",
        )?;
        let populated: Vec<String> = after
            .keys()
            .filter_map(|p| p.strip_prefix("shared.git").ok())
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        for required in ["config", "objects", "refs"] {
            if !populated.iter().any(|p| p == required) {
                return Err(format!(
                    "before-head abort did not populate {required}: {populated:?}"
                )
                .into());
            }
        }
        if populated
            .iter()
            .any(|p| p == "HEAD" || p.starts_with("HEAD"))
        {
            return Err(format!(
                "HEAD (or its temporary) exists before init.before-head: {populated:?}"
            )
            .into());
        }
        let config = fs::read_to_string(target.join("config"))?;
        if !config.contains("bare = true") {
            return Err(format!("populated config is not bare: {config:?}").into());
        }
        eprintln!(
            "NOTE: {} init.before-head abort populated {populated:?}",
            hash.name()
        );
        not_a_repository(&target)?;

        // Retry: refused (directory not empty), byte-identical.
        let stable = snapshot(sandbox.path())?;
        let output = driver.init(sandbox.path(), &bare_args(hash, "shared.git"))?;
        refused(&output, 128, "retry over a partially populated directory")?;
        assert_preserved(
            sandbox.path(),
            &stable,
            &snapshot(sandbox.path())?,
            Preserve::Exact,
            "retry",
        )?;
        assert_eq!(identity_of(&target)?, identity);
    }
    Ok(())
}

/// `init.before-head:error` (handled): the call undoes what it created, so
/// the directory is empty again (same inode and mode), then a retry works.
fn before_head_error_restores_an_empty_directory(driver: Driver) -> Result {
    if driver::lacks(driver, &["init"], "before_head_error") {
        return Ok(());
    }
    for hash in Hash::ALL {
        let sandbox = sandbox()?;
        let target = empty_target(sandbox.path(), "shared.git")?;
        let identity = identity_of(&target)?;
        let before = snapshot(sandbox.path())?;
        let mut command = driver.init_cmd(sandbox.path(), &bare_args(hash, "shared.git"));
        with_failpoint(&mut command, "init.before-head:error");
        expect_injected(&run(&mut command)?, "init.before-head")?;
        assert_preserved(
            sandbox.path(),
            &before,
            &snapshot(sandbox.path())?,
            Preserve::Exact,
            "handled error",
        )?;
        assert_eq!(
            fs::read_dir(&target)?.count(),
            0,
            "directory not empty again"
        );
        assert_eq!(identity_of(&target)?, identity);
        not_a_repository(&target)?;
        ok_quiet(
            &driver.init(sandbox.path(), &bare_args(hash, "shared.git"))?,
            "retry after handled error",
        )?;
        assert_eq!(identity_of(&target)?, identity);
        assert_valid_bare(&target, hash)?;
    }
    Ok(())
}

/// The publication path is chosen by the target: an absent target never
/// reaches `init.before-head` and does stop at `init.before-publish`; an
/// existing empty target never reaches `init.before-publish`.
fn absent_target_uses_before_publish(driver: Driver) -> Result {
    if driver::lacks(driver, &["init"], "absent_target_uses_before_publish") {
        return Ok(());
    }
    for hash in Hash::ALL {
        let sandbox = sandbox()?;
        let mut command = driver.init_cmd(sandbox.path(), &bare_args(hash, "absent.git"));
        with_failpoint(&mut command, "init.before-head");
        ok_quiet(
            &run(&mut command)?,
            "absent target armed at init.before-head",
        )?;
        assert_valid_bare(&sandbox.path().join("absent.git"), hash)?;

        let mut command = driver.init_cmd(sandbox.path(), &bare_args(hash, "absent2.git"));
        with_failpoint(&mut command, "init.before-publish");
        expect_abort(&run(&mut command)?, "init.before-publish")?;
        if fs::symlink_metadata(sandbox.path().join("absent2.git")).is_ok() {
            return Err("absent target published before init.before-publish".into());
        }

        let target = empty_target(sandbox.path(), "empty.git")?;
        let identity = identity_of(&target)?;
        let mut command = driver.init_cmd(sandbox.path(), &bare_args(hash, "empty.git"));
        with_failpoint(&mut command, "init.before-publish");
        ok_quiet(
            &run(&mut command)?,
            "empty target armed at init.before-publish",
        )?;
        assert_eq!(identity_of(&target)?, identity);
        assert_valid_bare(&target, hash)?;
    }
    Ok(())
}

rgit_only!(
    populated_in_place,
    init_from_inside_the_target,
    before_head_abort_leaves_a_non_repository,
    before_head_error_restores_an_empty_directory,
    absent_target_uses_before_publish,
);
