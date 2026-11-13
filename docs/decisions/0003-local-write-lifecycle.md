# 0003: First recoverable local write lifecycle (G1 contract)

Status: accepted for the G1 init/add/commit slice (Linear PWR-178, PWR-175,
PWR-174). Write support is experimental and unreleased.

## Scope

`init`, `add`, `commit` and compare-and-swap reference publication for
SHA-1 and SHA-256 repositories that use standard files refs and a version 2
or 3 index without split/sparse extensions. Every other repository shape is refused
with a typed error before the first mutation: reftable, split or sparse
indexes, `extensions.compatobjectformat`, in-progress merge/rebase/cherry-pick
state, submodule (gitlink) staging, and any path whose attributes or config
would need a clean/smudge/process filter, `ident`, `working-tree-encoding` or
end-of-line conversion. Hooks are never run. No runtime path invokes C Git,
a shell or a helper.

Reftable remains required before any write-enabled release. Branch/switch,
reset variants and linked-worktree add/remove are later G1 issues.

## Invariants

1. **Objects before names.** Every object rgit writes is synced (file and
   containing directory on Unix) before an index entry or reference names it.
   Before publishing, `commit` also syncs its parent commit and every tree of
   the new commit (including ones that already existed), and `update_ref`
   syncs the loose object it publishes. Blobs named by the index are durable
   only if their writer synced them (rgit `add` does); packed objects are
   assumed durable. No published reference points at an unavailable
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
   published file; unreferenced objects may remain and are harmless. Locks the
   interrupted process held may remain and refuse later writers until removed
   (see the table below). An interrupted `init` leaves no `.git`, only its
   private `.git.rgit-init-*` (or `.<name>.rgit-init-*` for bare) directory,
   which a later `init` neither consumes nor deletes. rgit cannot prove a lock
   or temporary directory stale and never removes one it did not create in the
   same process; removal is a manual step after confirming no writer is
   running. Handled errors (no crash) remove their own locks and temporaries.

Durability: on Unix, file content and directory entries are synced before
publication. On Windows, std offers no directory sync; guarantees there are
visibility/atomic-rename only.

## Reference and reflog ordering

A reference update is not a transaction across the reference and its reflogs.
The order is: validate the name and the new object; take `<ref>.lock` (and,
for a commit on a branch, `HEAD.lock`); read and compare the current value
under the lock; write and sync the lock; rename it over the reference; sync the
directory; then append one line to each applicable reflog (the reference's log
and, when this worktree's `HEAD` names it, `HEAD`'s log), each append synced.

Consequences, all observable by a fresh process:

| Interruption or failure | Reference | Reflog | Reported |
| --- | --- | --- | --- |
| Before the rename (including refusal, CAS conflict, held lock) | old value | unchanged | error; state unchanged except possible unreferenced objects |
| Crash after the rename, before the appends | new value | entry missing | process died; no false entry is ever written. For a commit on a branch, `HEAD.lock` (held across the branch rename so `HEAD` cannot be re-pointed mid-commit) remains and refuses later writers until removed |
| Append fails after the rename | new value | entry missing (or present in one log but not the other) | `ReflogAppendError`: the reference *was* updated; CLI exits 128 with that diagnostic |
| Directory sync fails after the rename | new value visible | appended | `RefPublishedUnsyncedError`: the reference *was* updated, its durability is unknown (and a reflog append failure is named too) |

C Git appends the reflog before renaming the reference, so its failure modes
differ: it can record an entry for an update that never became visible. rgit
prefers a possibly missing entry to a false one. A caller (or test oracle)
must never treat an error from a write operation as proof that the repository
is unchanged; only the "before the rename" rows guarantee that.

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
| `init.before-head` | existing empty bare target populated in place except `HEAD`; `HEAD` not yet published |
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
