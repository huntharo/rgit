//! Test-only fault injection. Compiled to a no-op unless the `failpoints`
//! feature is enabled; release binaries never read these variables.
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

#[cfg(not(feature = "failpoints"))]
#[inline(always)]
pub(crate) fn hit(_name: &'static str) -> Result<(), Error> {
    Ok(())
}

#[cfg(feature = "failpoints")]
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
