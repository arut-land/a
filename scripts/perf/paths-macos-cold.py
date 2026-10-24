"""Temporary cold-only paired macOS measurement, with resolver attribution."""
import json
import os
from pathlib import Path
import queue
import random
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time

from process import run_timed, stop_daemon

ROOT = Path(__file__).resolve().parents[2]
OUTPUT = ROOT / 'target/paths-macos-cold'
PROBE = '''fn main() {
    let started = std::time::Instant::now();
    let home = std::hint::black_box(sys::Home::from_env().unwrap());
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    std::hint::black_box(home.root());
    println!("{elapsed:.9}");
}
'''


def cold(binary):
    with tempfile.TemporaryDirectory(prefix='a-cold-', dir=os.environ['RUNNER_TEMP']) as directory:
        home = Path(directory)
        env = dict(os.environ, A_HOME=directory, A_TUI='plain')
        env.pop('A_HOST', None)
        (home / 'config.toml').write_text('[daemon]\nagent = "echo"\nidle_secs = 120\n')
        client = None
        try:
            run_timed([str(binary), 'ls'], env=env, cwd=home, capture_output=True, timeout=60)
            stop_daemon(binary, env, home)
            started = time.perf_counter()
            client = subprocess.Popen([str(binary)], env=env, cwd=home, stdin=subprocess.PIPE,
                                      stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
            first = queue.Queue()
            threading.Thread(target=lambda: first.put(client.stdout.readline()), daemon=True).start()
            assert first.get(timeout=60).startswith(b'[session')
            return (time.perf_counter() - started) * 1000
        finally:
            if client:
                client.stdin.close()
                try:
                    client.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    client.kill()
                    client.wait()
            stop_daemon(binary, env, home)


def main():
    OUTPUT.mkdir(parents=True, exist_ok=True)
    builds = {}
    for role, revision in [('main', '4b662d7ab8151802b8417e71378c8ec800834f34'), ('candidate', 'HEAD')]:
        source = OUTPUT / ('source-' + role)
        subprocess.run(['git', 'worktree', 'add', '--detach', str(source), revision], check=True)
        example = source / 'crates/sys/examples/paths-measure.rs'
        example.parent.mkdir(exist_ok=True)
        example.write_text(PROBE)
        env = dict(os.environ, CARGO_TARGET_DIR=str(OUTPUT / 'build'))
        subprocess.run(['cargo', 'build-release', '-p', 'a', '--features', 'fixtures'],
                       cwd=source, env=env, check=True)
        subprocess.run(['cargo', 'build-release', '-p', 'sys', '--example', 'paths-measure'],
                       cwd=source, env=env, check=True)
        directory = OUTPUT / role
        directory.mkdir()
        shutil.copy2(OUTPUT / 'build/release/a', directory / 'a')
        shutil.copy2(OUTPUT / 'build/release/examples/paths-measure', directory / 'paths-measure')
        builds[role] = dict(directory=str(directory), revision=subprocess.check_output(
            ['git', 'rev-parse', 'HEAD'], cwd=source, text=True).strip())
    records, resolver = [], {role: [] for role in builds}
    for trial in range(50):
        order = ['main', 'candidate'] if trial % 2 == 0 else ['candidate', 'main']
        for role in order:
            directory = Path(builds[role]['directory'])
            value = cold(directory / 'a')
            records.append(dict(trial=trial, role=role, cold_ms=value))
            env = dict(os.environ, A_HOME=str(OUTPUT / 'isolated-resolver'))
            elapsed = subprocess.check_output([str(directory / 'paths-measure')], env=env,
                                               text=True, timeout=30)
            resolver[role].append(float(elapsed))
        (OUTPUT / 'samples.json').write_text(json.dumps(dict(builds=builds, records=records,
                                                           resolver_ms=resolver), indent=2))
    pairs = [records[2*i:2*i+2] for i in range(50)]
    deltas = [next(v['cold_ms'] for v in p if v['role']=='candidate') -
              next(v['cold_ms'] for v in p if v['role']=='main') for p in pairs]
    rng = random.Random(0)
    bootstrap = sorted(statistics.median(rng.choices(deltas, k=len(deltas))) for _ in range(10000))
    summary = dict(rounds=50, builds=builds,
                   cold_median_ms={r: statistics.median(v['cold_ms'] for v in records if v['role']==r)
                                   for r in builds},
                   paired_delta_median_ms=statistics.median(deltas),
                   paired_delta_bootstrap_95_ms=[bootstrap[250], bootstrap[9749]],
                   resolver_median_ms={r:statistics.median(v) for r,v in resolver.items()})
    (OUTPUT / 'summary.json').write_text(json.dumps(summary, indent=2))
    print(json.dumps(summary, indent=2), flush=True)


if __name__ == '__main__':
    main()
