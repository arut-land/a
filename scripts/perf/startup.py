"""Compare the shipped executable with a native process-launch floor."""
import os
from pathlib import Path
import shlex
import subprocess
import sys

binary = Path(sys.argv[1]).resolve()
if os.name == "nt":
    binary = binary.with_suffix(".exe")
floor = binary.with_name("process-floor" + (".exe" if os.name == "nt" else ""))
subprocess.run(["rustc", str(Path(__file__).with_name("floor.rs")), "-O", "-o", str(floor)], check=True)
quote = shlex.join
subprocess.run(["hyperfine", "-N", "--warmup", "50", "-r", "500", "--metrics", "all",
                "--export-json", str(binary.with_name("startup.json")),
                quote([str(binary), "--version"]), quote([str(floor)])], check=True)
