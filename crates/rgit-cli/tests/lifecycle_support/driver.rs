//! Lifecycle drivers: the same scenario runs against C Git (proving the
//! oracle is not tailored to rgit) and against the rgit binary.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::OnceLock;

use super::proc::{Result, describe, home_for, identity, isolate, run};

/// THE integration switch. While false, rgit-driver tests probe whether the
/// binary implements init/add/commit and print an honest SKIP line if it
/// still returns the "not implemented" stub or lacks the subcommand. Flip to
/// true at integration: every rgit-driver test then runs unconditionally.
pub const RGIT_WRITES_INTEGRATED: bool = false;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Driver {
    CGit,
    Rgit,
}

impl Driver {
    pub fn name(self) -> &'static str {
        match self {
            Driver::CGit => "cgit",
            Driver::Rgit => "rgit",
        }
    }

    /// An isolated command operating on `dir`.
    pub fn command(self, dir: &Path) -> Command {
        let mut command = match self {
            Driver::CGit => Command::new("git"),
            Driver::Rgit => {
                let mut command = Command::new(env!("CARGO_BIN_EXE_rgit"));
                command.arg("-C").arg(dir);
                command
            }
        };
        command.current_dir(dir);
        isolate(&mut command, &home_for(dir));
        command
    }

    pub fn init_cmd<S: AsRef<OsStr>>(self, parent: &Path, args: &[S]) -> Command {
        let mut command = self.command(parent);
        command.arg("init").args(args);
        command
    }

    pub fn init<S: AsRef<OsStr>>(self, parent: &Path, args: &[S]) -> Result<Output> {
        run(&mut self.init_cmd(parent, args))
    }

    pub fn add_cmd<S: AsRef<OsStr>>(self, dir: &Path, paths: &[S]) -> Command {
        let mut command = self.command(dir);
        command.arg("add").arg("--").args(paths);
        command
    }

    pub fn add<S: AsRef<OsStr>>(self, dir: &Path, paths: &[S]) -> Result<Output> {
        run(&mut self.add_cmd(dir, paths))
    }

    pub fn commit_cmd(self, dir: &Path, parts: &[&str], when: i64, extra: &[&str]) -> Command {
        let mut command = self.command(dir);
        command.args(["commit", "-q"]).args(extra);
        for part in parts {
            command.arg("-m").arg(part);
        }
        identity(&mut command, when);
        command
    }

    pub fn commit(self, dir: &Path, parts: &[&str], when: i64) -> Result<Output> {
        run(&mut self.commit_cmd(dir, parts, when, &[]))
    }

    /// ADR 0003: initializing an existing repository exits 128 unchanged.
    /// C Git re-initializes successfully (documented divergence).
    pub fn refuses_reinit(self) -> bool {
        self == Driver::Rgit
    }

    /// rgit refuses an explicit FIFO/socket path; C Git silently skips it.
    pub fn refuses_explicit_special_file(self) -> bool {
        self == Driver::Rgit
    }

    /// Exit code for `commit` with nothing staged: rgit errors exit 128, C Git
    /// exits 1.
    pub fn nothing_to_commit_code(self) -> i32 {
        match self {
            Driver::Rgit => 128,
            Driver::CGit => 1,
        }
    }

    /// C Git's refused commit still rewrites the index's cache-tree, so for
    /// C Git only the index's entries (not bytes) must be preserved.
    pub fn index_bytes_stable_on_refusal(self) -> bool {
        self == Driver::Rgit
    }
}

pub fn ok(output: &Output, what: &str) -> Result {
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("{what} failed: {}", describe(output)).into())
    }
}

pub fn ok_quiet(output: &Output, what: &str) -> Result {
    ok(output, what)?;
    if !output.stdout.is_empty() {
        return Err(format!("{what} -q wrote stdout: {}", describe(output)).into());
    }
    Ok(())
}

pub fn refused(output: &Output, code: i32, what: &str) -> Result {
    if output.status.code() != Some(code) {
        return Err(format!("{what}: expected exit {code}: {}", describe(output)).into());
    }
    Ok(())
}

fn stub_refusal(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    (output.status.code() == Some(2) && stderr.contains("unrecognized subcommand"))
        || stderr.contains("not implemented")
}

fn brief(what: &str, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!(
        "`rgit {what}` exit {:?}: {}",
        output.status.code(),
        stderr.lines().next().unwrap_or_default()
    )
}

fn probe() -> std::result::Result<(), String> {
    let run_probe = || -> Result<Option<String>> {
        let sandbox = super::proc::sandbox()?;
        let parent = sandbox.path();
        let output = Driver::Rgit.init(parent, &["-q", "probe"])?;
        if stub_refusal(&output) {
            return Ok(Some(brief("init", &output)));
        }
        let repo = parent.join("probe");
        std::fs::create_dir_all(&repo)?;
        std::fs::write(repo.join("probe-file"), b"probe\n")?;
        let output = Driver::Rgit.add(&repo, &["probe-file"])?;
        if stub_refusal(&output) {
            return Ok(Some(brief("add", &output)));
        }
        let output = Driver::Rgit.commit(&repo, &["probe"], 1_767_225_600)?;
        if stub_refusal(&output) {
            return Ok(Some(brief("commit", &output)));
        }
        // Any other result, including a genuine failure, counts as
        // implemented so real bugs fail the scenarios instead of skipping.
        Ok(None)
    };
    match run_probe() {
        Ok(None) => Ok(()),
        Ok(Some(reason)) => Err(reason),
        Err(error) => Err(format!("probe error: {error}")),
    }
}

/// `Some(Driver::Rgit)` if rgit-driver scenarios must run; otherwise prints
/// an explicit SKIP line naming `scenario` and returns None.
pub fn rgit_or_skip(scenario: &str) -> Option<Driver> {
    if RGIT_WRITES_INTEGRATED {
        return Some(Driver::Rgit);
    }
    static PROBE: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    match PROBE.get_or_init(probe) {
        Ok(()) => Some(Driver::Rgit),
        Err(reason) => {
            eprintln!(
                "SKIP: rgit driver for {scenario}: rgit write commands not integrated yet ({})",
                reason
            );
            None
        }
    }
}

pub fn os(bytes: &[u8]) -> OsString {
    super::model::os_path(bytes).into_os_string()
}

/// Generate a C Git test and an rgit test for a scenario `fn(Driver) -> Result`.
#[macro_export]
macro_rules! both_drivers {
    ($($scenario:ident),* $(,)?) => {
        mod cgit {
            $(
                #[test]
                fn $scenario() -> $crate::lifecycle_support::proc::Result {
                    super::$scenario($crate::lifecycle_support::driver::Driver::CGit)
                }
            )*
        }
        mod rgit {
            $(
                #[test]
                fn $scenario() -> $crate::lifecycle_support::proc::Result {
                    match $crate::lifecycle_support::driver::rgit_or_skip(stringify!($scenario)) {
                        Some(driver) => super::$scenario(driver),
                        None => Ok(()),
                    }
                }
            )*
        }
    };
}

/// Generate rgit-only tests (failpoints, rgit-specific contract).
#[macro_export]
macro_rules! rgit_only {
    ($($scenario:ident),* $(,)?) => {
        mod rgit_only {
            $(
                #[test]
                fn $scenario() -> $crate::lifecycle_support::proc::Result {
                    match $crate::lifecycle_support::driver::rgit_or_skip(stringify!($scenario)) {
                        Some(driver) => super::$scenario(driver),
                        None => Ok(()),
                    }
                }
            )*
        }
    };
}
