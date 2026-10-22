"""Check the shipped bytes on every native target."""
import os
from pathlib import Path
import sys

binary = Path(sys.argv[1] + (".exe" if os.name == "nt" else ""))
content = binary.read_bytes()
assert b"Waits for a number of seconds." not in content, "sleep fixture in shipped binary"
assert b"Edits a text file by exact replacement." in content, "missing shipped edit tool"
print(f"a: {len(content) // 1024} KiB of 4096 KiB ({len(content)} bytes)")
assert len(content) <= 4096 * 1024, "shipped binary exceeds 4096 KiB"
