"""Release build policy, Cargo artifact discovery and executable staging."""
import json
import os
from pathlib import Path
import shutil
import subprocess

STD = ["-Zbuild-std=std,panic_unwind",
       "-Zbuild-std-features=panic-unwind,optimize_for_size"]
SUFFIX = ".exe" if os.name == "nt" else ""


def environment(target):
    env = dict(os.environ, CARGO_TARGET_DIR=str(target),
               CARGO_PROFILE_DEV_CODEGEN_BACKEND="llvm")
    env.setdefault("CARGO_BUILD_JOBS", "2")
    if os.name == "nt":
        # snmalloc checks Git Bash's MSYSTEM before the MSVC target.
        env.pop("MSYSTEM", None)
    return env


def variant_arguments(variant):
    config = list(variant.get("config", []))
    if flags := variant.get("rustflags", []):
        config.append("profile.release.rustflags=" + json.dumps(["-Cllvm-args=-addrsig", *flags]))
    return [arg for value in config for arg in ("--config", value)]


def stage(binary, destination):
    shutil.copy2(binary, destination)
    symbols = Path(str(binary) + ".dSYM")
    if symbols.exists():
        shutil.copytree(symbols, str(destination) + ".dSYM", dirs_exist_ok=True)
    return destination


def benchmark(source, env, target, overrides=(), features=(), destination=None, run=subprocess.run):
    result = run(["cargo", "test", "--release", *STD, *target,
                  *(["--features", ",".join(features)] if features else []),
                  "--no-run", "--message-format=json", *overrides],
                 cwd=source, env=env, check=True, stdout=subprocess.PIPE, text=True)
    executables = [Path(record["executable"]) for line in result.stdout.splitlines()
                   if (record := json.loads(line)).get("executable") and record.get("profile", {}).get("test")]
    if len(executables) != 1:
        raise ValueError(f"expected one test executable for {target}: {executables}")
    return stage(executables[0], destination) if destination else executables[0]
