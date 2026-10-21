"""Refresh verbatim headers from the pinned source manifest, without skill bodies."""
import json
from pathlib import Path
from urllib.request import urlopen

root = Path(__file__).resolve().parent
for entry in json.loads(root.joinpath("sources.json").read_text()):
    url = entry["source"].replace("https://github.com/", "https://raw.githubusercontent.com/").replace("/blob/", "/")
    lines = urlopen(url).read().decode("utf-8").splitlines(keepends=True)
    assert lines[0].strip() == "---", url
    end = next(i for i in range(1, len(lines)) if lines[i].strip() == "---")
    root.joinpath(entry["file"]).write_text("".join(lines[:end + 1]))
