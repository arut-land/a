"""Release build policy, Cargo artifact discovery and executable staging."""
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys

STD = ["-Zbuild-std=std,panic_unwind",
       "-Zbuild-std-features=panic-unwind,optimize_for_size"]
SUFFIX = ".exe" if os.name == "nt" else ""


def release_arguments():
    return ["--release", *STD]


def executable(source, env=None, arguments=(), run=subprocess.run, name="a", kind="bin"):
    result = run(["cargo", "build", *release_arguments(), "-p", "a", *arguments,
                  "--message-format=json"], cwd=source, env=env,
                 check=True, stdout=subprocess.PIPE, text=True)
    artifacts = [record for line in result.stdout.splitlines()
                 if (record := json.loads(line)).get("executable")
                 and record.get("target", {}).get("name") == name
                 and kind in record["target"]["kind"]]
    if len(artifacts) != 1:
        raise ValueError(f"expected one {name} executable: {artifacts}")
    return Path(artifacts[0]["executable"])


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


def dumpbin():
    if tool := shutil.which("dumpbin"):
        return tool
    installer = Path(os.environ["ProgramFiles(x86)"]) / "Microsoft Visual Studio/Installer/vswhere.exe"
    location = subprocess.check_output([str(installer), "-latest", "-property", "installationPath"], text=True).strip()
    native = "arm64" if platform.machine().lower() == "arm64" else "x64"
    tools = sorted((Path(location) / "VC/Tools/MSVC").glob(f"*/bin/Host*/{native}/dumpbin.exe"), reverse=True)
    return str(tools[0])


def stage(binary, destination):
    shutil.copy2(binary, destination)
    retained = []
    for symbols, staged in [(Path(str(binary) + ".dSYM"), Path(str(destination) + ".dSYM")),
                            (Path(str(binary) + ".dwp"), Path(str(destination) + ".dwp")),
                            (binary.with_suffix(".pdb"), destination.with_suffix(".pdb"))]:
        if not symbols.exists():
            continue
        if symbols.is_dir():
            shutil.copytree(symbols, staged, dirs_exist_ok=True)
            retained.extend(p for p in staged.rglob("*") if p.is_file())
        else:
            shutil.copy2(symbols, staged)
            retained.append(staged)
    files = [destination, *retained]
    identity = dict(source=str(binary), files={str(p.relative_to(destination.parent)): dict(
        bytes=p.stat().st_size, sha256=hashlib.sha256(p.read_bytes()).hexdigest()) for p in files})
    # Native IDs let offline symbolication check the image/debug-file pairing.
    tool = "dwarfdump" if sys.platform == "darwin" else "readelf" if sys.platform == "linux" else dumpbin()
    if shutil.which(tool):
        args = ["--uuid"] if sys.platform == "darwin" else ["-n"] if sys.platform == "linux" else ["/nologo", "/headers"]
        paths = [str(binary)]
        if sys.platform == "darwin" and Path(str(binary) + ".dSYM").exists():
            paths.append(str(binary) + ".dSYM")
        result = subprocess.run([tool, *args, *paths], capture_output=True, text=True, check=False)
        identity["native_identity"] = dict(tool=tool, returncode=result.returncode,
                                           stdout=result.stdout, stderr=result.stderr)
    Path(str(destination) + ".identity.json").write_text(json.dumps(identity, indent=2) + "\n")
    return destination


def benchmark(source, env, target, overrides=(), features=(), destination=None, run=subprocess.run):
    result = run(["cargo", "test", *release_arguments(), *target,
                  *(["--features", ",".join(features)] if features else []),
                  "--no-run", "--message-format=json", *overrides],
                 cwd=source, env=env, check=True, stdout=subprocess.PIPE, text=True)
    executables = [Path(record["executable"]) for line in result.stdout.splitlines()
                   if (record := json.loads(line)).get("executable") and record.get("profile", {}).get("test")]
    if len(executables) != 1:
        raise ValueError(f"expected one test executable for {target}: {executables}")
    return stage(executables[0], destination) if destination else executables[0]


def probe(source, output, env, variant=None, memory_only=False, run=subprocess.run):
    """Build against real modules without adding benchmark APIs to the product."""
    import tomllib
    variant = variant or {}
    root = Path(env["CARGO_TARGET_DIR"]).resolve() / ("perf-memory-probe-source" if memory_only else "perf-probe-source")
    root.mkdir(parents=True, exist_ok=True)
    harness = Path(__file__).resolve().parent.as_posix()
    if memory_only:
        text = (f'#[path = {json.dumps(harness + "/memory-probe.rs")}] mod memory;\n'
                'fn main() { memory::run(&std::env::args().skip(1).collect::<Vec<_>>()); }\n')
    else:
        text = Path(__file__).with_name("probe.rs").read_text().replace("@SOURCE@", source.as_posix()).replace("@HARNESS@", harness)
    (root / "main.rs").write_text(text)
    dependencies = tomllib.loads((source / "Cargo.toml").read_text())["workspace"]["dependencies"]
    manifest = '[workspace]\n[package]\nname="perf-probe"\nversion="0.0.0"\nedition="2024"\n[[bin]]\nname="perf-probe"\npath="main.rs"\n[dependencies]\n'
    for name in (["protocol", "sys"] if memory_only else ["client", "protocol", "tui", "sys", "serde_json"]):
        value = dependencies[name]
        if isinstance(value, dict):
            value = dict(value)
            if name == "sys":
                selected = [feature.split("/", 1)[1] for feature in variant.get("features", []) if feature.startswith("sys/")]
                if selected:
                    value["features"] = [*value.get("features", []), *selected]
            if "path" in value:
                value["path"] = str(source / value["path"])
            encoded = "{ " + ", ".join(f"{key} = {json.dumps(item)}" for key, item in value.items()) + " }"
        else:
            encoded = json.dumps(value)
        manifest += f'{name} = {encoded}\n'
    if not memory_only:
        manifest += 'hypertext = { version="0.12.1", default-features=false, features=["alloc"] }\n'
    manifest += '\n[profile.release]\nopt-level="z"\nlto="fat"\ncodegen-units=1\ndebug="line-tables-only"\nsplit-debuginfo="packed"\nstrip="symbols"\n'
    (root / "Cargo.toml").write_text(manifest)
    shutil.copy2(source / "Cargo.lock", root / "Cargo.lock")
    result = run(["cargo", "build", *release_arguments(), "--manifest-path", str(root / "Cargo.toml"),
                  *variant_arguments(variant), "--message-format=json"], cwd=source, env=env,
                 check=True, stdout=subprocess.PIPE, text=True)
    binaries = [Path(record["executable"]) for line in result.stdout.splitlines()
                if (record := json.loads(line)).get("executable")]
    if len(binaries) != 1:
        raise ValueError(f"expected one workload probe: {binaries}")
    return stage(binaries[0], output / ("perf-probe" + SUFFIX))


if __name__ == "__main__":
    source = Path(__file__).resolve().parents[2]
    if sys.argv[1] == "bloat":
        subprocess.run(["cargo", "bloat", *release_arguments(), "-p", "a",
                        "--config", 'profile.release.strip="none"', "--crates", "-n", "20"], check=True)
    else:
        binary = executable(source)
        subprocess.run([sys.executable, str(Path(__file__).with_name(sys.argv[1] + ".py")), str(binary)], check=True)
