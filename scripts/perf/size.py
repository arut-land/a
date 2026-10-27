"""Report shipped bytes and verify fixture exclusion on every native target."""
from pathlib import Path
import sys

binary = Path(sys.argv[1])
content = binary.read_bytes()
assert b"Waits for a number of seconds." not in content, "sleep fixture in shipped binary"
assert b"Edits a text file by exact replacement." in content, "missing shipped edit tool"
print(f"a: {len(content) // 1024} KiB ({len(content)} bytes)")
