#!/usr/bin/env python3
"""Copy public diagnostics without changing private daemon file permissions."""
from pathlib import Path
import shutil
import sys


def stage(source, destination):
    for path in source.rglob("*"):
        if path.is_file() and path.suffix in (".log", ".json", ".jsonl", ".md"):
            target = destination / path.relative_to(source)
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, target)
            target.chmod(0o644)


if __name__ == "__main__":
    stage(Path(sys.argv[1]), Path(sys.argv[2]))
