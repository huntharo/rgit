# 0001: Start with read-only Gitoxide operations

Status: accepted for the initial inspection slice; not a decision for every
future subsystem.

The first implementation must do useful repository work and provide an
independent C Git comparison without starting another Git process at runtime.
Use Gitoxide `gix` 0.90.0 with default features disabled and only SHA-1, SHA-256
and revision support enabled. Pin the direct version and commit Cargo.lock.

`rgit-core` owns discovery, revision/object access and reference enumeration.
`rgit-cli` handles native arguments, stdout/stderr and exit status. No library
operation changes cwd or terminates the process. No project crate permits
unsafe Rust. Gitoxide's permissive licensing allows evaluation; a project
license and complete dependency/unsafe-code review remain decisions before
broader distribution.

Reuse object/pack/revision primitives now rather than implementing binary
parsers before there is a conformance runner. Do not assume its full porcelain,
reftable, transport or APFS behavior meets our PRDs. For example, use fallible
commit/tag decoding and a bounded traversal for raw object peeling instead of
helpers that assume valid object data.

Isolated config and the minimal feature set keep this slice independent of
global Git configuration and executable helpers. Tests compare binary output
against C Git for loose/packed SHA-1/SHA-256 fixtures, then separately test
corruption, unsupported input, linked worktrees and execution without Git on
PATH. C Git is a comparison implementation, not the oracle for known bugs.

Revisit per component when adding index/status, reftable, graph algorithms,
transport and writes. In particular, this decision does not waive the APFS
live/final-unmap gate or select a SQLite backend.
