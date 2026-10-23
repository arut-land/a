"""Temporary hosted probe: DLL attribution and Known Folder export residency."""
import ctypes
from ctypes import wintypes
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

import psutil

sys.path.insert(0, str(Path(__file__).parent))
from process import stop_daemon

ROOT = Path(__file__).resolve().parents[2]
OUTPUT = ROOT / "target/paths-attribution"


def modules(pid):
    return sorted({m.path.lower() for m in psutil.Process(pid).memory_maps()
                   if m.path.lower().endswith((".dll", ".exe"))})


def export_probe(mode):
    before = modules(os.getpid())
    name = {"kernelbase": "kernelbase.dll",
            "apiset": "api-ms-win-shell-shellfolders-l1-1-0.dll",
            "shell": "shell32.dll"}[mode]
    library = ctypes.WinDLL(name, winmode=0x800)
    try:
        function = library.SHGetKnownFolderPath
    except AttributeError:
        return dict(mode=mode, available=False, before=before, loaded=modules(os.getpid()))
    function.argtypes = [ctypes.c_void_p, wintypes.DWORD, wintypes.HANDLE,
                         ctypes.POINTER(ctypes.c_void_p)]
    function.restype = ctypes.c_int32
    import uuid
    guid = ctypes.create_string_buffer(uuid.UUID("f1b32785-6fba-4fcf-9d55-7b8e7f157091").bytes_le)
    pointer = ctypes.c_void_p()
    free_library = ctypes.WinDLL("api-ms-win-core-com-l1-1-0.dll", winmode=0x800)
    free = free_library.CoTaskMemFree
    free.argtypes = [ctypes.c_void_p]
    free.restype = None
    result = function(guid, 0, None, ctypes.byref(pointer))
    path = ctypes.wstring_at(pointer) if result >= 0 else None
    free(pointer)
    loaded = modules(os.getpid())
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.FreeLibrary.argtypes = [wintypes.HMODULE]
    kernel.FreeLibrary.restype = wintypes.BOOL
    assert kernel.FreeLibrary(library._handle)
    assert kernel.FreeLibrary(free_library._handle)
    return dict(mode=mode, available=True, result=result, path=path,
                before=before, loaded=loaded, released=modules(os.getpid()))


def daemon_probe(binary):
    with tempfile.TemporaryDirectory(prefix="a-modules-", dir=os.environ["RUNNER_TEMP"]) as directory:
        home = Path(directory)
        env = dict(os.environ, A_HOME=directory)
        env.pop("A_HOST", None)
        (home / "config.toml").write_text('[daemon]\nagent = "echo"\nidle_secs = 120\n')
        try:
            subprocess.run([str(binary), "ls"], env=env, cwd=home, check=True,
                           capture_output=True, timeout=60)
            pid = int((home / "run/daemon.pid").read_text().splitlines()[0])
            time.sleep(3)
            return dict(pid=pid, rss_kib=psutil.Process(pid).memory_info().rss / 1024,
                        modules=modules(pid))
        finally:
            stop_daemon(binary, env, home)


def main():
    OUTPUT.mkdir(parents=True, exist_ok=True)
    if len(sys.argv) > 1:
        print(json.dumps(export_probe(sys.argv[1])))
        return
    for mode in ("kernelbase", "apiset", "shell"):
        result = subprocess.run([sys.executable, __file__, mode], capture_output=True,
                                text=True, timeout=60)
        (OUTPUT / (mode + ".json")).write_text(result.stdout if result.returncode == 0 else json.dumps(dict(error=result.stderr)))
    for role, revision in [("main", "781586d5a9fec1dd8b5e2b316a28b00a48470f8e"),
                           ("regressed", "9bec7823b1a97a5a23c917de2b9c89aae56d1e2c"),
                           ("current-main", "4d978b0b94290ecbf5b851f08b6d987940c2da0c"),
                           ("candidate", "HEAD")]:
        source = OUTPUT / role
        subprocess.run(["git", "worktree", "add", "--detach", str(source), revision], check=True)
        env = dict(os.environ, CARGO_TARGET_DIR=str(OUTPUT / "build"))
        subprocess.run(["cargo", "build-release", "-p", "a", "--features", "fixtures"],
                       cwd=source, env=env, check=True)
        binary = OUTPUT / "build/release/a.exe"
        (OUTPUT / (role + ".json")).write_text(json.dumps(dict(
            revision=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip(), **daemon_probe(binary)), indent=2))


if __name__ == "__main__":
    main()
