"""No feature owns a thread or a timer loop: outside `rt`, `sys` and tests,
nothing spawns a thread, sleeps one, or waits on a channel with a timeout.
Such work is a task, a timer or a blocking job on the runtime it was given.
The few places that cannot be are allowed below, each with its reason."""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
BANNED = re.compile(r"\bthread::(spawn|sleep|Builder)\b|\.recv_timeout\(")
OWNERS = ("crates/rt/", "crates/sys/")
# path: why it cannot be a task.
ALLOWED = {
    "apps/cli/src/main.rs": "The line REPL reads blocking platform stdin until process exit.",
    "apps/cli/src/once.rs": "Pipeline stdin may be a terminal or file and has no portable async registration.",
    "apps/tui/src/term.rs": "Native terminal input multiplexes signals; terminal restoration has a bounded give-back before reraising.",
    "crates/client/src/lib.rs": "The ssh bridge copies portable blocking stdin and exits when either direction closes.",
}


def is_test(path):
    parts = path.parts
    return "tests" in parts or "benches" in parts or "examples" in parts or path.name == "tests.rs" or path.name.endswith("_tests.rs")


def code(text):
    """The text with `#[cfg(test)]` items blanked, line numbers kept."""
    lines = text.splitlines()
    out = []
    skip = 0
    i = 0
    while i < len(lines):
        line = lines[i]
        if re.match(r"\s*#\[cfg\((all\()?test\b", line):
            # The item after the attribute, to its closing brace or semicolon.
            depth = 0
            opened = False
            j = i + 1
            out.append("")
            while j < len(lines):
                depth += lines[j].count("{") - lines[j].count("}")
                opened |= "{" in lines[j]
                out.append("")
                if (opened and depth <= 0) or (not opened and lines[j].rstrip().endswith(";")):
                    break
                j += 1
            i = j + 1
            continue
        out.append(line)
        i += 1
    return out


def main():
    found = []
    for path in sorted(ROOT.glob("**/*.rs")):
        rel = path.relative_to(ROOT)
        if rel.parts[0] not in ("apps", "crates") or str(rel).startswith(OWNERS):
            continue
        if "target" in rel.parts or is_test(rel):
            continue
        for number, line in enumerate(code(path.read_text(encoding="utf-8")), 1):
            if line.lstrip().startswith("//"):
                continue
            if BANNED.search(line):
                found.append((str(rel.as_posix()), number, line.strip()))
    unexpected = [f for f in found if f[0] not in ALLOWED]
    for rel, number, line in unexpected:
        print(f"{rel}:{number}: {line}")
    stale = set(ALLOWED) - {f[0] for f in found}
    for rel in sorted(stale):
        print(f"{rel}: allowed but no longer needs it")
    if unexpected or stale:
        print("a feature owns a thread or a timer: make it a task, a timer or a blocking job on its runtime")
        return 1
    return 0


sys.exit(main())
