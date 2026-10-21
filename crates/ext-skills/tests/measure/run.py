"""Run from the repository root; fresh targets, no compiler wrapper/cache.
Uses the shipped build-std and release settings. Does not run the agent.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

root = Path(__file__).resolve().parent
corpus = sorted(root.parent.joinpath("corpus").glob("*.md"))
results = []
reference = None
with tempfile.TemporaryDirectory(prefix="skill-parser-") as tmp:
    for parser in ("custom", "serde", "yaml"):
        target = Path(tmp) / parser
        env = dict(os.environ, CARGO_BUILD_JOBS="2", CARGO_TARGET_DIR=str(target), RUSTC_WRAPPER="", RUSTC_WORKSPACE_WRAPPER="")
        command = ["rustup", "run", "nightly-2026-10-17", "cargo", "build", "--release", "--manifest-path", str(root / "Cargo.toml"), "--features", parser, "-Zbuild-std=std,panic_unwind", "-Zbuild-std-features=panic-unwind,optimize_for_size"]
        start = time.monotonic()
        subprocess.run(command, env=env, check=True)
        seconds = time.monotonic() - start
        binary = target / "release/skill-parser-probe"
        output = subprocess.check_output([str(binary), *map(str, corpus)], text=True)
        if reference is None:
            reference = output
        assert output == reference, parser
        results.append(dict(parser=parser, bytes=binary.stat().st_size, seconds=round(seconds, 2), corpus=len(corpus)))
        print(json.dumps(results[-1]), flush=True)
    root.parent.joinpath("corpus/expected.json").write_text(json.dumps({path.name: json.loads(line) for path, line in zip(corpus, output.splitlines(), strict=True)}, indent=2) + "\n")
print(json.dumps(results, indent=2))
