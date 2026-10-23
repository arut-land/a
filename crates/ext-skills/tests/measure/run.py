"""Hosted-only parser comparison. Never rewrites the recorded corpus.

Run with Python 3.13 + psutil; uses the repository's release profile and toolchain.
"""
import json
import os
from pathlib import Path
import statistics
import subprocess
import tempfile
import time
import psutil

root = Path(__file__).resolve().parent
corpus = sorted(root.parent.joinpath("corpus").glob("*.md"))
expected = json.loads(root.parent.joinpath("corpus/expected.json").read_text(encoding="utf-8"))
results = []
cargo = os.environ.get("PROBE_CARGO", "cargo")
with tempfile.TemporaryDirectory(prefix="skill-parser-") as tmp:
    candidates = ["custom", "event", "serde", "yaml", "saphyr", "temp-custom", "temp-maintained"]
    if os.name != "nt":
        candidates += ["signal-custom", "signal-maintained"]
    for parser in candidates:
        infrastructure = parser.startswith(("temp-", "signal-"))
        bin_name = "infrastructure" if infrastructure else "skill-parser-probe"
        target = Path(tmp) / "build"
        env = dict(os.environ, CARGO_TARGET_DIR=str(target), RUSTC_WRAPPER="", RUSTC_WORKSPACE_WRAPPER="")
        manifest = str(root / "Cargo.toml")
        command = [cargo, "build", "--release", "--manifest-path", manifest, "--features", parser, "--bin", bin_name,
                   "-Zbuild-std=std,panic_unwind", "-Zbuild-std-features=panic-unwind,optimize_for_size"]
        subprocess.run(command, env=env, check=True)
        binary = target / "release" / (bin_name + (".exe" if os.name == "nt" else ""))
        if not infrastructure:
            output = subprocess.check_output([str(binary), *map(str, corpus)], text=True, encoding="utf-8")
            actual = {path.name: json.loads(line) for path, line in zip(corpus, output.splitlines(), strict=True)}
            assert actual == expected, {"parser": parser, "mismatched": [key for key in expected if actual.get(key) != expected[key]]}
        rounds = []
        for _ in range(5):
            process = subprocess.Popen([str(binary)] + ([] if infrastructure else ["--bench", *map(str, corpus)]), stdout=subprocess.PIPE, text=True, encoding="utf-8")
            observer = psutil.Process(process.pid)
            peak = 0
            while process.poll() is None:
                try:
                    peak = max(peak, observer.memory_info().rss)
                except psutil.NoSuchProcess:
                    break
                time.sleep(0.005)
            timing = json.loads(process.communicate()[0])
            assert process.returncode == 0
            rounds.append(dict(**timing, peak_rss_bytes=peak))
        tree = subprocess.check_output([cargo, "tree", "--manifest-path", manifest, "--features", parser, "--edges", "normal", "--prefix", "none"], env=env, text=True, encoding="utf-8")
        results.append(dict(parser=parser, bytes=binary.stat().st_size, corpus=len(corpus), rounds=rounds,
                            median_ns=statistics.median(r["ns_per_operation" if infrastructure else "ns_per_header"] for r in rounds), dependency_tree=tree))
        print(json.dumps(results[-1]), flush=True)
        Path("parser-results.json").write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")
Path("parser-results.json").write_text(json.dumps(results, indent=2) + "\n")
