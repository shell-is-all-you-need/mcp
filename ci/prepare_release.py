from __future__ import annotations

import json
import os
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
PLACEHOLDER = "__GITHUB_REPOSITORY__"


def repository() -> str:
    value = (sys.argv[1] if len(sys.argv) > 1 else os.environ.get("GITHUB_REPOSITORY", "")).strip()
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", value):
        raise SystemExit("expected GitHub repository as owner/name")
    return value


def replace_text(path: str, repo: str) -> None:
    target = ROOT / path
    text = target.read_text(encoding="utf-8")
    prepared = text.replace(PLACEHOLDER, repo)
    if repo not in prepared:
        raise SystemExit(f"repository metadata missing from {path}")
    target.write_text(prepared, encoding="utf-8")


def replace_json(path: str, repo: str) -> None:
    target = ROOT / path
    data = json.loads(target.read_text(encoding="utf-8"))
    raw = json.dumps(data)
    prepared = raw.replace(PLACEHOLDER, repo)
    if repo not in prepared:
        raise SystemExit(f"repository metadata missing from {path}")
    target.write_text(json.dumps(json.loads(prepared), indent=2) + "\n", encoding="utf-8")


def main() -> None:
    repo = repository()
    replace_text("Cargo.toml", repo)
    replace_json("packages/npm/package.json", repo)
    for path in (
        "packages/python/pyproject.toml",
        "packages/python/src/shell_is_all_you_need/__init__.py",
    ):
        replace_text(path, repo)
    print(f"PASS prepared_release repository={repo}")


if __name__ == "__main__":
    main()
