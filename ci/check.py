from __future__ import annotations

import json
import re
from pathlib import Path
import tomllib

ROOT = Path(__file__).resolve().parents[1]
IGNORED_PARTS = {".git", "target", "node_modules", "dist", "build", "__pycache__", "media-work"}


def toml(path: str) -> dict[str, object]:
    return tomllib.loads((ROOT / path).read_text(encoding="utf-8"))


def json_file(path: str) -> dict[str, object]:
    return json.loads((ROOT / path).read_text(encoding="utf-8"))


def template_placeholders(template: str) -> list[str]:
    names: list[str] = []
    index = 0
    while index < len(template):
        if template.startswith("{{", index) or template.startswith("}}", index):
            index += 2
            continue
        if template[index] == "{":
            end = template.find("}", index + 1)
            assert end >= 0, f"unclosed placeholder in {template!r}"
            name = template[index + 1 : end]
            assert name and all(ch.isalnum() or ch in "_-." for ch in name), (
                f"invalid placeholder {{{name}}} in {template!r}"
            )
            if name not in names:
                names.append(name)
            index = end + 1
            continue
        assert template[index] != "}", f"unmatched }} in {template!r}"
        index += 1
    return names


def main() -> None:
    cargo = toml("Cargo.toml")
    version = cargo["package"]["version"]
    assert cargo["package"]["rust-version"] == "1.98.1"
    assert "dependencies" not in cargo, "the core Rust crate must stay dependency-free"

    npm = json_file("packages/npm/package.json")
    python = toml("packages/python/pyproject.toml")

    versions = {
        npm["version"],
        python["project"]["version"],
    }
    assert versions == {version}, f"package versions differ: core={version}, others={versions}"
    python_launcher = (ROOT / "packages/python/src/shell_is_all_you_need/__init__.py").read_text(encoding="utf-8")
    assert f'__version__ = "{version}"' in python_launcher
    assert npm["engines"]["node"] == ">=24"
    assert list(python["project"]["scripts"]) == ["shell-is-all-you-need"]
    repository = "shell-is-all-you-need/mcp"
    assert cargo["package"]["repository"] == f"https://github.com/{repository}"
    assert repository in npm["repository"]["url"]
    assert python["project"]["urls"]["Repository"] == f"https://github.com/{repository}"

    release = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
    assert re.search(r"dtolnay/rust-toolchain@[0-9a-f]{40}", release)
    assert re.search(r"rust-lang/crates-io-auth-action@[0-9a-f]{40}", release)
    assert re.search(r"pypa/gh-action-pypi-publish@[0-9a-f]{40}", release)
    assert "CARGO_REGISTRY_TOKEN" in release
    assert ("CRATES" + "_IO_TOKEN") not in release
    assert "id-token: write" in release
    assert 'node-version: "24"' in release
    assert "ci/prepare_release.py" in release

    config = json_file("mcp.example.json")
    expected = {
        "files": ["read", "write", "edit", "patch"],
        "search": ["glob", "grep"],
        "web": ["fetch", "search"],
        "shell": ["run"],
        "tasks": ["run"],
        "skills": ["list", "get"],
    }
    assert list(config["servers"]) == list(expected)
    for namespace, names in expected.items():
        args = config["servers"][namespace]["args"]
        actual = [args[index + 1] for index, value in enumerate(args[:-1]) if value == "--name"]
        assert actual == names, f"{namespace}: expected {names}, got {actual}"

        starts = [index for index, value in enumerate(args) if value == "--tool"]
        for position, start in enumerate(starts):
            end = starts[position + 1] if position + 1 < len(starts) else len(args)
            block = args[start:end]
            tool_name = block[block.index("--name") + 1]
            schema = json.loads(block[block.index("--input-schema") + 1])
            declared = set(schema.get("properties", {}))
            exec_index = block.index("--exec")
            used: set[str] = set()
            for template in block[exec_index + 2 :]:
                used.update(template_placeholders(template))
            assert used == declared, (
                f"{namespace}_{tool_name}: schema/template mismatch "
                f"declared={sorted(declared)} used={sorted(used)}"
            )
            assert set(schema.get("required", [])) == declared
            assert schema.get("additionalProperties") is False

    standalone = json_file("mcp.media.workflow.json")
    assert list(standalone["servers"]) == ["media"]
    media_args = standalone["servers"]["media"]["args"]
    assert [media_args[index + 1] for index, value in enumerate(media_args[:-1]) if value == "--name"] == [
        "fetch_funny", "generate_image", "edit_image", "compare_images"
    ]
    starts = [index for index, value in enumerate(media_args) if value == "--tool"]
    for position, start in enumerate(starts):
        end = starts[position + 1] if position + 1 < len(starts) else len(media_args)
        block = media_args[start:end]
        schema = json.loads(block[block.index("--input-schema") + 1])
        command = block[block.index("--exec") + 1 :]
        assert command[:2] == ["python3", "-c"], "standalone tools must have inline code"
        assert "media_workflow.py" not in " ".join(command)
        assert template_placeholders(command[2]) == [], "escape literal Python braces"
        compile(command[2].replace("{{", "{").replace("}}", "}"), "<inline MCP tool>", "exec")
        used = {name for template in command[3:] for name in template_placeholders(template)}
        assert used == set(schema["properties"]) == set(schema["required"])

    web_args = config["servers"]["web"]["args"]
    web_text = "\n".join(web_args)
    assert "https://lite.duckduckgo.com/lite/?q=" in web_text
    assert "urllib.request" in web_text
    assert "urlsplit" in web_text and "http" in web_text and "https" in web_text
    assert "curl" not in web_text and "wget" not in web_text

    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    assert "## Project layout" not in readme
    assert "shell_shell" in readme and "shell_run" in readme
    assert "web_fetch" in readme and "web_search" in readme and "files_patch" in readme

    for path in ROOT.rglob("*"):
        if not path.is_file() or IGNORED_PARTS.intersection(path.parts):
            continue
        if path.suffix.lower() in {".png", ".jpg", ".jpeg", ".zip", ".gz", ".pyc"}:
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        forbidden = "o" + "x" + "y"
        assert forbidden not in text.lower(), f"forbidden legacy name in {path.relative_to(ROOT)}"

    for path in ROOT.rglob("*"):
        if not path.is_file() or IGNORED_PARTS.intersection(path.parts):
            continue
        if path.suffix.lower() in {".png", ".jpg", ".jpeg", ".zip", ".gz", ".pyc"}:
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        forbidden_alias = "s" + "i" + "a" + "y" + "n"
        assert forbidden_alias not in text.lower(), f"forbidden short alias in {path.relative_to(ROOT)}"

    print(f"PASS metadata_and_examples version={version}")


if __name__ == "__main__":
    main()
