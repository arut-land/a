"""OS process accounting; psutil calls native APIs on each hosted platform."""
import json
import sys

import psutil


def snapshot(pid, threads=False):
    process = psutil.Process(pid)
    cpu = process.cpu_times()
    memory = process.memory_info()
    result = {"rss_kib": process.memory_info().rss / 1024,
              "cpu_seconds": cpu.user + cpu.system,
              "user_seconds": cpu.user, "kernel_seconds": cpu.system,
              "thread_count": process.num_threads(),
              "ctx_switches": switches(process)}
    for field in ("pfaults", "pageins", "num_page_faults"):
        if hasattr(memory, field):
            result[field] = getattr(memory, field)
    if sys.platform == "linux":
        from pathlib import Path
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        result.update(minor_faults=int(fields[7]), major_faults=int(fields[9]))
    try:
        result.update(memory_peaks(pid, process))
    except (OSError, AttributeError):
        pass
    # Windows idle diagnosis needs thread counters; macOS requires a debug task port.
    if threads and sys.platform == "win32":
        result["threads"] = [dict(id=t.id, user_seconds=t.user_time, kernel_seconds=t.system_time)
                             for t in process.threads()]
    return result


def memory_maps(pid):
    """Diagnostic only: region size and residency have separate units/fields."""
    process = psutil.Process(pid)
    if sys.platform == "darwin":
        import subprocess
        result = subprocess.run(["vmmap", "-resident", str(pid)], capture_output=True,
                                text=True, timeout=30, check=True)
        return dict(source="vmmap -resident (rounded native region columns)", vmmap=result.stdout,
                    **vmmap_executable(result.stdout, process.exe()))
    maps = [m._asdict() for m in process.memory_maps(grouped=False)]
    if sys.platform == "win32":
        for region in maps:
            region["mapped_bytes"] = region.pop("rss")
        windows_residency(pid, maps)
    return dict(source="QueryWorkingSetEx" if sys.platform == "win32" else "smaps",
                maps=maps, executable=process.exe(),
                executable_resident_bytes=sum(m.get("resident_bytes", m.get("rss", 0)) for m in maps
                                              if m["path"] == process.exe()),
                executable_code_resident_bytes=sum(m.get("resident_bytes", m.get("rss", 0)) for m in maps
                    if m["path"] == process.exe() and "x" in m.get("perms", "")))


def vmmap_executable(text, executable):
    import re
    from pathlib import Path
    executable = str(Path(executable).resolve())
    mapped, resident, code = [], [], []
    for line in text.splitlines():
        if not line.endswith(" " + executable):
            continue
        match = re.match(r"(\S+)\s+[0-9a-f]+-[0-9a-f]+\s+\[\s*([\d.]+[BKMG]?)\s+([\d.]+[BKMG]?)", line)
        if match is None:
            continue
        region, virtual, physical = match.groups()
        def size(value):
            scale = {"B": 1, "K": 1024, "M": 1024 ** 2, "G": 1024 ** 3}
            return float(value[:-1]) * scale[value[-1]] if value[-1] in scale else float(value)
        mapped.append(size(virtual))
        resident.append(size(physical))
        if region == "__TEXT":
            code.append(size(physical))
    if not resident:
        raise ValueError(f"vmmap did not identify the executable {executable}")
    return dict(executable=executable, executable_mapped_bytes=sum(mapped),
                executable_resident_bytes=sum(resident), executable_code_resident_bytes=sum(code))


def windows_residency(pid, maps):
    import ctypes
    from ctypes import wintypes
    import mmap

    class Page(ctypes.Structure):
        _fields_ = [("address", ctypes.c_void_p), ("flags", ctypes.c_size_t)]

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    psapi = ctypes.WinDLL("psapi", use_last_error=True)
    kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel.OpenProcess.restype = wintypes.HANDLE
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel.CloseHandle.restype = wintypes.BOOL
    psapi.QueryWorkingSetEx.argtypes = [wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD]
    psapi.QueryWorkingSetEx.restype = wintypes.BOOL
    handle = kernel.OpenProcess(0x0400, False, pid)  # PROCESS_QUERY_INFORMATION
    if not handle:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        for region in maps:
            start = int(region["addr"], 16)
            end = start + region["mapped_bytes"]
            resident = 0
            # Bound diagnostic scratch, not the number or size of mappings.
            for base in range(start, end, mmap.PAGESIZE * 4096):
                pages = (Page * min(4096, (end - base + mmap.PAGESIZE - 1) // mmap.PAGESIZE))()
                for n, page in enumerate(pages):
                    page.address = base + n * mmap.PAGESIZE
                if not psapi.QueryWorkingSetEx(handle, pages, ctypes.sizeof(pages)):
                    raise ctypes.WinError(ctypes.get_last_error())
                resident += sum(bool(page.flags & 1) for page in pages) * mmap.PAGESIZE
            region["resident_bytes"] = resident
    finally:
        kernel.CloseHandle(handle)


def memory_peaks(pid, process):
    """The lifetime peak resident set, and on macOS the footprint the OS
    itself charges the process (freed-but-reusable pages leave it at once)."""
    if sys.platform == "linux":
        with open(f"/proc/{pid}/status") as status:
            for line in status:
                if line.startswith("VmHWM:"):
                    return {"peak_rss_kib": int(line.split()[1])}
        return {}
    if sys.platform == "win32":
        return {"peak_rss_kib": process.memory_info().peak_wset / 1024}
    if sys.platform == "darwin":
        import ctypes
        import struct
        buffer = ctypes.create_string_buffer(16 + 8 * 40)
        # proc_pid_rusage with RUSAGE_INFO_V4: a UUID, then 64-bit counters.
        if ctypes.CDLL("/usr/lib/libproc.dylib").proc_pid_rusage(int(pid), 4, buffer) != 0:
            return {}
        fields = struct.unpack_from("<40Q", buffer, 16)
        return {"footprint_kib": fields[7] / 1024, "peak_footprint_kib": fields[28] / 1024}
    return {}


def switches(process):
    """Context switches of the whole process: the task's count on macOS and the
    threads' counters on Windows. Linux's /proc/<pid>/status (what psutil reads)
    counts the main thread only, so the live threads' counts are summed there."""
    if not sys.platform.startswith("linux"):
        return sum(process.num_ctx_switches())
    from pathlib import Path
    total = 0
    for task in Path(f"/proc/{process.pid}/task").iterdir():
        try:
            status = (task / "status").read_text()
        except OSError:
            continue  # exited since listed
        total += sum(int(value) for name, value in (line.split(":", 1) for line in status.splitlines())
                     if name in ("voluntary_ctxt_switches", "nonvoluntary_ctxt_switches"))
    return total


def run_timed(argv, timeout=300, check=True, **kwargs):
    """Bound a measured command and stop only its owned process tree on timeout."""
    import subprocess

    if kwargs.pop("capture_output", False):
        import tempfile
        text = any(kwargs.get(key) for key in ("text", "universal_newlines", "encoding", "errors"))
        options = dict(mode="w+" if text else "w+b",
                       encoding=kwargs.get("encoding"), errors=kwargs.get("errors"))
        # An inherited pipe writer must not extend a completed command's lifetime.
        with tempfile.TemporaryFile(**options) as out, tempfile.TemporaryFile(**options) as err:
            try:
                result = run_timed(argv, timeout, check=False, stdout=out, stderr=err, **kwargs)
            except subprocess.TimeoutExpired as error:
                out.seek(0)
                err.seek(0)
                error.output, error.stderr = out.read(), err.read()
                raise
            out.seek(0)
            err.seek(0)
            result.stdout, result.stderr = out.read(), err.read()
        if check:
            result.check_returncode()
        return result
    child = subprocess.Popen(argv, **kwargs)
    try:
        owner = psutil.Process(child.pid)
    except psutil.NoSuchProcess:
        owner = None
    try:
        stdout, stderr = child.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        try:
            owned = [owner, *reversed(owner.children(recursive=True))] if owner else []
        except psutil.NoSuchProcess:
            owned = []
        for process in owned:
            try:
                process.kill()
            except psutil.NoSuchProcess:
                pass
        child.kill()
        stdout, stderr = child.communicate(timeout=10)
        raise subprocess.TimeoutExpired(argv, timeout, output=stdout, stderr=stderr) from None
    result = subprocess.CompletedProcess(argv, child.returncode, stdout, stderr)
    if check:
        result.check_returncode()
    return result


def stop_daemon(binary, env, cwd=None):
    """Wait for this isolated daemon to release its staged executable on Windows."""
    from pathlib import Path
    import subprocess

    pidfile = Path(env["A_HOME"]) / "run/daemon.pid"
    daemon = None
    if pidfile.exists():
        try:
            daemon = psutil.Process(int(pidfile.read_text().splitlines()[0]))
        except psutil.NoSuchProcess:
            pass
    try:
        run_timed([str(binary), "daemon", "stop"], env=env, cwd=cwd,
                  capture_output=True, timeout=60)
        if daemon is not None:
            daemon.wait(timeout=60)
    except (subprocess.SubprocessError, psutil.TimeoutExpired):
        if daemon is not None:
            try:
                daemon.kill()
                daemon.wait(timeout=10)
            except psutil.NoSuchProcess:
                pass
        raise


def measure_windows(argv, **kwargs):
    """Keep the child handle open after exit to query lifetime peak working set."""
    import ctypes
    from ctypes import wintypes
    import subprocess
    import time

    class Memory(ctypes.Structure):
        _fields_ = [("cb", wintypes.DWORD), ("faults", wintypes.DWORD),
                    *[(name, ctypes.c_size_t) for name in (
                        "peak", "working", "peak_paged", "paged", "peak_nonpaged",
                        "nonpaged", "pagefile", "peak_pagefile")]]

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    psapi = ctypes.WinDLL("psapi", use_last_error=True)
    psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(Memory), wintypes.DWORD]
    psapi.GetProcessMemoryInfo.restype = wintypes.BOOL
    kernel.GetProcessTimes.argtypes = [wintypes.HANDLE, *[ctypes.POINTER(wintypes.FILETIME)] * 4]
    kernel.GetProcessTimes.restype = wintypes.BOOL
    started = time.perf_counter()
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, **kwargs)
    try:
        child.wait(timeout=120)
        elapsed = time.perf_counter() - started
        memory = Memory()
        memory.cb = ctypes.sizeof(memory)
        times = [wintypes.FILETIME() for _ in range(4)]
        # Popen retains its native process handle until the object is released.
        handle = wintypes.HANDLE(int(child._handle))
        if not psapi.GetProcessMemoryInfo(handle, ctypes.byref(memory), memory.cb):
            raise ctypes.WinError(ctypes.get_last_error())
        if not kernel.GetProcessTimes(handle, *[ctypes.byref(value) for value in times]):
            raise ctypes.WinError(ctypes.get_last_error())
        assert child.returncode == 0, (argv, child.returncode)
        kernel_seconds, user_seconds = [((value.dwHighDateTime << 32) + value.dwLowDateTime) / 10_000_000
                                        for value in times[2:]]
        assert memory.peak > 0, "Windows must report a real peak working set"
        return dict(wall_seconds=elapsed, cpu_seconds=user_seconds + kernel_seconds,
                    user_seconds=user_seconds, kernel_seconds=kernel_seconds, peak_rss_kib=memory.peak / 1024,
                    page_faults=memory.faults)
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()


if __name__ == "__main__":
    if sys.argv[1] == "stop":
        import os
        stop_daemon(sys.argv[2], os.environ)
    else:
        record = snapshot(int(sys.argv[1]))
        print(int(record["rss_kib"]) if len(sys.argv) > 2 else json.dumps(record))
