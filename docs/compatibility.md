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
| `rgit -C <directory> merge-base [-a/--all] <left> <right>` | Best common ancestor(s); no base exits 1 with no output. Multiple bases are sorted by object ID; plain mode returns one of them. |
| `rgit -C <directory> merge-base --is-ancestor <left> <right>` | No stdout; exits 0 if left is an ancestor of right, 1 otherwise. Errors exit 128. |
| `rgit -C <directory> rev-list --count <revision>` | Count unique reachable commits from one revision, including the tip. |

`-C` accepts one directory. Each rev-parse invocation accepts one query. Other
commands/options fail explicitly. `cat-file -p`, batch mode, show-ref filtering,
JSON output and full Git revision/config compatibility are not implemented.
Argument errors exit 2; operation errors exit 128. Diagnostic prose differs from
C Git. A closed output pipe ends without a panic.

## Commit ancestry

Graph queries peel annotated tags to commits, load reachable parent edges and
treat declared shallow commits as roots, even if their parents exist locally.
Missing or malformed history is an error with no partial stdout. The graph is
validated iteratively, so cycles fail rather than loop and deep ancestry does
not consume the call stack. Dates and commit-graph generation metadata do not
participate in ancestry decisions. Graph data is read from object storage.

The CLI uses per-query limits of 1,000,000 commits and 4,000,000 parent links;
library callers can supply `GraphLimits`. Shallow-file reads are separately
capped at 66 bytes per permitted commit and one record per permitted commit.
These bounds cover retained graph structures, not total RSS or underlying
object/pack/delta decoding. Queries currently load the whole reachable graph,
so even a simple query can cost O(V + E) and may reject unrelated corruption in
that reachable history rather than stopping at the first sufficient answer.

Only two-input merge-base and one-input rev-list count are supported. Octopus,
independent/fork-point queries, revision ranges, filtered walks, commit listing,
replacement refs and nonempty legacy graft files are not implemented. Repositories
containing replacement refs or grafts are refused for graph queries, including
when a Git configuration would disable them. A plain merge-base may choose a
different valid best base than C Git. The independent oracle compares sets, not
an unspecified traversal order.

Roots and shallow boundaries are loaded once per query. There is no claim of an
atomic snapshot against concurrent repository mutation or object pruning.

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
