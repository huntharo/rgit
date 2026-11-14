# Proposal: safe handle-relative rename and link on Windows (PWR-197)

Status: proposal for review. Nothing here is implemented. Decision 0004 keeps
Windows on the legacy path backend (`fsx::HARDENING == LegacyPath`). Shipped
Windows `init`, `add`, `commit` and reference updates keep their behaviour.
This document describes what an anchored Windows backend would need, and how
a reviewed dependency could provide it without `unsafe` in rgit's own crates.
Neither own-crate `unsafe` nor a new package published by this project is
approved. The lack of an existing wrapper is a blocker, not evidence that one
cannot be built.

Primary sources are cited inline as [n]; the list is at the end. Every
Microsoft Learn citation was read by the author from learn.microsoft.com on
2026-11-13; statements the pages do not make (notably specific NTSTATUS
codes) are marked "confirm in CI" rather than asserted.

This proposal names candidates only. It does not select a dependency, and it
does not approve upstream pull requests or contact with other projects,
separately published packages, or own-crate `unsafe` exceptions.

## What is needed and why

The Unix backend resolves every component below a root through held
directory descriptors. A Windows backend with the same claim
("descriptor-root identity") would need the following operations, each bound
to handles rather than re-resolved paths:

| # | Operation | rgit use | Must not |
| --- | --- | --- | --- |
| 1 | Open or create an entry relative to a directory handle, with no reparse traversal | `Anchor::sub`, `lstat`, `open_read`, `create_new`, `open_append`, `mkdir` | follow a junction or symlink at any component |
| 2 | **No-replace rename** relative to directory handles | temporary-object and init publication (`rename_noreplace`) | replace an existing entry |
| 3 | **Replace rename** relative to directory handles | lock publication (`rename_replace`) | publish anything other than our lock |
| 4 | **Hard link** relative to a directory handle, no replace | loose-object publication (`hard_link`) | replace an existing object |
| 5 | **Rename of an open lock handle** (our own handle, not a path) | closes the close-before-rename window of `write/lock.rs` | require closing our deny-delete handle |
| 6 | Delete by handle | `unlink`, `rmdir`, abandoning a lock | delete a file other than the one opened |

Operation 1 already exists in safe wrappers (see Candidates below). Operations
2 to 5 are the blocker; operation 6 also exists. The window in operation 5
today: `LockFile::commit` (path form) must close its deny-delete handle
before `MoveFileExW`, because `MoveFileExW` reopens the source by path with
DELETE access. A replacement of the lock in that window would be published.
The legacy lock's FileId checks (PR B) detect some outcomes of this, but they
do not close the window.

## The ABI

### Win32: `SetFileInformationByHandle`

- Its Remarks table of settable classes lists `FileBasicInfo`,
  `FileRenameInfo`, `FileDispositionInfo`, `FileAllocationInfo`,
  `FileEndOfFileInfo` and `FileIoPriorityHintInfo` [1].
- `FILE_INFO_BY_HANDLE_CLASS` also defines `FileRenameInfoEx` and
  `FileDispositionInfoEx`, but with blank descriptions and no stated minimum
  OS [2]. Rust's std does call `SetFileInformationByHandle(FileRenameInfoEx)`
  internally, but only as a fallback that reopens the source by path [9].
- There is **no hard-link class** in the Win32 enum [2]. Operation 4 is not
  available through `SetFileInformationByHandle` at all; only through
  `NtSetInformationFile` or the path-based `CreateHardLinkW`.
- `FILE_RENAME_INFO` [3]: `union { BOOLEAN ReplaceIfExists; DWORD Flags; }`,
  `HANDLE RootDirectory`, `DWORD FileNameLength` (bytes, no terminating NUL),
  `WCHAR FileName[]`. `Flags` is used with `FileRenameInfoEx`,
  `ReplaceIfExists` with `FileRenameInfo`.
  - `RootDirectory`: "If FileName specifies a relative name, this field can
    be a handle to the directory relative to which the new name is resolved."
  - `ReplaceIfExists`: FALSE with an existing target "will return an error".
  - Minimum OS: Windows Vista / Server 2008 [1][3].

### Native: `NtSetInformationFile`

- `FileRenameInformation` (10): "The caller must have DELETE access to the
  file" [4].
- `FileLinkInformation` (72): creates a hard link; not supported on every
  file system (NTFS yes, FAT no) [4]. `FILE_LINK_INFORMATION`: "No specific
  access rights are required to set this information" [6].
- `FileDispositionInformation` (13) and `FileDispositionInformationEx` (64):
  the handle must have been opened with DELETE [4].
- `FileRenameInformationEx` and `FileLinkInformationEx` are absent from the
  `NtSetInformationFile` class table [4]. They are documented only through
  their structure pages, under version guards:
  - `FILE_RENAME_INFORMATION.Flags` exists for `_WIN32_WINNT_WIN10_RS1`
    (Windows 10 1607) [5];
  - `FILE_LINK_INFORMATION.Flags` exists for `_WIN32_WINNT_WIN10_RS5`
    (Windows 10 1809) [6].
- `FILE_RENAME_INFORMATION` [5]:
  - `RootDirectory` is "a handle for the root directory under which the file
    will reside after it is renamed", or NULL for the same directory or a
    full path. Open it with traverse and read-attribute access to avoid
    sharing conflicts [5].
  - `ReplaceIfExists` FALSE: "the rename operation will fail if a file with
    the given name already exists" [5].
  - Even with replace, renaming over a directory, a read-only file or a
    running executable fails. "A file cannot be renamed if a file with the
    same name exists and has open handles", except with
    `FILE_RENAME_POSIX_SEMANTICS` [5].
  - Flags (Ex): `FILE_RENAME_REPLACE_IF_EXISTS` 0x1,
    `FILE_RENAME_POSIX_SEMANTICS` 0x2 ("allow replacing a file even if there
    are existing handles to it"), `FILE_RENAME_IGNORE_READONLY_ATTRIBUTE`
    0x40 (needs WRITE_ATTRIBUTES on the replaced file), and storage-reserve
    flags not needed here [5].
- `FILE_LINK_INFORMATION` [6]: `RootDirectory` is "a handle for the directory
  where the link is to be created", or NULL for the same directory or a full
  path. `ReplaceIfExists` FALSE fails if the link already exists. Flags (Ex):
  `FILE_LINK_REPLACE_IF_EXISTS` 0x1 and `FILE_LINK_POSIX_SEMANTICS` 0x2. The
  buffer must be at least `sizeof(FILE_LINK_INFORMATION)` and 4-byte aligned.
- Minimum OS for `NtSetInformationFile`: Windows 2000 [4].

**Recommendation.** Use the native classes, with `RootDirectory` set to the
destination directory handle and `FileName` set to a single leaf component:
- `FileRenameInformation` with `ReplaceIfExists` FALSE (operation 2) or TRUE
  (operation 3);
- `FileLinkInformation` with `ReplaceIfExists` FALSE (operation 4).
These are the classic (non-Ex) classes with the widest support. They give one
documented `RootDirectory` contract for rename and link, and they cover link,
which Win32 cannot. Use `FILE_RENAME_POSIX_SEMANTICS` only if a measured need
appears, such as replacing a target that a reader holds open. It needs
Windows 10 1607 or later and NTFS, so it has to be optional with an explicit
refusal, not a silent fallback.

## Access, sharing and the open-lock rename (operation 5)

`CreateFileW`, `FILE_SHARE_DELETE`: "Otherwise, no process can open the file
or device if it requests delete access ... Delete access allows both delete
and rename operations" [7]. Today's lock is opened without
`FILE_SHARE_DELETE`, so while it is held nobody, including rgit, can open it
for DELETE. That is why `MoveFileExW` requires closing it first.

With a handle rename the lock would be created with:
- DesiredAccess including `DELETE` (plus `GENERIC_WRITE`; `FILE_READ_ATTRIBUTES`
  for identity);
- ShareMode `FILE_SHARE_READ | FILE_SHARE_WRITE` (still no
  `FILE_SHARE_DELETE`).

`FileRenameInformation` would then be set **on that handle**. The rename moves
exactly the file our handle refers to. No second open is needed, so the
deny-delete share mode stays in force for every other process until we close.
Because the share mode denies DELETE, no other process can rename or delete
the lock while we hold it. This closes the window rather than detecting it.
`RootDirectory` is the target's directory handle, and the lock stays open
across the rename, as on Unix.

Operation 6 sets `FileDispositionInformation` (DeleteFile = TRUE) on our own
DELETE-access handle. Abandoning a lock then deletes our file and never the
entry currently at the path, which closes the Drop window as well.

## FileId and ownership checks

Identity is `(volume serial, file index)` from `GetFileInformationByHandle`.
rgit reads it through `winapi-util` 0.1.11 in `fsx/portable.rs`. On ReFS the
complete identity is the 128-bit `FILE_ID_INFO.FileId` (`FileIdInfo`, Windows 8 /
Server 2012 or later) [8]. A wrapper should expose that class as well, and the
anchored backend should compare 128-bit IDs.

- Before the rename: identity is inherent, because the rename is applied to
  our handle. An anchored `lstat` of `<target>.lock` that matches the handle's
  FileId remains a cheap consistency assertion. A mismatch while we hold the
  deny-delete handle is not expected (nobody can rename or delete the lock),
  and is refused with `Locked` before the rename.
- After the rename: the target's FileId equals our handle's FileId. The
  handle stays open, so the file cannot have been freed and its index reused.
  A mismatch means another writer replaced the target after us, which is a
  legitimate concurrent publication. It is reported as observed state, never
  rolled back.
- No-replace operations (2, 4): success means our entry now exists at the
  destination. The FileId of the destination equals the source's (link) or
  our handle's (rename).

## Semantics and error mapping

| Condition | Expected status | rgit error |
| --- | --- | --- |
| Destination exists, no-replace (2, 4) | collision (Win32 `ERROR_ALREADY_EXISTS`; the NT status is not named in [5][6] and must be confirmed in CI) | `Error::AlreadyExists` (never retried as a replace) |
| Replace target is a directory, read-only, executing, or open without POSIX semantics | failure per [5] (`ERROR_ACCESS_DENIED` / sharing violation; confirm in CI) | refuse with the I/O cause; the lock remains ours and is abandoned by handle |
| Different volume (`RootDirectory` on another volume) | `ERROR_NOT_SAME_DEVICE` (confirm) | `Error::Io`, nothing published |
| File system without hard links (FAT) [4] | not supported | `UnsupportedCapability`, before any mutation if probed at `Anchor::open` |
| Ex class or POSIX flag on an older OS | invalid class / not supported | `UnsupportedCapability`; never a silent non-POSIX fallback |
| Rename succeeded, directory flush unavailable | n/a | as today on Windows: visible, durability unknown (documented) |

"Not named" rows are explicit gaps in the primary documentation. The native
CI tests below must pin them before the mapping is frozen.

## How a dependency could expose this safely

| Candidate | License, MSRV | What it offers | Gap |
| --- | --- | --- | --- |
| `fs_at` 0.2.1 (rbtcollins) | Apache-2.0, 1.71 | Safe `open_at`, `open_dir_at`, `mkdir_at`, `unlink_at`, `rmdir_at`, `symlink_at`, `read_dir`: `NtCreateFile` with `RootDirectory`, `FILE_OPEN_REPARSE_POINT`, delete by handle | No rename and no link. Its existing `NtCreateFile` and alignment code is the closest match to the surface below |
| `cap-primitives` / `cap-std` 4.0.3 (Bytecode Alliance) | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT; no declared MSRV | Handle-relative open and stat (`CreateFileAtW` → `NtCreateFile`) | `rename`, `hard_link`, `create_dir` and `remove_*` resolve the directory handle back to a path (`GetFinalPathNameByHandleW`) and call std path functions; no no-replace rename; about 10 extra crates |
| `winx` 0.36.4 | Apache-2.0 WITH LLVM-exception, 1.63 | Handle path, access and mode queries, reopen | No rename or link |
| `winapi-util` 0.1.11 | Unlicense OR MIT | Safe `GetFileInformationByHandle` (in use) | Identity only |
| `windows-sys` 0.61 / `ntapi` 0.4 | MIT OR Apache-2.0 | Raw bindings | `unsafe extern` only; unusable under `unsafe_code = "forbid"` |
| std | — | `fs::rename` = `MoveFileExW` by path; `FileRenameInfoEx` only as an internal fallback [9] | No public handle rename |

The minimal safe surface a reviewed dependency would need (shown as Rust
signatures; all `&File` are handles the caller opened):

```rust
/// FileRenameInformation on `source`; `RootDirectory = dest_dir`,
/// `FileName = leaf` (one component, no separators).
fn rename_by_handle(source: &File, dest_dir: &File, leaf: &OsStr, replace: bool) -> io::Result<()>;
/// FileLinkInformation on `source`; never replaces when `replace` is false.
fn link_by_handle(source: &File, dest_dir: &File, leaf: &OsStr, replace: bool) -> io::Result<()>;
/// FileDispositionInformation(DeleteFile = TRUE) on `file` (needs DELETE).
fn delete_by_handle(file: &File) -> io::Result<()>;
/// FileIdInfo: (VolumeSerialNumber, 128-bit FileId).
fn file_id_128(file: &File) -> io::Result<(u64, u128)>;
/// NtCreateFile relative to `dir`, FILE_OPEN_REPARSE_POINT, with explicit
/// access, share mode and disposition (create-new / open-existing).
fn open_relative(dir: &File, leaf: &OsStr, options: &RelativeOpen) -> io::Result<File>;
```

The wrapper must validate `leaf` (non-empty, no `\` or `/`, not `.` or `..`).
It must build the variable-length structure with correct size and 4-byte
alignment [6], and map NTSTATUS through `RtlNtStatusToDosError` into
`io::Error`. Which audited dependency, if any, provides this surface is an
integration-owner decision outside this proposal; for reference, `fs_at`
0.2.1 (Apache-2.0, MSRV 1.71) would add `aligned`, `cvt` and `windows-sys`
0.52 on Windows.

## Native Windows CI tests and mutation checks

Run on `windows-latest` (NTFS); each also runs against a FAT/exFAT volume
where the runner allows, to pin the refusal paths.

1. No-replace rename (2): a target that exists as a file, as a directory or
   as a junction is refused with `AlreadyExists`, and both entries are byte-
   and identity-unchanged. Mutation: set `ReplaceIfExists = TRUE` in the
   wrapper call; the test must fail.
2. Replace rename of an open lock (3, 5): hold the lock with DELETE access
   and no `FILE_SHARE_DELETE`. Then:
   - another process's `DeleteFileW`/`MoveFileExW` of the lock fails;
   - our handle rename publishes;
   - the target FileId equals the handle's FileId;
   - the lock path is gone.
   Mutation: close the handle and rename by path; a concurrent replacement
   test (step 3) must then catch the published foreign file.
3. Replacement window (5), deterministic: the writer pauses at a test-only
   failpoint immediately before publication, as the existing failpoints do.
   During the pause the test tries to delete the lock and create a foreign
   file at the lock path, then releases the writer.
   - With handle rename the delete fails (deny-delete handle), the target is
     our file (FileId) and the foreign file is never published.
   - Mutation: the path form (close the handle, then rename by path) with
     the pause placed between the close and the rename. The replacement then
     succeeds and the foreign file is published every time, so the test
     fails on every run, not "eventually".
4. Anchored parents: no behaviour is claimed here until the measurements in
   "To be measured in CI" below exist. Once they do: an intermediate
   directory replaced (if Windows permits it at all) by another directory or
   a junction to an outside directory, after the anchor is opened. Rename,
   link, create and delete operate on the original directory (by handle) or
   refuse, and nothing outside changes. Mutation: path-based resolution; the
   outside directory gains an entry.
5. Hard link (4): no-replace refusal; the link's FileId equals the source's;
   FAT refusal surfaces as `UnsupportedCapability` before any temporary file
   is created.
6. Delete by handle (6). While we hold the deny-delete handle, nobody can
   replace the lock path, so a replacement cannot be set up directly.
   Instead:
   - assert that the replacement attempt itself fails (`DeleteFileW` and
     `MoveFileExW` of the held lock are refused) and the lock is unchanged;
   - assert that abandoning by handle removes exactly our file: its FileId is
     gone, and a different file at another name in the same directory is
     untouched.
   - Separately, for the current path form (close, then `DeleteFileW` by
     path), show that the window exists: pause between the close and the
     delete, replace the lock path with a foreign file, release; the path
     form deletes the foreign file. This mutation demonstrates what handle
     deletion removes.
7. Error mapping: each row of the table above is asserted with the observed
   NTSTATUS/Win32 code, recorded in the decision before the mapping is frozen.
8. Existing Windows write suites (`init_add*`, `commit_refs*`, `lifecycle*`)
   pass unchanged with `fsx::HARDENING == Anchored` on Windows.

## To be measured in CI (no claim made)

- Whether a directory that contains a file with an open handle (with and
  without `FILE_SHARE_DELETE`) can be renamed, deleted, or replaced by a
  junction while the handle is open, on NTFS and on ReFS.
- How a held `RootDirectory` handle behaves after its directory has been
  renamed or replaced: whether a rename or link relative to it still lands in
  the original directory, and which status is returned if it cannot.
- The NTSTATUS and Win32 codes for each row of the error-mapping table.

The design above must not rely on any of these until they are measured and
recorded.

## Sources

1. SetFileInformationByHandle: https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-setfileinformationbyhandle
2. FILE_INFO_BY_HANDLE_CLASS: https://learn.microsoft.com/en-us/windows/win32/api/minwinbase/ne-minwinbase-file_info_by_handle_class
3. FILE_RENAME_INFO: https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_rename_info
4. NtSetInformationFile: https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/nf-ntifs-ntsetinformationfile
5. FILE_RENAME_INFORMATION: https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_file_rename_information
6. FILE_LINK_INFORMATION: https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_file_link_information
7. CreateFileW (dwShareMode, FILE_SHARE_DELETE, FILE_FLAG_DELETE_ON_CLOSE): https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew
8. FILE_ID_INFO: https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_id_info
9. Rust std, `library/std/src/sys/fs/windows.rs` (`fn rename`): `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`, then on `ERROR_ACCESS_DENIED` only, reopen the source **by path** with DELETE and `SetFileInformationByHandle(FileRenameInfoEx, REPLACE_IF_EXISTS | POSIX_SEMANTICS)` (read from the 1.93.0 rust-src component).

The Win32 `FILE_DISPOSITION_INFO_EX` page returned 404 when fetched, so its
flags (`FILE_DISPOSITION_FLAG_*`) and minimum OS are not cited here. The
proposal relies only on the classic `FileDispositionInformation`, whose DELETE
requirement is documented in [4].
