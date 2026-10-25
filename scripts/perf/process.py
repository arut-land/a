"""OS process accounting; psutil calls native APIs on each hosted platform."""
import json
import sys

import psutil


def snapshot(pid, threads=False):
    process = psutil.Process(pid)
    cpu = process.cpu_times()
    result = {"rss_kib": process.memory_info().rss / 1024,
              "cpu_seconds": cpu.user + cpu.system,
              "user_seconds": cpu.user, "kernel_seconds": cpu.system,
              "thread_count": process.num_threads(),
              # /proc status on Linux, the task's switch count on macOS, thread counters on Windows.
              "ctx_switches": sum(process.num_ctx_switches())}
    # Windows idle diagnosis needs thread counters; macOS requires a debug task port.
    if threads and sys.platform == "win32":
        result["threads"] = [dict(id=t.id, user_seconds=t.user_time, kernel_seconds=t.system_time)
                             for t in process.threads()]
    return result


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
                    user_seconds=user_seconds, kernel_seconds=kernel_seconds, peak_rss_kib=memory.peak / 1024)
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
