# Compatibility contract

This is an experimental foundation, not a replacement for daily Git workflows.
Inspection commands never write. `init`, `add` and `commit` are an experimental
local write slice (see [Local writes](#local-writes-experimental) and
[decision 0003](decisions/0003-local-write-lifecycle.md)); every other mutation,
network access and external execution are not implemented.

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
| `rgit -C <directory> init [-q] [--bare] [-b/--initial-branch <name>] [--object-format sha1\|sha256] [<directory>]` | Create a repository; the target directory is created if absent. Never reinitializes: an existing repository exits 128 unchanged. |
| `rgit -C <directory> add [--] <path>...` | Stage literal files or directories relative to `-C` (`.` for all). No pathspec magic or globbing. Silent on success. |
| `rgit -C <directory> commit [-q] -m <message>... [--allow-empty]` | Commit the index on HEAD's branch, an unborn branch or a detached HEAD. Prints `[<branch> (root-commit) <abbrev>] <subject>`. |

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
Status, diff and listing never refresh or write an index, store a blob, or
acquire an index lock. Tests snapshot repository and worktree bytes around inspection;
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

## Local writes (experimental)

`init`, `add` and `commit` follow decision 0003: objects are written and synced
before any index or reference names them; the index, `HEAD` and loose refs are
replaced only through an exclusively created `<file>.lock`, synced and renamed;
reference updates compare the expected value while holding the lock. A held
lock refuses the operation (exit 128, naming the lock) and is never removed by
rgit. On Unix, files and directory entries are synced; Windows has no directory
sync, so its guarantees are atomic visibility only. Hooks never run; signing,
`commit.gpgSign` and `i18n.commitEncoding` other than UTF-8 are refused.

Every refusal below is a capability, format or input check that happens before
the first object or index write. Lock contention is different: it is detected
when the lock is taken, after the operation has written the new objects it
needs, so a refused `add` or `commit` behind a held lock can leave new complete,
unreferenced loose objects (existing objects, refs, index and worktree are
unchanged). This follows the objects-before-names rule.

- Repositories: bare repositories for `add`/`commit`; reftable and other
  unsupported extensions (at discovery); index version 4, `index.version` other
  than 2/3, `feature.manyFiles`, split index, sparse checkout or sparse index;
  `core.symlinks=false`; in-progress merge, cherry-pick, revert, rebase or
  sequencer state.
- Paths: globs and `:` magic, absolute paths and paths leaving the worktree,
  pathspecs matching nothing, explicitly named ignored paths and any pathspec
  under an ignored directory (C Git stages tracked content there but exits 1;
  rgit refuses the whole call), symlinked parents, explicitly named special
  files (FIFOs/sockets found while walking a directory are skipped), nested
  repositories and gitlinks, skip-worktree and assume-unchanged entries,
  conflicted entries, and case-fold or Unicode-normalization spelling
  collisions when `core.ignorecase`/`core.precomposeunicode` is set.
- Content: any `filter` or `working-tree-encoding` attribute; `ident` when the
  content contains `$Id`; text/EOL/`core.autocrlf` settings that would convert
  the content, and `core.safecrlf=true` content it would reject. Content whose
  conversion is the identity (for example `text=auto` with LF only) is staged
  verbatim.
- Commit: unmerged or intent-to-add entries, gitlinks, a HEAD outside
  `refs/heads/`, symbolic or symlinked branch refs, identities containing `<`,
  `>`, newline or NUL, a NUL in the message, and an unchanged tree without
  `--allow-empty`.

Differences from C Git:

- `init` refuses an existing repository instead of reinitializing it. Its
  config mirrors C Git's probes (filemode, symlinks, ignorecase,
  precomposeunicode) but writes no hooks, description or `info/exclude`.
  The default branch is `master` because global `init.defaultBranch` is not read.
- `add` hashes every file under the pathspec instead of trusting stat data and
  buffers whole files. Racily clean entries are smudged as C Git does.
  Resolve-undo, untracked-cache and fsmonitor extensions are dropped when the
  index is rewritten (after an rgit `add`, `git update-index --unresolve` and
  `git checkout -m` can no longer restore a resolved conflict); `index.skipHash` is ignored and a checksum is always
  written. No `safecrlf` warnings are printed.
- `commit -m` applies C Git's default `whitespace` message cleanup and trims
  identities like C Git (leading/trailing whitespace and `,:;<>"'\`; inner
  `<`, `>` and newlines removed), so identical inputs produce identical commit
  IDs; tests compare them byte for byte. It never runs hooks or editors,
  cannot amend, and refuses during merge, rebase or sequencer operations.
  Identity comes from `GIT_{AUTHOR,COMMITTER}_{NAME,EMAIL}` or
  repository-local `user.*` only. Without `GIT_*_DATE` both timestamps use one
  reading of the current time with a `+0000` offset (no local time zone).
  Dates accept only `[@]<seconds> <±hhmm>` and ISO 8601
  `YYYY-MM-DD[T ]HH:MM:SS` with `Z`, `±HH:MM` or `±HHMM`; fractional seconds,
  a space before the zone, RFC 2822, zone-less dates and approximate dates
  are refused. Nothing to commit and an empty message exit 128, not 1. `commit` reads the index without taking
  `index.lock`, so it commits the index snapshot it read; a concurrent `add`
  published after that read is not included.
- Objects are always written loose (zlib level 1); there is no packing or
  auto-maintenance. Reflog lines are appended after the reference is
  published, whereas C Git appends before; see decision 0003 for the full
  failure table. A reflog append failure after publication exits 128 with a
  diagnostic stating that the reference was updated.
- Ignore and attribute rules come from the worktree, `info/` and
  repository-local config only, consistent with isolated discovery.
- `add` refuses absolute paths even inside the worktree, and `path/` must name a
  directory. A conflicted path is refused even without in-progress operation
  state (for example after a conflicting `git stash pop`), where C Git would
  resolve it. Any skip-worktree, assume-unchanged or gitlink entry under the
  pathspec refuses the whole call.
- An empty directory left where a reference must be created (for example
  `refs/heads/a/` after `refs/heads/a/b` was deleted) is a conflict; C Git
  removes it and proceeds.
- For a plain reference update outside `commit`, which reflogs to append
  (including `HEAD`'s) is decided before the lock; `HEAD` re-pointed
  concurrently can receive or miss that line. `commit` holds `HEAD.lock` and
  is not affected.
- Durability covers objects rgit writes. Objects another program wrote and
  rgit only references (for example a parent commit written by C Git without
  `core.fsync`) are checked for presence, not synced.

Known race windows, outside the claim that preflight is a filesystem sandbox:
a path swapped for a FIFO or symlink between `add`'s `lstat` and `open` can
block or be followed (content is re-hashed and verified, so wrong bytes are
never staged); and on Unix the rename that publishes `init` replaces an empty
`.git` directory that a concurrent `git init` created in that instant.

Recovery after interruption: a fresh process sees each of the index, `HEAD`
and the branch ref either completely old or completely new. Abrupt termination
bypasses cleanup, so a crash can leave the lock(s) the process held; they
refuse later rgit and C Git writers until removed. For a commit on a branch
interrupted after the branch rename, that is `HEAD.lock` (the branch is
already published and its own lock is gone). An interrupted `init` leaves no
`.git`, only one private `.git.rgit-init-*` (bare: `.<name>.rgit-init-*`)
directory; a later `init` succeeds and leaves that directory untouched, so it
appears as untracked until deleted. A bare `init` into an existing empty
directory populates it in place (preserving its inode, mode and ownership) and
publishes `HEAD` last; interrupted before `HEAD`, the directory holds `config`,
`objects/` and `refs/` but is not a repository, and a retry refuses it as
non-empty until it is emptied manually. Errors that occur after a repository
was published are reported as such ("the repository was created at ..."). rgit has no proof of staleness and never
deletes a lock or temporary directory it did not create in the same process:
remove one manually only after confirming that no rgit or Git process is still
running against the repository. Errors handled without a crash clean up their
own locks and temporary directories. A write error is not proof that nothing
changed: errors after a rename mean the new value is already visible.

Native macOS APFS tests extend the live and final-unmap shared-mapping gates to
the write path: bytes written through a closed-descriptor `MAP_SHARED` mapping,
with mtime and ctime preserved, are staged and committed by a fresh `rgit add`
and `rgit commit`. Same-second edits after `add` remain visible to `status` and
the next `add`. Crash durability on power loss is not tested.

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
2. Remaining G1 workflows: branch/switch, documented reset variants and
   linked-worktree add/remove, each with the lifecycle oracle.
3. Reftable interoperability, corruption handling and recovery contracts;
   reftable is required before any write-enabled release.
4. Preserve native live writable-mmap and fresh-process final-unmap APFS
   regressions, then extend the correctness gate to each future write-enabled
   workflow; no withdrawn broad ctime workaround.
5. Execution-capability policy before hooks, filters or transport.

The source of product requirements is the
[Rust Git engine project](https://linear.app/pwrdrvr/project/rust-git-engine-36a81e321166).
