# Development

Install Rust with rustup and C Git for the conformance tests.
On macOS, Python 3 is also required for the native APFS shared-mapping tests.

```sh
cargo build --workspace --locked
cargo run -p rgit-cli -- -C . rev-parse --verify HEAD
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

The runtime does not depend on a Git executable. Tests use C Git to create
disposable repositories and compare observable results. See
[the compatibility contract](docs/compatibility.md) before extending the command
set. Work on a branch and submit a focused PR/MR with the invariant, supported
behavior and validation results. Platform-specific semantics belong behind
explicit library interfaces and need native platform evidence.

The workspace forbids unsafe Rust in project crates. This does not mean its
dependencies contain no unsafe or native code.
