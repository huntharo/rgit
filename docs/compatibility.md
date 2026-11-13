# Initial compatibility contract

This is a read-only foundation, not a replacement for daily Git workflows.
No worktree status, index refresh, repository writes, network access or external
execution is implemented. In particular, reading objects with this CLI does not
establish APFS mmap change detection.

## Supported commands

| Invocation | Contract |
| --- | --- |
| `rgit -C <directory> rev-parse --show-toplevel` | Absolute worktree root; error for bare repositories. |
| `rgit -C <directory> rev-parse --absolute-git-dir` | Absolute per-worktree Git directory. |
| `rgit -C <directory> rev-parse --is-bare-repository` | `true` or `false`. |
| `rgit -C <directory> rev-parse --verify <revision>` | One object ID; missing/unborn revisions fail. |
| `rgit -C <directory> cat-file -t <object>` | Object type. |
| `rgit -C <directory> cat-file -s <object>` | Uncompressed object byte length. |
| `rgit -C <directory> cat-file <type> <object>` | Raw bytes for blob/tree/commit/tag, with bounded tag/commit peeling to the requested type. |
| `rgit -C <directory> show-ref` | Sorted refs and unpeeled object IDs; empty list exits 1. Collect all refs before output so corruption cannot look like partial success. |

`-C` accepts one directory. Each rev-parse invocation accepts one query. Other
commands/options fail explicitly. `cat-file -p`, batch mode, show-ref filtering,
JSON output and full Git revision/config compatibility are not implemented.
Argument errors exit 2; operation errors exit 128. Diagnostic prose differs from
C Git. A closed output pipe ends without a panic.

## Repository and platform boundaries

The test matrix covers SHA-1 and SHA-256, loose and packed objects/refs, nested
discovery, bare repositories and linked worktrees. Reftable is required before a
write-enabled release but is not yet supported by this foundation; a reftable
repository must fail explicitly. Unknown mandatory extensions also fail.

Discovery uses isolated Gitoxide options: repository-local config is read;
system/global config, includes and repository-selection environment overrides
are intentionally not loaded. This is a compatibility difference. An explicit
`-C` path selects the starting repository; an ownership/trust failure is an error.
There are no network, credential, hook, filter, pager, editor or remote-helper
features in the selected dependency feature set.

Gitoxide still brings transport and command utilities as transitive dependencies.
Their presence is not execution permission; runtime code exposes no transport or
helper call paths. The dependency surface still needs a broader audit.

Unix arguments and paths retain non-UTF-8 bytes. Windows paths/arguments that
cannot be represented by the initial Gitoxide byte interface fail without
lossy conversion. Full Windows native-path support remains an open requirement.

This is not a hardened parser for arbitrary hostile repositories. Object loading,
pack/delta resource limits, digest verification and graph traversal still need
dedicated audits and fuzzing. Object metadata queries use headers, not full blob
decompression; raw object reads currently buffer data. The 64-step peeling limit
does not bound total memory or delta work.

## Required next gates

1. Consumer command inventory and full machine-readable compatibility matrix.
2. Independent DAG, index/tree and acknowledged-ref models.
3. Reftable interoperability, corruption handling and recovery contracts.
4. Live writable-mmap and fresh-process final-unmap APFS correctness before any
   write-enabled preview; no withdrawn broad ctime workaround.
5. Execution-capability policy before hooks, filters or transport.

The source of product requirements is the
[Rust Git engine project](https://linear.app/pwrdrvr/project/rust-git-engine-36a81e321166).
