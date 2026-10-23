"""Temporary hosted probe: sampled daemon and descendant RSS during shell output."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import psutil

from process import stop_daemon

output = Path('target/hosted-perf')
binary = str((output / ('a-fixtures.exe' if os.name == 'nt' else 'a-fixtures')).resolve())
records = []
for count, kind in ((1, "text"), (8, "text"), (1, "terminal_metadata")):
    with tempfile.TemporaryDirectory(prefix='a-process-wave-', dir=os.environ.get('RUNNER_TEMP')) as root:
        root = Path(root)
        home = root / 'home'
        home.mkdir()
        (home / 'config.toml').write_text('[daemon]\nagent = "echo"\nidle_secs = 120\n')
        env = dict(os.environ, A_HOME=str(home), A_TUI='plain')
        env.pop('A_HOST', None)
        children, files = [], []
        try:
            subprocess.run([binary, 'ls'], env=env, cwd=root, check=True, stdout=subprocess.DEVNULL, timeout=60)
            daemon = psutil.Process(int((home / 'run/daemon.pid').read_text().splitlines()[0]))
            time.sleep(15)
            idle = daemon.memory_info().rss
            peak, tree_peak, samples = idle, idle, 0
            started = time.monotonic()
            command = '$ yes x | head -c 8388608' if kind == 'text' else "$ printf '\\033]0;'; yes x | head -c 8388608; printf '\\007'"
            for i in range(count):
                stream = (root / f'client-{i}.log').open('wb')
                files.append(stream)
                children.append(subprocess.Popen([binary, '--mode', 'auto', '-p', command],
                    env=env, cwd=root, stdin=subprocess.DEVNULL, stdout=stream, stderr=stream))
            while any(child.poll() is None for child in children):
                assert time.monotonic() - started < 120, 'shell workload timed out'
                rss = daemon.memory_info().rss
                tree = rss
                for child in daemon.children(recursive=True):
                    try:
                        tree += child.memory_info().rss
                    except psutil.NoSuchProcess:
                        pass
                peak, tree_peak = max(peak, rss), max(tree_peak, tree)
                samples += 1
                time.sleep(.005)
            assert all(child.wait() == 0 for child in children)
            wall = time.monotonic() - started
            spill = [path.stat().st_size for path in (home / 'spill').rglob('*.log')]
            assert kind != 'text' or spill, spill
            assert not spill or max(spill) <= 64 << 20, spill
            time.sleep(15)
            records.append(dict(concurrent=count, kind=kind, sample_interval_ms=5, samples=samples,
                idle_before_kib=idle/1024, sampled_daemon_peak_kib=peak/1024,
                sampled_tree_peak_kib=tree_peak/1024, return_idle_kib=daemon.memory_info().rss/1024,
                wall_seconds=wall, retained_bytes=sum(spill)))
        finally:
            for child in children:
                if child.poll() is None:
                    child.kill()
                child.wait()
            for stream in files:
                stream.close()
            stop_daemon(binary, env, root)
(output / 'process-wave.json').write_text(json.dumps(records, indent=2) + '\n')
print(json.dumps(records, indent=2))
