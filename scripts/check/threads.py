"""Features use runtime tasks and timers. Each native blocking exception names
one call site, its function and its API; another call in the file is not exempt.
"""

import pathlib
import re
import sys
from collections import Counter
from typing import NamedTuple

ROOT = pathlib.Path(__file__).resolve().parents[2]
OWNERS = ("crates/rt/", "crates/sys/")
APIS = {"spawn", "scope", "sleep", "Builder"}
# id: (path, function, API, reason)
ALLOWED = {
    "repl-stdin": ("apps/cli/src/main.rs", "read_stdin", "spawn",
                   "The line REPL reads blocking platform stdin until process exit."),
    "pipeline-stdin": ("apps/cli/src/once.rs", "run", "spawn",
                       "Pipeline stdin has no portable async registration."),
    "terminal-giveback": ("apps/tui/src/term.rs", "end", "spawn",
                         "Restoration may block on an unread terminal."),
    "terminal-deadline": ("apps/tui/src/term.rs", "end", "recv_timeout",
                         "Reraising must not wait indefinitely for restoration."),
    "terminal-input": ("apps/tui/src/term.rs", "spawn_input", "spawn",
                      "Native input multiplexes terminal events and signals."),
    "bridge-stdin": ("crates/client/src/lib.rs", "bridge", "spawn",
                    "The bridge copies portable blocking stdin until either side closes."),
}


class Token(NamedTuple):
    value: str
    line: int


class Finding(NamedTuple):
    line: int
    function: str
    api: str


def is_test(path):
    return ("tests" in path.parts or "benches" in path.parts or "examples" in path.parts
            or path.name == "tests.rs" or path.name.endswith("_tests.rs"))


def tokens(source):
    """Lex Rust punctuation and names, ignoring comments and literal contents."""
    result = []
    pos, line = 0, 1
    while pos < len(source):
        start = pos
        if source.startswith("//", pos):
            end = source.find("\n", pos)
            pos = len(source) if end < 0 else end
        elif source.startswith("/*", pos):
            pos += 2
            depth = 1
            while pos < len(source) and depth:
                if source.startswith("/*", pos):
                    depth += 1
                    pos += 2
                elif source.startswith("*/", pos):
                    depth -= 1
                    pos += 2
                else:
                    pos += 1
        elif match := re.match(r'(?:br|r)(#*)"', source[pos:]):
            closing = '"' + match[1]
            end = source.find(closing, pos + len(match[0]))
            pos = len(source) if end < 0 else end + len(closing)
        elif source[pos] == '"':
            pos += 1
            while pos < len(source):
                if source[pos] == "\\":
                    pos += 2
                elif source[pos] == '"':
                    pos += 1
                    break
                else:
                    pos += 1
        elif match := re.match(r"'(?:\\u\{[0-9a-fA-F_]+\}|\\x[0-9a-fA-F]{2}|\\.|[^'\\\n])'", source[pos:]):
            pos += len(match[0])
        elif match := re.match(r"(?:r#)?[^\W\d]\w*|::", source[pos:]):
            result.append(Token(match[0].removeprefix("r#"), line))
            pos += len(match[0])
        else:
            if not source[pos].isspace():
                result.append(Token(source[pos], line))
            pos += 1
        line += source[start:pos].count("\n")
    return result


def closing(items, start, left="{", right="}"):
    depth = 1
    for pos in range(start + 1, len(items)):
        depth += (items[pos].value == left) - (items[pos].value == right)
        if depth == 0:
            return pos
    raise ValueError(f"unclosed {left} at line {items[start].line}")


def requires_test(expression):
    if expression == ["test"]:
        return True
    if (len(expression) < 3 or expression[0] not in ("all", "any")
            or expression[1] != "(" or expression[-1] != ")"):
        return False
    parts, part, depth = [], [], 0
    for value in expression[2:-1] + [","]:
        if value == "," and depth == 0:
            if part:
                parts.append(requires_test(part))
            part = []
        else:
            part.append(value)
            depth += (value == "(") - (value == ")")
    return any(parts) if expression[0] == "all" else bool(parts) and all(parts)


def imports(items):
    """Expand a use tree into local names and the paths they import."""
    values = [item.value for item in items]
    pos = 0
    bindings = []

    def tree(prefix=()):
        nonlocal pos
        path = list(prefix)
        alias = None
        while pos < len(values) and values[pos] not in (",", "}"):
            value = values[pos]
            pos += 1
            if value == "{":
                while pos < len(values) and values[pos] != "}":
                    tree(tuple(path))
                    if pos < len(values) and values[pos] == ",":
                        pos += 1
                pos += 1
                return
            if value == "as":
                alias = values[pos]
                pos += 1
                break
            if value != "::":
                path.append(value)
        if path and path[-1] == "self":
            path.pop()
        if path:
            bindings.append((alias or path[-1], tuple(path)))

    tree()
    return bindings


def findings(source):
    items = tokens(source)
    found = []

    def resolve(path, aliases, modules, module):
        base = aliases
        if path[0] == "crate":
            base = modules[0] if modules else aliases
            path = path[1:]
        elif path[0] == "self":
            base = aliases if module or not modules else modules[-1]
            path = path[1:]
        elif path[0] == "super":
            parents = modules if module else modules[:-1]
            count = 0
            while path and path[0] == "super":
                count += 1
                path = path[1:]
            base = parents[-count] if count <= len(parents) else {}
        return base.get(path[0], (path[0],)) + path[1:] if path else ()

    def scope(start, end, inherited, function="", modules=(), module=False):
        bindings, children, refs = [], [], []
        pos, next_function, next_module = start, None, False
        while pos < end:
            value = items[pos].value
            if value == "#" and pos + 1 < end and items[pos + 1].value == "[":
                attribute_end = closing(items, pos + 1, "[", "]")
                attribute = [t.value for t in items[pos + 2:attribute_end]]
                if attribute[:2] == ["cfg", "("] and requires_test(attribute[2:-1]):
                    pos = attribute_end + 1
                    while pos < end and items[pos].value not in ("{", ";"):
                        pos += 1
                    if pos < end and items[pos].value == "{":
                        pos = closing(items, pos)
                    pos += 1
                    continue
                pos = attribute_end + 1
                continue
            if value == "use":
                stop = pos + 1
                while stop < end and items[stop].value != ";":
                    stop += 1
                bindings.extend(imports(items[pos + 1:stop]))
                pos = stop + 1
                continue
            if value == "extern" and [t.value for t in items[pos:pos + 4]] == ["extern", "crate", "std", "as"]:
                bindings.append((items[pos + 4].value, ("std",)))
            if value == "fn" and pos + 1 < end and items[pos + 1].value.isidentifier():
                next_function = items[pos + 1].value
            if value == "mod" and pos + 1 < end and items[pos + 1].value.isidentifier():
                next_module = True
            if value == "{":
                stop = closing(items, pos)
                children.append((pos + 1, stop, next_function or function, next_module))
                next_function = None
                next_module = False
                pos = stop + 1
                continue
            if value == ";":
                next_function = None
                next_module = False
            refs.append(pos)
            pos += 1
        aliases = dict(inherited)
        # A use may refer to an alias declared later in this scope.
        for _ in range(len(bindings) + 1):
            previous = dict(aliases)
            for name, path in bindings:
                target = resolve(path, aliases, modules, module)
                if name == "*":
                    if target == ("std", "thread", "*"):
                        aliases.update((api, ("std", "thread", api)) for api in APIS)
                else:
                    aliases[name] = target
            if aliases == previous:
                break
        consumed = -1
        for pos in refs:
            if pos <= consumed:
                continue
            value = items[pos].value
            if value == "." and pos + 2 < end and items[pos + 1].value == "recv_timeout":
                found.append(Finding(items[pos + 1].line, function, "recv_timeout"))
                continue
            path = [value]
            stop = pos
            while stop + 2 < end and items[stop + 1].value == "::":
                path.append(items[stop + 2].value)
                stop += 2
            target = resolve(tuple(path), aliases, modules, module)
            if target[:2] == ("std", "thread") and len(target) > 2 and target[2] in APIS:
                found.append(Finding(items[pos].line, function, target[2]))
            consumed = stop
        child_modules = modules + (aliases,) if module else modules
        for child_start, child_end, child_function, child_module in children:
            scope(child_start, child_end, aliases, child_function, child_modules, child_module)

    scope(0, len(items), {"std": ("std",), "thread": ("std", "thread")}, module=True)
    return sorted(found)


def exceptions(path, source, found, used):
    unexpected = []
    lines = source.splitlines()
    for finding in found:
        marker = re.fullmatch(r"\s*// thread-exception: ([\w-]+)\s*",
                              lines[finding.line - 2] if finding.line > 1 else "")
        name = marker[1] if marker else None
        allowed = ALLOWED.get(name)
        if allowed and allowed[:3] == (path, finding.function, finding.api) and not used[name]:
            used[name] += 1
        else:
            unexpected.append(finding)
    return unexpected


def main():
    used = Counter()
    failed = False
    for path in sorted(ROOT.glob("**/*.rs")):
        rel = path.relative_to(ROOT)
        if (rel.parts[0] not in ("apps", "crates") or rel.as_posix().startswith(OWNERS)
                or "target" in rel.parts or is_test(rel)):
            continue
        source = path.read_text(encoding="utf-8")
        for finding in exceptions(rel.as_posix(), source, findings(source), used):
            print(f"{rel}:{finding.line}: {finding.function}: {finding.api}")
            failed = True
    for name in sorted(ALLOWED.keys() - used.keys()):
        print(f"{ALLOWED[name][0]}: unused thread exception {name}")
        failed = True
    if failed:
        print("a feature owns a thread or a timer: use its runtime")
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
