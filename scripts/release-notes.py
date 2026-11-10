#!/usr/bin/env python3
"""Print one version's section of CHANGELOG.md as GitHub release notes.

    python3 scripts/release-notes.py 5.3.0 [CHANGELOG.md]

The section runs from the `## 5.3.0 (date)` heading to the next `## `
heading. GitHub renders every newline in a release body as a line break,
so the changelog's wrapped paragraphs and list items are joined back into
single lines; headings, tables, code blocks and blank lines are left alone.
Prints nothing if the version has no section.
"""
import re
import sys


def section(text: str, version: str) -> list[str]:
    out, on = [], False
    for line in text.splitlines():
        if line.startswith("## "):
            on = line.split()[1] == version
            continue
        if on:
            out.append(line)
    return out


def unwrap(lines: list[str]) -> list[str]:
    out: list[str] = []
    fenced = False
    for line in lines:
        starts_block = (
            not line.strip()
            or line.lstrip().startswith(("#", "|", "```", "- ", "* ", "> "))
            or re.match(r"\s*\d+\. ", line)
        )
        if line.lstrip().startswith("```"):
            fenced = not fenced
            out.append(line)
        elif fenced or starts_block or not out or not out[-1].strip():
            out.append(line)
        elif out[-1].lstrip().startswith(("#", "|", "```")):
            out.append(line)
        else:
            out[-1] = out[-1].rstrip() + " " + line.strip()
    return out


def main() -> None:
    version = sys.argv[1].removeprefix("v")
    path = sys.argv[2] if len(sys.argv) > 2 else "CHANGELOG.md"
    with open(path, encoding="utf-8") as f:
        lines = unwrap(section(f.read(), version))
    text = "\n".join(lines).strip()
    if text:
        print(text)


if __name__ == "__main__":
    main()
