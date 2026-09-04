#!/usr/bin/env python3
"""Keep code links in the docs pointing at the right lines.

A code link is written as `[file.rs::Symbol](path/to/file.rs#L10-L20)`. The text names the file
and a symbol in it (`Book::place`, `ConfirmationGate::intercept`, `unsupported_numbers`,
`Retention`); the anchor is derived. This script resolves every such link against the current
source and rewrites the anchor, or with `--check` fails when any anchor is stale, so a link can
never quietly point at the wrong lines after a refactor. Run `make docmap` after moving code.
"""
import re
import sys
from pathlib import Path

# script.md is the presenter's private, git-ignored notes; it is checked when present.
DOCS = ["README.md", *sorted(str(p) for p in Path("docs").glob("*.md")), "script.md"]
LINK = re.compile(r"\[([\w./-]+\.rs)::([\w:]+)\]\(([^)#\s]+\.rs)(#L\d+(?:-L\d+)?)?\)")
ITEM = re.compile(r"^\s*(?:pub(?:\([\w:]+\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:fn|struct|enum|const|static|trait|type|mod)\s+{name}\b")
IMPL = re.compile(r"^\s*(?:pub\s+)?impl(?:<[^>]*>)?\s+(?:[\w:<>, ]+\s+for\s+)?{name}\b")


def braces(line, in_string):
    """Net brace count of a line, ignoring braces inside string and char literals and after `//`.
    Returns (delta, in_string_at_end)."""
    delta = 0
    i = 0
    while i < len(line):
        ch = line[i]
        if in_string:
            if ch == "\\":
                i += 2
                continue
            if ch == '"':
                in_string = False
        elif ch == '"':
            in_string = True
        elif ch == "'" and i + 2 < len(line) and line[i + 2] == "'":
            i += 3  # a char literal such as '{'
            continue
        elif ch == "'" and i + 3 < len(line) and line[i + 1] == "\\" and line[i + 3] == "'":
            i += 4
            continue
        elif ch == "/" and line[i : i + 2] == "//":
            break
        elif ch == "{":
            delta += 1
        elif ch == "}":
            delta -= 1
        i += 1
    return delta, in_string


def item_end(lines, start):
    """Line index (inclusive) where the item starting at `start` ends: the matching brace, or the
    semicolon for a one-line item."""
    depth = 0
    seen_brace = False
    in_string = False
    for i in range(start, len(lines)):
        line = lines[i]
        delta, in_string = braces(line, in_string)
        if "{" in line or delta != 0:
            seen_brace = seen_brace or delta > 0 or depth > 0
        depth += delta
        if seen_brace and depth <= 0:
            return i
        if not seen_brace and not in_string and line.rstrip().endswith(";"):
            return i
    return len(lines) - 1


def find_item(lines, name, lo=0, hi=None):
    pat = re.compile(ITEM.pattern.format(name=re.escape(name)))
    hi = len(lines) if hi is None else hi
    for i in range(lo, hi):
        if pat.match(lines[i]):
            return i
    return None


def resolve(path, symbol):
    lines = Path(path).read_text().splitlines()
    parts = symbol.split("::")
    if len(parts) == 1:
        start = find_item(lines, parts[0])
        if start is None:
            return None
        return start, item_end(lines, start)
    owner, member = parts[-2], parts[-1]
    impl_pat = re.compile(IMPL.pattern.format(name=re.escape(owner)))
    for i, line in enumerate(lines):
        if impl_pat.match(line):
            end = item_end(lines, i)
            start = find_item(lines, member, i, end + 1)
            if start is not None:
                return start, item_end(lines, start)
    # A struct or enum field-less reference such as `Type::Variant` falls back to the type.
    start = find_item(lines, owner)
    if start is not None:
        return start, item_end(lines, start)
    return None


def process(doc, check):
    text = Path(doc).read_text()
    stale = []
    base = Path(doc).parent

    def fix(m):
        shown_file, symbol, path, anchor = m.groups()
        real = (base / path).resolve()
        if not real.exists():
            stale.append(f"{doc}: {path} does not exist")
            return m.group(0)
        if real.name != shown_file.split("/")[-1]:
            stale.append(f"{doc}: link text names {shown_file} but points at {path}")
        span = resolve(real, symbol)
        if span is None:
            stale.append(f"{doc}: {symbol} not found in {path}")
            return m.group(0)
        want = f"#L{span[0] + 1}-L{span[1] + 1}"
        if check and anchor != want:
            stale.append(f"{doc}: {path}::{symbol} is {want}, link says {anchor}")
        return f"[{shown_file}::{symbol}]({path}{want})"

    new = LINK.sub(fix, text)
    if not check and new != text:
        Path(doc).write_text(new)
    return stale


def main():
    check = "--check" in sys.argv
    problems = []
    links = 0
    for doc in DOCS:
        if Path(doc).exists():
            links += len(LINK.findall(Path(doc).read_text()))
            problems.extend(process(doc, check))
    if check and problems:
        print("\n".join(problems))
        print(f"{len(problems)} stale code links; run `make docmap`")
        sys.exit(1)
    if not check:
        print(f"{links} code links resolved" + (f"; {len(problems)} could not be" if problems else ""))
        for p in problems:
            print("  " + p)
        sys.exit(1 if problems else 0)
    print(f"{links} code links point at the right lines")


if __name__ == "__main__":
    main()
