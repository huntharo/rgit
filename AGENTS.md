# Development

- Read `docs/compatibility.md` and the relevant decision before changing behavior.
- Keep the public README short. The project is an experimental rewrite of Git using Rust.
- Keep repository operations in `rgit-core`; the CLI owns arguments, output and exit codes.
- Runtime code must not invoke C Git, hooks, helpers or a shell. Tests may use C Git as an independent fixture generator and comparison implementation.
- Preserve native path bytes. Refuse unsupported capabilities explicitly; do not silently fall back to another executable.
- Repository mutations follow `docs/decisions/0003-local-write-lifecycle.md`: refuse unsupported capabilities before the first write, write objects before names, publish only through lock-then-rename with compare-and-swap under the lock, and never remove another writer's lock. Each new write workflow needs the independent lifecycle oracle, failpoint recovery tests and native live/final-unmap APFS gates (Linear PWR-174) before it merges.
- Library operations return errors, preserve their causes and never exit or change process-global cwd.
- Add meaningful independent conformance or regression coverage when changing repository semantics. Record platform skips honestly.
- Run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings` and `cargo test --workspace --locked`.
- Keep PRs/MRs scoped. Record compatibility differences and decisions in `docs/`.

When drafting or editing PR titles and descriptions for huntharo, apply `~/.codex/pr-style.md` when available and the repository PR template. Use GitHub attachments for screenshots/videos; do not create commits solely to host media.
