//! Independent local-lifecycle oracle (G1, ADR 0003). Tests only.
//!
//! - `sha`: self-contained SHA-1/SHA-256.
//! - `model`: expected worktree/index/ref/commit state and object IDs.
//! - `observe`: C Git plumbing in fresh processes, compared with the model.
//! - `driver`: C Git and rgit drivers plus the single integration switch.
//! - `proc`: bounded processes, isolation and byte snapshots.

pub mod crash;
pub mod driver;
pub mod model;
pub mod observe;
pub mod proc;
pub mod sha;

use std::path::{Path, PathBuf};

use driver::{Driver, ok_quiet};
use model::{Hash, Model};
use proc::{Result, sandbox};

/// One repository under test: sandbox, driver and model of its state.
pub struct Scenario {
    pub sandbox: tempfile::TempDir,
    pub driver: Driver,
    pub model: Model,
}

impl Scenario {
    /// `<driver> init -q -b <branch> --object-format <hash> repo`.
    pub fn init(driver: Driver, hash: Hash, branch: &str) -> Result<Self> {
        let sandbox = sandbox()?;
        let output = driver.init(
            sandbox.path(),
            &["-q", "-b", branch, "--object-format", hash.name(), "repo"],
        )?;
        ok_quiet(&output, &format!("{} init", driver.name()))?;
        let root = sandbox.path().join("repo");
        Ok(Self {
            model: Model::new(hash, &root, branch),
            sandbox,
            driver,
        })
    }

    pub fn root(&self) -> PathBuf {
        self.model.root(0).to_path_buf()
    }

    pub fn dir(&self) -> &Path {
        self.sandbox.path()
    }

    /// Stage through the driver and the model.
    pub fn add(&mut self, c: usize, paths: &[&[u8]]) -> Result {
        let os: Vec<_> = paths.iter().map(|p| driver::os(p)).collect();
        let output = self.driver.add(self.model.root(c), &os)?;
        driver::ok(&output, &format!("{} add {:?}", self.driver.name(), os))?;
        if !output.stdout.is_empty() {
            return Err(format!("add wrote stdout: {}", proc::describe(&output)).into());
        }
        self.model.stage(c, paths)
    }

    /// Commit through the driver; returns the model-predicted commit ID after
    /// checking the actual state matches it.
    pub fn commit(&mut self, c: usize, parts: &[&str]) -> Result<String> {
        let when = self.model.tick();
        let output = self.driver.commit(self.model.root(c), parts, when)?;
        ok_quiet(&output, &format!("{} commit", self.driver.name()))?;
        let oid = self.model.commit(c, parts, when);
        self.check()?;
        Ok(oid)
    }

    pub fn check(&self) -> Result {
        observe::assert_state(&self.model)
            .map_err(|e| format!("[{} {}] {e}", self.driver.name(), self.model.hash.name()).into())
    }
}
