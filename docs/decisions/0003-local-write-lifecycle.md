# 0003: First recoverable local write lifecycle (G1 contract)

Status: proposed for the G1 init/add/commit slice (Linear PWR-178, PWR-175,
PWR-174). Write support is experimental and unreleased.

## Scope

`init`, `add`, `commit` and compare-and-swap reference publication for
SHA-1 and SHA-256 repositories that use standard files refs and a version 2-4
index without split/sparse extensions. Every other repository shape is refused
with a typed error before the first mutation: reftable, split or sparse
indexes, `extensions.compatobjectformat`, in-progress merge/rebase/cherry-pick
state, submodule (gitlink) staging, and any path whose attributes or config
would need a clean/smudge/process filter, `ident`, `working-tree-encoding` or
end-of-line conversion. Hooks are never run. No runtime path invokes C Git,
a shell or a helper.

Reftable remains required before any write-enabled release. Branch/switch,
reset variants and linked-worktree add/remove are later G1 issues.

## Invariants

1. **Objects before names.** Every object an index entry or reference names is
   written and synced (file and containing directory on Unix) before the index
   or reference is published. No published reference points at an unavailable
   object; `update_ref` verifies existence (and commit type for
   `refs/heads/*`) before locking.
2. **Lock then rename.** Index, `HEAD`, loose refs and `packed-refs` change only
   through an exclusively created `<file>.lock`, fully written and synced, then
   renamed over the target. Nothing is rewritten in place. A held lock refuses
   the operation with `Error::Locked` and leaves state unchanged. A crash can
   leave a stale lock, as with C Git; it is never silently broken.
3. **Compare-and-swap.** A commit reads the HEAD target before writing objects
   and publishes only if the ref still holds that value (or is still absent on
   an unborn branch), otherwise `Error::RefConflict`.
4. **Worktree boundaries.** In a linked worktree, HEAD and the index are
   per-worktree; branch refs, objects and `packed-refs` are in the common
   directory. An operation never modifies another worktree's HEAD or index.
5. **Recovery.** After an interruption at any point, a fresh process observes
   either the complete previous state or the complete new state for each
   published file; unreferenced objects may remain and are harmless.

Durability: on Unix, file content and directory entries are synced before
publication. On Windows, std offers no directory sync; guarantees there are
visibility/atomic-rename only. Loose reflog appends follow the ref rename and
are not part of the atomic publication.

## Ownership and frozen API

| Owner | Files | API |
| --- | --- | --- |
| Integration | `lib.rs` errors, `write/mod.rs`, `write/lock.rs`, `write/failpoint.rs`, CLI, docs, Cargo manifests, `tests/common` | `Error::{Locked, AlreadyExists, RefConflict, MissingObject, InvalidRefName, UnmergedIndex, NothingToCommit, Other, Injected}`; `LockFile`, `sync_parent`, `failpoint(name)` |
| WP1 init/add | `write/init.rs`, `write/object.rs`, `write/index.rs`, `crates/rgit-core/tests/init_add*.rs` | `write::init(&Path, &InitOptions) -> Repository`; `Repository::write_blob(&[u8])`; `Repository::add(&[&BStr]) -> AddOutcome` |
| WP2 commit/refs | `write/commit.rs`, `write/refs.rs`, optional `write/branch.rs`, `crates/rgit-core/tests/commit_refs*.rs` | `Repository::write_tree_from_index()`; `Repository::commit(&CommitRequest) -> CommitOutcome`; `Repository::update_ref(&RefUpdate)` |
| WP3 oracle | `crates/rgit-cli/tests/lifecycle*.rs`, `crates/rgit-cli/tests/lifecycle_support/**` | none (tests only) |

Changing a frozen signature or adding an `Error` variant goes through the
integration owner.

## Failpoints (test-only, feature `failpoints`)

`RGIT_FAILPOINT=<name>[:error|:pause]`; default action aborts the process.
Pause uses `RGIT_FAILPOINT_READY` / `RGIT_FAILPOINT_RELEASE` file paths.

| Name | Point |
| --- | --- |
| `lock.acquired` | lock file created, nothing written |
| `lock.before-rename` | lock content synced, not yet renamed |
| `lock.after-rename` | renamed, directory not yet synced |
| `init.before-publish` | private Git directory complete, not yet renamed |
| `add.objects-written` | blobs synced, index lock not yet taken |
| `add.index-locked` | `index.lock` held and written, before commit |
| `commit.objects-written` | tree/commit synced, ref not yet locked |
| `commit.ref-locked` | ref lock held, expected value verified, before rename |

## CLI contract (integration owner)

| Invocation | Contract |
| --- | --- |
| `rgit [-C dir] init [-q] [--bare] [-b/--initial-branch <name>] [--object-format sha1\|sha256] [<directory>]` | Prints `Initialized empty Git repository in <abs git dir>/` unless `-q`. Existing repository: exit 128, unchanged. |
| `rgit [-C dir] add [--] <path>...` | Literal paths relative to the `-C` directory; `.` allowed; no glob magic. Silent on success. |
| `rgit [-C dir] commit [-q] -m <msg>... [--allow-empty]` | Multiple `-m` join with a blank line, trailing newline added. Identity from `GIT_{AUTHOR,COMMITTER}_{NAME,EMAIL,DATE}`, else repository-local `user.name`/`user.email`; missing identity fails before any write. |

Errors exit 128 with `fatal:` diagnostics; argument errors exit 2.
