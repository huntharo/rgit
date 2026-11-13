"""Darwin MAP_SHARED regression fixture; deliberately never call msync/fsync.

Python's mmap object can retain a duplicate FD. Call libc directly so the only
mapper FD is closed before baseline status and before modifying mapped bytes.
The Rust test bounds this whole helper, including every fresh rgit process.
"""

import ctypes
import os
import subprocess
import sys


class StatFS(ctypes.Structure):
    # Darwin's public struct statfs (sys/mount.h), including its trailing fields.
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


def main():
    binary, root, phase = sys.argv[1:]
    assert sys.platform == "darwin"
    assert phase in ("live", "final-unmap")
    libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
    libc.statfs.argtypes = [ctypes.c_char_p, ctypes.POINTER(StatFS)]
    libc.statfs.restype = ctypes.c_int
    info = StatFS()
    check(libc.statfs(os.fsencode(root), ctypes.byref(info)), "statfs")
    filesystem = bytes(info.fstypename).decode("ascii")
    if filesystem != "apfs":
        print(f"SKIP: native APFS {phase} gate: filesystem is {filesystem}")
        return

    libc.mmap.argtypes = [
        ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int,
        ctypes.c_int, ctypes.c_int64,
    ]
    libc.mmap.restype = ctypes.c_void_p
    libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
    libc.munmap.restype = ctypes.c_int
    path = os.path.join(root, "mapped")
    original = b"A" * 8192
    changed = b"B" * len(original)
    descriptor = os.open(path, os.O_RDWR)
    try:
        assert os.fstat(descriptor).st_size == len(original)
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

        def status():
            result = subprocess.run(
                [binary, "-C", root, "status", "--porcelain=v1", "--no-renames", "-z"],
                capture_output=True, timeout=10, check=False,
            )
            assert result.returncode == 0, (result.returncode, result.stderr)
            assert result.stderr == b"", result.stderr
            return result.stdout

        assert status() == b"", "baseline must be clean with mapping already open"
        with open(path, "rb") as reader:
            assert reader.read() == original
        before = os.stat(path)
        ctypes.memmove(address, changed, len(changed))
        assert ctypes.string_at(address, len(changed)) == changed
        if phase == "final-unmap":
            check(libc.munmap(address, len(original)), "munmap")
            address = None  # Last shared mapping is gone before reader/status.
        # A new file descriptor verifies bytes independently from the mapping.
        with open(path, "rb") as reader:
            assert reader.read() == changed, "independent reader saw stale bytes"
        after = os.stat(path)
        assert before.st_size == after.st_size
        assert status() == b" M mapped\0", f"{phase}: fresh rgit missed visible bytes"
        print(
            f"PASS: APFS {phase}; closed mapper FD; independent bytes verified; "
            f"fresh rgit detected change; "
            f"mtime_ns {before.st_mtime_ns}->{after.st_mtime_ns}; "
            f"ctime_ns {before.st_ctime_ns}->{after.st_ctime_ns}"
        )
    finally:
        if address is not None:
            check(libc.munmap(address, len(original)), "munmap cleanup")


if __name__ == "__main__":
    main()
