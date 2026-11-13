//! Write-path correctness gates: native APFS MAP_SHARED changes (live and
//! after final unmap) must be staged and committed by fresh processes, and
//! special files must never hang or be staged.

#[allow(dead_code)]
mod lifecycle_support;

use lifecycle_support::Scenario;
use lifecycle_support::driver::Driver;
use lifecycle_support::model::Hash;
use lifecycle_support::proc::Result;

// ---------------------------------------------------------------------------
// Native APFS shared-mapping gates (macOS only).

#[cfg(target_os = "macos")]
mod apfs {
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::time::{Duration, SystemTime};

    use super::*;
    use lifecycle_support::proc::{describe, home_for, identity, isolate, run_within};

    pub struct Evidence {
        pub skipped: Option<String>,
        pub stat: String,
        pub results: Vec<(i32, String)>,
    }

    fn unhex(text: &str) -> Vec<u8> {
        lifecycle_support::model::unhex(text)
    }

    /// Commit an 8 KiB file with C Git, backdating its mtime so the index
    /// entry is not racily clean: only content inspection can see a change.
    pub fn mapped_fixture(hash: Hash) -> Result<Scenario> {
        let mut s = Scenario::init(Driver::CGit, hash, "main")?;
        s.model.write(0, b"mapped", &vec![b'A'; 8192])?;
        let past = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        fs::File::options()
            .write(true)
            .open(s.root().join("mapped"))?
            .set_modified(past)?;
        s.add(0, &[b"mapped"])?;
        s.commit(0, &["baseline"])?;
        Ok(s)
    }

    pub fn changed_bytes() -> Vec<u8> {
        vec![b'A' ^ 0x01; 8192]
    }

    /// Run the helper, executing `commands` through the mapping phase.
    pub fn map_and_run(
        root: &Path,
        phase: &str,
        when: i64,
        commands: &[Vec<String>],
    ) -> Result<Evidence> {
        let mut command = Command::new("python3");
        command
            .arg("-I")
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/lifecycle_support/apfs_write_gate.py"),
            )
            .arg(root)
            .arg(phase);
        for argv in commands {
            command.arg("::command::").args(argv);
        }
        isolate(&mut command, &home_for(root));
        identity(&mut command, when);
        let output = run_within(&mut command, Duration::from_secs(120))?;
        if !output.status.success() {
            return Err(format!("helper failed: {}", describe(&output)).into());
        }
        let text = String::from_utf8(output.stdout)?;
        let mut evidence = Evidence {
            skipped: None,
            stat: String::new(),
            results: Vec::new(),
        };
        let mut passed = false;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("STATUS SKIP ") {
                evidence.skipped = Some(rest.to_owned());
            } else if line.starts_with("STATUS PASS") {
                passed = true;
            } else if let Some(rest) = line.strip_prefix("STAT ") {
                evidence.stat = rest.to_owned();
            } else if let Some(rest) = line.strip_prefix("RESULT ") {
                let (code, stderr) = rest.split_once(' ').unwrap_or((rest, ""));
                evidence.results.push((
                    code.parse()?,
                    String::from_utf8_lossy(&unhex(stderr)).into_owned(),
                ));
            }
        }
        if evidence.skipped.is_none() && (!passed || evidence.results.len() != commands.len()) {
            return Err(format!("incomplete helper evidence: {text}").into());
        }
        Ok(evidence)
    }

    /// Both stat fields untouched means only content can reveal the change.
    pub fn stat_preserved(stat: &str) -> bool {
        let f: Vec<&str> = stat.split(' ').collect();
        f.len() == 6 && f[1] == f[2] && f[4] == f[5]
    }

    pub fn gate(driver: Driver, phase: &str) -> Result {
        for hash in Hash::ALL {
            let mut s = mapped_fixture(hash)?;
            s.driver = driver;
            let when = s.model.tick();
            let root = s.root();
            let commands = match driver {
                Driver::Rgit => vec![
                    vec![
                        env!("CARGO_BIN_EXE_rgit").into(),
                        "-C".into(),
                        root.display().to_string(),
                        "add".into(),
                        "--".into(),
                        "mapped".into(),
                    ],
                    vec![
                        env!("CARGO_BIN_EXE_rgit").into(),
                        "-C".into(),
                        root.display().to_string(),
                        "commit".into(),
                        "-q".into(),
                        "-m".into(),
                        "mapped".into(),
                    ],
                ],
                Driver::CGit => vec![
                    vec![
                        "git".into(),
                        "-C".into(),
                        root.display().to_string(),
                        "add".into(),
                        "--".into(),
                        "mapped".into(),
                    ],
                    vec![
                        "git".into(),
                        "-C".into(),
                        root.display().to_string(),
                        "commit".into(),
                        "-q".into(),
                        "-m".into(),
                        "mapped".into(),
                    ],
                ],
            };
            let evidence = map_and_run(&root, phase, when, &commands)?;
            if let Some(reason) = evidence.skipped {
                eprintln!("SKIP: {reason}");
                return Ok(());
            }
            let preserved = stat_preserved(&evidence.stat);
            assert_eq!(fs::read(root.join("mapped"))?, changed_bytes());
            s.model.observe_bytes(0, b"mapped", &changed_bytes())?;
            let staged_new = lifecycle_support::observe::index(&root)?[&b"mapped"[..]].oid
                == hash.object_hex("blob", &changed_bytes());
            if driver == Driver::CGit {
                // Reference only: C Git trusts matching stat data, so with
                // preserved mtime/ctime it is expected to miss the change.
                eprintln!(
                    "INFO: C Git {phase} {}: stat preserved={preserved} ({}); staged new bytes={staged_new}; results {:?}",
                    hash.name(),
                    evidence.stat,
                    evidence.results
                );
                continue;
            }
            for (code, stderr) in &evidence.results {
                if *code != 0 {
                    return Err(format!("{phase}: rgit exited {code}: {stderr}").into());
                }
            }
            s.model.stage(0, &[b"mapped"])?;
            s.model.commit(0, &["mapped"], when);
            s.check()?;
            eprintln!(
                "PASS: APFS {phase} write gate {}: stat preserved={preserved} ({}); fresh rgit add+commit stored the mapped bytes",
                hash.name(),
                evidence.stat
            );
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn apfs_live_mapping_add_commit(driver: Driver) -> Result {
    if lifecycle_support::driver::lacks(driver, &["add", "commit"], "apfs_live_mapping_add_commit")
    {
        return Ok(());
    }
    apfs::gate(driver, "live")
}

#[cfg(target_os = "macos")]
fn apfs_final_unmap_add_commit(driver: Driver) -> Result {
    if lifecycle_support::driver::lacks(driver, &["add", "commit"], "apfs_final_unmap_add_commit") {
        return Ok(());
    }
    apfs::gate(driver, "final-unmap")
}

#[cfg(not(target_os = "macos"))]
fn apfs_live_mapping_add_commit(_: Driver) -> Result {
    eprintln!("SKIP: native APFS live-mapping write gate requires macOS");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn apfs_final_unmap_add_commit(_: Driver) -> Result {
    eprintln!("SKIP: native APFS final-unmap write gate requires macOS");
    Ok(())
}

rgit_only!(apfs_live_mapping_add_commit, apfs_final_unmap_add_commit);

/// C Git under the same mapping, printed for reference (never asserted on
/// staging, since C Git's stat trust is the behavior the gate guards against).
#[test]
fn apfs_c_git_reference_behavior() -> Result {
    #[cfg(target_os = "macos")]
    for phase in ["live", "final-unmap"] {
        apfs::gate(Driver::CGit, phase)?;
    }
    #[cfg(not(target_os = "macos"))]
    eprintln!("SKIP: native APFS reference run requires macOS");
    Ok(())
}

// ---------------------------------------------------------------------------
// Special files: bounded, never staged.

#[cfg(unix)]
mod special {
    use std::fs;
    use std::os::unix::net::UnixListener;
    use std::path::Path;
    use std::process::{Command, Output};
    use std::time::Duration;

    use super::*;
    use lifecycle_support::driver::refused;
    use lifecycle_support::observe;
    use lifecycle_support::proc::{Preserve, assert_preserved, describe, run_within, snapshot};

    const BOUND: Duration = Duration::from_secs(10);

    fn mkfifo(path: &Path) -> Result {
        let output = Command::new("mkfifo").arg(path).output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!("mkfifo: {}", describe(&output)).into())
        }
    }

    fn add_bounded(driver: Driver, root: &Path, paths: &[&str]) -> Result<Output> {
        run_within(&mut driver.add_cmd(root, paths), BOUND).map_err(|e| {
            format!(
                "{} add {paths:?} did not finish within {BOUND:?}: {e}",
                driver.name()
            )
            .into()
        })
    }

    fn assert_no_special_entries(root: &Path, names: &[&[u8]]) -> Result {
        let index = observe::index(root)?;
        for name in names {
            if index.contains_key(*name) {
                return Err(format!(
                    "special file {:?} was staged",
                    String::from_utf8_lossy(name)
                )
                .into());
            }
        }
        Ok(())
    }

    /// Explicit FIFO/socket paths: rgit refuses (exit 128, nothing changes);
    /// C Git silently skips them. Neither may block or stage them.
    pub fn explicit(driver: Driver) -> Result {
        for socket in [false, true] {
            let mut s = Scenario::init(driver, Hash::Sha1, "main")?;
            s.model.write(0, b"regular", b"regular\n")?;
            s.add(0, &[b"regular"])?;
            s.commit(0, &["base"])?;
            let special = s.root().join("special");
            let _listener = if socket {
                Some(UnixListener::bind(&special)?)
            } else {
                mkfifo(&special)?;
                None
            };
            s.model.write(0, b"regular", b"regular v2\n")?;
            for paths in [&["special"][..], &["regular", "special"][..]] {
                let before = snapshot(s.dir())?;
                let output = add_bounded(driver, &s.root(), paths)?;
                let context = format!("{} add {paths:?} socket={socket}", driver.name());
                assert_no_special_entries(&s.root(), &[b"special"])?;
                if driver.refuses_explicit_special_file() {
                    refused(&output, 128, &context)?;
                    let preserve = if paths.len() == 1 {
                        Preserve::Exact
                    } else {
                        Preserve::AllowNewObjects
                    };
                    assert_preserved(s.dir(), &before, &snapshot(s.dir())?, preserve, &context)?;
                    if observe::index(&s.root())? != s.model.checkouts[0].index {
                        return Err(format!("{context}: refused add changed the index").into());
                    }
                } else {
                    lifecycle_support::driver::ok(&output, &context)?;
                    if paths.contains(&"regular") {
                        s.model.stage(0, &[b"regular"])?;
                    }
                    if observe::index(&s.root())? != s.model.checkouts[0].index {
                        return Err(format!("{context}: index differs from model").into());
                    }
                }
            }
            drop(_listener);
            fs::remove_file(&special)?;
            s.check()?;
        }
        Ok(())
    }

    /// A directory pathspec (and ".") containing a FIFO and a socket must
    /// finish promptly, never stage either, and be all-or-nothing.
    pub fn directory(driver: Driver) -> Result {
        for spec in ["dir", "."] {
            let mut s = Scenario::init(driver, Hash::Sha1, "main")?;
            s.model.write(0, b"dir/regular", b"regular\n")?;
            let fifo = s.root().join("dir/fifo");
            mkfifo(&fifo)?;
            let socket = s.root().join("dir/socket");
            let listener = UnixListener::bind(&socket)?;
            let before = snapshot(s.dir())?;
            let output = add_bounded(driver, &s.root(), &[spec])?;
            assert_no_special_entries(&s.root(), &[b"dir/fifo", b"dir/socket"])?;
            let context = format!("{} add {spec} with FIFO+socket", driver.name());
            match output.status.code() {
                Some(0) => {
                    s.model.stage(0, &[spec.as_bytes()])?;
                    eprintln!("NOTE: {context}: skipped special files and staged the rest");
                }
                Some(128) => {
                    assert_preserved(
                        s.dir(),
                        &before,
                        &snapshot(s.dir())?,
                        Preserve::AllowNewObjects,
                        &context,
                    )?;
                    eprintln!("NOTE: {context}: refused the whole operation");
                }
                _ => return Err(format!("{context}: {}", describe(&output)).into()),
            }
            if observe::index(&s.root())? != s.model.checkouts[0].index {
                return Err(format!("{context}: index differs from model").into());
            }
            drop(listener);
            fs::remove_file(&socket)?;
            fs::remove_file(&fifo)?;
            s.check()?;
        }
        Ok(())
    }

    /// A FIFO `.gitattributes`/`.gitignore` in the root or any ancestor of a
    /// staged path (or inside a staged directory) makes `add` refuse
    /// promptly with exit 128 and every byte unchanged, for a modified
    /// tracked file, a new file and a directory pathspec. rgit only: C Git
    /// opens such a FIFO and blocks.
    pub fn ancestor_metadata_fifo(driver: Driver) -> Result {
        for name in [".gitattributes", ".gitignore"] {
            for location in ["", "dir", "dir/sub"] {
                for spec in ["dir/sub/file", "dir/sub/new", "dir", "."] {
                    let mut s = Scenario::init(driver, Hash::Sha1, "main")?;
                    s.model.write(0, b"dir/sub/file", b"tracked\n")?;
                    s.add(0, &[b"dir/sub/file"])?;
                    s.commit(0, &["base"])?;
                    s.model.write(0, b"dir/sub/file", b"modified\n")?;
                    s.model.write(0, b"dir/sub/new", b"new\n")?;
                    let fifo = s.root().join(location).join(name);
                    mkfifo(&fifo)?;
                    let context = format!(
                        "add {spec} with FIFO {}",
                        Path::new(location).join(name).display()
                    );
                    let before = snapshot(s.dir())?;
                    let output = add_bounded(driver, &s.root(), &[spec])?;
                    refused(&output, 128, &context)?;
                    assert_preserved(
                        s.dir(),
                        &before,
                        &snapshot(s.dir())?,
                        Preserve::Exact,
                        &context,
                    )?;
                    fs::remove_file(&fifo)?;
                    s.check().map_err(|e| format!("{context}: {e}"))?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn ancestor_metadata_fifo_refuses_add_promptly(driver: Driver) -> Result {
    if lifecycle_support::driver::lacks(driver, &["add"], "ancestor_metadata_fifo") {
        return Ok(());
    }
    special::ancestor_metadata_fifo(driver)
}

#[cfg(unix)]
mod unix_rgit {
    use super::*;
    rgit_only!(ancestor_metadata_fifo_refuses_add_promptly);
}

#[cfg(unix)]
fn explicit_fifo_and_socket_paths_never_block_or_stage(driver: Driver) -> Result {
    if lifecycle_support::driver::lacks(driver, &["add"], "explicit_fifo_and_socket_paths") {
        return Ok(());
    }
    special::explicit(driver)
}

#[cfg(unix)]
fn directory_pathspec_with_fifo_and_socket_never_blocks(driver: Driver) -> Result {
    if lifecycle_support::driver::lacks(driver, &["add"], "directory_pathspec_with_fifo") {
        return Ok(());
    }
    special::directory(driver)
}

#[cfg(unix)]
mod unix {
    use super::*;
    both_drivers!(
        explicit_fifo_and_socket_paths_never_block_or_stage,
        directory_pathspec_with_fifo_and_socket_never_blocks,
    );
}
