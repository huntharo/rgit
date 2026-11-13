# Compatibility contract

This is a read-only foundation, not a replacement for daily Git workflows.
Worktree status and diffs compare content without refreshing the on-disk index.
Repository writes, network access and external execution are not implemented.

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
| `rgit -C <directory> ls-files [-s/--stage] [-u/--unmerged] [-z]` | Index paths, optionally modes, object IDs and conflict stages; unmerged implies stage output. |
| `rgit -C <directory> ls-tree -r [--name-only] [-z] <revision>` | Flattened recursive tree entries, including symlinks and gitlinks. |
| `rgit -C <directory> status [--porcelain[=v1]] [-s] [-z] [-u[no\|normal\|all]] [--no-renames]` | Porcelain v1 changes, with tracked and untracked groups separately sorted. Default output is short, not Git's long human format. |
| `rgit -C <directory> diff [--cached/--staged] [--name-only\|--name-status] [-z] [--quiet] [--exit-code]` | Index-to-worktree or HEAD-to-index comparison; default output is a patch. Quiet implies exit-code; changes exit 1, unchanged exits 0, errors exit 128. |
| `rgit -C <directory> merge-base [-a/--all] <left> <right>` | Best common ancestor(s); no base exits 1 with no output. Multiple bases are sorted by object ID; plain mode returns one of them. |
| `rgit -C <directory> merge-base --is-ancestor <left> <right>` | No stdout; exits 0 if left is an ancestor of right, 1 otherwise. Errors exit 128. |
| `rgit -C <directory> rev-list --count <revision>` | Count unique reachable commits from one revision, including the tip. |

`-C` accepts one directory. Each rev-parse invocation accepts one query. Other
commands/options fail explicitly. `cat-file -p`, batch mode, show-ref filtering,
JSON output and full Git revision/config compatibility are not implemented.
Argument errors exit 2; operation errors exit 128. Diagnostic prose differs from
C Git. A closed output pipe ends without a panic.

## Index, status and diffs

Index reads verify present checksums and structural ordering; Git's all-zero
checksum omission is accepted. Symlink and special index files are refused.
Missing indexes are
empty. Split and sparse indexes are explicitly unsupported. Trees and index
paths retain byte ordering and conflict stages; malformed or unsafe paths fail.
These operations never refresh or write an index, store a blob, or acquire an
index lock. Tests snapshot repository and worktree bytes around inspection;
those snapshots do not detect same-byte rewrites or metadata-only touches.

Status deliberately compares tracked file content even when stat metadata and
fsmonitor flags claim a file is unchanged. Explicit assume-unchanged and
skip-worktree flags remain user opt-outs. Native APFS tests use closed file
descriptors and writable shared mappings: one inspection occurs with the
mapping live, another in a fresh process after final unmap. This establishes
the initial read-only detection behavior; it does not establish a future
write-enabled command's transaction or durability contract, crash recovery or
multi-mapper interleavings. Content scanning
costs more I/O than trusting the stat cache; no performance parity is claimed.

Builtin text/EOL/ident conversions participate in comparisons. External
clean/smudge/process filters, custom attributes/excludes paths and submodule
worktree status are refused. Status includes untracked directory collapsing,
intent-to-add, mode/type changes and all seven conflict stage combinations.
Renames are always represented as delete/add, including when --no-renames is
omitted. Ignored files are omitted; ignored-file output and pathspecs are not
implemented. Status porcelain v2 and long human output are not implemented.

Diffs support text patches, binary change notices, modes and symlink targets.
Patches preserve raw content bytes. Binary patches, rename/copy detection,
combined conflict patches, arbitrary revision comparisons, pathspecs and
submodule content diffs are not implemented. Name output can report conflicts
as U; requesting an unsupported patch is an error before any stdout is written.
`--no-ext-diff` and `--no-textconv` are accepted; drivers and textconv are never
executed. Diff output need not choose the same hunks as C Git; patch tests apply
the result with C Git and compare resulting tree IDs. Output and file contents
are buffered; there is no total-memory bound for large files or patches yet.

Before reading worktree content or attribute/ignore metadata, inspection rejects
symlinked tracked parents, special tracked leaves and symlink/special metadata
files. Status conservatively scans directory entries, including ignored
directories, without following symlinks or descending into nested repositories.
It refuses more than 1,000,000 entries or metadata files over 8 MiB. This may
refuse a repository that Git accepts and makes `-uno` traverse directories too.
Preflight is not a race-free filesystem sandbox: a concurrent writer can replace
paths between checks and reads. Race-resistant file access remains a requirement
before claiming safe inspection of actively hostile worktrees.

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
There are no runtime network, credential, hook, pager, editor or remote-helper
entry points. The status dependency enables filter infrastructure; execution is
refused by the inspection policy before using it.

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
4. Preserve native live writable-mmap and fresh-process final-unmap APFS
   regressions, then extend the correctness gate to each future write-enabled
   workflow; no withdrawn broad ctime workaround.
5. Execution-capability policy before hooks, filters or transport.

The source of product requirements is the
[Rust Git engine project](https://linear.app/pwrdrvr/project/rust-git-engine-36a81e321166).
