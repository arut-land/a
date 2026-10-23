"""Temporary hosted repetition of the early-exit status acquisition invariant."""
import json
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[2]
OUTPUT = ROOT / "target/paths-stress"
TEST = "process::tests::an_exited_watch_waits_for_reapable_status"
OUTPUT.mkdir(parents=True, exist_ok=True)
results = {}
for role, revision, count in [("before", "4d978b0b94290ecbf5b851f08b6d987940c2da0c", 100),
                              ("after", "HEAD", 500)]:
    source = OUTPUT / role
    subprocess.run(["git", "worktree", "add", "--detach", str(source), revision], check=True)
    env = dict(os.environ, CARGO_TARGET_DIR=str(OUTPUT / "build"))
    built = subprocess.run(["cargo", "test", "-p", "ext-shell", "--lib", "--no-run",
                            "--message-format=json"], cwd=source, env=env,
                           text=True, stdout=subprocess.PIPE, check=True)
    binaries = [v['executable'] for line in built.stdout.splitlines()
                if (v := json.loads(line)).get('executable') and v.get('profile', {}).get('test')]
    assert len(binaries) == 1, binaries
    failures = []
    for trial in range(count):
        sample = subprocess.run([binaries[0], "--exact", TEST], capture_output=True,
                                text=True, timeout=30)
        if sample.returncode:
            failures.append(dict(trial=trial, stdout=sample.stdout, stderr=sample.stderr))
    results[role] = dict(revision=subprocess.check_output(['git', 'rev-parse', 'HEAD'],
                                                         cwd=source, text=True).strip(),
                         samples=count, failures=failures)
    (OUTPUT / 'reaping.json').write_text(json.dumps(results, indent=2))
    print(role, count, 'samples,', len(failures), 'failures', flush=True)
assert not results['after']['failures'], results['after']
