"""Darwin MAP_SHARED write gate for add/commit; never calls msync/fsync.

Adapted from tests/support/native_mmap.py (status gate). The only mapper FD is
closed before the mapped bytes change, so the change reaches the file through
the shared mapping alone and APFS may leave mtime/ctime untouched. Commands in
argv[3:] (each introduced by "::command::") then run as fresh processes, either while
the mapping is still live or after the final munmap. The Rust caller bounds
this whole helper and verifies the resulting repository independently.

Output is line based (no JSON dependency in the Rust test):
  STATUS PASS|SKIP <detail>
  STAT mtime_ns <before> <after> ctime_ns <before> <after>
  RESULT <rc> <stderr hex>
"""

import ctypes
import os
import subprocess
import sys


class StatFS(ctypes.Structure):
    # Darwin's public struct statfs (sys/mount.h), including trailing fields.
    _fields_ = [
        ("bsize", ctypes.c_uint32),
        ("iosize", ctypes.c_int32),
        ("blocks", ctypes.c_uint64),
        ("bfree", ctypes.c_uint64),
        ("bavail", ctypes.c_uint64),
        ("files", ctypes.c_uint64),
        ("ffree", ctypes.c_uint64),
        ("fsid", ctypes.c_int32 * 2),
        ("owner", ctypes.c_uint32),
        ("type", ctypes.c_uint32),
        ("flags", ctypes.c_uint32),
        ("fssubtype", ctypes.c_uint32),
        ("fstypename", ctypes.c_char * 16),
        ("mntonname", ctypes.c_char * 1024),
        ("mntfromname", ctypes.c_char * 1024),
        ("flags_ext", ctypes.c_uint32),
        ("reserved", ctypes.c_uint32 * 7),
    ]


def check(result, operation):
    if result == -1:
        raise OSError(ctypes.get_errno(), operation)
    return result


def commands(argv):
    groups, current = [], []
    for arg in argv:
        if arg == "::command::":
            if current:
                groups.append(current)
            current = []
        else:
            current.append(arg)
    if current:
        groups.append(current)
    return groups


def main():
    root, phase = sys.argv[1], sys.argv[2]
    to_run = commands(sys.argv[3:])
    assert sys.platform == "darwin"
    assert phase in ("live", "final-unmap")
    libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
    libc.statfs.argtypes = [ctypes.c_char_p, ctypes.POINTER(StatFS)]
    libc.statfs.restype = ctypes.c_int
    info = StatFS()
    check(libc.statfs(os.fsencode(root), ctypes.byref(info)), "statfs")
    filesystem = bytes(info.fstypename).decode("ascii")
    if filesystem != "apfs":
        print(f"STATUS SKIP native APFS {phase} write gate: filesystem is {filesystem}")
        return

    libc.mmap.argtypes = [
        ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int,
        ctypes.c_int, ctypes.c_int64,
    ]
    libc.mmap.restype = ctypes.c_void_p
    libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
    libc.munmap.restype = ctypes.c_int
    path = os.path.join(root, "mapped")
    with open(path, "rb") as reader:
        original = reader.read()
    changed = bytes(b ^ 0x01 for b in original)
    descriptor = os.open(path, os.O_RDWR)
    try:
        # PROT_READ | PROT_WRITE, MAP_SHARED; no Python mmap duplicate FD.
        address = libc.mmap(None, len(original), 0x1 | 0x2, 0x1, descriptor, 0)
        if address == ctypes.c_void_p(-1).value:
            raise OSError(ctypes.get_errno(), "mmap")
    finally:
        os.close(descriptor)
    try:
        try:
            os.fstat(descriptor)
        except OSError as error:
            assert error.errno == 9  # EBADF: mapper FD really is closed.
        else:
            raise AssertionError("mapper FD remained open")
        before = os.stat(path)
        ctypes.memmove(address, changed, len(changed))
        assert ctypes.string_at(address, len(changed)) == changed
        if phase == "final-unmap":
            check(libc.munmap(address, len(original)), "munmap")
            address = None
        with open(path, "rb") as reader:
            assert reader.read() == changed, "independent reader saw stale bytes"
        after = os.stat(path)
        assert before.st_size == after.st_size
        print(f"STATUS PASS APFS {phase}; closed mapper FD; independent bytes verified")
        print(
            f"STAT mtime_ns {before.st_mtime_ns} {after.st_mtime_ns} "
            f"ctime_ns {before.st_ctime_ns} {after.st_ctime_ns}"
        )
        for command in to_run:
            result = subprocess.run(
                command, capture_output=True, timeout=30, check=False,
            )
            print(f"RESULT {result.returncode} {result.stderr.hex()}")
        sys.stdout.flush()
    finally:
        if address is not None:
            check(libc.munmap(address, len(original)), "munmap cleanup")


if __name__ == "__main__":
    main()
