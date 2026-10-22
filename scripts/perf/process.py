"""OS process accounting; psutil calls native APIs on each hosted platform."""
import json
import sys

import psutil


def snapshot(pid):
    process = psutil.Process(pid)
    cpu = process.cpu_times()
    return {"rss_kib": process.memory_info().rss / 1024,
            "cpu_seconds": cpu.user + cpu.system}


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
    subprocess.run([str(binary), "daemon", "stop"], env=env, cwd=cwd,
                   capture_output=True, check=True, timeout=60)
    if daemon is not None:
        daemon.wait(timeout=60)


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
        cpu = sum((value.dwHighDateTime << 32) + value.dwLowDateTime for value in times[2:]) / 10_000_000
        assert memory.peak > 0, "Windows must report a real peak working set"
        return dict(wall_seconds=elapsed, cpu_seconds=cpu, peak_rss_kib=memory.peak / 1024)
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
