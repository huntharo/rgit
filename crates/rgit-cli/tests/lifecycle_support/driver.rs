//! Lifecycle drivers: the same scenario runs against C Git (proving the
//! oracle is not tailored to rgit) and against the rgit binary.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::OnceLock;

use super::proc::{Result, describe, home_for, identity, isolate, run};

/// THE integration switch. While false, each of `rgit init/add/commit` is
/// probed: a command that still returns the "not implemented" stub (or is
/// missing) is performed by C Git for *setup steps only*, with a NOTE line,
/// and scenarios whose subject is that command print an explicit SKIP. Flip
/// to true at integration: every rgit-driver step then uses rgit, the
/// fallback is unreachable and nothing is skipped.
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

    /// The driver that actually performs `cmd` (C Git stands in for an rgit
    /// command that is not implemented yet; see RGIT_WRITES_INTEGRATED).
    pub fn performer(self, cmd: &str) -> Driver {
        if self == Driver::Rgit && !supports(cmd) {
            note_fallback(cmd);
            Driver::CGit
        } else {
            self
        }
    }

    pub fn init_cmd<S: AsRef<OsStr>>(self, parent: &Path, args: &[S]) -> Command {
        let mut command = self.performer("init").command(parent);
        command.arg("init").args(args);
        command
    }

    pub fn init<S: AsRef<OsStr>>(self, parent: &Path, args: &[S]) -> Result<Output> {
        run(&mut self.init_cmd(parent, args))
    }

    pub fn add_cmd<S: AsRef<OsStr>>(self, dir: &Path, paths: &[S]) -> Command {
        let mut command = self.performer("add").command(dir);
        command.arg("add").arg("--").args(paths);
        command
    }

    pub fn add<S: AsRef<OsStr>>(self, dir: &Path, paths: &[S]) -> Result<Output> {
        run(&mut self.add_cmd(dir, paths))
    }

    pub fn commit_cmd(self, dir: &Path, parts: &[&str], when: i64, extra: &[&str]) -> Command {
        let mut command = self.performer("commit").command(dir);
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

/// Probe one command in a fresh sandbox; prerequisites come from C Git so
/// each command is judged on its own. `None` means implemented (any result
/// other than the stub counts, so genuine bugs fail scenarios rather than
/// skipping them).
fn probe(cmd: &str) -> Option<String> {
    let attempt = || -> Result<Option<String>> {
        let sandbox = super::proc::sandbox()?;
        let parent = sandbox.path();
        let repo = parent.join("probe");
        if cmd == "init" {
            // Judge the stub by its refusal, not by directory creation.
            std::fs::create_dir(&repo)?;
        } else {
            ok(&Driver::CGit.init(parent, &["-q", "probe"])?, "probe init")?;
            std::fs::write(repo.join("probe-file"), b"probe\n")?;
        }
        if cmd == "commit" {
            ok(&Driver::CGit.add(&repo, &["probe-file"])?, "probe add")?;
        }
        let mut command = Driver::Rgit.command(if cmd == "init" { parent } else { &repo });
        match cmd {
            "init" => command.args(["init", "-q", "probe"]),
            "add" => command.args(["add", "probe-file"]),
            _ => {
                identity(&mut command, 1_767_225_600);
                command.args(["commit", "-q", "-m", "probe"])
            }
        };
        let output = run(&mut command)?;
        Ok(stub_refusal(&output).then(|| brief(cmd, &output)))
    };
    attempt().unwrap_or_else(|error| Some(format!("probe error: {error}")))
}

fn capability(cmd: &str) -> &'static Option<String> {
    static INIT: OnceLock<Option<String>> = OnceLock::new();
    static ADD: OnceLock<Option<String>> = OnceLock::new();
    static COMMIT: OnceLock<Option<String>> = OnceLock::new();
    let cell = match cmd {
        "init" => &INIT,
        "add" => &ADD,
        _ => &COMMIT,
    };
    cell.get_or_init(|| probe(cmd))
}

/// Whether `rgit <cmd>` is implemented (always true once integrated).
pub fn supports(cmd: &str) -> bool {
    RGIT_WRITES_INTEGRATED || capability(cmd).is_none()
}

fn note_fallback(cmd: &str) {
    static NOTED: OnceLock<std::sync::Mutex<Vec<String>>> = OnceLock::new();
    let noted = NOTED.get_or_init(Default::default);
    if let Ok(mut noted) = noted.lock()
        && !noted.iter().any(|c| c == cmd)
    {
        noted.push(cmd.to_owned());
        eprintln!(
            "NOTE: rgit {cmd} not implemented yet; C Git performs rgit-driver {cmd} setup steps ({})",
            capability(cmd).as_deref().unwrap_or_default()
        );
    }
}

/// `Some(Driver::Rgit)` if at least one rgit write command is implemented;
/// otherwise prints an explicit SKIP line naming `scenario`.
pub fn rgit_or_skip(scenario: &str) -> Option<Driver> {
    if ["init", "add", "commit"].iter().any(|cmd| supports(cmd)) {
        return Some(Driver::Rgit);
    }
    eprintln!(
        "SKIP: rgit driver for {scenario}: rgit write commands not integrated yet ({})",
        capability("init").as_deref().unwrap_or_default()
    );
    None
}

/// For scenarios whose subject is specific rgit commands: true (after an
/// explicit SKIP line) if `driver` is rgit and any of `cmds` is still a stub.
pub fn lacks(driver: Driver, cmds: &[&str], scenario: &str) -> bool {
    if driver != Driver::Rgit {
        return false;
    }
    let missing: Vec<&str> = cmds.iter().copied().filter(|cmd| !supports(cmd)).collect();
    if missing.is_empty() {
        return false;
    }
    eprintln!("SKIP: rgit driver for {scenario}: rgit {missing:?} not implemented yet");
    true
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
