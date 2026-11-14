# 0004: Anchored filesystem access for write operations (PWR-194 contract)

Status: proposed for the PWR-194 hardening batch. Applies to the experimental
write slice (`init`, `add`, `commit`, reference publication) only; read-only
status/diff/inspection keep their documented preflight model.

## Threat model

In scope: another process running as the same user modifies the worktree or
Git directory concurrently with an rgit write, by swapping an ancestor
directory for a symlink (or junction), swapping a leaf for a symlink, FIFO or
other file, replacing `.git/info/{exclude,attributes}`, or removing and
recreating a lock file. rgit must not read from, write to, publish into or
delete anything outside the policy root it intended, must not block
indefinitely, and must not publish or unlink another writer's lock.

Out of scope, and documented as such: the policy root path itself being
replaced before rgit opens it (the root is resolved once, following symlinks
as C Git does); hostile ownership or permissions; kernel/filesystem bugs;
power loss; read-only commands. No claim of a general hostile-filesystem
sandbox is made beyond what the tests below demonstrate.

## Policy roots

`crate::fsx::Anchor` holds an open directory handle for one of three roots:
`Root::Worktree` (`Repository::work_dir`), `Root::GitDir` (per-worktree Git
directory: `HEAD`, `index`, per-worktree refs and logs) and `Root::CommonDir`
(objects, shared refs, `packed-refs`, shared logs, `info/`). In a linked
worktree `GitDir` and `CommonDir` differ and are anchored separately. Relative
paths are `/`-separated native bytes; empty, `.` and `..` components are
refused; every component is resolved without following symlinks.

## Frozen API (`crates/rgit-core/src/fsx/mod.rs`)

`Anchor::{open, sub, lstat, open_read, read_link, read_dir, create_new, mkdir,
rename_noreplace, rename_replace, hard_link, unlink, sync, display_path}`,
`fsx::file_id(&File) -> FileId`, types `Root`, `Kind`, `Meta`, `FileId`
(`(dev, ino)` on Unix, `(volume serial, file index)` on Windows; never size or
timestamps), and `LockFile::acquire_at(&Anchor, rel)`. A backend that cannot
provide an operation's guarantee returns `UnsupportedCapability`; there is no
weaker fallback. Signature changes go through the integration owner.

## Ownership

| Owner | Files |
| --- | --- |
| Integration | `fsx/mod.rs`, `fsx/portable.rs` (baseline, path-based, not anchored), `lib.rs`, `write/mod.rs`, Cargo manifests/lockfile, CLI, docs, CI |
| WP1 Unix anchoring | `fsx/unix.rs` (rustix `openat`/`statat`/`mkdirat`/`renameat_with`/`linkat`/`unlinkat`/`readlinkat`/`Dir`, `O_NOFOLLOW|O_DIRECTORY|O_CLOEXEC|O_NONBLOCK`); call-site adoption in `write/index.rs`, `write/init.rs`, `write/object.rs`, `write/refs.rs`, `write/commit.rs`; `.git/info/{exclude,attributes}` captured through anchored reads into memory-backed Gitoxide sources; tests `crates/rgit-core/tests/fsx_unix*.rs` |
| WP2 Windows identity | `fsx/windows.rs` (handle-based identity, e.g. `winapi-util::file::information`, already in the dependency tree; publication strategy for the close-before-rename window); `write/lock.rs` (including `acquire_at` and Windows identity/publication); tests `crates/rgit-core/tests/fsx_windows*.rs` |
| WP3 oracle | `crates/rgit-cli/tests/fs_hardening*.rs`, `crates/rgit-cli/tests/fs_hardening_support/**` (tests only) |

New failpoints are proposed by their owner and recorded here by the
integration owner. Own crates keep `unsafe_code = "forbid"`; dependencies may
use unsafe internally. Dependencies stay pinned (`--locked`); a new direct
dependency needs integration approval. MSRV remains 1.88.

## Acceptance

Deterministic pause-point tests (WP3, plus each worker's own) demonstrate,
per platform where the mechanism exists:

1. An ancestor directory of a staged path swapped for a symlink to an outside
   directory after the anchor was opened: the operation refuses or completes
   on the original tree; no byte outside the root is read or written.
2. Same for the Git directory: object, index, ref and reflog writes cannot be
   redirected by swapping `objects/xx`, `refs/heads/...` or `logs/...`.
3. `.git/info/exclude` and `.git/info/attributes` swapped for a FIFO or symlink
   at a pause point: bounded refusal or consistent completion, never a hang.
4. Lock identity: a lock removed and recreated (Unix) is never published or
   unlinked; on Windows deletion of a held lock is refused and the
   close-before-rename window is either closed or explicitly bounded with a
   concrete blocker.
5. Existing APFS live/final-unmap, lifecycle, failpoint and recovery suites
   stay green unchanged; meaningful guards are mutation-checked.
