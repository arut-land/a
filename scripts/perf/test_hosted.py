"""Hosted Cargo selection applies to actions and descendant processes alike."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

import hosted


@unittest.skipUnless(shutil.which("rustup"), "requires the installed native Rust toolchain")
class NativeTools(unittest.TestCase):
    def test_job_path_selects_native_cargo_for_descendants_and_metadata(self):
        with tempfile.TemporaryDirectory(prefix="native tools ") as directory:
            root = Path(directory)
            job_path = root / "github-path"
            env = dict(os.environ, GITHUB_PATH=str(job_path))
            subprocess.run([sys.executable, hosted.__file__, "--phase", "toolchain", "--runner", "test"],
                           env=env, check=True, capture_output=True, text=True)
            native = Path(job_path.read_text().strip())
            env["PATH"] = str(native) + os.pathsep + env["PATH"]
            (root / "Cargo.toml").write_text('[workspace]\nmembers=[]\n')
            # A separate interpreter models tools/actions that launch their own Cargo.
            result = subprocess.run([sys.executable, "-c", """
import json, shutil, subprocess
print(json.dumps(dict(cargo=shutil.which('cargo'), rustc=shutil.which('rustc'),
    version=subprocess.check_output(['rustc', '-vV'], text=True),
    metadata=json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--no-deps', '--format-version=1'], text=True)))))
"""], cwd=root, env=env, check=True, capture_output=True, text=True)
            result = json.loads(result.stdout)
            self.assertEqual(Path(result["cargo"]).parent, native)
            self.assertEqual(Path(result["rustc"]).parent, native)
            self.assertEqual(Path(result["metadata"]["workspace_root"]).resolve(), root.resolve())
            self.assertEqual(result["version"], subprocess.check_output(
                [str(native / ("rustc.exe" if os.name == "nt" else "rustc")), "-vV"], text=True))
            print("Native descendant toolchain:", result["cargo"], result["version"], flush=True)

    def test_native_path_precedes_cargo_consumers_in_the_workflow(self):
        # Scope this contract to measurement; autostart has its own toolchain setup.
        workflow = (hosted.ROOT / ".github/workflows/perf.yml").read_text().split("  measure:", 1)[1].split("  autostart:", 1)[0]
        selection = workflow.index("--phase toolchain")
        for consumer in ("cargo install", "Swatinem/rust-cache", "--phase build"):
            self.assertLess(selection, workflow.index(consumer))
        self.assertNotIn("PERF_CARGO", workflow)
        self.assertNotIn("mise exec --", workflow)


if __name__ == "__main__":
    unittest.main()
