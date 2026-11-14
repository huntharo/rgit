//! The compiled filesystem backend is the one ADR 0004 selects for this
//! platform. Any acceptance claim for anchored behaviour depends on this.

use rgit_core::write::{FilesystemHardening, filesystem_hardening};

#[test]
fn backend_selection_matches_the_platform_decision() {
    let expected = if cfg!(unix) {
        // The descriptor-relative backend (fsx/unix.rs, PWR-194).
        FilesystemHardening::Anchored
    } else {
        FilesystemHardening::LegacyPath
    };
    assert_eq!(filesystem_hardening(), expected);
}
