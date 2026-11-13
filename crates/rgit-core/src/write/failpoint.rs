//! Test-only fault injection. Compiled to a no-op unless the `failpoints`
//! feature is enabled AND debug assertions are on, so an optimized release
//! build never reads these variables even if the feature is enabled by
//! accident.
//!
//! `RGIT_FAILPOINT=<name>` aborts the process at that point (simulated crash:
//! no destructors, locks and temporary files stay behind).
//! `RGIT_FAILPOINT=<name>:error` returns [`Error::Injected`] instead.
//! `RGIT_FAILPOINT=<name>:pause` creates the file named by
//! `RGIT_FAILPOINT_READY`, then waits up to 60s for the file named by
//! `RGIT_FAILPOINT_RELEASE` to exist before continuing.
//!
//! Names are a stable test contract; see docs/decisions/0003.

use crate::Error;

#[cfg(not(all(feature = "failpoints", debug_assertions)))]
#[inline(always)]
pub(crate) fn hit(_name: &'static str) -> Result<(), Error> {
    Ok(())
}

#[cfg(all(feature = "failpoints", debug_assertions))]
pub(crate) fn hit(name: &'static str) -> Result<(), Error> {
    let Some(spec) = std::env::var_os("RGIT_FAILPOINT") else {
        return Ok(());
    };
    let spec = spec.to_string_lossy();
    let (point, action) = spec.split_once(':').unwrap_or((&spec, "abort"));
    if point != name {
        return Ok(());
    }
    match action {
        "error" => Err(Error::Injected(name)),
        "pause" => {
            let ready = std::env::var_os("RGIT_FAILPOINT_READY")
                .ok_or(Error::Injected("RGIT_FAILPOINT_READY unset"))?;
            let release = std::env::var_os("RGIT_FAILPOINT_RELEASE")
                .ok_or(Error::Injected("RGIT_FAILPOINT_RELEASE unset"))?;
            std::fs::write(&ready, name)?;
            let release = std::path::Path::new(&release);
            for _ in 0..6000 {
                if release.exists() {
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(Error::Injected(name))
        }
        _ => std::process::abort(),
    }
}

/// Runs only as `cargo test --release -p rgit-core --features failpoints`:
/// proves an optimized build ignores the environment even with the feature on.
#[cfg(all(test, feature = "failpoints", not(debug_assertions)))]
mod release_tests {
    #![allow(clippy::expect_used)]

    #[test]
    fn release_build_ignores_failpoint_environment() {
        // Re-run this test in a child with the variable set: an active
        // failpoint would abort the child, so success proves it is inert.
        let exe = std::env::current_exe().expect("test executable");
        if std::env::var_os("RGIT_FAILPOINT_CHILD").is_some() {
            assert!(super::hit("lock.acquired").is_ok());
            return;
        }
        let status = std::process::Command::new(exe)
            .args([
                "--exact",
                "write::failpoint::release_tests::release_build_ignores_failpoint_environment",
            ])
            .env("RGIT_FAILPOINT_CHILD", "1")
            .env("RGIT_FAILPOINT", "lock.acquired")
            .status()
            .expect("spawn child");
        assert!(status.success(), "failpoint fired in a release build");
    }
}
